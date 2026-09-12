mod history;
mod scanner;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;
use tokio::sync::Mutex;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use homenode_sdk::proto::{HealthState, ModuleRegistration, UpsertDevicesRequest};
use homenode_sdk::{connect_control_client, device_record, module_health, module_manifest, ModuleEnvironment};

use crate::history::DeviceHistoryStore;
use crate::scanner::{NetworkScanner, ScannerConfig};

#[derive(Debug, Deserialize)]
struct NetworkDiscoveryConfig {
    #[serde(default = "default_health_message")]
    health_message: String,
    #[serde(default = "default_scan_interval_secs")]
    scan_interval_secs: u64,
    #[serde(default = "default_max_active_targets")]
    max_active_targets: usize,
    #[serde(default = "default_enable_active_icmp")]
    enable_active_icmp: bool,
    #[serde(default)]
    demo_devices: Vec<DemoDevice>,
}

impl Default for NetworkDiscoveryConfig {
    fn default() -> Self {
        Self {
            health_message: default_health_message(),
            scan_interval_secs: default_scan_interval_secs(),
            max_active_targets: default_max_active_targets(),
            enable_active_icmp: default_enable_active_icmp(),
            demo_devices: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct DemoDevice {
    device_id: String,
    display_name: String,
    kind: String,
    #[serde(default)]
    capabilities: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let env = ModuleEnvironment::from_env()?;
    let config = load_config(&env.config_path)?;
    let mut client = wait_for_client(&env.socket_path).await?;

    client
        .register_module(ModuleRegistration {
            manifest: Some(module_manifest(
                env.module_id.clone(),
                "Network Discovery",
                env!("CARGO_PKG_VERSION"),
                ["network/discovery", "device/inventory", "scanner/icmp", "scanner/arp"],
            )),
            initial_health: Some(module_health(
                env.module_id.clone(),
                HealthState::Starting,
                "Starting network discovery scanner",
            )),
        })
        .await?;

    if !config.demo_devices.is_empty() {
        let mut initial_demo = Vec::new();
        append_demo_devices(&env.module_id, &mut initial_demo, &config.demo_devices);
        let _ = client
            .upsert_devices(UpsertDevicesRequest {
                module_id: env.module_id.clone(),
                devices: initial_demo,
            })
            .await;
    }

    let scanner_config = ScannerConfig {
        max_active_targets: config.max_active_targets,
        enable_active_icmp: config.enable_active_icmp,
        interface_allowlist: Vec::new(),
        interface_denylist: Vec::new(),
    };

    let mut definitions_engine = homenode_definitions::RhaiDeviceEngine::new();
    let workspace_root = env
        .server_config_path
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));

    let candidate_paths = [
        workspace_root.join("definitions").join("devices"),
        std::path::PathBuf::from("definitions/devices"),
        std::path::PathBuf::from("../../definitions/devices"),
    ];

    let mut loaded_scripts = 0;
    for path in candidate_paths {
        if path.exists() {
            if let Ok(n) = definitions_engine.load_from_dir(&path) {
                if n > 0 {
                    loaded_scripts = n;
                    info!("Loaded {n} Rhai device definitions from {}", path.display());
                    break;
                }
            }
        }
    }
    if loaded_scripts == 0 {
        tracing::warn!("No Rhai device definitions loaded; using fallback classification");
    }

    let candidate_catalog_paths = [
        workspace_root.join("definitions").join("catalog.json"),
        std::path::PathBuf::from("definitions/catalog.json"),
        std::path::PathBuf::from("../../definitions/catalog.json"),
    ];

    let mut catalog = homenode_definitions::CatalogDatabase::new();
    for path in candidate_catalog_paths {
        if path.exists() {
            if let Ok(c) = homenode_definitions::CatalogDatabase::load_from_path(&path) {
                info!(
                    "Loaded hardware catalog with {} vendors and {} products from {}",
                    c.vendors.len(),
                    c.products.len(),
                    path.display()
                );
                catalog = c;
                break;
            }
        }
    }

    let catalog_arc = std::sync::Arc::new(catalog);
    let scanner = NetworkScanner::new(
        scanner_config,
        std::sync::Arc::new(definitions_engine),
        catalog_arc.clone(),
    );

    let data_dir = workspace_root.join("data");
    let _ = std::fs::create_dir_all(&data_dir);
    let history_path = data_dir.join("device_history.json");
    let categories_path = data_dir.join("device_categories.json");
    let docs_path = data_dir.join("device_documentation.json");

    let mut store_obj = DeviceHistoryStore::load_from_path(&history_path);
    store_obj.seed_known_devices(&categories_path, &docs_path, &catalog_arc);
    let history_store = Arc::new(Mutex::new(store_obj));

    // Initial scan
    info!("Running initial network discovery sweep...");
    let sweep = match scanner.scan().await {
        Ok(discovered) => discovered,
        Err(err) => {
            error!("Initial network scan failed: {err}");
            Vec::new()
        }
    };

    let mut devices = {
        let mut store = history_store.lock().await;
        let devs = store.reconcile_sweep(&env.module_id, sweep);
        if let Err(e) = store.save_to_path(&history_path) {
            warn!("Failed to save initial device history: {e}");
        }
        devs
    };
    append_demo_devices(&env.module_id, &mut devices, &config.demo_devices);

    let active_count = devices.iter().filter(|d| d.metadata.get("status").map(|s| s.as_str()) == Some("active")).count();
    let total_count = devices.len();

    client
        .upsert_devices(UpsertDevicesRequest {
            module_id: env.module_id.clone(),
            devices,
        })
        .await?;

    client
        .report_health(module_health(
            env.module_id.clone(),
            HealthState::Ready,
            format!("{} ({} active / {} total)", config.health_message, active_count, total_count),
        ))
        .await?;

    // Background periodic scan loop
    let socket_path = env.socket_path.clone();
    let module_id = env.module_id.clone();
    let scan_interval = Duration::from_secs(config.scan_interval_secs);
    let health_message = config.health_message.clone();
    let demo_devices = config.demo_devices.clone();

    // Channel for triggering on-demand scans via control commands
    let (trigger_tx, mut trigger_rx) = tokio::sync::mpsc::channel::<()>(1);

    // Command listener task
    let cmd_socket_path = env.socket_path.clone();
    let cmd_module_id = env.module_id.clone();
    let trigger_sender = trigger_tx.clone();
    let history_store_cmd = history_store.clone();
    let history_path_cmd = history_path.clone();

    tokio::spawn(async move {
        loop {
            match connect_control_client(&cmd_socket_path).await {
                Ok(mut rpc) => {
                    match rpc
                        .subscribe_commands(homenode_sdk::proto::SubscribeCommandsRequest {
                            module_id: cmd_module_id.clone(),
                        })
                        .await
                    {
                        Ok(response) => {
                            let mut stream = response.into_inner();
                            while let Ok(Some(cmd)) = stream.message().await {
                                if cmd.action == "scan" {
                                    info!("Received on-demand scan command from supervisor");
                                    let _ = trigger_sender.try_send(());
                                } else if cmd.action == "forget" {
                                    if let Some(target) = cmd.params.get("device_id").or_else(|| cmd.params.get("id")) {
                                        info!("Received command to forget device: {}", target);
                                        let mut store = history_store_cmd.lock().await;
                                        if store.forget_device(target) {
                                            let _ = store.save_to_path(&history_path_cmd);
                                            info!("Device {} removed from history, triggering rescan", target);
                                            let _ = trigger_sender.try_send(());
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            error!("Failed to subscribe to supervisor commands: {e}");
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to connect for command subscription: {e}");
                }
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    });

    let history_store_loop = history_store.clone();
    let history_path_loop = history_path.clone();

    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(scan_interval);
        ticker.tick().await; // skip immediate first tick

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    info!("Running periodic network discovery sweep...");
                }
                _ = trigger_rx.recv() => {
                    info!("Running triggered on-demand network discovery sweep...");
                }
            }

            let sweep = match scanner.scan().await {
                Ok(discovered) => discovered,
                Err(err) => {
                    error!("Network scan sweep failed: {err}");
                    Vec::new()
                }
            };

            let mut updated = {
                let mut store = history_store_loop.lock().await;
                let devs = store.reconcile_sweep(&module_id, sweep);
                if let Err(e) = store.save_to_path(&history_path_loop) {
                    warn!("Failed to save device history: {e}");
                }
                devs
            };

            append_demo_devices(&module_id, &mut updated, &demo_devices);
            if updated.is_empty() {
                continue;
            }

            let active_count = updated.iter().filter(|d| d.metadata.get("status").map(|s| s.as_str()) == Some("active")).count();
            let total_count = updated.len();

            match connect_control_client(&socket_path).await {
                Ok(mut rpc) => {
                    if let Err(e) = rpc
                        .upsert_devices(UpsertDevicesRequest {
                            module_id: module_id.clone(),
                            devices: updated,
                        })
                        .await
                    {
                        error!("Failed to update devices over gRPC: {e}");
                    } else {
                        let _ = rpc
                            .report_health(module_health(
                                module_id.clone(),
                                HealthState::Ready,
                                format!("{} ({} active / {} total)", health_message, active_count, total_count),
                            ))
                            .await;
                    }
                }
                Err(e) => {
                    error!("Failed to reconnect control client: {e}");
                }
            }
        }
    });

    std::future::pending::<()>().await;
    Ok(())
}

fn append_demo_devices(
    module_id: &str,
    devices: &mut Vec<homenode_sdk::proto::DeviceRecord>,
    demo_devices: &[DemoDevice],
) {
    for demo in demo_devices {
        if !devices.iter().any(|d| d.device_id == demo.device_id) {
            devices.push(device_record(
                module_id.to_string(),
                demo.device_id.clone(),
                demo.display_name.clone(),
                demo.kind.clone(),
                demo.capabilities.clone(),
                HashMap::<String, String>::new(),
            ));
        }
    }
}

async fn wait_for_client(
    socket_path: &Path,
) -> Result<homenode_sdk::proto::home_node_control_client::HomeNodeControlClient<tonic::transport::Channel>>
{
    let mut last_error = None;
    for _ in 0..30 {
        match connect_control_client(socket_path).await {
            Ok(client) => return Ok(client),
            Err(error) => {
                last_error = Some(error);
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("failed to connect network discovery")))
}

fn load_config(path: &Path) -> Result<NetworkDiscoveryConfig> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config at {}", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(NetworkDiscoveryConfig::default());
    }
    toml::from_str(&raw).with_context(|| format!("failed to parse TOML config at {}", path.display()))
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

fn default_health_message() -> String {
    String::from("Network discovery scanner active")
}

fn default_scan_interval_secs() -> u64 {
    300
}

fn default_max_active_targets() -> usize {
    256
}

fn default_enable_active_icmp() -> bool {
    true
}
