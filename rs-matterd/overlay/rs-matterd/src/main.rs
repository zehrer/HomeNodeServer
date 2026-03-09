use core::pin::pin;

use std::env;
use std::fs;
use std::net::UdpSocket;
use std::path::PathBuf;

use embassy_futures::select::{select, select4};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use log::info;
use rand::RngCore;
use rs_matter::crypto::{default_crypto, Crypto};
use rs_matter::dm::clusters::basic_info::BasicInfoConfig;
use rs_matter::dm::clusters::desc::{self, ClusterHandler as _};
use rs_matter::dm::clusters::level_control::LevelControlHooks;
use rs_matter::dm::clusters::net_comm::NetworkType;
use rs_matter::dm::clusters::on_off::{self, test::TestOnOffDeviceLogic, OnOffHooks};
use rs_matter::dm::devices::test::{DAC_PRIVKEY, TEST_DEV_ATT, TEST_DEV_DET};
use rs_matter::dm::devices::DEV_TYPE_ON_OFF_LIGHT;
use rs_matter::dm::endpoints;
use rs_matter::dm::events::NO_EVENTS;
use rs_matter::dm::networks::unix::UnixNetifs;
use rs_matter::dm::subscriptions::DefaultSubscriptions;
use rs_matter::dm::IMBuffer;
use rs_matter::dm::{
    Async, AsyncHandler, AsyncMetadata, DataModel, Dataver, EmptyHandler, Endpoint, EpClMatcher,
    Node,
};
use rs_matter::error::Error;
use rs_matter::pairing::qr::QrTextType;
use rs_matter::pairing::DiscoveryCapabilities;
use rs_matter::persist::{Psm, NO_NETWORKS};
use rs_matter::respond::DefaultResponder;
use rs_matter::sc::pase::MAX_COMM_WINDOW_TIMEOUT_SECS;
use rs_matter::transport::MATTER_SOCKET_BIND_ADDR;
use rs_matter::utils::init::InitMaybeUninit;
use rs_matter::utils::select::Coalesce;
use rs_matter::utils::storage::pooled::PooledBuffers;
use rs_matter::{clusters, devices, BasicCommData, Matter, MATTER_PORT};
use static_cell::StaticCell;

mod mdns;

const DEFAULT_SETUP_PIN: u32 = 20_202_021;
const DEFAULT_DISCRIMINATOR: u16 = 3840;
const DEFAULT_DEVICE_NAME: &str = "HomeNodeServer";
const DEFAULT_PRODUCT_NAME: &str = "rs-matterd";
const DEFAULT_VENDOR_NAME: &str = "HomeNode";

static MATTER: StaticCell<Matter> = StaticCell::new();
static BUFFERS: StaticCell<PooledBuffers<10, NoopRawMutex, IMBuffer>> = StaticCell::new();
static SUBSCRIPTIONS: StaticCell<DefaultSubscriptions> = StaticCell::new();
static PSM: StaticCell<Psm<4096>> = StaticCell::new();

fn main() -> Result<(), Error> {
    let thread = std::thread::Builder::new()
        .stack_size(550 * 1024)
        .spawn(run)
        .unwrap();

    thread.join().unwrap()
}

fn run() -> Result<(), Error> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let setup_pin = read_u32("RS_MATTERD_SETUP_PIN", DEFAULT_SETUP_PIN);
    let discriminator = read_u16("RS_MATTERD_DISCRIMINATOR", DEFAULT_DISCRIMINATOR);
    let state_dir = PathBuf::from(
        env::var("RS_MATTERD_STATE_DIR").unwrap_or_else(|_| String::from("/var/lib/rs-matterd")),
    );

    fs::create_dir_all(&state_dir).map_err(|_| rs_matter::error::ErrorCode::StdIoError)?;

    let dev_comm = build_commissioning_data(setup_pin, discriminator);

    let matter = MATTER.uninit().init_with(Matter::init(
        &RS_MATTERD_DEV_DET,
        dev_comm,
        &TEST_DEV_ATT,
        rs_matter::utils::epoch::sys_epoch,
        MATTER_PORT,
    ));

    matter.initialize_transport_buffers()?;

    let buffers = BUFFERS.uninit().init_with(PooledBuffers::init(0));
    let subscriptions = SUBSCRIPTIONS
        .uninit()
        .init_with(DefaultSubscriptions::init());

    let crypto = default_crypto::<NoopRawMutex, _>(rand::thread_rng(), DAC_PRIVKEY);
    let mut rand = crypto.rand()?;

    let on_off_handler = on_off::OnOffHandler::new_standalone(
        Dataver::new_rand(&mut rand),
        1,
        TestOnOffDeviceLogic::new(true),
    );

    let dm = DataModel::new(
        matter,
        &crypto,
        buffers,
        subscriptions,
        NO_EVENTS,
        dm_handler(rand, &on_off_handler),
    );

    let responder = DefaultResponder::new(&dm);
    let mut respond = pin!(responder.run::<4, 4>());
    let mut dm_job = pin!(dm.run());

    let socket = async_io::Async::<UdpSocket>::bind(MATTER_SOCKET_BIND_ADDR)?;
    let mut mdns = pin!(mdns::run_mdns(matter, &crypto, &dm));
    let mut transport = pin!(matter.run(&crypto, &socket, &socket));

    let psm = PSM.uninit().init_with(Psm::init());
    psm.load(&state_dir, matter, NO_NETWORKS, NO_EVENTS)?;

    log_startup(
        state_dir.as_path(),
        setup_pin,
        discriminator,
        matter.is_commissioned(),
    );

    if !matter.is_commissioned() {
        matter.print_standard_qr_text(DiscoveryCapabilities::IP)?;
        matter.print_standard_qr_code(QrTextType::Unicode, DiscoveryCapabilities::IP)?;
        matter.open_basic_comm_window(MAX_COMM_WINDOW_TIMEOUT_SECS, &crypto, &dm)?;
        info!("commissioning enabled; use chip-tool pairing onnetwork or pairing ethernet");
    } else {
        info!(
            "loaded existing commissioning state from {}",
            state_dir.display()
        );
    }

    let mut persist = pin!(psm.run(&state_dir, matter, NO_NETWORKS, NO_EVENTS));

    let all = select4(
        &mut transport,
        &mut mdns,
        &mut persist,
        select(&mut respond, &mut dm_job).coalesce(),
    );

    futures_lite::future::block_on(all.coalesce())
}

fn build_commissioning_data(setup_pin: u32, discriminator: u16) -> BasicCommData {
    BasicCommData {
        password: setup_pin.to_le_bytes().into(),
        discriminator,
    }
}

fn read_u32(key: &str, default: u32) -> u32 {
    env::var(key)
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(default)
}

fn read_u16(key: &str, default: u16) -> u16 {
    env::var(key)
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(default)
}

fn log_startup(
    state_dir: &std::path::Path,
    setup_pin: u32,
    discriminator: u16,
    commissioned: bool,
) {
    info!("starting rs-matterd");
    info!("state directory: {}", state_dir.display());
    info!(
        "device: vendor='{}' product='{}' node='{}'",
        DEFAULT_VENDOR_NAME, DEFAULT_PRODUCT_NAME, DEFAULT_DEVICE_NAME
    );
    info!("commissioned: {commissioned}");

    if !commissioned {
        info!("setup pin: {setup_pin}");
        info!("discriminator: {discriminator}");
    }
}

const NODE: Node<'static> = Node {
    id: 0,
    endpoints: &[
        endpoints::root_endpoint(NetworkType::Ethernet),
        Endpoint {
            id: 1,
            device_types: devices!(DEV_TYPE_ON_OFF_LIGHT),
            clusters: clusters!(desc::DescHandler::CLUSTER, TestOnOffDeviceLogic::CLUSTER),
        },
    ],
};

fn dm_handler<'a, OH: OnOffHooks, LH: LevelControlHooks>(
    mut rand: impl RngCore + Copy,
    on_off: &'a on_off::OnOffHandler<'a, OH, LH>,
) -> impl AsyncMetadata + AsyncHandler + 'a {
    (
        NODE,
        endpoints::with_eth(
            &(),
            &UnixNetifs,
            rand,
            endpoints::with_sys(
                &false,
                rand,
                EmptyHandler
                    .chain(
                        EpClMatcher::new(Some(1), Some(desc::DescHandler::CLUSTER.id)),
                        Async(desc::DescHandler::new(Dataver::new_rand(&mut rand)).adapt()),
                    )
                    .chain(
                        EpClMatcher::new(Some(1), Some(TestOnOffDeviceLogic::CLUSTER.id)),
                        on_off::HandlerAsyncAdaptor(on_off),
                    ),
            ),
        ),
    )
}

const RS_MATTERD_DEV_DET: BasicInfoConfig<'static> = BasicInfoConfig {
    serial_no: "rs-matterd-001",
    product_name: DEFAULT_PRODUCT_NAME,
    vendor_name: DEFAULT_VENDOR_NAME,
    device_name: DEFAULT_DEVICE_NAME,
    ..TEST_DEV_DET
};
