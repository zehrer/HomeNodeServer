use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use hyper_util::rt::TokioIo;
use tokio::net::UnixStream;
use tonic::transport::{Channel, Endpoint};
use tower::service_fn;

pub mod proto {
    tonic::include_proto!("homenode.v1");
}

pub const IMPLEMENTED_MODULE_IDS: &[&str] = &[
    "web",
    "matter-controller",
    "matter-bridge",
    "network-discovery",
    "bthome",
    "philips-hue",
];

pub const RESERVED_MODULE_IDS: &[&str] = &[
    "web",
    "matter-controller",
    "matter-bridge",
    "network-discovery",
    "bthome",
    "philips-hue",
    "tuya",
    "shelly",
    "govee",
    "zigbee",
    "switchbot",
    "native-devices",
    "ai-local",
    "extensions",
];

#[derive(Debug, Clone)]
pub struct ModuleEnvironment {
    pub socket_path: PathBuf,
    pub config_path: PathBuf,
    pub module_id: String,
    pub server_config_path: PathBuf,
    pub local_ip: Option<String>,
}

impl ModuleEnvironment {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            socket_path: std::env::var("HOMENODE_SOCKET_PATH")
                .map(PathBuf::from)
                .context("missing HOMENODE_SOCKET_PATH")?,
            config_path: std::env::var("HOMENODE_MODULE_CONFIG")
                .map(PathBuf::from)
                .context("missing HOMENODE_MODULE_CONFIG")?,
            module_id: std::env::var("HOMENODE_MODULE_ID")
                .context("missing HOMENODE_MODULE_ID")?,
            server_config_path: std::env::var("HOMENODE_SERVER_CONFIG")
                .map(PathBuf::from)
                .context("missing HOMENODE_SERVER_CONFIG")?,
            local_ip: std::env::var("HOMENODE_LOCAL_IP").ok().filter(|s| !s.trim().is_empty() && s != "auto"),
        })
    }
}

pub fn is_known_module_id(module_id: &str) -> bool {
    RESERVED_MODULE_IDS.contains(&module_id)
}

pub fn now_timestamp_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

pub fn module_manifest(
    id: impl Into<String>,
    display_name: impl Into<String>,
    version: impl Into<String>,
    capabilities: impl IntoIterator<Item = impl Into<String>>,
) -> proto::ModuleManifest {
    proto::ModuleManifest {
        id: id.into(),
        display_name: display_name.into(),
        version: version.into(),
        capabilities: capabilities.into_iter().map(Into::into).collect(),
    }
}

pub fn module_health(
    module_id: impl Into<String>,
    state: proto::HealthState,
    message: impl Into<String>,
) -> proto::ModuleHealth {
    proto::ModuleHealth {
        module_id: module_id.into(),
        state: state as i32,
        message: message.into(),
        updated_at: now_timestamp_secs(),
    }
}

pub fn device_record(
    module_id: impl Into<String>,
    device_id: impl Into<String>,
    display_name: impl Into<String>,
    kind: impl Into<String>,
    capabilities: impl IntoIterator<Item = impl Into<String>>,
    metadata: impl IntoIterator<Item = (impl Into<String>, impl Into<String>)>,
) -> proto::DeviceRecord {
    proto::DeviceRecord {
        module_id: module_id.into(),
        device_id: device_id.into(),
        display_name: display_name.into(),
        kind: kind.into(),
        capabilities: capabilities.into_iter().map(Into::into).collect(),
        metadata: metadata
            .into_iter()
            .map(|(key, value)| (key.into(), value.into()))
            .collect::<HashMap<_, _>>(),
    }
}

pub async fn connect_control_client(
    socket_path: impl AsRef<Path>,
) -> Result<proto::home_node_control_client::HomeNodeControlClient<Channel>> {
    let socket_path = socket_path.as_ref().to_path_buf();
    let endpoint = Endpoint::try_from("http://[::]:50051")?;
    let channel = endpoint
        .connect_with_connector(service_fn(move |_| {
            let socket_path = socket_path.clone();
            async move {
                let stream = UnixStream::connect(socket_path).await?;
                Ok::<_, std::io::Error>(TokioIo::new(stream))
            }
        }))
        .await
        .context("failed to connect to HomeNode control socket")?;

    Ok(proto::home_node_control_client::HomeNodeControlClient::new(channel))
}

pub fn module_command(
    target_module_id: impl Into<String>,
    action: impl Into<String>,
    params: impl IntoIterator<Item = (impl Into<String>, impl Into<String>)>,
) -> proto::ModuleCommand {
    proto::ModuleCommand {
        target_module_id: target_module_id.into(),
        action: action.into(),
        params: params
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect::<HashMap<_, _>>(),
    }
}

/// Detect the host system's primary IPv4 address on the local network (LAN)
pub fn detect_local_network_ip() -> Option<String> {
    if let Ok(ip) = std::env::var("HOMENODE_LOCAL_IP") {
        if !ip.trim().is_empty() && ip != "auto" {
            return Some(ip.trim().to_string());
        }
    }

    // 1. Query OS kernel routing table via UDP connect (no network packets actually transmitted)
    let targets = [
        "8.8.8.8:80",
        "1.1.1.1:80",
        "192.168.178.1:80",
        "192.168.1.1:80",
        "192.168.0.1:80",
        "10.0.0.1:80",
    ];

    if let Ok(socket) = std::net::UdpSocket::bind("0.0.0.0:0") {
        for target in &targets {
            if socket.connect(target).is_ok() {
                if let Ok(addr) = socket.local_addr() {
                    let ip = addr.ip();
                    if !ip.is_loopback() && !ip.is_unspecified() {
                        if let std::net::IpAddr::V4(ipv4) = ip {
                            return Some(ipv4.to_string());
                        }
                    }
                }
            }
        }
    }

    // 2. Fallback: inspect network interfaces directly using if-addrs
    if let Ok(ifaces) = if_addrs::get_if_addrs() {
        for iface in ifaces {
            if !iface.is_loopback() {
                if let std::net::IpAddr::V4(ipv4) = iface.ip() {
                    let octets = ipv4.octets();
                    if (octets[0] == 192 && octets[1] == 168)
                        || octets[0] == 10
                        || (octets[0] == 172 && (16..=31).contains(&octets[1]))
                    {
                        return Some(ipv4.to_string());
                    }
                }
            }
        }
    }

    None
}
