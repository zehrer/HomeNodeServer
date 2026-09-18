mod energy;
mod govee;
mod rooms;

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::extract::{Path as AxumPath, State};
use axum::response::{Html, IntoResponse, Json, Response};
use axum::routing::{delete, get, post};
use axum::Router;
use homenode_definitions::{deduce_floor_from_name, slugify_room_id, RoomRecord};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

use homenode_sdk::proto::{DeviceRecord, Empty, HealthState, ModuleRegistration, RuntimeSnapshot};
use homenode_sdk::{connect_control_client, module_health, module_manifest, ModuleEnvironment};

#[derive(Debug, Clone, Deserialize)]
struct WebConfig {
    #[serde(default = "default_listen_addr")]
    listen_addr: String,
    #[serde(default = "default_status_title")]
    status_title: String,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            listen_addr: default_listen_addr(),
            status_title: default_status_title(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DeviceDocumentation {
    #[serde(default)]
    pub name: Option<String>,
    pub notes: String,
    #[serde(default)]
    pub room: Option<String>,
    #[serde(default)]
    pub manual_url: Option<String>,
    #[serde(default)]
    pub product_id: Option<String>,
    #[serde(default)]
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MatterFabricMeta {
    pub name: String,
    #[serde(default)]
    pub icon: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientMatterFabric {
    pub fabric_id: String,
    pub node_id: String,
    pub port: u16,
    pub interface: String,
    pub fabric_name: String,
    pub fabric_icon: String,
}

#[derive(Clone)]
struct WebState {
    socket_path: PathBuf,
    status_title: String,
    docs_path: PathBuf,
    links_path: PathBuf,
    categories_path: PathBuf,
    matter_fabrics_path: PathBuf,
    definitions_dir: PathBuf,
    #[allow(dead_code)]
    catalog_path: PathBuf,
    catalog_overrides_path: PathBuf,
    docs_store: Arc<RwLock<HashMap<String, DeviceDocumentation>>>,
    links_store: Arc<RwLock<HashMap<String, Vec<String>>>>,
    categories_store: Arc<RwLock<HashMap<String, String>>>,
    matter_fabrics_store: Arc<RwLock<HashMap<String, MatterFabricMeta>>>,
    catalog_store: Arc<RwLock<homenode_definitions::CatalogDatabase>>,
    ignored_store: homenode_definitions::IgnoredDevicesStore,
    rooms_path: PathBuf,
    rooms_store: homenode_definitions::RoomsStore,
    #[allow(dead_code)]
    light_groups_path: PathBuf,
    light_groups_store: homenode_definitions::LightGroupsStore,
    mobile_ble_path: PathBuf,
    mobile_ble_store: Arc<RwLock<HashMap<String, MobileBleScanItem>>>,
    govee_manager: govee::GoveeManager,
    started_at: std::time::Instant,
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
                "Web Status",
                env!("CARGO_PKG_VERSION"),
                ["status/http", "runtime/snapshot"],
            )),
            initial_health: Some(module_health(
                env.module_id.clone(),
                HealthState::Starting,
                "Starting web status server",
            )),
        })
        .await?;

    let workspace_root = env
        .server_config_path
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));

    let data_dir = workspace_root.join("data");
    let _ = std::fs::create_dir_all(&data_dir);
    let docs_path = data_dir.join("device_documentation.json");
    let links_path = data_dir.join("device_links.json");
    let categories_path = data_dir.join("device_categories.json");
    let matter_fabrics_path = data_dir.join("matter_fabrics.json");
    let definitions_dir = workspace_root.join("definitions").join("devices");
    let catalog_path = workspace_root.join("definitions").join("catalog.json");
    let catalog_overrides_path = data_dir.join("catalog_overrides.json");
    let ignored_path = data_dir.join("ignored_devices.json");
    let ignored_store = homenode_definitions::IgnoredDevicesStore::load_or_create(&ignored_path);
    let rooms_path = data_dir.join("rooms.json");
    let rooms_store = homenode_definitions::RoomsStore::load_or_create(&rooms_path);
    let light_groups_path = data_dir.join("light_groups.json");
    let light_groups_store = homenode_definitions::LightGroupsStore::new(&light_groups_path);
    if light_groups_store.list().is_empty() {
        let default_group = homenode_definitions::LightGroup::new(
            "group-wohnzimmer-vorhaenge",
            "Wohnzimmer Vorhänge",
            "Wohnzimmer",
            vec!["net-192-168-178-42".to_string(), "net-192-168-178-43".to_string()],
            Some("✨".to_string()),
        );
        let _ = light_groups_store.upsert(default_group);
    }
    let started_at = std::time::Instant::now();

    let mobile_ble_path = data_dir.join("mobile_ble_devices.json");
    let initial_mobile_ble: HashMap<String, MobileBleScanItem> = load_json_map(&mobile_ble_path);
    let mobile_ble_store = Arc::new(RwLock::new(initial_mobile_ble.clone()));

    let mut initial_docs: HashMap<String, DeviceDocumentation> = load_json_map(&docs_path);
    let govee_defaults = [
        ("d0:c9:07:3c:1b:5c", "192.168.178.40", "Govee LED Sophie", "Sophie Zimmer"),
        ("d0:c9:07:a4:c0:dc", "192.168.178.41", "Govee LED Dachgeschoss", "Dachgeschoss"),
        ("d0:c9:07:39:f4:4c", "192.168.178.42", "Govee LED EG Links", "Wohnzimmer"),
        ("d0:c9:07:3c:3a:d4", "192.168.178.43", "Govee LED EG Rechts", "Wohnzimmer"),
    ];
    let mut modified_docs = false;
    for (mac, ip, default_name, default_room) in govee_defaults {
        let doc_key = mac.to_lowercase();
        let ip_key = format!("net-{}", ip.replace('.', "-"));
        if !initial_docs.contains_key(&doc_key) && !initial_docs.contains_key(&ip_key) {
            initial_docs.insert(
                doc_key,
                DeviceDocumentation {
                    name: Some(default_name.to_string()),
                    room: Some(default_room.to_string()),
                    notes: "Govee RGBIC Smart Light (UDP LAN Steuerung)".to_string(),
                    manual_url: Some("https://www.govee.com".to_string()),
                    product_id: Some("govee_rgbic_light".to_string()),
                    updated_at: chrono::Utc::now().to_rfc3339(),
                },
            );
            modified_docs = true;
        }
    }
    if modified_docs {
        let _ = persist_json(&docs_path, &initial_docs);
    }

    let govee_manager = govee::GoveeManager::new().await?;
    let govee_candidates = vec![
        "192.168.178.40".to_string(),
        "192.168.178.41".to_string(),
        "192.168.178.42".to_string(),
        "192.168.178.43".to_string(),
    ];
    govee_manager.scan(&govee_candidates).await;

    let initial_links = load_json_map(&links_path);
    let initial_categories = load_json_map(&categories_path);
    let mut initial_fabrics: HashMap<String, MatterFabricMeta> = load_json_map(&matter_fabrics_path);
    let mut modified_fabrics = false;
    if !initial_fabrics.contains_key("6ABEDCB982EC2223") {
        initial_fabrics.insert(
            "6ABEDCB982EC2223".to_string(),
            MatterFabricMeta {
                name: "Apple Home".to_string(),
                icon: "🍎".to_string(),
                description: "Apple Home ecosystem fabric".to_string(),
            },
        );
        modified_fabrics = true;
    }
    if !initial_fabrics.contains_key("4518A03EC84FB6E7") {
        initial_fabrics.insert(
            "4518A03EC84FB6E7".to_string(),
            MatterFabricMeta {
                name: "Home Assistant".to_string(),
                icon: "🏡".to_string(),
                description: "Home Assistant Matter Server ecosystem fabric".to_string(),
            },
        );
        modified_fabrics = true;
    }
    if !initial_fabrics.contains_key("A38D674BFAAFF432") {
        initial_fabrics.insert(
            "A38D674BFAAFF432".to_string(),
            MatterFabricMeta {
                name: "Direct Device Fabric".to_string(),
                icon: "💡".to_string(),
                description: "Direct vendor fabric (e.g. Govee)".to_string(),
            },
        );
        modified_fabrics = true;
    }
    if modified_fabrics {
        let _ = persist_json(&matter_fabrics_path, &initial_fabrics);
    }

    let mut initial_catalog = if catalog_path.exists() {
        homenode_definitions::CatalogDatabase::load_from_path(&catalog_path).unwrap_or_default()
    } else {
        homenode_definitions::CatalogDatabase::new()
    };
    if catalog_overrides_path.exists() {
        if let Ok(overrides) = homenode_definitions::CatalogDatabase::load_from_path(&catalog_overrides_path) {
            initial_catalog.merge(overrides);
        }
    }

    let docs_store = Arc::new(RwLock::new(initial_docs));
    let links_store = Arc::new(RwLock::new(initial_links));
    let categories_store = Arc::new(RwLock::new(initial_categories));
    let matter_fabrics_store = Arc::new(RwLock::new(initial_fabrics));
    let catalog_store = Arc::new(RwLock::new(initial_catalog));

    let listener = tokio::net::TcpListener::bind(&config.listen_addr).await?;
    let local_ip = env.local_ip.clone().unwrap_or_else(|| {
        homenode_sdk::detect_local_network_ip().unwrap_or_else(|| "127.0.0.1".to_string())
    });
    let port_str = config.listen_addr.split(':').nth(1).unwrap_or("8080");
    info!(
        "HomeNode Web Server bound on http://{} (Local LAN URL: http://{}:{})",
        config.listen_addr, local_ip, port_str
    );
    let startup_mobile_records: Vec<_> = initial_mobile_ble.values()
        .filter_map(mobile_ble_item_to_record)
        .collect();
    if !startup_mobile_records.is_empty() {
        info!("Restoring {} persisted mobile BLE devices to supervisor", startup_mobile_records.len());
        let _ = client.upsert_devices(homenode_sdk::proto::UpsertDevicesRequest {
            module_id: env.module_id.clone(),
            devices: startup_mobile_records,
            replace_all: false,
        }).await;
    }

    client
        .report_health(module_health(
            env.module_id,
            HealthState::Ready,
            format!("Serving status page on {} (http://{}:{})", config.listen_addr, local_ip, port_str),
        ))
        .await?;

    let app = Router::new()
        .route("/", get(dashboard_handler))
        .route("/devices", get(devices_handler))
        .route("/energy", get(energy_handler))
        .route("/api/energy/live", get(energy_live_api_handler))
        .route("/api/events", get(events_api_handler))
        .route("/matter", get(matter_handler))
        .route("/catalog", get(catalog_handler))
        .route("/status", get(status_handler))
        .route("/scan", post(scan_trigger_form_handler))
        .route("/api/scan", post(scan_trigger_api_handler))
        .route("/api/matter/fabrics", get(get_matter_fabrics_handler))
        .route("/api/matter/fabrics/:id", post(update_matter_fabric_handler))
        .route("/api/catalog", get(get_catalog_handler))
        .route("/api/catalog/vendor", post(add_vendor_handler))
        .route("/api/catalog/product", post(add_product_handler))
        .route("/api/devices/:id/assign-product", post(assign_device_product_handler))
        .route(
            "/api/devices/:id/documentation",
            get(get_device_doc_handler).post(save_device_doc_handler),
        )
        .route("/api/devices/:id/category", post(update_device_category_handler))
        .route("/api/devices/:id/analyze", post(analyze_device_handler))
        .route("/api/devices/:id/ping", post(ping_device_handler))
        .route("/api/devices/link", post(link_devices_handler))
        .route("/api/devices/unlink", post(unlink_devices_handler))
        .route("/api/devices/:id/forget", post(forget_device_handler))
        .route("/api/definitions/save", post(save_definition_handler))
        .route("/api/hue/status", get(hue_status_api_handler))
        .route("/api/hue/pair", post(hue_pair_api_handler))
        .route("/api/hue/lights/:id/toggle", post(hue_toggle_api_handler))
        .route("/api/v1/health", get(v1_health_api_handler))
        .route("/api/v1/info", get(v1_info_api_handler))
        .route("/api/v1/mobile/ble", post(v1_mobile_ble_ingest_handler))
        .route("/api/v1/devices/claim", post(v1_claim_device_handler))
        .route("/api/ignored-devices", get(get_ignored_devices_handler))
        .route("/api/devices/:id/ignore", post(ignore_device_handler))
        .route("/api/devices/:id/unignore", post(unignore_device_handler))
        .route("/api/rooms", get(get_rooms_handler).post(save_room_handler))
        .route("/api/rooms/:id", delete(delete_room_handler))
        .route("/api/floors", get(get_floors_handler))
        .route("/api/rooms/import-hue", post(import_hue_rooms_handler))
        .route("/rooms", get(rooms_page_handler))
        .route("/api/light-groups", get(list_light_groups_handler).post(create_light_group_handler))
        .route("/api/light-groups/:id", delete(delete_light_group_handler))
        .route("/api/light-groups/:id/power", post(light_group_power_handler))
        .route("/api/light-groups/:id/brightness", post(light_group_brightness_handler))
        .route("/api/light-groups/:id/color", post(light_group_color_handler))
        .route("/api/light-groups/:id/temperature", post(light_group_temperature_handler))
        .route("/api/rooms/:name/scene", post(room_scene_handler))
        .route("/api/govee/lights", get(get_govee_lights_handler))
        .route("/api/govee/lights/:ip/toggle", post(toggle_govee_light_handler))
        .route("/api/govee/lights/:ip/power", post(set_govee_power_handler))
        .route("/api/govee/lights/:ip/brightness", post(set_govee_brightness_handler))
        .route("/api/govee/lights/:ip/color", post(set_govee_color_handler))
        .route("/api/govee/lights/:ip/temperature", post(set_govee_temperature_handler))
        .route("/api/govee/lights/:ip/status", get(get_govee_status_handler))
        .with_state(WebState {
            socket_path: env.socket_path,
            status_title: config.status_title,
            docs_path,
            links_path,
            categories_path,
            matter_fabrics_path,
            definitions_dir,
            catalog_path,
            catalog_overrides_path,
            docs_store,
            links_store,
            categories_store,
            matter_fabrics_store,
            catalog_store,
            ignored_store,
            rooms_path,
            rooms_store,
            light_groups_path,
            light_groups_store,
            mobile_ble_path,
            mobile_ble_store,
            govee_manager,
            started_at,
        });

    #[cfg(target_os = "macos")]
    {
        let port_str = config.listen_addr.split(':').nth(1).unwrap_or("8080").to_string();
        tokio::spawn(async move {
            info!("Broadcasting HomeNode Server via mDNS/Bonjour on port {}...", port_str);
            let mut cmd = tokio::process::Command::new("dns-sd");
            cmd.args(["-R", "HomeNode Server", "_homenode._tcp", "local", &port_str]);
            if let Ok(mut child) = cmd.spawn() {
                let _ = child.wait().await;
            }
        });
    }

    axum::serve(listener, app).await?;
    Ok(())
}

fn load_json_map<T: for<'de> Deserialize<'de> + Default>(path: &Path) -> HashMap<String, T> {
    if !path.exists() {
        return HashMap::new();
    }
    match std::fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
        Err(err) => {
            warn!("Failed to read JSON from {}: {err}", path.display());
            HashMap::new()
        }
    }
}

fn persist_json<T: Serialize>(path: &Path, data: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(data)?;
    std::fs::write(path, content)?;
    Ok(())
}

async fn scan_trigger_form_handler(State(state): State<WebState>) -> axum::response::Redirect {
    let _ = trigger_network_scan(&state.socket_path).await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    axum::response::Redirect::to("/")
}

async fn scan_trigger_api_handler(
    State(state): State<WebState>,
) -> (axum::http::StatusCode, &'static str) {
    match trigger_network_scan(&state.socket_path).await {
        Ok(_) => (axum::http::StatusCode::ACCEPTED, r#"{"status":"scan_started"}"#),
        Err(_) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            r#"{"error":"failed_to_trigger_scan"}"#,
        ),
    }
}

async fn trigger_network_scan(socket_path: &Path) -> Result<()> {
    let mut client = connect_control_client(socket_path).await?;
    client
        .send_command(homenode_sdk::proto::ModuleCommand {
            target_module_id: "network-discovery".to_string(),
            action: "scan".to_string(),
            params: std::collections::HashMap::new(),
        })
        .await?;
    Ok(())
}

#[derive(Debug, Clone, Deserialize)]
pub struct VerifiedShellyGateway {
    pub ip: String,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub mac: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub gen: i64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub ble_supported: bool,
    #[serde(default)]
    pub ble_enabled: bool,
    #[serde(default)]
    pub ws_connected: bool,
    #[serde(default)]
    pub last_seen: String,
}

async fn fetch_verified_shelly_gateways() -> Vec<VerifiedShellyGateway> {
    match energy::http_get_json("127.0.0.1", 8124, "/api/gateways", 400).await {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

async fn dashboard_handler(State(state): State<WebState>) -> Html<String> {
    let docs = state.docs_store.read().await.clone();
    let links = state.links_store.read().await.clone();
    let verified_gws = fetch_verified_shelly_gateways().await;
    let body = match load_snapshot(&state.socket_path).await {
        Ok(snapshot) => render_dashboard_page(
            &state.status_title,
            &snapshot,
            &docs,
            &links,
            &verified_gws,
        ),
        Err(error) => render_error(&state.status_title, "dashboard", &error.to_string()),
    };
    Html(body)
}

async fn devices_handler(State(state): State<WebState>) -> Html<String> {
    let docs = state.docs_store.read().await.clone();
    let mut links = state.links_store.read().await.clone();
    let catalog = state.catalog_store.read().await.clone();
    let categories = state.categories_store.read().await.clone();
    let matter_fabrics = state.matter_fabrics_store.read().await.clone();
    let verified_gws = fetch_verified_shelly_gateways().await;
    let rooms = state.rooms_store.list();
    let govee_states: HashMap<String, govee::GoveeDeviceState> = state
        .govee_manager
        .list_devices()
        .await
        .into_iter()
        .map(|d| (d.ip.clone(), d))
        .collect();
    let body = match load_snapshot(&state.socket_path).await {
        Ok(snapshot) => {
            if auto_link_deterministic_devices(&snapshot.devices, &mut links) {
                let mut store = state.links_store.write().await;
                *store = links.clone();
                let _ = persist_json(&state.links_path, &*store);
            }
            render_devices_page(
                &state.status_title,
                &snapshot,
                &docs,
                &links,
                &catalog,
                &categories,
                &matter_fabrics,
                &verified_gws,
                &state.ignored_store,
                &rooms,
                &govee_states,
            )
        }
        Err(error) => render_error(&state.status_title, "devices", &error.to_string()),
    };
    Html(body)
}

async fn matter_handler(State(state): State<WebState>) -> Html<String> {
    let docs = state.docs_store.read().await.clone();
    let links = state.links_store.read().await.clone();
    let matter_fabrics = state.matter_fabrics_store.read().await.clone();
    let body = match load_snapshot(&state.socket_path).await {
        Ok(snapshot) => render_matter_page(
            &state.status_title,
            &snapshot,
            &docs,
            &links,
            &matter_fabrics,
        ),
        Err(error) => render_error(&state.status_title, "matter", &error.to_string()),
    };
    Html(body)
}

async fn catalog_handler(State(state): State<WebState>) -> Html<String> {
    let catalog = state.catalog_store.read().await.clone();
    let body = render_catalog_page(&state.status_title, &catalog);
    Html(body)
}

async fn status_handler(State(state): State<WebState>) -> Html<String> {
    let body = match load_snapshot(&state.socket_path).await {
        Ok(snapshot) => render_status_page(&state.status_title, &snapshot),
        Err(error) => render_error(&state.status_title, "status", &error.to_string()),
    };
    Html(body)
}

async fn events_api_handler(State(state): State<WebState>) -> impl IntoResponse {
    let raw = match energy::http_get_json("127.0.0.1", 8124, "/api/events", 800).await {
        Ok(body) => body,
        Err(_) => "[]".to_string(),
    };

    let Ok(mut events) = serde_json::from_str::<Vec<serde_json::Value>>(&raw) else {
        return (
            axum::http::StatusCode::OK,
            [("Content-Type", "application/json")],
            raw,
        );
    };

    let docs = state.docs_store.read().await.clone();
    let snapshot = load_snapshot(&state.socket_path).await.ok();

    for ev in &mut events {
        if let Some(obj) = ev.as_object_mut() {
            let gw_ip = obj.get("gateway_ip").and_then(|v| v.as_str()).map(|s| s.to_string());
            let gw_id = obj.get("gateway_id").and_then(|v| v.as_str()).map(|s| s.to_string());
            let gw_raw = obj.get("gateway").and_then(|v| v.as_str()).unwrap_or("").to_string();

            let mut matched_device_name = None;
            let mut matched_room = None;

            // 1. Search in snapshot devices
            if let Some(snap) = &snapshot {
                for dev in &snap.devices {
                    let dev_ip = dev.metadata.get("ip").map(|s| s.as_str()).unwrap_or("");
                    let dev_mac = dev.metadata.get("mac").map(|s| s.as_str()).unwrap_or("");
                    let dev_id = &dev.device_id;
                    let hostname = dev.metadata.get("hostname").map(|s| s.as_str()).unwrap_or("");

                    let ip_match = gw_ip.as_deref().map(|ip| ip == dev_ip).unwrap_or(false)
                        || (!gw_raw.is_empty() && dev_ip.len() > 6 && gw_raw.contains(dev_ip));

                    let id_match = gw_id.as_deref().map(|id| {
                        let id_norm = id.replace(['-', '_', ':'], "").to_lowercase();
                        let mac_norm = dev_mac.replace(['-', '_', ':'], "").to_lowercase();
                        !id_norm.is_empty() && (id_norm.contains(&mac_norm) || mac_norm.contains(&id_norm) || dev_id.to_lowercase().contains(&id_norm))
                    }).unwrap_or(false);

                    if ip_match || id_match {
                        let doc_entry = docs.get(dev_id).or_else(|| docs.get(dev_mac));
                        let custom_name = doc_entry.and_then(|d| d.name.clone());
                        let room_entry = doc_entry.and_then(|d| d.room.clone());

                        matched_device_name = custom_name
                            .or_else(|| if !dev.display_name.is_empty() { Some(dev.display_name.clone()) } else { None })
                            .or_else(|| if !hostname.is_empty() { Some(hostname.to_string()) } else { None });

                        if let Some(r) = room_entry {
                            matched_room = Some(r);
                        } else {
                            // Deduce room from name/hostname if possible
                            let lower = format!("{} {}", dev.display_name, hostname).to_lowercase();
                            if lower.contains("wohnzimmer") || lower.contains("living") {
                                matched_room = Some("Wohnzimmer".to_string());
                            } else if lower.contains("büro") || lower.contains("buero") || lower.contains("office") {
                                matched_room = Some("Büro".to_string());
                            } else if lower.contains("küche") || lower.contains("kueche") || lower.contains("kitchen") {
                                matched_room = Some("Küche".to_string());
                            } else if lower.contains("schlafzimmer") || lower.contains("bedroom") {
                                matched_room = Some("Schlafzimmer".to_string());
                            } else if lower.contains("balkon") || lower.contains("balcony") {
                                matched_room = Some("Balkon".to_string());
                            } else if lower.contains("flur") || lower.contains("hall") {
                                matched_room = Some("Flur".to_string());
                            } else if lower.contains("keller") || lower.contains("basement") {
                                matched_room = Some("Keller".to_string());
                            } else if lower.contains("garage") {
                                matched_room = Some("Garage".to_string());
                            }
                        }
                        break;
                    }
                }
            }

            // Also check device's own documented room if gateway didn't provide one
            if matched_room.is_none() {
                let dev_mac = obj.get("mac").and_then(|v| v.as_str()).unwrap_or("");
                if let Some(doc) = docs.get(dev_mac) {
                    if let Some(r) = &doc.room {
                        matched_room = Some(r.clone());
                    }
                }
            }

            if let Some(name) = matched_device_name {
                obj.insert("gateway_name".to_string(), serde_json::Value::String(name));
            }
            if let Some(room) = matched_room {
                obj.insert("room".to_string(), serde_json::Value::String(room));
            }
        }
    }

    let enriched = serde_json::to_string(&events).unwrap_or(raw);
    (
        axum::http::StatusCode::OK,
        [("Content-Type", "application/json")],
        enriched,
    )
}

async fn energy_handler(State(state): State<WebState>) -> Html<String> {
    let body = render_energy_page(&state.status_title);
    Html(body)
}

async fn energy_live_api_handler(State(state): State<WebState>) -> Json<energy::EnergyLiveSnapshot> {
    let mut fronius_host = "fronius.fritz.box".to_string();
    let mut shelly_host = "shellypro3em.fritz.box".to_string();
    let mut batteries = Vec::new();

    if let Ok(snapshot) = load_snapshot(&state.socket_path).await {
        for dev in &snapshot.devices {
            let host = dev.metadata.get("hostname").map(|s| s.as_str()).unwrap_or("");
            let name = dev.display_name.to_lowercase();
            let ip = dev.metadata.get("ip").map(|s| s.as_str()).unwrap_or("");

            if (host.contains("fronius") || name.contains("fronius")) && !ip.is_empty() {
                fronius_host = ip.to_string();
            } else if (host.contains("shellypro3em") || name.contains("shelly pro 3em") || name.contains("shellypro3em")) && !ip.is_empty() {
                shelly_host = ip.to_string();
            } else if host.contains("ecoflow") || name.contains("ecoflow") {
                batteries.push(energy::BatteryInfo {
                    name: dev.display_name.clone(),
                    hostname: host.to_string(),
                    ip: ip.to_string(),
                    soc_pct: None,
                    power_w: None,
                    status: "Connected to LAN".to_string(),
                });
            }
        }
    }

    if batteries.is_empty() {
        batteries.push(energy::BatteryInfo {
            name: "EcoFlow PowerStream (Balcony 1)".to_string(),
            hostname: "ecoflow1.fritz.box".to_string(),
            ip: "192.168.178.96".to_string(),
            soc_pct: None,
            power_w: None,
            status: "Connected to LAN".to_string(),
        });
        batteries.push(energy::BatteryInfo {
            name: "EcoFlow PowerStream (Balcony 2)".to_string(),
            hostname: "ecoflow2.fritz.box".to_string(),
            ip: "192.168.178.105".to_string(),
            soc_pct: None,
            power_w: None,
            status: "Connected to LAN".to_string(),
        });
    }

    let snapshot = energy::collect_energy_snapshot(&fronius_host, &shelly_host, batteries).await;
    Json(snapshot)
}

async fn load_snapshot(socket_path: &Path) -> Result<RuntimeSnapshot> {
    let mut client = connect_control_client(socket_path).await?;
    let snapshot = client.get_runtime_snapshot(Empty {}).await?.into_inner();
    Ok(snapshot)
}

// ------------------------------------------------------------------------------------------------
// Documentation & Link API Handlers
// ------------------------------------------------------------------------------------------------

#[derive(Deserialize)]
struct SaveDocRequest {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    room: Option<String>,
    #[serde(default)]
    notes: Option<String>,
    #[serde(default)]
    manual_url: Option<String>,
}

async fn get_device_doc_handler(
    AxumPath(device_id): AxumPath<String>,
    State(state): State<WebState>,
) -> Json<DeviceDocumentation> {
    let store = state.docs_store.read().await;
    let doc = store.get(&device_id).cloned().unwrap_or_default();
    Json(doc)
}

async fn save_device_doc_handler(
    AxumPath(device_id): AxumPath<String>,
    State(state): State<WebState>,
    Json(payload): Json<SaveDocRequest>,
) -> Response {
    let mut store = state.docs_store.write().await;
    let now = chrono::Utc::now().to_rfc3339();
    let existing_prod = store.get(&device_id).and_then(|d| d.product_id.clone());
    let clean_name = payload.name.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let clean_room = payload.room.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let clean_notes = payload.notes.unwrap_or_default();
    let clean_url = payload.manual_url.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());

    let is_empty_doc = clean_name.is_none() && clean_room.is_none() && clean_notes.trim().is_empty() && clean_url.is_none() && existing_prod.is_none();
    if is_empty_doc {
        store.remove(&device_id);
    } else {
        let entry = DeviceDocumentation {
            name: clean_name.clone(),
            notes: clean_notes,
            room: clean_room.clone(),
            manual_url: clean_url,
            product_id: existing_prod,
            updated_at: now,
        };
        store.insert(device_id.clone(), entry);
    }
    if let Some(ref r_name) = clean_room {
        if state.rooms_store.find_by_name(r_name).is_none() {
            let rec = RoomRecord::new(slugify_room_id(r_name), r_name, None, None, None);
            let _ = state.rooms_store.upsert(rec);
        }
    }
    if let Err(err) = persist_json(&state.docs_path, &*store) {
        error!("Failed to persist device documentation: {err}");
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": err.to_string()})),
        )
            .into_response();
    }

    // Also sync custom name or fallback to device_history.json
    let history_path = state.docs_path.parent().unwrap_or(std::path::Path::new(".")).join("device_history.json");
    if let Ok(content) = std::fs::read_to_string(&history_path) {
        if let Ok(mut hist) = serde_json::from_str::<serde_json::Value>(&content) {
            let norm = device_id.trim().to_lowercase();
            if let Some(records) = hist.get_mut("records").and_then(|r| r.as_object_mut()) {
                let mut updated = false;
                for (k, v) in records.iter_mut() {
                    let k_norm = k.trim().to_lowercase();
                    let mac_norm = v.get("mac").and_then(|m| m.as_str()).map(|m| m.trim().to_lowercase());
                    let id_norm = v.get("device_id").and_then(|m| m.as_str()).map(|m| m.trim().to_lowercase());
                    if k_norm == norm || mac_norm.as_deref() == Some(&norm) || id_norm.as_deref() == Some(&norm) {
                        if let Some(obj) = v.as_object_mut() {
                            if let Some(ref new_name) = clean_name {
                                obj.insert("display_name".to_string(), serde_json::Value::String(new_name.clone()));
                                updated = true;
                            } else {
                                let fallback = obj.get("hostname")
                                    .and_then(|h| h.as_str())
                                    .filter(|h| !h.is_empty())
                                    .or_else(|| obj.get("device_id").and_then(|id| id.as_str()))
                                    .unwrap_or("Unknown Device");
                                obj.insert("display_name".to_string(), serde_json::Value::String(fallback.to_string()));
                                updated = true;
                            }
                        }
                    }
                }
                if updated {
                    let _ = persist_json(&history_path, &hist);
                }
            }
        }
    }

    Json(serde_json::json!({"status": "saved"})).into_response()
}

#[derive(Deserialize)]
struct LinkRequest {
    primary_id: String,
    linked_id: String,
}

async fn link_devices_handler(
    State(state): State<WebState>,
    Json(payload): Json<LinkRequest>,
) -> Response {
    // If primary is BLE and linked is network (or primary is secondary WLAN),
    // normalize so the primary device is the main network interface.
    let (primary_id, linked_id) = if (payload.primary_id.starts_with("mobile-ble-") || payload.primary_id.starts_with("bthome-"))
        && (!payload.linked_id.starts_with("mobile-ble-") && !payload.linked_id.starts_with("bthome-"))
    {
        (payload.linked_id, payload.primary_id)
    } else {
        (payload.primary_id, payload.linked_id)
    };

    let mut links = state.links_store.write().await;
    let list = links.entry(primary_id).or_default();
    if !list.contains(&linked_id) {
        list.push(linked_id);
    }
    if let Err(err) = persist_json(&state.links_path, &*links) {
        error!("Failed to persist device links: {err}");
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": err.to_string()})),
        )
            .into_response();
    }
    Json(serde_json::json!({"status": "linked"})).into_response()
}

async fn unlink_devices_handler(
    State(state): State<WebState>,
    Json(payload): Json<LinkRequest>,
) -> Response {
    let mut links = state.links_store.write().await;
    if let Some(list) = links.get_mut(&payload.primary_id) {
        list.retain(|id| id != &payload.linked_id);
    }
    if let Some(list) = links.get_mut(&payload.linked_id) {
        list.retain(|id| id != &payload.primary_id);
    }
    if let Err(err) = persist_json(&state.links_path, &*links) {
        error!("Failed to persist device links: {err}");
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": err.to_string()})),
        )
            .into_response();
    }
    Json(serde_json::json!({"status": "unlinked"})).into_response()
}

#[derive(Debug, Deserialize)]
struct UpdateDeviceCategoryPayload {
    category: String,
}

async fn update_device_category_handler(
    AxumPath(id): AxumPath<String>,
    State(state): State<WebState>,
    Json(payload): Json<UpdateDeviceCategoryPayload>,
) -> Response {
    let mut categories = state.categories_store.write().await;
    let clean_cat = payload.category.trim().to_lowercase();
    if clean_cat.is_empty() {
        categories.remove(&id);
    } else {
        categories.insert(id.clone(), clean_cat);
    }
    if let Err(err) = persist_json(&state.categories_path, &*categories) {
        error!("Failed to persist device categories: {err}");
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": err.to_string()})),
        )
            .into_response();
    }
    Json(serde_json::json!({"status": "updated"})).into_response()
}

async fn forget_device_handler(
    AxumPath(id): AxumPath<String>,
    State(state): State<WebState>,
) -> Response {
    let mut client = match connect_control_client(&state.socket_path).await {
        Ok(c) => c,
        Err(err) => {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": err.to_string()})),
            )
                .into_response();
        }
    };
    if id.starts_with("mobile-ble-") {
        let norm_target = id.trim_start_matches("mobile-ble-");
        let mut store = state.mobile_ble_store.write().await;
        store.retain(|k, _| {
            homenode_definitions::normalize_identifier(k) != norm_target
        });
        let _ = persist_json(&state.mobile_ble_path, &*store);
        let remaining: Vec<_> = store.values().filter_map(mobile_ble_item_to_record).collect();
        let _ = client.upsert_devices(homenode_sdk::proto::UpsertDevicesRequest {
            module_id: "web".to_string(),
            devices: remaining,
            replace_all: true,
        }).await;
        return Json(serde_json::json!({"status": "forgotten"})).into_response();
    }

    let mut params = std::collections::HashMap::new();
    params.insert("device_id".to_string(), id);
    match client
        .send_command(homenode_sdk::proto::ModuleCommand {
            target_module_id: "network-discovery".to_string(),
            action: "forget".to_string(),
            params,
        })
        .await
    {
        Ok(_) => Json(serde_json::json!({"status": "forgotten"})).into_response(),
        Err(err) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": err.to_string()})),
        )
            .into_response(),
    }
}

// ------------------------------------------------------------------------------------------------
// Philips Hue API Handlers
// ------------------------------------------------------------------------------------------------

async fn hue_status_api_handler() -> Response {
    match energy::http_get_json("127.0.0.1", 8125, "/api/status", 500).await {
        Ok(raw) => (
            axum::http::StatusCode::OK,
            [("content-type", "application/json")],
            raw,
        )
            .into_response(),
        Err(err) => (
            axum::http::StatusCode::OK,
            Json(serde_json::json!({
                "paired": false,
                "error": err,
            })),
        )
            .into_response(),
    }
}

async fn hue_pair_api_handler() -> Response {
    match energy::http_post_json("127.0.0.1", 8125, "/api/pair", "{}", 6000).await {
        Ok(raw) => (
            axum::http::StatusCode::OK,
            [("content-type", "application/json")],
            raw,
        )
            .into_response(),
        Err(err) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "status": "error",
                "message": format!("Hue Modul nicht erreichbar: {err}"),
            })),
        )
            .into_response(),
    }
}

async fn hue_toggle_api_handler(
    AxumPath(id): AxumPath<String>,
) -> Response {
    let path = format!("/api/lights/{id}/toggle");
    match energy::http_post_json("127.0.0.1", 8125, &path, "{}", 3000).await {
        Ok(raw) => (
            axum::http::StatusCode::OK,
            [("content-type", "application/json")],
            raw,
        )
            .into_response(),
        Err(err) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "status": "error",
                "message": format!("Fehler beim Schalten: {err}"),
            })),
        )
            .into_response(),
    }
}

// ------------------------------------------------------------------------------------------------
// Mobile & Ignore API Handlers (mHomeNode Client & Neighbor Device Blocklist)
// ------------------------------------------------------------------------------------------------

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct HealthResponse {
    version: String,
    uptime_seconds: u64,
    active_matter_nodes: usize,
    active_ble_gateways: usize,
    active_devices: usize,
}

async fn v1_health_api_handler(State(state): State<WebState>) -> Response {
    let uptime_seconds = state.started_at.elapsed().as_secs();
    let mut active_devices = 0;
    let mut active_matter_nodes = 0;

    if let Ok(snapshot) = load_snapshot(&state.socket_path).await {
        for dev in &snapshot.devices {
            let status = dev.metadata.get("status").map(|s| s.as_str()).unwrap_or("active");
            if status == "active" {
                active_devices += 1;
            }
            let sources = dev.metadata.get("sources").map(|s| s.as_str()).unwrap_or("");
            let source = dev.metadata.get("source").map(|s| s.as_str()).unwrap_or("");
            if sources.contains("matter") || source == "matter" || dev.metadata.contains_key("matter_endpoint") {
                active_matter_nodes += 1;
            }
        }
    }

    let verified_gws = fetch_verified_shelly_gateways().await;
    let active_ble_gateways = verified_gws.iter().filter(|g| g.ble_supported).count();

    Json(HealthResponse {
        version: env!("CARGO_PKG_VERSION").to_string(),
        uptime_seconds,
        active_matter_nodes,
        active_ble_gateways,
        active_devices,
    })
    .into_response()
}

async fn v1_info_api_handler(State(state): State<WebState>) -> Response {
    Json(serde_json::json!({
        "server": "HomeNode Server",
        "version": env!("CARGO_PKG_VERSION"),
        "title": state.status_title,
        "mdns_service": "_homenode._tcp.local",
        "uptime_seconds": state.started_at.elapsed().as_secs(),
    }))
    .into_response()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MobileBleScanItem {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub rssi: Option<i16>,
    #[serde(default)]
    pub service_uuids: Vec<String>,
    #[serde(default)]
    pub manufacturer_data_hex: Option<String>,
    #[serde(default)]
    pub family: Option<String>,
    #[serde(default)]
    pub assigned_room: Option<String>,
    #[serde(default)]
    pub scout_name: Option<String>,
    #[serde(default)]
    pub bthome_version: Option<u8>,
    #[serde(default)]
    pub battery: Option<u8>,
    #[serde(default)]
    pub temperature_c: Option<f32>,
    #[serde(default)]
    pub humidity_pct: Option<f32>,
    #[serde(default)]
    pub illuminance_lux: Option<f32>,
    #[serde(default)]
    pub pressure_hpa: Option<f32>,
    #[serde(default)]
    pub contact_open: Option<bool>,
    #[serde(default)]
    pub motion_detected: Option<bool>,
    #[serde(default)]
    pub button_event: Option<String>,
}

pub fn mobile_ble_item_to_record(item: &MobileBleScanItem) -> Option<homenode_sdk::proto::DeviceRecord> {
    let norm_id = homenode_definitions::normalize_identifier(&item.id);
    if norm_id.is_empty() {
        return None;
    }

    let scout_label = item.scout_name.clone().unwrap_or_else(|| "iPhone".to_string());
    let dev_id = format!("mobile-ble-{}", norm_id);
    let display_name = item.name.clone().unwrap_or_else(|| {
        let prefix_len = item.id.len().min(8);
        format!("BLE {}", &item.id[..prefix_len])
    });
    let family_str = item.family.clone().unwrap_or_else(|| "Bluetooth LE".to_string());

    let mut meta = HashMap::new();
    meta.insert("source".to_string(), "mobile-scout".to_string());
    meta.insert("sources".to_string(), "mobile-scout,ble".to_string());
    meta.insert("scout".to_string(), scout_label);
    meta.insert("family".to_string(), family_str);
    meta.insert("status".to_string(), "active".to_string());
    meta.insert("protocol".to_string(), "ble".to_string());
    meta.insert("mac".to_string(), item.id.clone());

    if let Some(r) = item.rssi {
        meta.insert("rssi".to_string(), r.to_string());
    }
    if let Some(ref room) = item.assigned_room {
        meta.insert("room".to_string(), room.clone());
    }
    if let Some(bat) = item.battery {
        meta.insert("battery".to_string(), bat.to_string());
    }
    if let Some(temp) = item.temperature_c {
        meta.insert("temperature_c".to_string(), format!("{:.2}", temp));
    }
    if let Some(hum) = item.humidity_pct {
        meta.insert("humidity_pct".to_string(), format!("{:.1}", hum));
    }
    if let Some(lux) = item.illuminance_lux {
        meta.insert("illuminance_lux".to_string(), format!("{:.1}", lux));
    }
    if let Some(open) = item.contact_open {
        meta.insert("contact_state".to_string(), if open { "open".to_string() } else { "closed".to_string() });
    }
    if let Some(ref btn) = item.button_event {
        meta.insert("button_event".to_string(), btn.clone());
    }

    let kind = if item.contact_open.is_some() {
        "contact-sensor"
    } else if item.motion_detected.is_some() {
        "motion-sensor"
    } else if item.button_event.is_some() {
        "button"
    } else {
        "sensor"
    };

    Some(homenode_sdk::device_record(
        "web",
        dev_id,
        display_name,
        kind,
        ["ble", "mobile-scout"],
        meta,
    ))
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum MobileBleScanPayload {
    Batch(Vec<MobileBleScanItem>),
    Single(MobileBleScanItem),
}

async fn v1_mobile_ble_ingest_handler(
    State(state): State<WebState>,
    Json(payload): Json<MobileBleScanPayload>,
) -> Response {
    let items = match payload {
        MobileBleScanPayload::Batch(list) => list,
        MobileBleScanPayload::Single(item) => vec![item],
    };

    let mut ingested_count = 0;
    let mut ignored_count = 0;
    let mut bthome_forward_items = Vec::new();
    let mut dev_records = Vec::new();
    let mut valid_items = Vec::new();

    for item in items {
        let norm_id = homenode_definitions::normalize_identifier(&item.id);
        if norm_id.is_empty() {
            continue;
        }
        let name_clean = item.name.as_deref().unwrap_or("");

        if state.ignored_store.is_ignored(&item.id)
            || state.ignored_store.is_ignored(&norm_id)
            || (!name_clean.is_empty() && state.ignored_store.is_ignored(name_clean))
        {
            debug!("Mobile BLE scan item ignored (neighbor blocklist): {} ({})", item.id, name_clean);
            ignored_count += 1;
            continue;
        }

        if let Some(ref mfg_hex) = item.manufacturer_data_hex {
            let scout = item.scout_name.clone().unwrap_or_else(|| "iPhone (Mobile Scout)".to_string());
            bthome_forward_items.push(serde_json::json!({
                "mac": item.id,
                "rssi": item.rssi.unwrap_or(-70),
                "gateway": scout,
                "payload": mfg_hex,
                "data": mfg_hex,
                "payload_bytes": mfg_hex,
            }));
        }

        if let Some(record) = mobile_ble_item_to_record(&item) {
            dev_records.push(record);
            valid_items.push(item);
            ingested_count += 1;
        }
    }

    if !dev_records.is_empty() {
        {
            let mut store = state.mobile_ble_store.write().await;
            for it in valid_items {
                store.insert(it.id.clone(), it);
            }
            let _ = persist_json(&state.mobile_ble_path, &*store);
        }

        if let Ok(mut client) = connect_control_client(&state.socket_path).await {
            let _ = client.upsert_devices(homenode_sdk::proto::UpsertDevicesRequest {
                module_id: "web".to_string(),
                devices: dev_records,
                replace_all: false,
            }).await;

            if let Ok(snapshot) = load_snapshot(&state.socket_path).await {
                let mut links = state.links_store.read().await.clone();
                if auto_link_deterministic_devices(&snapshot.devices, &mut links) {
                    let mut store = state.links_store.write().await;
                    *store = links.clone();
                    let _ = persist_json(&state.links_path, &*store);
                }
            }
        }
    }

    if !bthome_forward_items.is_empty() {
        tokio::spawn(async move {
            for f_item in bthome_forward_items {
                let _ = energy::http_post_json("127.0.0.1", 8124, "/api/bthome", &f_item.to_string(), 1000).await;
            }
        });
    }

    Json(serde_json::json!({
        "status": "ok",
        "ingested": ingested_count,
        "ignored": ignored_count,
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
pub struct ClaimDevicePayload {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub family: Option<String>,
    #[serde(default)]
    pub assigned_room: Option<String>,
}

async fn v1_claim_device_handler(
    State(state): State<WebState>,
    Json(payload): Json<ClaimDevicePayload>,
) -> Response {
    let mut docs = state.docs_store.write().await;
    let entry = docs.entry(payload.id.clone()).or_default();
    if let Some(n) = payload.name {
        if !n.trim().is_empty() {
            entry.name = Some(n);
        }
    }
    if let Some(r) = payload.assigned_room {
        if !r.trim().is_empty() {
            entry.room = Some(r);
        }
    }
    entry.updated_at = chrono::Utc::now().to_rfc3339();

    let _ = persist_json(&state.docs_path, &*docs);

    Json(serde_json::json!({
        "status": "claimed",
        "id": payload.id,
    }))
    .into_response()
}

async fn get_ignored_devices_handler(State(state): State<WebState>) -> Response {
    let list = state.ignored_store.list();
    Json(list).into_response()
}

#[derive(Debug, Deserialize)]
pub struct IgnoreDeviceRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

async fn ignore_device_handler(
    AxumPath(id): AxumPath<String>,
    State(state): State<WebState>,
    body: Option<Json<IgnoreDeviceRequest>>,
) -> Response {
    let (name, reason) = if let Some(Json(b)) = body {
        (b.name, b.reason)
    } else {
        (None, None)
    };

    let record = homenode_definitions::IgnoredDeviceRecord::new(id.clone(), name, reason);
    match state.ignored_store.ignore(record) {
        Ok(_) => {
            info!("Device {} added to ignored devices blocklist", id);
            Json(serde_json::json!({"status": "ok", "ignored": true, "id": id})).into_response()
        }
        Err(err) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": err.to_string()})),
        )
            .into_response(),
    }
}

async fn unignore_device_handler(
    AxumPath(id): AxumPath<String>,
    State(state): State<WebState>,
) -> Response {
    match state.ignored_store.unignore(&id) {
        Ok(removed) => {
            info!("Device {} removed from ignored devices blocklist (removed={})", id, removed);
            Json(serde_json::json!({"status": "ok", "unignored": true, "removed": removed, "id": id})).into_response()
        }
        Err(err) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": err.to_string()})),
        )
            .into_response(),
    }
}

// ------------------------------------------------------------------------------------------------
// Room & Location Management API Handlers (Matter 1.3+ & Apple Home Compatible)
// ------------------------------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct RoomWithCount {
    #[serde(flatten)]
    pub room: RoomRecord,
    pub device_count: usize,
}

async fn get_rooms_handler(State(state): State<WebState>) -> Response {
    let rooms = state.rooms_store.list();
    let docs = state.docs_store.read().await;

    let mut counts: HashMap<String, usize> = HashMap::new();
    for doc in docs.values() {
        if let Some(ref r) = doc.room {
            let key = r.trim().to_lowercase();
            *counts.entry(key).or_insert(0) += 1;
        }
    }

    let response: Vec<RoomWithCount> = rooms
        .into_iter()
        .map(|r| {
            let count = *counts.get(&r.name.trim().to_lowercase()).unwrap_or(&0);
            RoomWithCount {
                room: r,
                device_count: count,
            }
        })
        .collect();

    Json(response).into_response()
}

async fn save_room_handler(
    State(state): State<WebState>,
    Json(mut payload): Json<RoomRecord>,
) -> Response {
    let old_room = if !payload.id.trim().is_empty() {
        state.rooms_store.get(&payload.id)
    } else {
        None
    };

    let old_name = old_room.as_ref().map(|r| r.name.clone());
    let old_hue_group_id = old_room.as_ref().and_then(|r| r.hue_group_id.clone());

    if payload.hue_group_id.is_none() {
        payload.hue_group_id = old_hue_group_id.clone();
    }

    match state.rooms_store.upsert(payload) {
        Ok(saved) => {
            // 1. If name changed, propagate to docs and light_groups
            if let Some(old_n) = old_name {
                if old_n.to_lowercase() != saved.name.to_lowercase() {
                    info!("Room renamed from '{}' to '{}'. Propagating to device documentation and light groups...", old_n, saved.name);
                    // Update device documentation
                    let mut docs_guard = state.docs_store.write().await;
                    let mut docs_changed = false;
                    for doc in docs_guard.values_mut() {
                        if doc.room.as_deref().map(|s| s.to_lowercase()) == Some(old_n.to_lowercase()) {
                            doc.room = Some(saved.name.clone());
                            doc.updated_at = chrono::Utc::now().to_rfc3339();
                            docs_changed = true;
                        }
                    }
                    if docs_changed {
                        let _ = persist_json(&state.docs_path, &*docs_guard);
                    }

                    // Update light groups
                    let groups = state.light_groups_store.list();
                    for mut g in groups {
                        if g.room.to_lowercase() == old_n.to_lowercase() {
                            g.room = saved.name.clone();
                            let _ = state.light_groups_store.upsert(g);
                        }
                    }
                }
            }

            // 2. If hue_group_id is present, sync with Hue Bridge
            if let Some(ref gid) = saved.hue_group_id {
                let creds_path = state.rooms_path.parent().unwrap_or(Path::new(".")).join("hue_credentials.json");
                if creds_path.exists() {
                    if let Ok(creds_data) = std::fs::read_to_string(&creds_path) {
                        if let Ok(creds) = serde_json::from_str::<serde_json::Value>(&creds_data) {
                            let bridge_ip = creds.get("bridge_ip").and_then(|v| v.as_str()).unwrap_or("192.168.178.12");
                            let username = creds.get("username").and_then(|v| v.as_str()).unwrap_or("");
                            if !username.is_empty() {
                                let client = reqwest::Client::builder()
                                    .timeout(Duration::from_secs(4))
                                    .build()
                                    .unwrap_or_default();
                                let hue_class = saved.hue_class.clone().unwrap_or_else(|| {
                                    match saved.archetype.as_deref() {
                                        Some("living_room") => "Living room".to_string(),
                                        Some("kitchen") => "Kitchen".to_string(),
                                        Some("dining_room") => "Dining".to_string(),
                                        Some("bedroom") => "Bedroom".to_string(),
                                        Some("kids_room") => "Kids bedroom".to_string(),
                                        Some("bathroom") => "Bathroom".to_string(),
                                        Some("office") => "Home office".to_string(),
                                        Some("hallway") => "Hallway".to_string(),
                                        Some("outdoor") => "Garden".to_string(),
                                        Some("garage") => "Garage".to_string(),
                                        _ => "Other".to_string(),
                                    }
                                });
                                let hue_payload = serde_json::json!({
                                    "name": saved.name,
                                    "class": hue_class
                                });
                                let url = format!("http://{}/api/{}/groups/{}", bridge_ip, username, gid);
                                tokio::spawn(async move {
                                    match client.put(&url).json(&hue_payload).send().await {
                                        Ok(resp) => {
                                            let text = resp.text().await.unwrap_or_default();
                                            info!("Synced room update to Hue Bridge {}: {}", url, text);
                                        }
                                        Err(e) => {
                                            warn!("Failed to sync room update to Hue Bridge: {}", e);
                                        }
                                    }
                                });
                            }
                        }
                    }
                }
            }

            info!("Room {} ('{}') saved successfully", saved.id, saved.name);
            Json(serde_json::json!({
                "status": "ok",
                "room": saved
            }))
            .into_response()
        }
        Err(err) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "status": "error", "message": err.to_string() })),
        )
            .into_response(),
    }
}

async fn delete_room_handler(
    AxumPath(id): AxumPath<String>,
    State(state): State<WebState>,
) -> Response {
    match state.rooms_store.delete(&id) {
        Ok(deleted) => {
            info!("Room {} deleted (found: {})", id, deleted);
            Json(serde_json::json!({
                "status": "ok",
                "deleted": deleted,
                "id": id
            }))
            .into_response()
        }
        Err(err) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "status": "error", "message": err.to_string() })),
        )
            .into_response(),
    }
}

async fn get_floors_handler(State(state): State<WebState>) -> Response {
    let floors = state.rooms_store.distinct_floors();
    Json(floors).into_response()
}

async fn import_hue_rooms_handler(State(state): State<WebState>) -> Response {
    let mut imported_count = 0;
    let mut assigned_devices = 0;

    // 1. Try reading groups directly from Hue Bridge using saved credentials
    let creds_path = state.rooms_path.parent().unwrap_or(Path::new(".")).join("hue_credentials.json");
    let mut bridge_success = false;

    if creds_path.exists() {
        if let Ok(creds_data) = std::fs::read_to_string(&creds_path) {
            if let Ok(creds) = serde_json::from_str::<serde_json::Value>(&creds_data) {
                let bridge_ip = creds.get("bridge_ip").and_then(|v| v.as_str()).unwrap_or("192.168.178.12");
                let username = creds.get("username").and_then(|v| v.as_str()).unwrap_or("");
                if !username.is_empty() {
                    let client = reqwest::Client::builder()
                        .timeout(Duration::from_secs(5))
                        .build()
                        .unwrap_or_default();
                    let url = format!("http://{}/api/{}/groups", bridge_ip, username);
                    if let Ok(resp) = client.get(&url).send().await {
                        if let Ok(groups) = resp.json::<HashMap<String, serde_json::Value>>().await {
                            bridge_success = true;
                            let mut docs_guard = state.docs_store.write().await;
                            let mut docs_changed = false;

                            for (group_id, g) in &groups {
                                let g_type = g.get("type").and_then(|v| v.as_str()).unwrap_or("");
                                if g_type == "Room" || g_type == "Zone" {
                                    let name = g.get("name").and_then(|v| v.as_str()).unwrap_or("").trim();
                                    if name.is_empty() { continue; }
                                    let g_class = g.get("class").and_then(|v| v.as_str()).unwrap_or("");
                                    let room_id = slugify_room_id(name);

                                    let mut rec = RoomRecord::new(
                                        room_id.clone(),
                                        name.to_string(),
                                        None,
                                        None,
                                        Some(g_class.to_string()),
                                    );
                                    rec.hue_group_id = Some(group_id.clone());
                                    rec.hue_class = Some(g_class.to_string());
                                    if let Ok(_) = state.rooms_store.upsert(rec) {
                                        imported_count += 1;
                                    }

                                    // Assign lights
                                    if let Some(lights) = g.get("lights").and_then(|v| v.as_array()) {
                                        for lid in lights {
                                            if let Some(lid_str) = lid.as_str() {
                                                let dev_key = format!("hue-light-{}", lid_str);
                                                let entry = docs_guard.entry(dev_key).or_insert_with(|| DeviceDocumentation {
                                                    updated_at: chrono::Utc::now().to_rfc3339(),
                                                    ..Default::default()
                                                });
                                                if entry.room.as_deref() != Some(name) {
                                                    entry.room = Some(name.to_string());
                                                    entry.updated_at = chrono::Utc::now().to_rfc3339();
                                                    docs_changed = true;
                                                    assigned_devices += 1;
                                                }
                                            }
                                        }
                                    }

                                    // Assign sensors
                                    if let Some(sensors) = g.get("sensors").and_then(|v| v.as_array()) {
                                        for sid in sensors {
                                            if let Some(sid_str) = sid.as_str() {
                                                let dev_key = format!("hue-sensor-{}", sid_str);
                                                let entry = docs_guard.entry(dev_key).or_insert_with(|| DeviceDocumentation {
                                                    updated_at: chrono::Utc::now().to_rfc3339(),
                                                    ..Default::default()
                                                });
                                                if entry.room.as_deref() != Some(name) {
                                                    entry.room = Some(name.to_string());
                                                    entry.updated_at = chrono::Utc::now().to_rfc3339();
                                                    docs_changed = true;
                                                    assigned_devices += 1;
                                                }
                                            }
                                        }
                                    }
                                }
                            }

                            if docs_changed {
                                let _ = persist_json(&state.docs_path, &*docs_guard);
                            }
                        }
                    }
                }
            }
        }
    }

    // 2. Fallback to querying the Hue module on port 8125 if direct bridge failed
    if !bridge_success {
        if let Ok(raw_rooms) = energy::http_get_json("127.0.0.1", 8125, "/api/rooms", 3000).await {
            if let Ok(hue_rooms) = serde_json::from_str::<Vec<RoomRecord>>(&raw_rooms) {
                for r in hue_rooms {
                    if let Ok(_) = state.rooms_store.upsert(r) {
                        imported_count += 1;
                    }
                }
            }
        }
    }

    info!("Imported {} room(s) and assigned {} device(s) from Hue", imported_count, assigned_devices);
    Json(serde_json::json!({
        "status": "ok",
        "imported_rooms": imported_count,
        "assigned_devices": assigned_devices,
        "message": format!("{} Räume importiert und {} Geräte zugeordnet.", imported_count, assigned_devices)
    })).into_response()
}

// ------------------------------------------------------------------------------------------------
// Govee Local UDP API & Rooms Page Handlers
// ------------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct SetGoveePowerPayload {
    on: bool,
}

#[derive(Debug, Deserialize)]
struct SetGoveeBrightnessPayload {
    brightness: u8,
}

#[derive(Debug, Deserialize)]
struct SetGoveeColorPayload {
    r: u8,
    g: u8,
    b: u8,
}

#[derive(Debug, Deserialize)]
struct SetGoveeTempPayload {
    kelvin: u32,
}

async fn get_govee_lights_handler(State(state): State<WebState>) -> Response {
    let devs = state.govee_manager.list_devices().await;
    Json(devs).into_response()
}

async fn toggle_govee_light_handler(
    AxumPath(ip): AxumPath<String>,
    State(state): State<WebState>,
) -> Response {
    let current = state.govee_manager.get_device(&ip).await;
    let next_state = current.map(|s| !s.on_off).unwrap_or(true);
    match state.govee_manager.set_power(&ip, next_state).await {
        Ok(_) => Json(serde_json::json!({ "status": "ok", "on": next_state })).into_response(),
        Err(err) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "status": "error", "message": err.to_string() })),
        )
            .into_response(),
    }
}

async fn set_govee_power_handler(
    AxumPath(ip): AxumPath<String>,
    State(state): State<WebState>,
    Json(payload): Json<SetGoveePowerPayload>,
) -> Response {
    match state.govee_manager.set_power(&ip, payload.on).await {
        Ok(_) => Json(serde_json::json!({ "status": "ok", "on": payload.on })).into_response(),
        Err(err) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "status": "error", "message": err.to_string() })),
        )
            .into_response(),
    }
}

async fn set_govee_brightness_handler(
    AxumPath(ip): AxumPath<String>,
    State(state): State<WebState>,
    Json(payload): Json<SetGoveeBrightnessPayload>,
) -> Response {
    match state.govee_manager.set_brightness(&ip, payload.brightness).await {
        Ok(_) => Json(serde_json::json!({ "status": "ok", "brightness": payload.brightness })).into_response(),
        Err(err) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "status": "error", "message": err.to_string() })),
        )
            .into_response(),
    }
}

async fn set_govee_color_handler(
    AxumPath(ip): AxumPath<String>,
    State(state): State<WebState>,
    Json(payload): Json<SetGoveeColorPayload>,
) -> Response {
    match state.govee_manager.set_color(&ip, payload.r, payload.g, payload.b).await {
        Ok(_) => Json(serde_json::json!({ "status": "ok", "r": payload.r, "g": payload.g, "b": payload.b })).into_response(),
        Err(err) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "status": "error", "message": err.to_string() })),
        )
            .into_response(),
    }
}

async fn set_govee_temperature_handler(
    AxumPath(ip): AxumPath<String>,
    State(state): State<WebState>,
    Json(payload): Json<SetGoveeTempPayload>,
) -> Response {
    match state.govee_manager.set_color_temp(&ip, payload.kelvin).await {
        Ok(_) => Json(serde_json::json!({ "status": "ok", "kelvin": payload.kelvin })).into_response(),
        Err(err) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "status": "error", "message": err.to_string() })),
        )
            .into_response(),
    }
}

async fn get_govee_status_handler(
    AxumPath(ip): AxumPath<String>,
    State(state): State<WebState>,
) -> Response {
    let _ = state.govee_manager.query_status(&ip).await;
    let dev = state.govee_manager.get_device(&ip).await;
    Json(dev).into_response()
}

async fn rooms_page_handler(State(state): State<WebState>) -> Html<String> {
    let docs = state.docs_store.read().await.clone();
    let mut links = state.links_store.read().await.clone();
    let matter_fabrics = state.matter_fabrics_store.read().await.clone();
    let rooms = state.rooms_store.list();
    let light_groups = state.light_groups_store.list();
    let govee_list = state.govee_manager.list_devices().await;
    let govee_states: HashMap<String, govee::GoveeDeviceState> = govee_list.into_iter().map(|d| (d.ip.clone(), d)).collect();

    let body = match load_snapshot(&state.socket_path).await {
        Ok(snapshot) => {
            let _ = auto_link_deterministic_devices(&snapshot.devices, &mut links);
            let unified_devices = build_unified_devices(&snapshot.devices, &links);
            rooms::render_rooms_page(
                &state.status_title,
                &rooms,
                &unified_devices,
                &docs,
                &govee_states,
                &matter_fabrics,
                &light_groups,
            )
        }
        Err(error) => render_error(&state.status_title, "rooms", &error.to_string()),
    };
    Html(body)
}

#[derive(Debug, Deserialize)]
struct CreateLightGroupPayload {
    name: String,
    room: String,
    device_ids: Vec<String>,
    #[serde(default)]
    icon: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GroupPowerPayload {
    on: bool,
}

#[derive(Debug, Deserialize)]
struct GroupBrightnessPayload {
    brightness: u8,
}

#[derive(Debug, Deserialize)]
struct GroupColorPayload {
    r: u8,
    g: u8,
    b: u8,
}

#[derive(Debug, Deserialize)]
struct GroupTempPayload {
    kelvin: u32,
}

#[derive(Debug, Deserialize)]
struct RoomScenePayload {
    scene: String,
}

async fn list_light_groups_handler(State(state): State<WebState>) -> Json<Vec<homenode_definitions::LightGroup>> {
    Json(state.light_groups_store.list())
}

async fn create_light_group_handler(
    State(state): State<WebState>,
    Json(payload): Json<CreateLightGroupPayload>,
) -> Response {
    let id = format!("group-{}", homenode_definitions::slugify_room_id(&payload.name));
    let group = homenode_definitions::LightGroup::new(
        id,
        payload.name,
        payload.room,
        payload.device_ids,
        payload.icon.or_else(|| Some("✨".to_string())),
    );
    match state.light_groups_store.upsert(group.clone()) {
        Ok(_) => (axum::http::StatusCode::CREATED, Json(serde_json::json!({ "status": "ok", "group": group }))).into_response(),
        Err(e) => (axum::http::StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "status": "error", "message": e.to_string() }))).into_response(),
    }
}

async fn delete_light_group_handler(
    AxumPath(id): AxumPath<String>,
    State(state): State<WebState>,
) -> Response {
    match state.light_groups_store.delete(&id) {
        Ok(true) => Json(serde_json::json!({ "status": "ok" })).into_response(),
        Ok(false) => (axum::http::StatusCode::NOT_FOUND, Json(serde_json::json!({ "status": "not_found" }))).into_response(),
        Err(e) => (axum::http::StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "status": "error", "message": e.to_string() }))).into_response(),
    }
}

async fn control_devices_power(
    state: &WebState,
    device_ids: &[String],
    on: bool,
) {
    let snapshot = load_snapshot(&state.socket_path).await.ok();
    for dev_id in device_ids {
        if let Some(ref snap) = snapshot {
            if let Some(dev) = snap.devices.iter().find(|d| d.device_id == *dev_id) {
                if let Some(ip) = dev.metadata.get("ip") {
                    if !ip.is_empty() && ip != "-" && !ip.contains('(') {
                        let _ = state.govee_manager.set_power(ip, on).await;
                    }
                }
                if dev.module_id == "philips-hue" || dev.metadata.get("source").map(|s| s.as_str()) == Some("philips-hue") {
                    let hue_id = dev.metadata.get("hue_light_id").cloned()
                        .unwrap_or_else(|| dev.device_id.replace("hue-light-", ""));
                    let path = format!("/api/lights/{hue_id}/state");
                    let _ = energy::http_post_json("127.0.0.1", 8125, &path, &format!(r#"{{"on":{on}}}"#), 2000).await;
                }
            }
        }
        if dev_id.starts_with("net-") {
            let ip = dev_id.trim_start_matches("net-").replace('-', ".");
            let _ = state.govee_manager.set_power(&ip, on).await;
        }
    }
}

async fn control_devices_brightness(
    state: &WebState,
    device_ids: &[String],
    brightness: u8,
) {
    let snapshot = load_snapshot(&state.socket_path).await.ok();
    let hue_bri = ((brightness as f32 / 100.0) * 254.0).round() as u8;
    for dev_id in device_ids {
        if let Some(ref snap) = snapshot {
            if let Some(dev) = snap.devices.iter().find(|d| d.device_id == *dev_id) {
                if let Some(ip) = dev.metadata.get("ip") {
                    if !ip.is_empty() && ip != "-" && !ip.contains('(') {
                        let _ = state.govee_manager.set_brightness(ip, brightness).await;
                    }
                }
                if dev.module_id == "philips-hue" || dev.metadata.get("source").map(|s| s.as_str()) == Some("philips-hue") {
                    let hue_id = dev.metadata.get("hue_light_id").cloned()
                        .unwrap_or_else(|| dev.device_id.replace("hue-light-", ""));
                    let path = format!("/api/lights/{hue_id}/state");
                    let _ = energy::http_post_json("127.0.0.1", 8125, &path, &format!(r#"{{"on":true,"bri":{hue_bri}}}"#), 2000).await;
                }
            }
        }
        if dev_id.starts_with("net-") {
            let ip = dev_id.trim_start_matches("net-").replace('-', ".");
            let _ = state.govee_manager.set_brightness(&ip, brightness).await;
        }
    }
}

async fn control_devices_color(
    state: &WebState,
    device_ids: &[String],
    r: u8,
    g: u8,
    b: u8,
) {
    let snapshot = load_snapshot(&state.socket_path).await.ok();
    for dev_id in device_ids {
        if let Some(ref snap) = snapshot {
            if let Some(dev) = snap.devices.iter().find(|d| d.device_id == *dev_id) {
                if let Some(ip) = dev.metadata.get("ip") {
                    if !ip.is_empty() && ip != "-" && !ip.contains('(') {
                        let _ = state.govee_manager.set_color(ip, r, g, b).await;
                    }
                }
                if dev.module_id == "philips-hue" || dev.metadata.get("source").map(|s| s.as_str()) == Some("philips-hue") {
                    let hue_id = dev.metadata.get("hue_light_id").cloned()
                        .unwrap_or_else(|| dev.device_id.replace("hue-light-", ""));
                    let path = format!("/api/lights/{hue_id}/state");
                    let _ = energy::http_post_json("127.0.0.1", 8125, &path, r#"{"on":true}"#, 2000).await;
                }
            }
        }
        if dev_id.starts_with("net-") {
            let ip = dev_id.trim_start_matches("net-").replace('-', ".");
            let _ = state.govee_manager.set_color(&ip, r, g, b).await;
        }
    }
}

async fn control_devices_temp(
    state: &WebState,
    device_ids: &[String],
    kelvin: u32,
) {
    let snapshot = load_snapshot(&state.socket_path).await.ok();
    let mired = (1_000_000 / kelvin.max(2000).min(6500)).clamp(153, 500);
    for dev_id in device_ids {
        if let Some(ref snap) = snapshot {
            if let Some(dev) = snap.devices.iter().find(|d| d.device_id == *dev_id) {
                if let Some(ip) = dev.metadata.get("ip") {
                    if !ip.is_empty() && ip != "-" && !ip.contains('(') {
                        let _ = state.govee_manager.set_color_temp(ip, kelvin).await;
                    }
                }
                if dev.module_id == "philips-hue" || dev.metadata.get("source").map(|s| s.as_str()) == Some("philips-hue") {
                    let hue_id = dev.metadata.get("hue_light_id").cloned()
                        .unwrap_or_else(|| dev.device_id.replace("hue-light-", ""));
                    let path = format!("/api/lights/{hue_id}/state");
                    let _ = energy::http_post_json("127.0.0.1", 8125, &path, &format!(r#"{{"on":true,"ct":{mired}}}"#), 2000).await;
                }
            }
        }
        if dev_id.starts_with("net-") {
            let ip = dev_id.trim_start_matches("net-").replace('-', ".");
            let _ = state.govee_manager.set_color_temp(&ip, kelvin).await;
        }
    }
}

async fn light_group_power_handler(
    AxumPath(id): AxumPath<String>,
    State(state): State<WebState>,
    Json(payload): Json<GroupPowerPayload>,
) -> Response {
    if let Some(group) = state.light_groups_store.get(&id) {
        control_devices_power(&state, &group.device_ids, payload.on).await;
        Json(serde_json::json!({ "status": "ok", "on": payload.on, "group_id": id })).into_response()
    } else {
        (axum::http::StatusCode::NOT_FOUND, Json(serde_json::json!({ "status": "not_found" }))).into_response()
    }
}

async fn light_group_brightness_handler(
    AxumPath(id): AxumPath<String>,
    State(state): State<WebState>,
    Json(payload): Json<GroupBrightnessPayload>,
) -> Response {
    if let Some(group) = state.light_groups_store.get(&id) {
        control_devices_brightness(&state, &group.device_ids, payload.brightness).await;
        Json(serde_json::json!({ "status": "ok", "brightness": payload.brightness, "group_id": id })).into_response()
    } else {
        (axum::http::StatusCode::NOT_FOUND, Json(serde_json::json!({ "status": "not_found" }))).into_response()
    }
}

async fn light_group_color_handler(
    AxumPath(id): AxumPath<String>,
    State(state): State<WebState>,
    Json(payload): Json<GroupColorPayload>,
) -> Response {
    if let Some(group) = state.light_groups_store.get(&id) {
        control_devices_color(&state, &group.device_ids, payload.r, payload.g, payload.b).await;
        Json(serde_json::json!({ "status": "ok", "color": [payload.r, payload.g, payload.b], "group_id": id })).into_response()
    } else {
        (axum::http::StatusCode::NOT_FOUND, Json(serde_json::json!({ "status": "not_found" }))).into_response()
    }
}

async fn light_group_temperature_handler(
    AxumPath(id): AxumPath<String>,
    State(state): State<WebState>,
    Json(payload): Json<GroupTempPayload>,
) -> Response {
    if let Some(group) = state.light_groups_store.get(&id) {
        control_devices_temp(&state, &group.device_ids, payload.kelvin).await;
        Json(serde_json::json!({ "status": "ok", "kelvin": payload.kelvin, "group_id": id })).into_response()
    } else {
        (axum::http::StatusCode::NOT_FOUND, Json(serde_json::json!({ "status": "not_found" }))).into_response()
    }
}

async fn room_scene_handler(
    AxumPath(room): AxumPath<String>,
    State(state): State<WebState>,
    Json(payload): Json<RoomScenePayload>,
) -> Response {
    let room_lower = room.trim().to_lowercase();
    let docs = state.docs_store.read().await.clone();
    let mut target_device_ids = Vec::new();

    if let Ok(snapshot) = load_snapshot(&state.socket_path).await {
        for dev in &snapshot.devices {
            let mac = dev.metadata.get("mac").cloned().unwrap_or_default();
            let doc_key = if !mac.is_empty() { mac.clone() } else { dev.device_id.clone() };
            let doc = docs.get(&doc_key)
                .or_else(|| docs.get(&mac))
                .or_else(|| docs.get(&mac.to_lowercase()))
                .or_else(|| docs.get(&dev.device_id));
            let dev_room = doc.and_then(|d| d.room.as_deref())
                .or_else(|| dev.metadata.get("room").map(|s| s.as_str()))
                .unwrap_or("");
            if dev_room.trim().to_lowercase() == room_lower {
                let is_button_or_sensor = dev.kind == "button"
                    || dev.metadata.get("category").map(|s| s.as_str()) == Some("button")
                    || dev.kind == "sensor"
                    || dev.device_id.starts_with("hue-sensor-");

                let is_light = !is_button_or_sensor && (
                    dev.metadata.get("category").map(|s| s.as_str()) == Some("lighting")
                    || dev.kind == "lighting"
                    || (dev.module_id == "philips-hue" && dev.metadata.get("hue_light_id").is_some())
                    || dev.display_name.to_lowercase().contains("govee")
                    || dev.metadata.get("hostname").map(|h| h.to_lowercase().contains("govee")).unwrap_or(false)
                );
                if is_light {
                    target_device_ids.push(dev.device_id.clone());
                }
            }
        }
    }

    match payload.scene.as_str() {
        "all_off" => {
            control_devices_power(&state, &target_device_ids, false).await;
        }
        "all_on" => {
            control_devices_power(&state, &target_device_ids, true).await;
            control_devices_brightness(&state, &target_device_ids, 100).await;
            control_devices_temp(&state, &target_device_ids, 4000).await;
        }
        "relax" => {
            control_devices_power(&state, &target_device_ids, true).await;
            control_devices_brightness(&state, &target_device_ids, 40).await;
            control_devices_temp(&state, &target_device_ids, 2700).await;
        }
        "focus" => {
            control_devices_power(&state, &target_device_ids, true).await;
            control_devices_brightness(&state, &target_device_ids, 100).await;
            control_devices_temp(&state, &target_device_ids, 6500).await;
        }
        "night" => {
            control_devices_power(&state, &target_device_ids, true).await;
            control_devices_brightness(&state, &target_device_ids, 10).await;
            control_devices_temp(&state, &target_device_ids, 2200).await;
        }
        "cinema" => {
            control_devices_power(&state, &target_device_ids, true).await;
            control_devices_brightness(&state, &target_device_ids, 20).await;
            control_devices_color(&state, &target_device_ids, 0, 100, 255).await;
        }
        _ => {}
    }

    Json(serde_json::json!({
        "status": "ok",
        "scene": payload.scene,
        "room": room,
        "devices_count": target_device_ids.len()
    })).into_response()
}

// ------------------------------------------------------------------------------------------------
// Matter Fabrics API Handlers
// ------------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct UpdateMatterFabricPayload {
    name: String,
    #[serde(default)]
    icon: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

async fn get_matter_fabrics_handler(State(state): State<WebState>) -> Response {
    let fabrics = state.matter_fabrics_store.read().await;
    Json(&*fabrics).into_response()
}

async fn update_matter_fabric_handler(
    AxumPath(fabric_id): AxumPath<String>,
    State(state): State<WebState>,
    Json(payload): Json<UpdateMatterFabricPayload>,
) -> Response {
    let mut fabrics = state.matter_fabrics_store.write().await;
    let key = fabric_id.trim().to_uppercase();
    let entry = fabrics.entry(key).or_insert_with(|| MatterFabricMeta {
        name: String::new(),
        icon: "✨".to_string(),
        description: String::new(),
    });
    entry.name = payload.name.trim().to_string();
    if let Some(icon) = payload.icon {
        if !icon.trim().is_empty() {
            entry.icon = icon.trim().to_string();
        }
    }
    if let Some(desc) = payload.description {
        entry.description = desc.trim().to_string();
    }
    if let Err(err) = persist_json(&state.matter_fabrics_path, &*fabrics) {
        error!("Failed to persist matter fabrics: {err}");
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": err.to_string()})),
        )
            .into_response();
    }
    Json(serde_json::json!({"status": "updated"})).into_response()
}

// ------------------------------------------------------------------------------------------------
// Vendor & Product Catalog API Handlers
// ------------------------------------------------------------------------------------------------

async fn get_catalog_handler(State(state): State<WebState>) -> Response {
    let catalog = state.catalog_store.read().await;
    Json(&*catalog).into_response()
}

async fn add_vendor_handler(
    State(state): State<WebState>,
    Json(vendor): Json<homenode_definitions::Vendor>,
) -> Response {
    let mut catalog = state.catalog_store.write().await;
    catalog.add_or_update_vendor(vendor);
    if let Err(err) = catalog.save_to_path(&state.catalog_overrides_path) {
        error!("Failed to persist catalog overrides: {err}");
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": err.to_string()})),
        )
            .into_response();
    }
    Json(serde_json::json!({"status": "saved"})).into_response()
}

async fn add_product_handler(
    State(state): State<WebState>,
    Json(product): Json<homenode_definitions::Product>,
) -> Response {
    let mut catalog = state.catalog_store.write().await;
    catalog.add_or_update_product(product);
    if let Err(err) = catalog.save_to_path(&state.catalog_overrides_path) {
        error!("Failed to persist catalog overrides: {err}");
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": err.to_string()})),
        )
            .into_response();
    }
    Json(serde_json::json!({"status": "saved"})).into_response()
}

#[derive(Deserialize)]
struct AssignProductRequest {
    doc_key: String,
    product_id: String,
}

async fn assign_device_product_handler(
    AxumPath(device_id): AxumPath<String>,
    State(state): State<WebState>,
    Json(payload): Json<AssignProductRequest>,
) -> Response {
    let catalog = state.catalog_store.read().await;
    if let Some(product) = catalog.find_product(&payload.product_id) {
        let mut docs = state.docs_store.write().await;
        let entry = docs.entry(payload.doc_key).or_default();
        entry.product_id = Some(product.id.clone());
        if let Some(doc_url) = &product.documentation_url {
            if entry.manual_url.is_none() || entry.manual_url.as_deref() == Some("") {
                entry.manual_url = Some(doc_url.clone());
            }
        }
        entry.updated_at = chrono::Utc::now().to_rfc3339();
        let _ = persist_json(&state.docs_path, &*docs);
    }
    Json(serde_json::json!({
        "status": "assigned",
        "device_id": device_id,
        "product_id": payload.product_id
    }))
    .into_response()
}

// ------------------------------------------------------------------------------------------------
// Device Analyzer & Rhai Generation handlers
// ------------------------------------------------------------------------------------------------

#[derive(Serialize)]
struct AnalysisPortResult {
    port: u16,
    service: &'static str,
}

#[derive(Serialize)]
struct DeviceAnalysisResponse {
    device_id: String,
    ip: String,
    mac: Option<String>,
    hostname: Option<String>,
    vendor: Option<String>,
    open_ports: Vec<AnalysisPortResult>,
    total_ports_scanned: usize,
    http_title: Option<String>,
    http_server: Option<String>,
    suggested_category: String,
    suggested_title: String,
    suggested_icon: String,
    suggested_filename: String,
    suggested_rhai_script: String,
}

async fn analyze_device_handler(
    AxumPath(device_id): AxumPath<String>,
    State(state): State<WebState>,
) -> Response {
    let snapshot = match load_snapshot(&state.socket_path).await {
        Ok(s) => s,
        Err(err) => {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": err.to_string()})),
            )
                .into_response();
        }
    };

    let target = snapshot.devices.into_iter().find(|d| d.device_id == device_id);
    let device = match target {
        Some(d) => d,
        None => {
            return (
                axum::http::StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "device_not_found"})),
            )
                .into_response();
        }
    };

    let ip_str = device
        .metadata
        .get("ip")
        .cloned()
        .unwrap_or_else(|| device.device_id.replace("net-", "").replace('-', "."));
    let ip: Ipv4Addr = match ip_str.parse() {
        Ok(addr) => addr,
        Err(_) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "invalid_device_ip"})),
            )
                .into_response();
        }
    };

    let probe_ports = [
        21, 22, 23, 53, 80, 81, 443, 554, 1883, 1900, 5000, 5001, 5060, 6053, 8080, 8081, 8443,
        8883, 9000,
    ];
    let total_ports_scanned = probe_ports.len();
    let mut join_set = tokio::task::JoinSet::new();
    for port in probe_ports {
        join_set.spawn(async move {
            let addr = SocketAddr::new(IpAddr::V4(ip), port);
            let open = tokio::time::timeout(Duration::from_millis(200), TcpStream::connect(addr))
                .await
                .is_ok_and(|r| r.is_ok());
            (port, open)
        });
    }

    let mut open_ports = Vec::new();
    while let Some(res) = join_set.join_next().await {
        if let Ok((port, true)) = res {
            open_ports.push(port);
        }
    }
    open_ports.sort_unstable();

    let mut http_server = None;
    let mut http_title = None;
    for web_port in [80, 5000, 8080, 443] {
        if open_ports.contains(&web_port) {
            if let Some((server, title)) = probe_http_banner(ip, web_port).await {
                if http_server.is_none() {
                    http_server = server;
                }
                if http_title.is_none() {
                    http_title = title;
                }
                break;
            }
        }
    }

    let mac = device.metadata.get("mac").cloned();
    let hostname = device.metadata.get("hostname").cloned();
    let vendor = device.metadata.get("vendor").cloned();

    let (suggested_cat, suggested_title, suggested_icon) = deduce_analyzer_category(
        &device.display_name,
        hostname.as_deref(),
        vendor.as_deref(),
        &open_ports,
        http_title.as_deref(),
    );

    let script_name = device_id
        .replace("net-", "")
        .replace('.', "_")
        .replace('-', "_")
        .to_lowercase();
    let suggested_filename = format!("{script_name}.rhai");

    let suggested_rhai_script = generate_rhai_script(
        &script_name,
        &device.display_name,
        hostname.as_deref().unwrap_or_default(),
        vendor.as_deref().unwrap_or_default(),
        &suggested_cat,
        suggested_title,
        suggested_icon,
        &open_ports,
        http_title.as_deref(),
    );

    let port_results = open_ports
        .into_iter()
        .map(|p| AnalysisPortResult {
            port: p,
            service: port_description(p),
        })
        .collect();

    Json(DeviceAnalysisResponse {
        device_id: device.device_id,
        ip: ip_str,
        mac,
        hostname,
        vendor,
        open_ports: port_results,
        total_ports_scanned,
        http_title,
        http_server,
        suggested_category: suggested_cat,
        suggested_title: suggested_title.to_string(),
        suggested_icon: suggested_icon.to_string(),
        suggested_filename,
        suggested_rhai_script,
    })
    .into_response()
}

async fn ping_device_handler(
    AxumPath(device_id): AxumPath<String>,
    State(state): State<WebState>,
) -> Response {
    let snapshot = match load_snapshot(&state.socket_path).await {
        Ok(s) => s,
        Err(err) => {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "reachable": false,
                    "error": err.to_string(),
                    "message": "Failed to load runtime snapshot"
                })),
            )
                .into_response();
        }
    };

    let found_ip = snapshot
        .devices
        .iter()
        .find(|d| d.device_id == device_id || d.metadata.get("mac").map(|m| m.as_str()) == Some(&device_id))
        .and_then(|d| d.metadata.get("ip").cloned())
        .or_else(|| {
            let history_path = state.docs_path.parent().unwrap_or(std::path::Path::new(".")).join("device_history.json");
            if let Ok(content) = std::fs::read_to_string(&history_path) {
                if let Ok(hist) = serde_json::from_str::<serde_json::Value>(&content) {
                    let norm = device_id.trim().to_lowercase();
                    if let Some(records) = hist.get("records").and_then(|r| r.as_object()) {
                        for (k, v) in records {
                            let k_norm = k.trim().to_lowercase();
                            let mac_norm = v.get("mac").and_then(|m| m.as_str()).map(|m| m.trim().to_lowercase());
                            let id_norm = v.get("device_id").and_then(|m| m.as_str()).map(|m| m.trim().to_lowercase());
                            if k_norm == norm || mac_norm.as_deref() == Some(&norm) || id_norm.as_deref() == Some(&norm) {
                                return v.get("ip").and_then(|ip| ip.as_str()).map(|s| s.to_string());
                            }
                        }
                    }
                }
            }
            None
        })
        .or_else(|| {
            if device_id.starts_with("net-") {
                let candidate = device_id.replace("net-", "").replace('-', ".");
                if candidate.parse::<Ipv4Addr>().is_ok() {
                    return Some(candidate);
                }
            }
            None
        });

    let ip_str = match found_ip {
        Some(ip) => ip,
        None => {
            return (
                axum::http::StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "reachable": false,
                    "error": "device_ip_not_found",
                    "message": "Device IP address could not be determined"
                })),
            )
                .into_response();
        }
    };

    let ip: Ipv4Addr = match ip_str.parse() {
        Ok(addr) => addr,
        Err(_) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "reachable": false,
                    "error": "invalid_ip",
                    "message": format!("Invalid IP address '{ip_str}'")
                })),
            )
                .into_response();
        }
    };

    let now = chrono::Utc::now().to_rfc3339();
    let mut reachable = false;
    let mut rtt_ms: Option<f64> = None;
    let mut method = String::new();

    // 1. ICMP ping (1 packet, 1 second timeout)
    let ping_cmd = tokio::process::Command::new("ping")
        .args(["-c", "1", "-t", "1", &ip_str])
        .output()
        .await;

    if let Ok(output) = ping_cmd {
        if output.status.success() {
            reachable = true;
            method = "icmp".to_string();
            let stdout = String::from_utf8_lossy(&output.stdout);
            if let Some(pos) = stdout.find("time=") {
                let rest = &stdout[pos + 5..];
                if let Some(end) = rest.find(" ms") {
                    if let Ok(val) = rest[..end].trim().parse::<f64>() {
                        rtt_ms = Some(val);
                    }
                }
            }
        }
    }

    // 2. If ICMP didn't respond, try fast TCP connects on standard ports
    if !reachable {
        let probe_ports = [80, 443, 8080, 22, 62078, 5000, 5353];
        for port in probe_ports {
            let addr = SocketAddr::new(IpAddr::V4(ip), port);
            let start = std::time::Instant::now();
            if tokio::time::timeout(Duration::from_millis(150), TcpStream::connect(addr))
                .await
                .is_ok_and(|r| r.is_ok())
            {
                reachable = true;
                method = format!("tcp:{}", port);
                rtt_ms = Some((start.elapsed().as_micros() as f64) / 1000.0);
                break;
            }
        }
    }

    let message = if reachable {
        if let Some(rtt) = rtt_ms {
            format!("Online (reply in {:.1} ms via {})", rtt, method)
        } else {
            format!("Online (responded via {})", method)
        }
    } else {
        "Device did not respond to ping or TCP probes (100% packet loss)".to_string()
    };

    if reachable {
        // Sync to device_history.json
        let history_path = state.docs_path.parent().unwrap_or(std::path::Path::new(".")).join("device_history.json");
        if let Ok(content) = std::fs::read_to_string(&history_path) {
            if let Ok(mut hist) = serde_json::from_str::<serde_json::Value>(&content) {
                let norm = device_id.trim().to_lowercase();
                if let Some(records) = hist.get_mut("records").and_then(|r| r.as_object_mut()) {
                    let mut updated = false;
                    for (k, v) in records.iter_mut() {
                        let k_norm = k.trim().to_lowercase();
                        let mac_norm = v.get("mac").and_then(|m| m.as_str()).map(|m| m.trim().to_lowercase());
                        let id_norm = v.get("device_id").and_then(|m| m.as_str()).map(|m| m.trim().to_lowercase());
                        let ip_match = v.get("ip").and_then(|i| i.as_str()) == Some(&ip_str);
                        if k_norm == norm || mac_norm.as_deref() == Some(&norm) || id_norm.as_deref() == Some(&norm) || ip_match {
                            if let Some(obj) = v.as_object_mut() {
                                obj.insert("is_active".to_string(), serde_json::Value::Bool(true));
                                obj.insert("last_seen".to_string(), serde_json::Value::String(now.clone()));
                                updated = true;
                            }
                        }
                    }
                    if updated {
                        let _ = persist_json(&history_path, &hist);
                    }
                }
            }
        }

        // Trigger background rescan in network-discovery so supervisor snapshot updates
        if let Ok(mut client) = connect_control_client(&state.socket_path).await {
            let _ = client.send_command(homenode_sdk::proto::ModuleCommand {
                target_module_id: "network-discovery".to_string(),
                action: "scan".to_string(),
                params: std::collections::HashMap::new(),
            }).await;
        }
    }

    Json(serde_json::json!({
        "reachable": reachable,
        "rtt_ms": rtt_ms,
        "method": method,
        "last_seen": now,
        "message": message
    }))
    .into_response()
}

async fn probe_http_banner(ip: Ipv4Addr, port: u16) -> Option<(Option<String>, Option<String>)> {
    let addr = SocketAddr::new(IpAddr::V4(ip), port);
    let mut stream = tokio::time::timeout(Duration::from_millis(250), TcpStream::connect(addr))
        .await
        .ok()?
        .ok()?;

    let req = format!(
        "GET / HTTP/1.1\r\nHost: {ip}\r\nUser-Agent: HomeNodeAnalyzer/1.0\r\nConnection: close\r\n\r\n"
    );
    let _ = stream.write_all(req.as_bytes()).await;

    let mut buf = vec![0u8; 8192];
    let n = tokio::time::timeout(Duration::from_millis(350), stream.read(&mut buf))
        .await
        .ok()?
        .ok()?;

    if n == 0 {
        return None;
    }

    let text = String::from_utf8_lossy(&buf[..n]);
    let server = text
        .lines()
        .find(|l| l.to_lowercase().starts_with("server:"))
        .map(|l| l[7..].trim().to_string());

    let title = {
        let lower = text.to_lowercase();
        if let Some(start) = lower.find("<title>") {
            let after = &text[start + 7..];
            if let Some(end) = after.to_lowercase().find("</title>") {
                Some(after[..end].trim().to_string())
            } else {
                None
            }
        } else {
            None
        }
    };

    Some((server, title))
}

fn port_description(port: u16) -> &'static str {
    match port {
        21 => "FTP File Transfer",
        22 => "SSH Terminal Access",
        23 => "Telnet Console",
        53 => "DNS Server",
        80 => "HTTP Web Interface (HMI)",
        81 => "HTTP Alternate (Admin)",
        443 => "HTTPS Web Interface",
        554 => "RTSP Video Stream",
        1883 => "MQTT Broker / Client",
        1900 => "SSDP / UPnP Discovery",
        5000 => "Synology DSM Web UI / UPnP",
        5001 => "Synology DSM HTTPS",
        5060 => "SIP VoIP Telephony",
        6053 => "ESPHome Native API",
        8080 => "HTTP Alt Web Interface",
        8081 => "HTTP Alt Admin",
        8443 => "HTTPS Alt Web Interface",
        8883 => "Secure MQTT",
        9000 => "Web Admin / Portainer",
        _ => "TCP Service",
    }
}

fn deduce_analyzer_category(
    name: &str,
    hostname: Option<&str>,
    vendor: Option<&str>,
    open_ports: &[u16],
    http_title: Option<&str>,
) -> (String, &'static str, &'static str) {
    let lower_name = name.to_lowercase();
    let lower_host = hostname.unwrap_or_default().to_lowercase();
    let lower_vendor = vendor.unwrap_or_default().to_lowercase();
    let lower_title = http_title.unwrap_or_default().to_lowercase();

    if lower_name.contains("vpn") || lower_host.contains("vpn") || lower_host.contains("wireguard") || lower_name.contains("wireguard") || lower_host.contains("iphonestephan") {
        return ("vpn".to_string(), "VPN & Virtual Devices", "🛡️");
    }
    if lower_name.contains("ecoflow") || lower_host.contains("ecoflow") || lower_vendor.contains("ecoflow") {
        return ("energy".to_string(), "Solar & Energy Systems", "☀️");
    }
    if lower_name.contains("3em") || lower_host.contains("3em") {
        return ("energy".to_string(), "Solar & Energy Systems", "☀️");
    }
    if lower_name.contains("hue") || lower_host.contains("hue") || lower_vendor.contains("philips") {
        return ("hub".to_string(), "Smart Home Hubs", "🎛️");
    }
    if lower_name.contains("netatmo") || lower_host.contains("netatmo") || lower_vendor.contains("netatmo") {
        return ("sensor".to_string(), "Sensors & Detectors", "🌡️");
    }
    if lower_name.contains("synology") || lower_host.contains("synology") || open_ports.contains(&5000) {
        return ("nas".to_string(), "Network Storage & NAS", "🗄️");
    }
    if lower_name.contains("repeater") || (lower_host.contains("repeater") && lower_vendor.contains("avm")) {
        return ("router".to_string(), "Routers & Gateways", "🌐");
    }
    if lower_name.contains("awtrix") || lower_host.contains("awtrix") {
        return ("display".to_string(), "Smart Clocks & Displays", "⏰");
    }
    if lower_name.contains("meshtastic") || lower_host.contains("meshtastic") {
        return ("radio".to_string(), "LoRa & Mesh Radios", "📻");
    }
    if lower_name.contains("presence") || lower_host.contains("presence") {
        return ("sensor".to_string(), "Sensors & Detectors", "👁️");
    }
    if lower_name.contains("fronius") || lower_host.contains("fronius") || lower_vendor.contains("fronius") {
        return ("energy".to_string(), "Solar & Energy Systems", "☀️");
    }
    if lower_name.contains("miele") || lower_host.contains("miele") || lower_vendor.contains("miele") {
        return ("appliance".to_string(), "Home Appliances", "🧺");
    }
    if lower_name.contains("govee") || lower_host.contains("govee") || lower_host.starts_with("led-") {
        return ("lighting".to_string(), "Smart Lighting", "💡");
    }
    if lower_name.contains("switchbot") || lower_host.contains("switchbot") || lower_vendor.contains("woan") {
        return ("hub".to_string(), "Smart Home Hubs", "🎛️");
    }
    if lower_name.contains("plug") || lower_host.contains("plug") || lower_host.contains("outlet") || lower_title.contains("tasmota") {
        return ("smart-plug".to_string(), "Smart Plugs & Sockets", "🔌");
    }
    if open_ports.contains(&80) || open_ports.contains(&8080) {
        return ("iot".to_string(), "Smart Home & IoT", "💡");
    }

    ("network-device".to_string(), "Network & Other Devices", "🔌")
}

fn generate_rhai_script(
    script_id: &str,
    name: &str,
    hostname: &str,
    vendor: &str,
    category: &str,
    category_title: &str,
    category_icon: &str,
    open_ports: &[u16],
    http_title: Option<&str>,
) -> String {
    let mut rules = Vec::new();
    if !hostname.is_empty() {
        let base = hostname.split('.').next().unwrap_or(hostname).to_lowercase();
        rules.push(format!("    let host = obs.hostname.to_lower();\n    host.contains(\"{base}\")"));
    }
    if !vendor.is_empty() {
        rules.push(format!("    let vendor = obs.vendor.to_lower();\n    vendor.contains(\"{}\")", vendor.to_lowercase()));
    }
    if let Some(title) = http_title {
        if !title.is_empty() {
            rules.push(format!("    let name = obs.name.to_lower();\n    name.contains(\"{}\")", title.to_lowercase()));
        }
    }
    if rules.is_empty() {
        rules.push(format!("    let name = obs.name.to_lower();\n    name.contains(\"{}\")", name.to_lowercase()));
    }

    let rules_code = rules.join(" ||\n");

    let mut caps = vec!["\"network\"".to_string()];
    if open_ports.contains(&80) || open_ports.contains(&443) || open_ports.contains(&5000) || open_ports.contains(&8080) {
        caps.push("\"http\"".to_string());
    }
    if open_ports.contains(&1883) || open_ports.contains(&8883) {
        caps.push("\"mqtt\"".to_string());
    }
    if open_ports.contains(&22) {
        caps.push("\"ssh\"".to_string());
    }
    let caps_str = caps.join(", ");

    format!(
r#"// Rhai Device Definition for {name}
// Auto-generated by HomeNode Unknown Device Analyzer

fn meta() {{
    #{{
        id: "{script_id}",
        name: "{name}",
        category: "{category}",
        category_title: "{category_title}",
        category_icon: "{category_icon}",
        capabilities: [{caps_str}]
    }}
}}

fn identify(obs) {{
{rules_code}
}}
"#
    )
}

#[derive(Deserialize)]
struct SaveDefinitionRequest {
    filename: String,
    content: String,
}

async fn save_definition_handler(
    State(state): State<WebState>,
    Json(payload): Json<SaveDefinitionRequest>,
) -> Response {
    let clean_name = payload
        .filename
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '.' || *c == '-')
        .collect::<String>();

    if clean_name.is_empty() || !clean_name.ends_with(".rhai") {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "invalid_filename_must_end_with_rhai"})),
        )
            .into_response();
    }

    let target_path = state.definitions_dir.join(&clean_name);
    let _ = std::fs::create_dir_all(&state.definitions_dir);

    match std::fs::write(&target_path, &payload.content) {
        Ok(_) => {
            info!("Saved new Rhai device definition: {}", target_path.display());
            Json(serde_json::json!({
                "status": "saved",
                "path": target_path.display().to_string()
            }))
            .into_response()
        }
        Err(err) => {
            error!("Failed to write definition {}: {err}", target_path.display());
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": err.to_string()})),
            )
                .into_response()
        }
    }
}

// ------------------------------------------------------------------------------------------------
// Dual-Homed Correlation & Unified Device Model
// ------------------------------------------------------------------------------------------------

#[derive(Clone)]
pub(crate) struct UnifiedDevice {
    pub(crate) primary: DeviceRecord,
    pub(crate) secondary_interfaces: Vec<DeviceRecord>,
    pub(crate) merge_candidate: Option<DeviceRecord>,
}

fn extract_shelly_mac_hex(name: &str) -> Option<String> {
    let name_lower = name.to_lowercase();
    if !name_lower.contains("shelly") {
        return None;
    }
    let cleaned = name.trim();
    if let Some(pos) = cleaned.rfind(|c| c == '-' || c == '_' || c == ' ') {
        let suffix = &cleaned[pos + 1..];
        if suffix.len() == 12 && suffix.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some(suffix.to_uppercase());
        }
    }
    if cleaned.len() >= 12 {
        let suffix = &cleaned[cleaned.len() - 12..];
        if suffix.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some(suffix.to_uppercase());
        }
    }
    None
}

pub(crate) fn auto_link_deterministic_devices(
    devices: &[DeviceRecord],
    links: &mut HashMap<String, Vec<String>>,
) -> bool {
    let mut modified = false;

    let mut already_linked_secondaries = std::collections::HashSet::new();
    for (_, sec_list) in links.iter() {
        for sec in sec_list {
            already_linked_secondaries.insert(sec.clone());
        }
    }

    let mut net_mac_map: HashMap<String, String> = HashMap::new();
    let mut net_shelly_prefix_map: HashMap<String, String> = HashMap::new();

    for dev in devices {
        let is_ble = dev.metadata.get("protocol").map(|s| s.as_str()) == Some("ble")
            || dev.device_id.starts_with("mobile-ble-")
            || dev.module_id == "bthome";
        if is_ble {
            continue;
        }

        if let Some(mac) = dev.metadata.get("mac") {
            let norm_mac = mac.replace([':', '-'], "").to_uppercase();
            if norm_mac.len() == 12 {
                net_mac_map.insert(norm_mac.clone(), dev.device_id.clone());
                if norm_mac.len() >= 10 {
                    net_mac_map.insert(norm_mac[..10].to_string(), dev.device_id.clone());
                }
            }
        }

        let host = dev.metadata.get("hostname").cloned().unwrap_or_default().to_uppercase();
        if host.contains("SHELLY") {
            if let Some(hex_mac) = extract_shelly_mac_hex(&host) {
                net_mac_map.insert(hex_mac.clone(), dev.device_id.clone());
                if hex_mac.len() >= 10 {
                    net_mac_map.insert(hex_mac[..10].to_string(), dev.device_id.clone());
                }
            }
        }

        let name_lower = dev.display_name.to_lowercase();
        let host_lower = host.to_lowercase();
        let is_dev_active = dev.metadata.get("is_active").map(|s| s.as_str()) == Some("true")
            || dev.metadata.get("status").map(|s| s.as_str()) == Some("active");
        for key in &["shellypro3em", "shellypro4pm", "shellyplus1"] {
            if name_lower.contains(key) || host_lower.contains(key) {
                if is_dev_active || !net_shelly_prefix_map.contains_key(*key) {
                    net_shelly_prefix_map.insert(key.to_string(), dev.device_id.clone());
                }
            }
        }
    }

    for dev in devices {
        let is_ble = dev.metadata.get("protocol").map(|s| s.as_str()) == Some("ble")
            || dev.device_id.starts_with("mobile-ble-")
            || dev.module_id == "bthome";
        if !is_ble {
            continue;
        }

        if already_linked_secondaries.contains(&dev.device_id) {
            continue;
        }

        if let Some(hex_mac) = extract_shelly_mac_hex(&dev.display_name) {
            let matched_primary = net_mac_map.get(&hex_mac)
                .or_else(|| if hex_mac.len() >= 10 { net_mac_map.get(&hex_mac[..10]) } else { None })
                .or_else(|| {
                    let name_lower = dev.display_name.to_lowercase();
                    for key in &["shellypro3em", "shellypro4pm", "shellyplus1"] {
                        if name_lower.contains(key) {
                            if let Some(target_id) = net_shelly_prefix_map.get(*key) {
                                return Some(target_id);
                            }
                        }
                    }
                    None
                });

            if let Some(primary_dev_id) = matched_primary {
                if primary_dev_id != &dev.device_id {
                    let list = links.entry(primary_dev_id.clone()).or_default();
                    if !list.contains(&dev.device_id) {
                        info!("Auto-linking Shelly BLE device {} to primary network device {}", dev.display_name, primary_dev_id);
                        list.push(dev.device_id.clone());
                        already_linked_secondaries.insert(dev.device_id.clone());
                        modified = true;
                    }
                }
            }
        }
    }

    modified
}

fn detect_merge_candidate(
    dev: &DeviceRecord,
    all_devices: &[DeviceRecord],
    links: &HashMap<String, Vec<String>>,
) -> Option<DeviceRecord> {
    let name = dev.display_name.to_lowercase();
    let host = dev.metadata.get("hostname").cloned().unwrap_or_default().to_lowercase();
    let ip = dev.metadata.get("ip").cloned().unwrap_or_default();
    let mac = dev.metadata.get("mac").cloned().unwrap_or_default();
    let is_dev_ble = dev.metadata.get("protocol").map(|s| s.as_str()) == Some("ble")
        || dev.device_id.starts_with("mobile-ble-")
        || dev.module_id == "bthome";

    // Skip if already linked as a secondary
    if links.values().any(|v| v.contains(&dev.device_id) || (!mac.is_empty() && v.contains(&mac))) {
        return None;
    }

    let current_secondaries: &[String] = links.get(&dev.device_id)
        .or_else(|| if !mac.is_empty() { links.get(&mac) } else { None })
        .map(|v| v.as_slice())
        .unwrap_or(&[]);

    for other in all_devices {
        if other.device_id == dev.device_id {
            continue;
        }
        let other_mac = other.metadata.get("mac").cloned().unwrap_or_default();

        // Skip if already linked to this dev or linked to another device
        if current_secondaries.contains(&other.device_id) || (!other_mac.is_empty() && current_secondaries.contains(&other_mac)) {
            continue;
        }
        if links.values().any(|v| v.contains(&other.device_id) || (!other_mac.is_empty() && v.contains(&other_mac))) {
            continue;
        }

        let other_name = other.display_name.to_lowercase();
        let other_host = other.metadata.get("hostname").cloned().unwrap_or_default().to_lowercase();
        let other_ip = other.metadata.get("ip").cloned().unwrap_or_default();
        let is_other_ble = other.metadata.get("protocol").map(|s| s.as_str()) == Some("ble")
            || other.device_id.starts_with("mobile-ble-")
            || other.module_id == "bthome";

        if !ip.is_empty() && ip == other_ip {
            continue;
        }

        // Heuristic 1: Shelly 12-Hex MAC match or unique model match
        let shelly_mac_1 = extract_shelly_mac_hex(&dev.display_name);
        let shelly_mac_2 = extract_shelly_mac_hex(&other.display_name);
        if let Some(ref smac) = shelly_mac_1 {
            let other_norm_mac = other_mac.replace([':', '-'], "").to_uppercase();
            if !other_norm_mac.is_empty() && (other_norm_mac == *smac || (other_norm_mac.len() >= 10 && smac.starts_with(&other_norm_mac[..10]))) {
                return Some(other.clone());
            }
        }
        if let Some(ref smac) = shelly_mac_2 {
            let dev_norm_mac = mac.replace([':', '-'], "").to_uppercase();
            if !dev_norm_mac.is_empty() && (dev_norm_mac == *smac || (dev_norm_mac.len() >= 10 && smac.starts_with(&dev_norm_mac[..10]))) {
                return Some(other.clone());
            }
        }

        // Heuristic 2: MacBook / Laptop match (e.g. macbookprom2 vs mbp-m2-2 or mbp-m2)
        let is_mac_1 = name.contains("macbook") || host.contains("macbook") || name.contains("mbp") || host.contains("mbp");
        let is_mac_2 = other_name.contains("macbook") || other_host.contains("macbook") || other_name.contains("mbp") || other_host.contains("mbp");
        if is_mac_1 && is_mac_2 {
            let chip1 = ["m1", "m2", "m3", "m4", "m5"].iter().find(|&&c| name.contains(c) || host.contains(c));
            let chip2 = ["m1", "m2", "m3", "m4", "m5"].iter().find(|&&c| other_name.contains(c) || other_host.contains(c));
            if chip1.is_some() && chip1 == chip2 {
                return Some(other.clone());
            }
            if chip1.is_none() && chip2.is_none() {
                if (name.contains("macbook") && (other_name.contains("mbp") || other_host.contains("mbp")))
                    || (other_name.contains("macbook") && (name.contains("mbp") || host.contains("mbp"))) {
                    return Some(other.clone());
                }
            }
        }

        // Heuristic 3: iPad match (e.g. iPad Pro M5 vs iPadM5)
        let is_ipad_1 = name.contains("ipad") || host.contains("ipad");
        let is_ipad_2 = other_name.contains("ipad") || other_host.contains("ipad");
        if is_ipad_1 && is_ipad_2 {
            let chip1 = ["m1", "m2", "m3", "m4", "m5"].iter().find(|&&c| name.contains(c) || host.contains(c));
            let chip2 = ["m1", "m2", "m3", "m4", "m5"].iter().find(|&&c| other_name.contains(c) || other_host.contains(c));
            if chip1.is_some() && chip1 == chip2 {
                return Some(other.clone());
            }
            if chip1.is_none() && chip2.is_none() {
                let clean1 = name.replace([' ', '-', '_', '.'], "").replace("fritzbox", "");
                let clean2 = other_name.replace([' ', '-', '_', '.'], "").replace("fritzbox", "");
                if !clean1.is_empty() && clean1 == clean2 {
                    return Some(other.clone());
                }
            }
        }

        // Heuristic 4: Suffix match (e.g. host and host-2 or host-wlan)
        let generic_names = ["switch", "host", "pc", "device", "lan", "wlan"];
        let clean_name1 = name.replace("-2", "").replace(".fritz.box", "").replace(".local", "");
        let clean_name2 = other_name.replace("-2", "").replace(".fritz.box", "").replace(".local", "");
        let is_suffix_match = clean_name1 == clean_name2
            && (name.contains("-2") || other_name.contains("-2"))
            && !generic_names.contains(&clean_name1.as_str());
        if is_suffix_match {
            return Some(other.clone());
        }

        // Heuristic 5: General Normalized Match between IP and BLE device
        if (is_dev_ble != is_other_ble) || (!ip.is_empty() && !other_ip.is_empty()) {
            let norm1: String = name.chars().filter(|c| c.is_alphanumeric()).collect();
            let norm2: String = other_name.chars().filter(|c| c.is_alphanumeric()).collect();
            if norm1.len() >= 4 && norm1 == norm2 && !generic_names.contains(&norm1.as_str()) {
                return Some(other.clone());
            }
        }
    }

    None
}

pub(crate) fn build_unified_devices(
    devices: &[DeviceRecord],
    links: &HashMap<String, Vec<String>>,
) -> Vec<UnifiedDevice> {
    // Defense-in-depth deduplication by IP: prioritize records with MAC and active status
    let mut deduped: Vec<DeviceRecord> = Vec::new();
    let mut seen_ips: HashMap<String, usize> = HashMap::new();
    for d in devices {
        let ip = d.metadata.get("ip").cloned().unwrap_or_default();
        if ip.is_empty() {
            deduped.push(d.clone());
            continue;
        }
        if let Some(&existing_idx) = seen_ips.get(&ip) {
            let existing = &deduped[existing_idx];
            let existing_has_mac = existing.metadata.get("mac").is_some();
            let new_has_mac = d.metadata.get("mac").is_some();
            let existing_active = existing.metadata.get("is_active").map(|s| s.as_str()) == Some("true");
            let new_active = d.metadata.get("is_active").map(|s| s.as_str()) == Some("true");

            let prefer_new = (!existing_has_mac && new_has_mac)
                || (!existing_active && new_active)
                || (existing.display_name.starts_with("Host ") && !d.display_name.starts_with("Host "));

            if prefer_new {
                deduped[existing_idx] = d.clone();
            }
        } else {
            seen_ips.insert(ip, deduped.len());
            deduped.push(d.clone());
        }
    }

    let mut dev_map: HashMap<String, DeviceRecord> = HashMap::new();
    let mut mac_map: HashMap<String, String> = HashMap::new();

    for d in &deduped {
        dev_map.insert(d.device_id.clone(), d.clone());
        if let Some(mac) = d.metadata.get("mac") {
            mac_map.insert(mac.clone(), d.device_id.clone());
        }
    }

    // Collect all IDs that are secondary
    let mut secondary_ids = std::collections::HashSet::new();
    for (_, sec_list) in links {
        for sec in sec_list {
            secondary_ids.insert(sec.clone());
            if let Some(mapped) = mac_map.get(sec) {
                secondary_ids.insert(mapped.clone());
            }
        }
    }

    let mut unified = Vec::new();
    for d in &deduped {
        if secondary_ids.contains(&d.device_id) {
            continue;
        }
        let mac = d.metadata.get("mac").cloned().unwrap_or_default();
        if !mac.is_empty() && secondary_ids.contains(&mac) {
            continue;
        }

        let mut secondaries = Vec::new();
        let ip = d.metadata.get("ip").cloned().unwrap_or_default();
        let ip_key = if !ip.is_empty() { format!("net-{}", ip.replace('.', "-")) } else { String::new() };
        let configured_secs = links.get(&d.device_id)
            .or_else(|| if !mac.is_empty() { links.get(&mac) } else { None })
            .or_else(|| if !ip_key.is_empty() { links.get(&ip_key) } else { None });
        if let Some(list) = configured_secs {
            for sec_id in list {
                let actual_id = mac_map.get(sec_id).unwrap_or(sec_id);
                if let Some(sec_dev) = dev_map.get(actual_id) {
                    secondaries.push(sec_dev.clone());
                }
            }
        }

        let candidate = detect_merge_candidate(d, &deduped, links);

        unified.push(UnifiedDevice {
            primary: d.clone(),
            secondary_interfaces: secondaries,
            merge_candidate: candidate,
        });
    }

    unified
}

// ------------------------------------------------------------------------------------------------
// HTML Rendering: Master-Detail Layout
// ------------------------------------------------------------------------------------------------

pub(crate) fn page_layout(title: &str, current_tab: &str, content: &str) -> String {
    let dashboard_active = if current_tab == "dashboard" { "class=\"active\"" } else { "" };
    let devices_active = if current_tab == "devices" { "class=\"active\"" } else { "" };
    let rooms_active = if current_tab == "rooms" { "class=\"active\"" } else { "" };
    let energy_active = if current_tab == "energy" { "class=\"active\"" } else { "" };
    let matter_active = if current_tab == "matter" { "class=\"active\"" } else { "" };
    let catalog_active = if current_tab == "catalog" { "class=\"active\"" } else { "" };
    let status_active = if current_tab == "status" { "class=\"active\"" } else { "" };
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <title>{title}</title>
    <style>
        :root {{
            --bg: #f8fafc;
            --surface: #ffffff;
            --border: #e2e8f0;
            --text: #0f172a;
            --muted: #64748b;
            --primary: #2563eb;
            --primary-bg: #eff6ff;
            --badge-bg: #f1f5f9;
            --badge-text: #334155;
            --status-green: #10b981;
            --status-yellow: #f59e0b;
            --status-red: #ef4444;
            --code-bg: #f1f5f9;
        }}
        @media (prefers-color-scheme: dark) {{
            :root {{
                --bg: #0f172a;
                --surface: #1e293b;
                --border: #334155;
                --text: #f8fafc;
                --muted: #94a3b8;
                --primary: #3b82f6;
                --primary-bg: #1e3a8a33;
                --badge-bg: #334155;
                --badge-text: #e2e8f0;
                --code-bg: #0b132b;
            }}
        }}
        * {{ box-sizing: border-box; margin: 0; padding: 0; }}
        body {{
            font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
            background: var(--bg);
            color: var(--text);
            line-height: 1.5;
            padding: 20px 24px;
        }}
        .container {{ max-width: 1440px; margin: 0 auto; }}
        header {{
            display: flex;
            align-items: center;
            justify-content: space-between;
            margin-bottom: 24px;
            padding-bottom: 16px;
            border-bottom: 1px solid var(--border);
            flex-wrap: wrap;
            gap: 12px;
        }}
        h1 {{ font-size: 20px; font-weight: 700; }}
        nav {{ display: flex; gap: 8px; }}
        nav a {{
            padding: 6px 14px;
            border-radius: 6px;
            text-decoration: none;
            color: var(--muted);
            font-size: 13px;
            font-weight: 500;
            transition: all 0.15s;
        }}
        nav a:hover {{ color: var(--text); background: var(--border); }}
        nav a.active {{ color: var(--primary); background: var(--primary-bg); font-weight: 600; }}
        
        .card {{
            background: var(--surface);
            border: 1px solid var(--border);
            border-radius: 10px;
            padding: 16px 20px;
            margin-bottom: 20px;
            box-shadow: 0 1px 3px rgba(0,0,0,0.04);
        }}
        h2 {{ font-size: 16px; font-weight: 600; margin-bottom: 14px; }}
        h3 {{ font-size: 15px; font-weight: 600; }}
        
        /* Master-Detail Split Layout */
        .workspace-grid {{
            display: grid;
            grid-template-columns: 420px 1fr;
            gap: 20px;
            align-items: start;
        }}
        @media (max-width: 980px) {{
            .workspace-grid {{ grid-template-columns: 1fr; }}
        }}
        
        .inspector-panel {{
            position: sticky;
            top: 20px;
            max-height: calc(100vh - 40px);
            overflow-y: auto;
            background: var(--surface);
            border: 1px solid var(--border);
            border-radius: 10px;
            padding: 20px;
            box-shadow: 0 2px 8px rgba(0,0,0,0.04);
        }}
        
        .inspector-empty {{
            text-align: center;
            padding: 60px 20px;
            color: var(--muted);
        }}
        
        .device-item {{
            cursor: pointer;
            transition: all 0.15s ease;
            border-left: 3px solid transparent;
        }}
        .device-item:hover {{
            background-color: var(--badge-bg);
        }}
        .device-item.active-device {{
            background-color: var(--primary-bg);
            border-left: 3px solid var(--primary);
        }}
        
        table {{
            width: 100%;
            border-collapse: collapse;
            font-size: 13px;
            text-align: left;
        }}
        th, td {{
            padding: 8px 10px;
            border-bottom: 1px solid var(--border);
            vertical-align: middle;
        }}
        th {{
            font-size: 11px;
            font-weight: 600;
            color: var(--muted);
            text-transform: uppercase;
        }}
        tr:last-child td {{ border-bottom: none; }}
        
        .badge {{
            display: inline-block;
            padding: 2px 7px;
            font-size: 11px;
            border-radius: 9999px;
            background: var(--badge-bg);
            color: var(--badge-text);
            white-space: nowrap;
        }}
        .badge-kind {{
            background: #e0e7ff;
            color: #3730a3;
        }}
        .badge-dual {{
            background: #e0f2fe;
            color: #0369a1;
            font-weight: 600;
            border: 1px solid #bae6fd;
        }}
        @media (prefers-color-scheme: dark) {{
            .badge-kind {{ background: #312e81; color: #c7d2fe; }}
            .badge-dual {{ background: #0c4a6e; color: #bae6fd; border-color: #0284c7; }}
        }}
        .status-dot {{
            display: inline-block;
            width: 8px;
            height: 8px;
            border-radius: 50%;
            margin-right: 6px;
        }}
        .status-ready {{ background-color: var(--status-green); }}
        .status-starting {{ background-color: var(--status-yellow); }}
        .status-error {{ background-color: var(--status-red); }}
        
        .toolbar {{
            display: flex;
            align-items: center;
            justify-content: space-between;
            flex-wrap: wrap;
            gap: 12px;
            margin-bottom: 14px;
        }}
        .toolbar h2 {{ font-size: 16px; font-weight: 600; }}
        .btn {{
            display: inline-flex;
            align-items: center;
            gap: 6px;
            padding: 7px 14px;
            border-radius: 6px;
            border: 1px solid var(--border);
            background: var(--surface);
            color: var(--text);
            font-size: 13px;
            cursor: pointer;
            transition: all 0.15s;
        }}
        .btn:hover {{ background: var(--badge-bg); }}
        .btn-primary {{
            background: var(--primary);
            color: #fff;
            border-color: var(--primary);
        }}
        .btn-primary:hover {{ opacity: 0.9; background: var(--primary); }}
        .btn-sm {{
            padding: 3px 8px;
            font-size: 11px;
            border-radius: 4px;
            cursor: pointer;
        }}
        .btn-web {{
            background: #dbeafe;
            color: #1d4ed8;
            border: 1px solid #bfdbfe;
            font-weight: 600;
            text-decoration: none;
            display: inline-flex;
            align-items: center;
            gap: 4px;
        }}
        .btn-web:hover {{
            background: #bfdbfe;
        }}
        @media (prefers-color-scheme: dark) {{
            .btn-web {{
                background: #1e3a8a;
                color: #bfdbfe;
                border-color: #2563eb;
            }}
            .btn-web:hover {{
                background: #1d4ed8;
            }}
        }}
        .btn-ping {{
            background: #f8fafc;
            color: #475569;
            border: 1px solid #cbd5e1;
            font-weight: 600;
            display: inline-flex;
            align-items: center;
            gap: 4px;
            transition: all 0.15s;
        }}
        .btn-ping:hover {{
            background: #e2e8f0;
            color: #0f172a;
        }}
        @media (prefers-color-scheme: dark) {{
            .btn-ping {{
                background: #1e293b;
                color: #cbd5e1;
                border-color: #475569;
            }}
            .btn-ping:hover {{
                background: #334155;
            }}
        }}
        
        .pills {{
            display: flex;
            gap: 8px;
            flex-wrap: wrap;
            margin-bottom: 16px;
        }}
        .pill {{
            padding: 5px 12px;
            border-radius: 9999px;
            border: 1px solid var(--border);
            background: var(--surface);
            font-size: 12px;
            cursor: pointer;
            color: var(--muted);
            transition: all 0.15s;
        }}
        .pill:hover {{ color: var(--text); border-color: var(--text); }}
        .pill.active {{
            background: var(--primary);
            border-color: var(--primary);
            color: #fff;
            font-weight: 600;
        }}
        
        .status-pills {{
            display: flex;
            gap: 6px;
            align-items: center;
            margin-bottom: 14px;
            flex-wrap: wrap;
        }}
        .status-pill {{
            padding: 4px 11px;
            border-radius: 6px;
            border: 1px solid var(--border);
            background: var(--surface);
            font-size: 12px;
            cursor: pointer;
            color: var(--muted);
            transition: all 0.15s;
        }}
        .status-pill:hover {{ color: var(--text); border-color: var(--muted); }}
        .status-pill.active {{
            background: var(--primary-bg);
            border-color: var(--primary);
            color: var(--primary);
            font-weight: 600;
        }}
        
        .search-input {{
            padding: 6px 12px;
            border-radius: 6px;
            border: 1px solid var(--border);
            background: var(--surface);
            color: var(--text);
            font-size: 13px;
            min-width: 240px;
        }}
        .search-input:focus {{
            outline: 2px solid var(--primary);
            border-color: transparent;
        }}
        
        .group-header {{
            display: flex;
            align-items: center;
            justify-content: space-between;
            margin-bottom: 10px;
            padding-bottom: 6px;
            border-bottom: 2px solid var(--border);
        }}
        .group-header h3 {{
            font-size: 14px;
            font-weight: 600;
            display: flex;
            align-items: center;
            gap: 6px;
        }}
        
        .empty-state {{
            text-align: center;
            padding: 40px 20px;
            color: var(--muted);
        }}
        .empty-state p {{ font-size: 13px; margin-top: 6px; }}
        
        .inspector-title {{
            font-size: 12px;
            font-weight: 700;
            text-transform: uppercase;
            letter-spacing: 0.5px;
            color: var(--muted);
            margin-bottom: 12px;
            display: flex;
            align-items: center;
            justify-content: space-between;
        }}
        .inspector-sec {{
            margin-top: 20px;
            padding-top: 16px;
            border-top: 1px solid var(--border);
        }}
        .form-control {{
            width: 100%;
            padding: 6px 10px;
            border-radius: 6px;
            border: 1px solid var(--border);
            background: var(--bg);
            color: var(--text);
            font-size: 12px;
            font-family: inherit;
        }}
        textarea.form-control {{
            min-height: 80px;
            resize: vertical;
        }}
        .iface-card {{
            background: var(--bg);
            border: 1px solid var(--border);
            border-radius: 6px;
            padding: 8px 10px;
            margin-bottom: 8px;
            font-size: 12px;
        }}
        .merge-box {{
            background: var(--primary-bg);
            border: 1px dashed var(--primary);
            border-radius: 6px;
            padding: 10px;
            margin-top: 10px;
            font-size: 12px;
        }}
        .code-box {{
            background: var(--code-bg);
            border: 1px solid var(--border);
            border-radius: 6px;
            padding: 8px;
            font-family: monospace;
            font-size: 11px;
            overflow-x: auto;
            white-space: pre;
            max-height: 220px;
        }}
    </style>
</head>
<body>
<div class="container">
    <header>
        <h1>{title}</h1>
        <nav>
            <a href="/" {dashboard_active}>🏠 Dashboard</a>
            <a href="/devices" {devices_active}>📱 Devices</a>
            <a href="/rooms" {rooms_active}>🚪 Rooms</a>
            <a href="/energy" {energy_active}>⚡ Energy</a>
            <a href="/matter" {matter_active}>✨ Matter Fabrics</a>
            <a href="/catalog" {catalog_active}>🏢 Hardware Catalog</a>
            <a href="/status" {status_active}>⚙️ System Status</a>
        </nav>
    </header>
    {content}
</div>
</body>
</html>"#
    )
}

fn default_category_presentation(key: &str) -> (&'static str, &'static str) {
    match key {
        "button" => ("Buttons & Remote Controls", "🔘"),
        "sensor" => ("Sensors & Detectors", "👁️"),
        "contact-sensor" => ("Doors & Windows", "🚪"),
        "motion-sensor" => ("Motion Detectors", "🚶"),
        "phone" => ("Smartphones", "📱"),
        "tablet" => ("Tablets", "📟"),
        "voip-phone" => ("VoIP Phones", "☎️"),
        "computer" => ("Computers & Laptops", "💻"),
        "nas" => ("Network Storage & NAS", "🗄️"),
        "router" => ("Routers & Gateways", "🌐"),
        "lighting" => ("Smart Lighting", "💡"),
        "smart-plug" => ("Smart Plugs & Sockets", "🔌"),
        "display" => ("Smart Clocks & Displays", "⏰"),
        "energy" => ("Solar & Energy Systems", "☀️"),
        "appliance" => ("Home Appliances", "🧺"),
        "radio" => ("LoRa & Mesh Radios", "📻"),
        "hub" => ("Smart Home Hubs", "🎛️"),
        "iot" => ("Smart Home & IoT", "💡"),
        "wearable" => ("Wearables", "⌚"),
        "camera" => ("Cameras", "📷"),
        "audio" => ("Audio & Speakers", "🔊"),
        "printer" => ("Printers", "🖨️"),
        "3d-printer" => ("3D Printers & Makers", "🧊"),
        "streaming" => ("TV & Streaming", "📺"),
        "vpn" => ("VPN & Virtual Devices", "🛡️"),
        _ => ("Network & Other Devices", "🔌"),
    }
}

fn canonical_category_key(key: &str, title: &str) -> &'static str {
    match title {
        "Buttons & Remote Controls" | "Buttons" | "Taster & Schalter" | "Remote Controls & Switches" | "Wall Switch" | "Wandschalter" => "button",
        "Doors & Windows" | "Door & Window" | "Tür & Fenster" => "contact-sensor",
        "Motion Detectors" | "Motion" | "Bewegungsmelder" => "motion-sensor",
        "Smartphones" => "phone",
        "Tablets" => "tablet",
        "VoIP Phones" => "voip-phone",
        "Computers & Laptops" => "computer",
        "Network Storage & NAS" => "nas",
        "Routers & Gateways" => "router",
        "Network Switches" | "Switches" | "Netzwerk-Switches" => "switch",
        "Smart Lighting" => "lighting",
        "Smart Plugs & Sockets" => "smart-plug",
        "Smart Clocks & Displays" => "display",
        "Sensors & Detectors" => "sensor",
        "Solar & Energy Systems" => "energy",
        "Home Appliances" => "appliance",
        "LoRa & Mesh Radios" => "radio",
        "Smart Home Hubs" => "hub",
        "Smart Home & IoT" => "iot",
        "Wearables" => "wearable",
        "Cameras" => "camera",
        "Audio & Speakers" => "audio",
        "Printers" => "printer",
        "3D Printers & Makers" | "3D-Drucker" | "3D Drucker" => "3d-printer",
        "TV & Streaming" => "streaming",
        "VPN & Virtual Devices" => "vpn",
        _ => match key {
            "button" | "buttons" | "remote" => "button",
            "contact-sensor" | "door" | "window" => "contact-sensor",
            "motion-sensor" | "motion" => "motion-sensor",
            "phone" | "smartphone" | "smartphones" => "phone",
            "tablet" | "tablets" | "ipad" => "tablet",
            "voip-phone" | "voip" => "voip-phone",
            "computer" | "computers" | "laptop" | "pc" => "computer",
            "nas" => "nas",
            "router" | "gateway" => "router",
            "switch" | "switches" => "switch",
            "lighting" | "light" => "lighting",
            "smart-plug" | "plug" => "smart-plug",
            "display" => "display",
            "sensor" => "sensor",
            "energy" => "energy",
            "appliance" => "appliance",
            "radio" => "radio",
            "hub" => "hub",
            "iot" => "iot",
            "wearable" => "wearable",
            "camera" => "camera",
            "audio" => "audio",
            "printer" => "printer",
            "3d-printer" | "3d_printer" | "3dprinter" => "3d-printer",
            "streaming" => "streaming",
            "vpn" => "vpn",
            _ => "network-device",
        },
    }
}

fn category_sort_order(key: &str) -> u32 {
    match key {
        "button" => 1,
        "sensor" => 2,
        "contact-sensor" => 3,
        "motion-sensor" => 4,
        "phone" => 5,
        "tablet" => 6,
        "voip-phone" => 7,
        "computer" => 8,
        "nas" => 9,
        "router" => 10,
        "switch" => 11,
        "lighting" => 12,
        "smart-plug" => 13,
        "display" => 14,
        "energy" => 15,
        "appliance" => 16,
        "radio" => 17,
        "hub" => 18,
        "iot" => 19,
        "wearable" => 20,
        "camera" => 18,
        "audio" => 19,
        "printer" => 20,
        "3d-printer" => 21,
        "streaming" => 22,
        "vpn" => 23,
        "network-device" => 99,
        _ => 50,
    }
}

#[derive(Clone)]
struct DynamicCategory {
    key: String,
    title: String,
    icon: String,
    devices: Vec<UnifiedDevice>,
}

fn extract_device_matter_fabrics(
    dev: &DeviceRecord,
    secondaries: &[DeviceRecord],
    fabric_metas: &HashMap<String, MatterFabricMeta>,
) -> Vec<ClientMatterFabric> {
    let mut fabrics = Vec::new();
    let mut seen = std::collections::HashSet::new();

    let mut raw_strings = Vec::new();
    if let Some(s) = dev.metadata.get("matter_fabrics") {
        raw_strings.push(s.as_str());
    }
    for sec in secondaries {
        if let Some(s) = sec.metadata.get("matter_fabrics") {
            raw_strings.push(s.as_str());
        }
    }

    for raw in raw_strings {
        if let Ok(parsed) = serde_json::from_str::<Vec<serde_json::Value>>(raw) {
            for item in parsed {
                let fabric_id = item.get("fabric_id").and_then(|v| v.as_str()).unwrap_or_default().to_uppercase();
                let node_id = item.get("node_id").and_then(|v| v.as_str()).unwrap_or_default().to_uppercase();
                let port = item.get("port").and_then(|v| v.as_u64()).unwrap_or(5540) as u16;
                let interface = item.get("interface").and_then(|v| v.as_str()).unwrap_or("wifi").to_string();

                if !fabric_id.is_empty() && seen.insert((fabric_id.clone(), node_id.clone())) {
                    let meta = fabric_metas.get(&fabric_id);
                    let fabric_name = meta.map(|m| m.name.clone()).unwrap_or_else(|| {
                        format!("Fabric {}", &fabric_id[..fabric_id.len().min(8)])
                    });
                    let fabric_icon = meta.map(|m| m.icon.clone()).unwrap_or_else(|| "✨".to_string());

                    fabrics.push(ClientMatterFabric {
                        fabric_id,
                        node_id,
                        port,
                        interface,
                        fabric_name,
                        fabric_icon,
                    });
                }
            }
        }
    }

    fabrics
}

fn render_devices_page(
    title: &str,
    snapshot: &RuntimeSnapshot,
    docs: &HashMap<String, DeviceDocumentation>,
    links: &HashMap<String, Vec<String>>,
    catalog: &homenode_definitions::CatalogDatabase,
    category_overrides: &HashMap<String, String>,
    matter_fabric_metas: &HashMap<String, MatterFabricMeta>,
    verified_gateways: &[VerifiedShellyGateway],
    ignored_store: &homenode_definitions::IgnoredDevicesStore,
    rooms: &[RoomRecord],
    govee_states: &HashMap<String, govee::GoveeDeviceState>,
) -> String {
    let verified_gw_ips: std::collections::HashSet<String> = verified_gateways.iter()
        .filter(|g| g.ble_supported)
        .map(|g| g.ip.clone())
        .collect();
    let verified_gw_macs: std::collections::HashSet<String> = verified_gateways.iter()
        .filter(|g| g.ble_supported)
        .map(|g| g.mac.to_lowercase().replace([':', '-'], ""))
        .collect();

    if snapshot.devices.is_empty() {
        let content = r#"
        <div class="toolbar">
            <h2>Detected Devices (0)</h2>
            <form action="/scan" method="POST" id="scan-form" style="margin:0;">
                <button type="submit" id="scan-btn" class="btn btn-primary">
                    <span>🔄</span> <span id="scan-label">Scan Network Now</span>
                </button>
            </form>
        </div>
        <div class="card"><div class="empty-state"><h3>No devices detected yet</h3><p>Click "Scan Network Now" or wait for background discovery.</p></div></div>"#;
        return page_layout(title, "devices", content);
    }

    let unified_devices = build_unified_devices(&snapshot.devices, links);
    let ignored_count = unified_devices
        .iter()
        .filter(|u| {
            let p = &u.primary;
            let mac = p.metadata.get("mac").map(|s| s.as_str()).unwrap_or("");
            ignored_store.is_ignored(&p.device_id) || (!mac.is_empty() && ignored_store.is_ignored(mac))
        })
        .count();

    let active_count = unified_devices
        .iter()
        .filter(|u| {
            let p = &u.primary;
            let mac = p.metadata.get("mac").map(|s| s.as_str()).unwrap_or("");
            if ignored_store.is_ignored(&p.device_id) || (!mac.is_empty() && ignored_store.is_ignored(mac)) {
                return false;
            }
            let s = p.metadata.get("status").map(|s| s.as_str()).unwrap_or("active");
            s == "active"
        })
        .count();
    let former_count = unified_devices
        .iter()
        .filter(|u| {
            let p = &u.primary;
            let mac = p.metadata.get("mac").map(|s| s.as_str()).unwrap_or("");
            if ignored_store.is_ignored(&p.device_id) || (!mac.is_empty() && ignored_store.is_ignored(mac)) {
                return false;
            }
            let s = p.metadata.get("status").map(|s| s.as_str()).unwrap_or("active");
            s == "inactive"
        })
        .count();
    let archive_count = unified_devices
        .iter()
        .filter(|u| {
            let p = &u.primary;
            let mac = p.metadata.get("mac").map(|s| s.as_str()).unwrap_or("");
            if ignored_store.is_ignored(&p.device_id) || (!mac.is_empty() && ignored_store.is_ignored(mac)) {
                return false;
            }
            let s = p.metadata.get("status").map(|s| s.as_str()).unwrap_or("active");
            s == "archive"
        })
        .count();
    let total_count = active_count + former_count + archive_count;

    let mut category_map: HashMap<String, DynamicCategory> = HashMap::new();
    for udev in &unified_devices {
        let dev = &udev.primary;
        let mac = dev.metadata.get("mac").cloned().unwrap_or_default().trim().to_lowercase();
        let doc_key = if !mac.is_empty() { mac.clone() } else { dev.device_id.clone() };

        let (cat_key, title, icon) = if let Some(over_cat) = category_overrides.get(&doc_key).or_else(|| category_overrides.get(&dev.device_id)) {
            let (f_title, f_icon) = default_category_presentation(over_cat);
            (over_cat.clone(), f_title.to_string(), f_icon.to_string())
        } else {
            let cat = dev
                .metadata
                .get("category")
                .cloned()
                .unwrap_or_else(|| dev.kind.clone());
            let (fallback_title, fallback_icon) = default_category_presentation(&cat);
            let t = dev
                .metadata
                .get("category_title")
                .cloned()
                .unwrap_or_else(|| fallback_title.to_string());
            let i = dev
                .metadata
                .get("category_icon")
                .cloned()
                .unwrap_or_else(|| fallback_icon.to_string());
            (cat, t, i)
        };

        let canon_key = canonical_category_key(&cat_key, &title);
        let entry = category_map
            .entry(canon_key.to_string())
            .or_insert_with(|| DynamicCategory {
                key: canon_key.to_string(),
                title,
                icon,
                devices: Vec::new(),
            });

        entry.devices.push(udev.clone());
    }

    let mut categories: Vec<_> = category_map.into_values().collect();
    categories.sort_by_key(|c| (category_sort_order(&c.key), c.title.clone()));

    // Filter pills
    let mut pills_html = format!(
        r#"<button type="button" class="pill active" onclick="selectCategory('all', this)">All ({})</button>"#,
        unified_devices.len()
    );

    for cat in &categories {
        pills_html.push_str(&format!(
            r#"<button type="button" class="pill" onclick="selectCategory('{}', this)">{} {} ({})</button>"#,
            cat.key, cat.icon, cat.title, cat.devices.len()
        ));
    }

    // Devices JSON payload for live client-side Inspector
    let client_devices: Vec<serde_json::Value> = unified_devices
        .iter()
        .map(|udev| {
            let p = &udev.primary;
            let ip = p.metadata.get("ip").cloned().unwrap_or_default();
            let mac = p.metadata.get("mac").cloned().unwrap_or_default();
            let vendor = p.metadata.get("vendor").cloned().unwrap_or_default();
            let hostname = p.metadata.get("hostname").cloned().unwrap_or_default();
            let web_url = p.metadata.get("web_url").cloned();
            let doc_key = if !mac.is_empty() { mac.clone() } else { p.device_id.clone() };

            let (category, category_title, category_icon) = if let Some(over_cat) = category_overrides.get(&doc_key).or_else(|| category_overrides.get(&p.device_id)) {
                let (f_title, f_icon) = default_category_presentation(over_cat);
                (over_cat.clone(), f_title.to_string(), f_icon.to_string())
            } else {
                let cat = p.metadata.get("category").cloned().unwrap_or_else(|| p.kind.clone());
                let (fallback_title, fallback_icon) = default_category_presentation(&cat);
                let t = p.metadata.get("category_title").cloned().unwrap_or_else(|| fallback_title.to_string());
                let i = p.metadata.get("category_icon").cloned().unwrap_or_else(|| fallback_icon.to_string());
                let canon = canonical_category_key(&cat, &t);
                (canon.to_string(), t, i)
            };

            let mac_lower = mac.to_lowercase();
            let doc = docs.get(&doc_key)
                .or_else(|| if !mac.is_empty() { docs.get(&mac).or_else(|| docs.get(&mac_lower)) } else { None })
                .or_else(|| docs.get(&p.device_id))
                .cloned()
                .unwrap_or_default();

            let effective_display_name = if let Some(ref custom) = doc.name.as_deref().filter(|n| !n.trim().is_empty()) {
                custom.to_string()
            } else {
                p.display_name.clone()
            };

            // Match product (checking manual/doc assignment first, then fresh catalog rules, then discovery metadata)
            let prod_match = doc.product_id.as_deref().and_then(|id| catalog.find_product(id))
                .or_else(|| {
                    catalog.match_product(
                        p.metadata.get("hostname").map(|s| s.as_str()).unwrap_or(&p.display_name),
                        p.metadata.get("vendor").map(|s| s.as_str()),
                        &[],
                    )
                })
                .or_else(|| p.metadata.get("product_id").and_then(|id| catalog.find_product(id)));

            let product_json = prod_match.map(|prod| {
                let v = catalog.find_vendor(&prod.vendor_id);
                serde_json::json!({
                    "id": prod.id,
                    "name": prod.name,
                    "model_number": prod.model_number,
                    "category": prod.category,
                    "category_icon": prod.category_icon,
                    "connectivity": prod.connectivity,
                    "matter_device_type": prod.matter_device_type,
                    "default_ports": prod.default_ports,
                    "documentation_url": prod.documentation_url,
                    "specs": prod.specs,
                    "rhai_script_ref": prod.rhai_script_ref,
                    "vendor_id": prod.vendor_id,
                    "vendor_name": v.map(|ven| ven.name.clone()).unwrap_or_else(|| prod.vendor_id.clone()),
                    "vendor_website": v.and_then(|ven| ven.website.clone()),
                    "vendor_icon": v.map(|ven| ven.icon.clone()).unwrap_or_else(|| "🏢".to_string()),
                })
            });

            let secondaries_json: Vec<serde_json::Value> = udev.secondary_interfaces.iter().map(|s| {
                serde_json::json!({
                    "device_id": s.device_id,
                    "name": s.display_name,
                    "ip": s.metadata.get("ip").cloned().unwrap_or_default(),
                    "mac": s.metadata.get("mac").cloned().unwrap_or_default(),
                    "vendor": s.metadata.get("vendor").cloned().unwrap_or_default(),
                    "hostname": s.metadata.get("hostname").cloned().unwrap_or_default(),
                    "protocol": s.metadata.get("protocol").cloned().unwrap_or_default(),
                    "source": s.metadata.get("source").cloned().unwrap_or_default(),
                    "scout": s.metadata.get("scout").cloned().unwrap_or_default(),
                    "rssi": s.metadata.get("rssi").cloned().unwrap_or_default(),
                    "metadata": s.metadata.clone(),
                })
            }).collect();

            let candidate_json = udev.merge_candidate.as_ref().map(|c| {
                serde_json::json!({
                    "device_id": c.device_id,
                    "name": c.display_name,
                    "ip": c.metadata.get("ip").cloned().unwrap_or_default(),
                    "mac": c.metadata.get("mac").cloned().unwrap_or_default(),
                    "hostname": c.metadata.get("hostname").cloned().unwrap_or_default(),
                    "protocol": c.metadata.get("protocol").cloned().unwrap_or_default(),
                    "source": c.metadata.get("source").cloned().unwrap_or_default(),
                })
            });

            let dev_matter_fabrics = extract_device_matter_fabrics(p, &udev.secondary_interfaces, matter_fabric_metas);
            let status = p.metadata.get("status").cloned().unwrap_or_else(|| "active".to_string());
            let is_active = status == "active";
            let first_seen = p.metadata.get("first_seen").cloned().unwrap_or_default();
            let last_seen = p.metadata.get("last_seen").cloned().unwrap_or_default();
            let sources_str = p.metadata.get("sources").cloned().or_else(|| p.metadata.get("source").cloned()).unwrap_or_default();
            let norm_mac_p = mac.to_lowercase().replace([':', '-'], "");
            let is_verified_ble_gw = verified_gw_ips.contains(&ip)
                || (!norm_mac_p.is_empty() && verified_gw_macs.contains(&norm_mac_p));

            let mut sources: Vec<String> = sources_str.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
            if is_verified_ble_gw && !sources.contains(&"shelly-gateway".to_string()) {
                sources.push("shelly-gateway".to_string());
            }
            let was_ever_active = p.metadata.get("was_ever_active").map(|s| s.as_str()) == Some("true") || is_active;
            let is_dev_ignored = ignored_store.is_ignored(&p.device_id) || (!mac.is_empty() && ignored_store.is_ignored(&mac));
            let effective_room = doc.room.clone().or_else(|| p.metadata.get("room").cloned()).unwrap_or_default();
            let room_rec = if !effective_room.is_empty() {
                rooms.iter().find(|r| r.name.eq_ignore_ascii_case(&effective_room))
            } else {
                None
            };
            let effective_floor = room_rec.and_then(|r| r.floor.clone()).unwrap_or_else(|| deduce_floor_from_name(&effective_room).map(|s| s.to_string()).unwrap_or_default());

            serde_json::json!({
                "device_id": p.device_id,
                "display_name": effective_display_name,
                "original_name": p.display_name,
                "custom_name": doc.name.unwrap_or_default(),
                "category": category,
                "category_title": category_title,
                "category_icon": category_icon,
                "ip": ip,
                "mac": mac,
                "vendor": vendor,
                "hostname": hostname,
                "web_url": web_url,
                "doc_key": doc_key,
                "notes": doc.notes,
                "room": effective_room,
                "floor": effective_floor,
                "manual_url": doc.manual_url.unwrap_or_default(),
                "updated_at": doc.updated_at,
                "secondaries": secondaries_json,
                "candidate": candidate_json,
                "product": product_json,
                "matter_fabrics": dev_matter_fabrics,
                "status": status,
                "is_active": is_active,
                "is_ignored": is_dev_ignored,
                "was_ever_active": was_ever_active,
                "sources": sources,
                "first_seen": first_seen,
                "last_seen": last_seen,
                "metadata": p.metadata.clone(),
            })
        })
        .collect();

    let client_devices_json = serde_json::to_string(&client_devices).unwrap_or_else(|_| "[]".to_string());

    let all_products_json: Vec<serde_json::Value> = catalog.products.iter().map(|prod| {
        let v = catalog.find_vendor(&prod.vendor_id);
        serde_json::json!({
            "id": prod.id,
            "vendor_id": prod.vendor_id,
            "vendor_name": v.map(|ven| ven.name.clone()).unwrap_or_else(|| prod.vendor_id.clone()),
            "name": prod.name,
            "category": prod.category,
        })
    }).collect();
    let all_products_str = serde_json::to_string(&all_products_json).unwrap_or_else(|_| "[]".to_string());

    // Right side device list groups
    let mut group_cards_html = String::new();
    for cat in &categories {
        let rows = cat
            .devices
            .iter()
            .map(|udev| {
                let device = &udev.primary;
                let ip = device.metadata.get("ip").cloned().unwrap_or_else(|| "-".to_string());
                let mac = device.metadata.get("mac").cloned().unwrap_or_default().trim().to_lowercase();
                let vendor = device.metadata.get("vendor").cloned().unwrap_or_default();
                let web_url = device.metadata.get("web_url").cloned();

                let doc_key = if !mac.is_empty() { mac.clone() } else { device.device_id.clone() };
                let mac_lower = mac.to_lowercase();
                let doc = docs.get(&doc_key)
                    .or_else(|| if !mac.is_empty() { docs.get(&mac).or_else(|| docs.get(&mac_lower)) } else { None })
                    .or_else(|| docs.get(&device.device_id))
                    .cloned()
                    .unwrap_or_default();

                let effective_display_name = if let Some(ref custom) = doc.name.as_deref().filter(|n| !n.trim().is_empty()) {
                    custom.to_string()
                } else {
                    device.display_name.clone()
                };

                let has_docs = !doc.notes.trim().is_empty() || doc.manual_url.is_some() || doc.name.is_some();

                let dev_fabrics = extract_device_matter_fabrics(device, &udev.secondary_interfaces, matter_fabric_metas);
                let mut matter_badges = String::new();
                for fab in &dev_fabrics {
                    matter_badges.push_str(&format!(
                        r#" <span class="badge" style="background:#059669; color:#fff; font-size:10px; padding:2px 6px; border-radius:4px; margin-left:3px;" title="Matter Fabric: {} (Node ID: {})">✨ {} {}</span>"#,
                        fab.fabric_id, fab.node_id, fab.fabric_icon, fab.fabric_name
                    ));
                }

                let mut iface_badges = String::new();
                if !udev.secondary_interfaces.is_empty() {
                    let mut has_lan = false;
                    let mut has_wlan = false;
                    let mut has_ble = false;

                    let check_dev = |d: &DeviceRecord, is_secondary: bool, lan: &mut bool, wlan: &mut bool, ble: &mut bool| {
                        let proto = d.metadata.get("protocol").map(|s| s.as_str()).unwrap_or("");
                        let src = d.metadata.get("source").map(|s| s.as_str()).unwrap_or("");
                        let iface = d.metadata.get("interface").map(|s| s.as_str()).unwrap_or("");
                        let name_lower = d.display_name.to_lowercase();
                        let host_lower = d.metadata.get("hostname").map(|h| h.to_lowercase()).unwrap_or_default();

                        let is_ble_dev = proto == "ble"
                            || src == "mobile-ble"
                            || src == "mobile-scout"
                            || src == "bthome"
                            || d.device_id.starts_with("mobile-ble-");

                        if is_ble_dev {
                            *ble = true;
                        } else if iface == "wlan" || iface == "wifi"
                            || name_lower.contains("wlan") || host_lower.contains("wlan")
                            || name_lower.contains("wifi") || host_lower.contains("wifi")
                            || name_lower.contains("-2") || host_lower.contains("-2")
                            || d.kind == "smart-plug" || d.kind == "tablet" || d.kind == "phone" || d.kind == "wearable"
                            || cat.key == "smart-plug" || cat.key == "tablet" || cat.key == "phone" || cat.key == "wearable"
                            || name_lower.contains("plug")
                            || name_lower.contains("plus1")
                            || name_lower.contains("ipad")
                            || name_lower.contains("iphone")
                            || host_lower.contains("ipad")
                            || host_lower.contains("iphone")
                            || is_secondary
                        {
                            *wlan = true;
                        } else {
                            *lan = true;
                        }
                    };

                    check_dev(device, false, &mut has_lan, &mut has_wlan, &mut has_ble);
                    for sec in &udev.secondary_interfaces {
                        check_dev(sec, true, &mut has_lan, &mut has_wlan, &mut has_ble);
                    }

                    let mut parts = Vec::new();
                    if has_lan { parts.push("LAN"); }
                    if has_wlan { parts.push("WLAN"); }
                    if has_ble { parts.push("BLE"); }
                    if parts.is_empty() { parts.push("Multi"); }

                    let badge_label = parts.join(" + ");
                    let total_count = udev.secondary_interfaces.len() + 1;

                    iface_badges.push_str(&format!(
                        r#" <span class="badge badge-dual" title="Multi-interface device ({total_count} interfaces)">{badge_label} ({total_count})</span>"#
                    ));
                }

                let is_zigbee = device.metadata.get("protocol").map(|s| s.as_str()) == Some("zigbee")
                    || device.module_id == "philips-hue"
                    || device.metadata.get("source").map(|s| s.as_str()) == Some("philips-hue");
                let is_ble = device.metadata.get("protocol").map(|s| s.as_str()) == Some("ble")
                    || device.module_id == "bthome"
                    || device.metadata.get("source").map(|s| s.as_str()) == Some("bthome")
                    || device.metadata.get("source").map(|s| s.as_str()) == Some("mobile-ble");
                let ip_display = if ip.is_empty() || ip == "Layer 2" || ip == "-" {
                    if is_zigbee {
                        let gw = device.metadata.get("bridge_ip").or_else(|| device.metadata.get("gateway")).map(|s| s.as_str()).unwrap_or("Hue Bridge");
                        format!("Zigbee (via {gw})")
                    } else if is_ble {
                        "Bluetooth LE".to_string()
                    } else if cat.key == "switch" {
                        "Layer 2 (Unmanaged)".to_string()
                    } else {
                        "Non-IP Device".to_string()
                    }
                } else {
                    ip.clone()
                };

                let network_info = if mac.is_empty() {
                    format!("<code>{ip_display}</code>{iface_badges}")
                } else if vendor.is_empty() {
                    format!("<code>{ip_display}</code>{iface_badges}<br><small style=\"color:var(--muted)\">{mac}</small>")
                } else {
                    format!("<code>{ip_display}</code>{iface_badges}<br><small style=\"color:var(--muted)\">{mac} &bull; {vendor}</small>")
                };

                let web_button = if let Some(url) = web_url {
                    format!(r#"<a href="{url}" target="_blank" class="btn-sm btn-web" onclick="event.stopPropagation()" title="Open Web Interface">🌐 Web UI</a>"#)
                } else {
                    String::new()
                };

                let raw_status = device.metadata.get("status").map(|s| s.as_str()).unwrap_or("active");
                let is_active = raw_status == "active";
                let is_archive = raw_status == "archive";
                let status_val = raw_status;
                let last_seen = device.metadata.get("last_seen").cloned().unwrap_or_default();

                let status_dot = if is_active {
                    r#"<span class="status-dot" style="background:#10b981; display:inline-block; width:8px; height:8px; border-radius:50%; margin-right:6px;" title="Online / Active"></span>"#
                } else if is_archive {
                    r#"<span class="status-dot" style="background:#cbd5e1; display:inline-block; width:8px; height:8px; border-radius:50%; margin-right:6px;" title="Router Archive (FRITZ!Box)"></span>"#
                } else {
                    r#"<span class="status-dot" style="background:#94a3b8; display:inline-block; width:8px; height:8px; border-radius:50%; margin-right:6px;" title="Offline / Former (HomeNode)"></span>"#
                };

                let offline_badge = if is_archive {
                    let ls_hint = if !last_seen.is_empty() {
                        format!(r#" title="In FRITZ!Box hinterlegt: {}""#, last_seen)
                    } else {
                        r#" title="Reines FRITZ!Box Router-Archiv""#.to_string()
                    };
                    format!(r#" <span class="badge" style="background:#f8fafc; color:#94a3b8; font-size:10px; border:1px dashed #cbd5e1; padding:1px 5px;"{}>📦 Archiv</span>"#, ls_hint)
                } else if !is_active {
                    let ls_hint = if !last_seen.is_empty() {
                        format!(r#" title="Zuletzt online: {}""#, last_seen)
                    } else {
                        String::new()
                    };
                    format!(r#" <span class="badge" style="background:#f1f5f9; color:#64748b; font-size:10px; border:1px solid #cbd5e1; padding:1px 5px;"{}>⚪ Offline</span>"#, ls_hint)
                } else {
                    String::new()
                };

                let mut dev_sources_str = device.metadata.get("sources")
                    .cloned()
                    .or_else(|| device.metadata.get("source").cloned())
                    .unwrap_or_default();

                let dev_ip = device.metadata.get("ip").map(|s| s.as_str()).unwrap_or("");
                let dev_mac = device.metadata.get("mac").map(|s| s.as_str()).unwrap_or("");
                let norm_dev_mac = dev_mac.to_lowercase().replace([':', '-'], "");
                let is_verified_ble_gw = verified_gw_ips.contains(dev_ip)
                    || (!norm_dev_mac.is_empty() && verified_gw_macs.contains(&norm_dev_mac));

                if is_verified_ble_gw && !dev_sources_str.contains("shelly-gateway") {
                    if !dev_sources_str.is_empty() {
                        dev_sources_str.push_str(",shelly-gateway");
                    } else {
                        dev_sources_str = "shelly-gateway".to_string();
                    }
                }

                let mut scanner_badges = String::new();
                for s in dev_sources_str.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
                    let (badge_text, bg_color) = match s {
                        "arp" => ("ARP", "#6366f1"),
                        "ping" | "active-probe" => ("Ping", "#0284c7"),
                        "http-probe" => ("Web", "#0ea5e9"),
                        "mdns" => ("mDNS", "#8b5cf6"),
                        "ssdp" => ("SSDP", "#d97706"),
                        "fritzbox-tr064" => ("TR-064", "#059669"),
                        "matter" | "matter-mdns" => ("Matter", "#10b981"),
                        "home-assistant" | "mdns_homeassistant" => ("HA", "#0284c7"),
                        "bthome" | "bthome-v2" => ("BTHome", "#3b82f6"),
                        "shelly-gateway" => ("Shelly BLE", "#0284c7"),
                        "philips-hue" | "hue" => ("Hue", "#eab308"),
                        "zigbee" => ("Zigbee", "#eab308"),
                        "ble" => ("BLE", "#6366f1"),
                        _ => (s, "#64748b"),
                    };
                    scanner_badges.push_str(&format!(
                        r#" <span class="badge" style="background:{bg_color}; color:#fff; font-size:9px; padding:1px 4px; border-radius:3px; opacity:0.85; margin-left:3px;" title="Interface: {s}">{badge_text}</span>"#
                    ));
                }

                let doc_icon = if has_docs { r#" <span style="color:var(--status-green); font-size:11px;" title="Documentation / Name saved">📝✓</span>"# } else { "" };

                let has_valid_ip = !ip.is_empty() && ip != "Layer 2" && ip != "-" && ip.parse::<std::net::Ipv4Addr>().is_ok();
                let light_toggle_btn = if dev_sources_str.contains("philips-hue") && device.kind == "lighting" {
                    let light_id = device.metadata.get("light_id").cloned().unwrap_or_else(|| device.device_id.replace("hue-light-", ""));
                    let is_on = device.metadata.get("power_state").map(|s| s == "on").unwrap_or(false);
                    let btn_label = if is_on { "💡 An" } else { "🔌 Aus" };
                    let btn_bg = if is_on { "rgba(234,179,8,0.2)" } else { "var(--surface)" };
                    let btn_border = if is_on { "#eab308" } else { "var(--border)" };
                    format!(
                        r#"<button type="button" class="btn-sm" style="background:{btn_bg}; border:1px solid {btn_border}; font-weight:600; font-size:11px; padding:2px 8px; border-radius:4px; cursor:pointer;" onclick="event.stopPropagation(); toggleHueLight('{light_id}')" title="Licht umschalten">{btn_label}</button>"#
                    )
                } else {
                    String::new()
                };

                let action_buttons = if !is_active {
                    let ping_btn = if has_valid_ip {
                        format!(
                            r#"<button type="button" class="btn-sm btn-ping" data-ping-id="{}" onclick="event.stopPropagation(); pingDevice('{}', this)" title="Ping device to check reachability">📡 Ping</button>"#,
                            device.device_id, device.device_id
                        )
                    } else if is_zigbee {
                        r#"<span class="badge" style="background:#fef3c7; color:#92400e; font-size:10px; padding:2px 6px; border:1px solid #fde68a;" title="Zigbee Gerät nicht erreichbar">Offline</span>"#.to_string()
                    } else if is_ble {
                        r#"<span class="badge" style="background:#e0f2fe; color:#0369a1; font-size:10px; padding:2px 6px; border:1px solid #bae6fd;" title="Bluetooth LE Gerät nicht in Reichweite">Offline</span>"#.to_string()
                    } else if cat.key == "switch" {
                        r#"<span class="badge" style="background:#f1f5f9; color:#94a3b8; font-size:11px; padding:3px 6px; border:1px solid #e2e8f0;" title="Reines Layer-2-Gerät ohne IP-Adresse">L2 Switch</span>"#.to_string()
                    } else {
                        r#"<span class="badge" style="background:#f1f5f9; color:#94a3b8; font-size:11px; padding:3px 6px; border:1px solid #e2e8f0;" title="Gerät ohne eigene IP-Adresse">No IP</span>"#.to_string()
                    };
                    if !light_toggle_btn.is_empty() {
                        format!(r#"<div style="display:inline-flex; gap:6px; justify-content:flex-end; align-items:center;">{light_toggle_btn}{ping_btn}</div>"#)
                    } else if !web_button.is_empty() {
                        format!(r#"<div style="display:inline-flex; gap:6px; justify-content:flex-end; align-items:center;">{ping_btn}{web_button}</div>"#)
                    } else {
                        ping_btn
                    }
                } else if !light_toggle_btn.is_empty() {
                    light_toggle_btn
                } else {
                    web_button
                };

                let is_dev_ignored = ignored_store.is_ignored(&device.device_id)
                    || (!mac.is_empty() && ignored_store.is_ignored(&mac));
                let ignored_badge = if is_dev_ignored {
                    r#" <span class="badge" style="background:#fee2e2; color:#b91c1c; font-size:10px; border:1px solid #fca5a5; padding:1px 5px;" title="Nachbargerät / Ignoriert">🚫 Ignoriert</span>"#
                } else {
                    ""
                };

                let dev_room = doc.room.as_deref().or_else(|| device.metadata.get("room").map(|s| s.as_str())).unwrap_or("").trim();
                let room_rec = if !dev_room.is_empty() {
                    rooms.iter().find(|r| r.name.eq_ignore_ascii_case(dev_room))
                } else {
                    None
                };
                let dev_floor = room_rec.and_then(|r| r.floor.as_deref()).unwrap_or_else(|| deduce_floor_from_name(dev_room).unwrap_or(""));
                let room_badge = if !dev_room.is_empty() {
                    let icon = room_rec.and_then(|r| r.icon.as_deref()).unwrap_or("🚪");
                    format!(r#" <span class="badge" style="background:#e0e7ff; color:#3730a3; font-size:10px; padding:1px 5px; border-radius:3px; margin-left:3px;" title="Raum: {dev_room}">{} {}</span>"#, icon, dev_room)
                } else {
                    String::new()
                };

                format!(
                    r#"<tr class="device-item" data-id="{}" data-status="{}" data-active="{}" data-ignored="{}" data-sources="{}" data-room="{}" data-floor="{}" onclick="selectDevice('{}')">
                        <td>{}<strong class="dev-display-name">{}</strong>{}{}{}{}{}<br><small style="color:var(--muted)">{} &bull; {}</small>{}</td>
                        <td>{}</td>
                        <td style="text-align:right;">{}</td>
                    </tr>"#,
                    device.device_id,
                    status_val,
                    is_active,
                    is_dev_ignored,
                    dev_sources_str,
                    dev_room,
                    dev_floor,
                    device.device_id,
                    status_dot,
                    effective_display_name,
                    offline_badge,
                    ignored_badge,
                    room_badge,
                    doc_icon,
                    matter_badges,
                    device.device_id,
                    device.module_id,
                    scanner_badges,
                    network_info,
                    action_buttons,
                )
            })
            .collect::<Vec<_>>()
            .join("");

        group_cards_html.push_str(&format!(
            r#"<div class="card device-group" data-category="{}">
                <div class="group-header">
                    <h3>{} {} <span class="badge">{}</span></h3>
                </div>
                <div style="overflow-x:auto">
                    <table>
                        <thead>
                            <tr>
                                <th>Device</th>
                                <th>Network (IP / MAC)</th>
                                <th style="text-align:right;">Quick Action</th>
                            </tr>
                        </thead>
                        <tbody>{}</tbody>
                    </table>
                </div>
            </div>"#,
            cat.key,
            cat.icon,
            cat.title,
            cat.devices.len(),
            rows
        ));
    }

    let client_rooms_json = serde_json::to_string(rooms).unwrap_or_else(|_| "[]".to_string());
    let client_govee_json = serde_json::to_string(govee_states).unwrap_or_else(|_| "{}".to_string());
    let mut room_filter_options = String::from(r#"<option value="all">Alle Räume</option><option value="unassigned">-- Ohne Raum --</option>"#);
    let mut floor_groups: std::collections::BTreeMap<String, Vec<&RoomRecord>> = std::collections::BTreeMap::new();
    for r in rooms {
        let f = r.floor.clone().unwrap_or_else(|| "Sonstige".to_string());
        floor_groups.entry(f).or_default().push(r);
    }
    for (floor, f_rooms) in &floor_groups {
        room_filter_options.push_str(&format!(r#"<optgroup label="{}">"#, floor));
        for r in f_rooms {
            let icon = r.icon.as_deref().unwrap_or("📍");
            room_filter_options.push_str(&format!(r#"<option value="{}">{} {}</option>"#, r.name, icon, r.name));
        }
        room_filter_options.push_str("</optgroup>");
    }

    let script = format!(
        r#"
    <script>
    const allDevices = {};
    const allProducts = {};
    let allRooms = {};
    const allGoveeStates = {};
    let currentCategory = 'all';
    let currentStatusFilter = 'all';
    let currentScannerFilter = 'all';
    let currentRoomFilter = 'all';
    let currentFloorFilter = 'all';
    let selectedDeviceId = null;

    function selectCategory(cat, el) {{
        currentCategory = cat;
        document.querySelectorAll('.pill').forEach(p => p.classList.remove('active'));
        el.classList.add('active');
        filterDevices();
    }}

    function setStatusFilter(status, el) {{
        currentStatusFilter = status;
        document.querySelectorAll('.status-pill').forEach(p => p.classList.remove('active'));
        el.classList.add('active');
        filterDevices();
    }}

    function setScannerFilter(sc) {{
        currentScannerFilter = sc.toLowerCase();
        filterDevices();
    }}

    function setRoomFilter(r) {{
        currentRoomFilter = (r || 'all').toLowerCase();
        filterDevices();
    }}

    function setFloorFilter(f) {{
        currentFloorFilter = (f || 'all').toLowerCase();
        filterDevices();
    }}

    function formatRelativeTime(iso) {{
        if (!iso) return 'Unknown';
        const date = new Date(iso);
        if (isNaN(date.getTime())) return iso;
        const now = new Date();
        const diffSec = Math.floor((now - date) / 1000);
        if (diffSec < 0 || diffSec < 60) return 'Just now';
        const diffMin = Math.floor(diffSec / 60);
        if (diffMin < 60) return diffMin + 'm ago';
        const diffHours = Math.floor(diffMin / 60);
        if (diffHours < 24) return diffHours + 'h ago';
        const diffDays = Math.floor(diffHours / 24);
        if (diffDays < 7) return diffDays + 'd ago';
        return date.toLocaleDateString(undefined, {{ month: 'short', day: 'numeric', year: 'numeric' }});
    }}

    function formatFullDate(iso) {{
        if (!iso) return 'Not recorded';
        const date = new Date(iso);
        if (isNaN(date.getTime())) return iso;
        return date.toLocaleString(undefined, {{
            month: 'short',
            day: 'numeric',
            year: 'numeric',
            hour: '2-digit',
            minute: '2-digit'
        }});
    }}

    function filterDevices() {{
        const q = (document.getElementById('device-search').value || '').toLowerCase();
        const groups = document.querySelectorAll('.device-group');

        groups.forEach(group => {{
            const cat = group.getAttribute('data-category');
            const matchesCat = (currentCategory === 'all' || currentCategory === cat);

            let visibleRows = 0;
            const rows = group.querySelectorAll('tbody tr');
            rows.forEach(row => {{
                const text = row.innerText.toLowerCase();
                const matchesSearch = !q || text.includes(q);
                const status = row.getAttribute('data-status') || 'active';
                const isIgnored = row.getAttribute('data-ignored') === 'true';

                let matchesStatus = false;
                if (currentStatusFilter === 'ignored') {{
                    matchesStatus = isIgnored;
                }} else {{
                    matchesStatus = !isIgnored && (currentStatusFilter === 'all' || currentStatusFilter === status);
                }}

                const rowSources = (row.getAttribute('data-sources') || '').toLowerCase();
                const matchesScanner = (currentScannerFilter === 'all'
                    || rowSources.includes(currentScannerFilter)
                    || (currentScannerFilter === 'ping' && rowSources.includes('active-probe'))
                    || (currentScannerFilter === 'matter' && rowSources.includes('matter-mdns'))
                    || (currentScannerFilter === 'bthome' && (rowSources.includes('bthome') || rowSources.includes('shelly-gateway')))
                    || (currentScannerFilter === 'shelly-gateway' && rowSources.includes('shelly-gateway'))
                    || (currentScannerFilter === 'philips-hue' && (rowSources.includes('philips-hue') || rowSources.includes('hue')))
                    || (currentScannerFilter === 'mobile-scout' && rowSources.includes('mobile-scout')));

                const rowRoom = (row.getAttribute('data-room') || '').toLowerCase();
                const rowFloor = (row.getAttribute('data-floor') || '').toLowerCase();

                let matchesRoom = false;
                if (currentRoomFilter === 'all') {{
                    matchesRoom = true;
                }} else if (currentRoomFilter === 'unassigned') {{
                    matchesRoom = (rowRoom === '');
                }} else {{
                    matchesRoom = (rowRoom === currentRoomFilter);
                }}

                let matchesFloor = (currentFloorFilter === 'all' || rowFloor === currentFloorFilter);

                if (matchesSearch && matchesStatus && matchesScanner && matchesRoom && matchesFloor) {{
                    row.style.display = '';
                    visibleRows++;
                }} else {{
                    row.style.display = 'none';
                }}
            }});

            if (matchesCat && visibleRows > 0) {{
                group.style.display = '';
            }} else {{
                group.style.display = 'none';
            }}
        }});
    }}

    function selectDevice(deviceId) {{
        selectedDeviceId = deviceId;
        window.location.hash = deviceId;

        document.querySelectorAll('.device-item').forEach(el => {{
            if (el.getAttribute('data-id') === deviceId) {{
                el.classList.add('active-device');
            }} else {{
                el.classList.remove('active-device');
            }}
        }});

        const dev = allDevices.find(d => d.device_id === deviceId);
        if (!dev) return;

        renderInspector(dev);
    }}

    function renderInspector(dev) {{
        const panel = document.getElementById('inspector-content');

        let webBtn = '';
        if (dev.web_url) {{
            webBtn = `<div style="margin-top:10px;"><a href="${{dev.web_url}}" target="_blank" class="btn btn-primary btn-sm" style="font-size:12px; width:100%; justify-content:center;">🌐 Open Web Interface</a></div>`;
        }}

        // Category options generator
        const standardCategories = [
            {{ key: 'button', label: '🔘 Buttons & Remote Controls' }},
            {{ key: 'contact-sensor', label: '🚪 Doors & Windows' }},
            {{ key: 'sensor', label: '👁️ Sensors & Detectors' }},
            {{ key: 'phone', label: '📱 Smartphones' }},
            {{ key: 'tablet', label: '📟 Tablets' }},
            {{ key: 'computer', label: '💻 Computers & Laptops' }},
            {{ key: 'lighting', label: '💡 Smart Lighting' }},
            {{ key: 'smart-plug', label: '🔌 Smart Plugs & Sockets' }},
            {{ key: 'display', label: '⏰ Smart Clocks & Displays' }},
            {{ key: 'sensor', label: '👁️ Sensors & Detectors' }},
            {{ key: 'energy', label: '☀️ Solar & Energy Systems' }},
            {{ key: 'appliance', label: '🧺 Home Appliances' }},
            {{ key: 'radio', label: '📻 LoRa & Mesh Radios' }},
            {{ key: 'hub', label: '🎛️ Smart Home Hubs' }},
            {{ key: 'nas', label: '🗄️ Network Storage & NAS' }},
            {{ key: 'router', label: '🌐 Routers & Gateways' }},
            {{ key: 'voip-phone', label: '☎️ VoIP Phones' }},
            {{ key: 'camera', label: '📷 Cameras' }},
            {{ key: 'audio', label: '🔊 Audio & Speakers' }},
            {{ key: 'streaming', label: '📺 TV & Streaming' }},
            {{ key: 'printer', label: '🖨️ Printers' }},
            {{ key: '3d-printer', label: '🧊 3D Printers & Makers' }},
            {{ key: 'vpn', label: '🛡️ VPN & Virtual Devices' }},
            {{ key: 'iot', label: '💡 Smart Home & IoT' }},
            {{ key: 'network-device', label: '🔌 Network & Other Devices' }}
        ];

        let catOptions = '';
        let foundCat = false;
        standardCategories.forEach(c => {{
            const sel = (dev.category === c.key) ? 'selected' : '';
            if (dev.category === c.key) foundCat = true;
            catOptions += `<option value="${{c.key}}" ${{sel}}>${{c.label}}</option>`;
        }});
        if (!foundCat && dev.category) {{
            catOptions += `<option value="${{dev.category}}" selected>Current: ${{dev.category_title || dev.category}}</option>`;
        }}

        let categorySelectorBox = `
            <div style="background:var(--bg); border:1px solid var(--border); border-radius:6px; padding:6px 10px; margin-top:8px;">
                <label style="font-size:11px; font-weight:600; color:var(--muted); display:block; margin-bottom:4px;">Change Device Category</label>
                <div style="display:flex; gap:6px; align-items:center;">
                    <select id="insp-change-cat" class="form-control" style="font-size:11px; padding:4px 6px; flex:1;">
                        ${{catOptions}}
                    </select>
                    <button type="button" class="btn btn-sm btn-primary" style="font-size:11px; padding:4px 10px;" onclick="updateDeviceCategory('${{dev.doc_key}}', '${{dev.device_id}}')">Update</button>
                </div>
                <span id="cat-status" style="font-size:11px; margin-top:2px; display:block;"></span>
            </div>
        `;

        // Hardware Product Profile (compact, omitted for routers to prevent clutter)
        let productCard = '';
        if (dev.product && dev.product.category !== 'router' && dev.category !== 'router') {{
            const p = dev.product;
            let matterBadge = '';
            if (p.matter_device_type) {{
                matterBadge = `<span class="badge" style="background:#059669; color:#fff; font-size:10px; margin-top:2px;">✨ Matter: ${{escapeHtml(p.matter_device_type)}}</span> `;
            }}
            let vendorLink = escapeHtml(p.vendor_name);
            if (p.vendor_website) {{
                vendorLink = `<a href="${{p.vendor_website}}" target="_blank" style="color:var(--primary); text-decoration:none; font-weight:600;">${{p.vendor_icon || '🏢'}} ${{escapeHtml(p.vendor_name)}} ↗</a>`;
            }}
            let docBtn = '';
            if (p.documentation_url) {{
                docBtn = `<div style="margin-top:4px;"><a href="${{p.documentation_url}}" target="_blank" class="btn btn-sm" style="background:var(--badge-bg); color:var(--text); font-size:11px; text-decoration:none;">📖 Product Manual ↗</a></div>`;
            }}
            let modelText = p.model_number ? `<span style="font-size:11px; color:var(--muted);">&bull; Model: <code>${{escapeHtml(p.model_number)}}</code></span>` : '';

            productCard = `
                <div class="inspector-sec" style="background:var(--primary-bg); border:1px solid rgba(37,99,235,0.2); border-radius:8px; padding:10px; margin-top:10px;">
                    <div style="font-size:13px; font-weight:700;">🏷️ ${{escapeHtml(p.name)}} ${{modelText}}</div>
                    <div style="font-size:11px; margin-top:2px;">${{vendorLink}}</div>
                    ${{matterBadge}}
                    ${{docBtn}}
                </div>
            `;
        }}

        // Matter Operational Fabrics (Fabric IDs, Node IDs, Ports, and Custom Friendly Labels)
        let matterFabricsCard = '';
        if (dev.matter_fabrics && dev.matter_fabrics.length > 0) {{
            let fabricsListHtml = dev.matter_fabrics.map(f => `
                <div style="background:var(--bg); border:1px solid var(--border); border-radius:6px; padding:8px 10px; margin-top:6px;">
                    <div style="display:flex; justify-content:space-between; align-items:center;">
                        <div style="font-weight:600; font-size:13px;">
                            <span>${{f.fabric_icon}}</span> ${{escapeHtml(f.fabric_name)}}
                        </div>
                        <button type="button" class="btn-sm" style="font-size:10px; padding:2px 6px; cursor:pointer;" onclick="renameMatterFabric('${{f.fabric_id}}', '${{escapeAttr(f.fabric_name)}}', '${{escapeAttr(f.fabric_icon)}}')">✏️ Rename</button>
                    </div>
                    <div style="margin-top:6px; font-size:11px; color:var(--muted); display:grid; grid-template-columns: 80px 1fr; gap: 3px 6px;">
                        <span style="font-weight:600;">Fabric ID:</span> <code>${{f.fabric_id}}</code>
                        <span style="font-weight:600;">Node ID:</span> <code>${{f.node_id}}</code>
                        <span style="font-weight:600;">Port:</span> <code>${{f.port}}</code>
                        <span style="font-weight:600;">Interface:</span> <code>${{f.interface}}</code>
                    </div>
                </div>
            `).join('');

            matterFabricsCard = `
                <div class="inspector-sec" style="background:linear-gradient(135deg, rgba(5,150,105,0.06) 0%, rgba(16,185,129,0.06) 100%); border:1px solid rgba(5,150,105,0.25); border-radius:8px; padding:10px; margin-top:10px;">
                    <div class="inspector-title" style="color:#059669; font-weight:700; margin-bottom:2px; display:flex; justify-content:space-between; align-items:center;">
                        <span>✨ Matter Operational Fabrics (${{dev.matter_fabrics.length}})</span>
                    </div>
                    <div style="font-size:11px; color:var(--muted); margin-bottom:6px;">
                        Active operational fabrics discovered via mDNS (<code>_matter._tcp</code>).
                    </div>
                    ${{fabricsListHtml}}
                </div>
            `;
        }}

        // Product assignment dropdown
        let productOptions = '<option value="">-- Associate Product / Model --</option>';
        if (typeof allProducts !== 'undefined') {{
            allProducts.forEach(prod => {{
                const sel = (dev.product && dev.product.id === prod.id) ? 'selected' : '';
                productOptions += `<option value="${{prod.id}}" ${{sel}}>${{prod.vendor_name || prod.vendor_id}}: ${{prod.name}}</option>`;
            }});
        }}
        let assignBox = `
            <div style="margin-top:8px; display:flex; gap:6px;">
                <select id="insp-assign-prod" class="form-control" style="font-size:11px; padding:4px 6px;">
                    ${{productOptions}}
                </select>
                <button type="button" class="btn btn-sm" style="font-size:11px; padding:4px 8px;" onclick="assignProduct('${{dev.device_id}}', '${{dev.doc_key}}')">Assign</button>
            </div>
        `;

        // Secondary / Linked Interfaces
        let isZigbee = dev.protocol === 'zigbee' || (dev.sources && dev.sources.includes('philips-hue')) || (dev.metadata && (dev.metadata.protocol === 'zigbee' || dev.metadata.source === 'philips-hue'));
        let isBle = (dev.sources && (dev.sources.includes('bthome') || dev.sources.includes('mobile-ble'))) || dev.protocol === 'ble';
        let bridgeIp = (dev.metadata && (dev.metadata.bridge_ip || dev.metadata.gateway)) || '';

        function formatDevIp(rawIp) {{
            if (rawIp && rawIp !== 'Layer 2' && rawIp !== '-' && rawIp.trim() !== '' && !rawIp.includes('(')) {{
                return rawIp;
            }}
            if (isZigbee) {{
                return bridgeIp ? `Zigbee (via ${{bridgeIp}})` : 'Zigbee Mesh';
            }}
            if (isBle) {{
                return 'Bluetooth LE (BLE)';
            }}
            if (dev.category === 'switch') {{
                return 'Layer 2 (Unmanaged)';
            }}
            return 'Non-IP Device';
        }}

        let isPrimaryBle = isBle || (dev.sources && dev.sources.includes('ble')) || dev.device_id.startsWith('mobile-ble-');
        let devNameLow = dev.display_name.toLowerCase();
        let isWlan = devNameLow.includes('wlan') || devNameLow.includes('wifi') || dev.display_name.endsWith('-2')
            || dev.category === 'tablet' || dev.category === 'phone' || dev.category === 'smart-plug' || dev.category === 'wearable'
            || devNameLow.includes('ipad') || devNameLow.includes('iphone') || devNameLow.includes('plug') || devNameLow.includes('plus1');
        let primaryTypeLabel = isPrimaryBle ? 'Bluetooth LE' : (isWlan ? 'WLAN' : 'LAN/Main');
        let primaryTypeIcon = isPrimaryBle ? '🔵' : (primaryTypeLabel === 'WLAN' ? '📶' : '🔌');
        let primaryIpDisplay = formatDevIp(dev.ip);
        let ifacesHtml = `
            <div class="iface-card">
                <strong>${{primaryTypeIcon}} Primary Interface (${{primaryTypeLabel}})</strong><br>
                <code>${{primaryIpDisplay}}</code> ${{dev.mac ? '&bull; <small>' + dev.mac + '</small>' : ''}}<br>
                <small style="color:var(--muted)">${{dev.vendor || 'Unknown Vendor'}} ${{dev.hostname ? '&bull; ' + dev.hostname : ''}}</small>
            </div>
        `;

        if (dev.secondaries && dev.secondaries.length > 0) {{
            dev.secondaries.forEach(sec => {{
                let isSecBle = (sec.protocol === 'ble') || (sec.source === 'mobile-scout') || (sec.source === 'mobile-ble') || sec.device_id.startsWith('mobile-ble-') || sec.ip === 'Bluetooth LE' || !sec.ip || sec.ip === '-';
                let secIpDisplay = isSecBle ? 'Bluetooth LE' : formatDevIp(sec.ip);
                let secTitle = isSecBle ? 'Linked Interface (Bluetooth LE)' : 'Linked Interface (WLAN/Secondary)';
                let secIcon = isSecBle ? '🔵' : '📶';
                let secMeta = sec.metadata || {{}};
                let extraBadges = '';
                if (sec.rssi || secMeta.rssi) extraBadges += ` &bull; <small>📶 ${{sec.rssi || secMeta.rssi}} dBm</small>`;
                if (sec.scout || secMeta.scout) extraBadges += ` &bull; <small>📱 ${{escapeHtml(sec.scout || secMeta.scout)}}</small>`;
                if (secMeta.battery) extraBadges += ` &bull; <small>🔋 ${{secMeta.battery}}%</small>`;

                ifacesHtml += `
                    <div class="iface-card">
                        <div style="display:flex; justify-content:space-between; align-items:center;">
                            <strong>${{secIcon}} ${{secTitle}}</strong>
                            <button class="btn-sm" style="color:var(--status-red); border:none; padding:2px; cursor:pointer;" onclick="unlinkInterface('${{dev.device_id}}', '${{sec.device_id}}')">Unlink</button>
                        </div>
                        <code>${{secIpDisplay}}</code> ${{sec.mac ? '&bull; <small>' + sec.mac + '</small>' : ''}}${{extraBadges}}<br>
                        <small style="color:var(--muted)">${{escapeHtml(sec.name || sec.vendor || 'Secondary Device')}} ${{sec.hostname ? '&bull; ' + escapeHtml(sec.hostname) : ''}}</small>
                    </div>
                `;
            }});
        }}

        // Candidate merge box
        let candidateBox = '';
        if (dev.candidate) {{
            let isCandBle = (dev.candidate.protocol === 'ble') || (dev.candidate.source === 'mobile-scout') || (dev.candidate.source === 'mobile-ble') || dev.candidate.device_id.startsWith('mobile-ble-') || !dev.candidate.ip || dev.candidate.ip === '-';
            let isCurrentBle = isPrimaryBle;
            let mergeTypeLabel = (isCandBle || isCurrentBle) ? 'WLAN + BLE' : 'LAN + WLAN';
            let primaryArg = (isCurrentBle && !isCandBle) ? dev.candidate.device_id : dev.device_id;
            let linkedArg = (isCurrentBle && !isCandBle) ? dev.device_id : dev.candidate.device_id;
            let candIpDisplay = (dev.candidate.ip && dev.candidate.ip !== '-') ? dev.candidate.ip : 'Bluetooth LE';

            candidateBox = `
                <div class="merge-box">
                    <strong>💡 Merge Candidate (${{mergeTypeLabel}}):</strong><br>
                    <span>${{escapeHtml(dev.candidate.name)}} (<code>${{escapeHtml(candIpDisplay)}}</code>)</span><br>
                    <button class="btn btn-sm btn-primary" style="margin-top:6px;" onclick="linkInterface('${{escapeAttr(primaryArg)}}', '${{escapeAttr(linkedArg)}}')">
                        🔗 Merge Interfaces (${{mergeTypeLabel}})
                    </button>
                </div>
            `;
        }}

        // Manual interface link dropdown
        let currentSecondaryIds = (dev.secondaries || []).map(s => s.device_id);
        let linkableOptions = allDevices
            .filter(other => other.device_id !== dev.device_id && !currentSecondaryIds.includes(other.device_id))
            .map(other => {{
                let otherIsBle = (other.sources && other.sources.includes('ble')) || other.device_id.startsWith('mobile-ble-');
                let otherIp = (other.ip && other.ip !== '-') ? other.ip : (otherIsBle ? 'Bluetooth LE' : 'No IP');
                return `<option value="${{escapeAttr(other.device_id)}}">${{escapeHtml(other.display_name)}} (${{escapeHtml(otherIp)}})</option>`;
            }}).join('');

        let manualLinkHtml = `
            <div style="margin-top:10px; padding-top:10px; border-top:1px dashed var(--border);">
                <label style="font-size:11px; font-weight:600; color:var(--muted); display:block; margin-bottom:4px;">
                    🔗 Weiteres Interface manuell verknüpfen:
                </label>
                <div style="display:flex; gap:6px;">
                    <select id="insp-manual-link-select" class="form-control" style="font-size:11px; flex:1; cursor:pointer;">
                        <option value="">-- Gerät als Interface auswählen --</option>
                        ${{linkableOptions}}
                    </select>
                    <button type="button" class="btn btn-sm" style="background:var(--badge-bg); color:var(--text); border:1px solid var(--border); font-size:11px; cursor:pointer;" onclick="linkManualInterface('${{escapeAttr(dev.device_id)}}')">
                        Verknüpfen
                    </button>
                </div>
            </div>
        `;

        let statusBadge = '';
        if (dev.is_active) {{
            statusBadge = `
                <div style="display:flex; align-items:center; justify-content:space-between; margin-top:8px; padding:6px 10px; background:#ecfdf5; border:1px solid #a7f3d0; border-radius:6px;">
                    <div style="display:flex; align-items:center; gap:6px;">
                        <span style="display:inline-block; width:8px; height:8px; border-radius:50%; background:#10b981;"></span>
                        <span style="font-weight:600; font-size:12px; color:#065f46;">Active (Online)</span>
                    </div>
                    <span style="font-size:11px; color:#047857;" title="${{dev.last_seen || ''}}">Seen: ${{formatRelativeTime(dev.last_seen)}}</span>
                </div>
            `;
        }} else if (dev.status === 'archive') {{
            let hasValidIp = dev.ip && dev.ip !== 'Layer 2' && dev.ip !== '-' && dev.ip.trim() !== '' && !dev.ip.includes('(');
            let badgeLabel = isZigbee ? 'Zigbee' : (isBle ? 'BLE' : (dev.category === 'switch' ? 'L2 Switch' : 'No IP'));
            let pingBtnHtml = hasValidIp
                ? `<button type="button" class="btn-sm btn-ping" style="font-size:11px; padding:2px 8px; cursor:pointer;" onclick="pingDevice('${{dev.device_id}}', this)" title="Ping device to check reachability">📡 Ping</button>`
                : `<span class="badge" style="background:#f1f5f9; color:#94a3b8; font-size:10px; padding:2px 6px; border:1px solid #e2e8f0;" title="Gerät ohne direkte IP-Adresse">${{badgeLabel}}</span>`;
            statusBadge = `
                <div style="display:flex; align-items:center; justify-content:space-between; margin-top:8px; padding:6px 10px; background:#f8fafc; border:1px dashed #cbd5e1; border-radius:6px;">
                    <div style="display:flex; align-items:center; gap:6px;">
                        <span style="font-size:13px;">📦</span>
                        <div>
                            <span style="font-weight:600; font-size:12px; color:#64748b;">Router-Archiv (FRITZ!Box)</span><br>
                            <span style="font-size:10px; color:var(--muted);">Nur in FRITZ!Box Historie registriert</span>
                        </div>
                    </div>
                    <div style="display:flex; align-items:center; gap:8px;">
                        <span style="font-size:11px; color:#94a3b8;" title="${{dev.last_seen || ''}}">${{formatRelativeTime(dev.last_seen)}}</span>
                        ${{pingBtnHtml}}
                    </div>
                </div>
            `;
        }} else {{
            let hasValidIp = dev.ip && dev.ip !== 'Layer 2' && dev.ip !== '-' && dev.ip.trim() !== '' && !dev.ip.includes('(');
            let badgeLabel = isZigbee ? 'Zigbee' : (isBle ? 'BLE' : (dev.category === 'switch' ? 'L2 Switch' : 'No IP'));
            let pingBtnHtml = hasValidIp
                ? `<button type="button" class="btn-sm btn-ping" style="font-size:11px; padding:2px 8px; cursor:pointer;" onclick="pingDevice('${{dev.device_id}}', this)" title="Ping device to check reachability">📡 Ping</button>`
                : `<span class="badge" style="background:#f1f5f9; color:#94a3b8; font-size:10px; padding:2px 6px; border:1px solid #e2e8f0;" title="Gerät ohne direkte IP-Adresse">${{badgeLabel}}</span>`;
            statusBadge = `
                <div style="display:flex; align-items:center; justify-content:space-between; margin-top:8px; padding:6px 10px; background:#f8fafc; border:1px solid #cbd5e1; border-radius:6px;">
                    <div style="display:flex; align-items:center; gap:6px;">
                        <span style="display:inline-block; width:8px; height:8px; border-radius:50%; background:#94a3b8;"></span>
                        <span style="font-weight:600; font-size:12px; color:#475569;">Former Device (Offline)</span>
                    </div>
                    <div style="display:flex; align-items:center; gap:8px;">
                        <span style="font-size:11px; color:#64748b;" title="${{dev.last_seen || ''}}">Last seen: ${{formatRelativeTime(dev.last_seen)}}</span>
                        ${{pingBtnHtml}}
                    </div>
                </div>
            `;
        }}

        let historyBox = `
            <div class="inspector-sec" style="background:var(--bg); border:1px solid var(--border); border-radius:6px; padding:8px 10px; margin-top:8px;">
                <div style="font-size:11px; font-weight:600; color:var(--muted); margin-bottom:4px;">⏱️ Device History</div>
                <div style="display:grid; grid-template-columns: 85px 1fr; gap: 3px 6px; font-size:11px;">
                    <span style="color:var(--muted);">First Seen:</span> <span title="${{dev.first_seen || ''}}">${{formatFullDate(dev.first_seen)}}</span>
                    <span style="color:var(--muted);">Last Seen:</span> <span title="${{dev.last_seen || ''}}">${{formatFullDate(dev.last_seen)}}</span>
                </div>
                ${{!dev.is_active ? `
                    <div style="margin-top:8px; text-align:right;">
                        <button type="button" class="btn btn-sm" style="color:var(--status-red); border-color:#fca5a5; font-size:11px; cursor:pointer;" onclick="forgetDevice('${{dev.device_id}}', '${{escapeAttr(dev.display_name)}}')">
                            🗑️ Remove from History
                        </button>
                    </div>
                ` : ''}}
            </div>
        `;

        const sourceMap = {{
            'bthome': {{ name: 'BTHome BLE Interface', icon: '📶', desc: 'Bluetooth Low Energy V2 Sensor-Protokoll' }},
            'shelly-gateway': {{ name: 'Shelly BLE Gateway', icon: '📡', desc: 'Lokales Shelly Gen3/Gen2 Outbound WebSocket Gateway' }},
            'bthome-v2': {{ name: 'BTHome V2', icon: '📶', desc: 'BTHome Version 2 BLE Broadcast' }},
            'ble': {{ name: 'Bluetooth LE Interface', icon: '🔵', desc: 'Bluetooth Low Energy Advertisement' }},
            'arp': {{ name: 'ARP Interface', icon: '📡', desc: 'MAC-Adresse & IP-Zuordnung über Layer-2 ARP' }},
            'ping': {{ name: 'ICMP/TCP Ping Interface', icon: '⚡', desc: 'Aktive IP-Erreichbarkeit' }},
            'mdns': {{ name: 'mDNS / Bonjour Interface', icon: '🔍', desc: 'Hostname & lokale Netzwerkdienste' }},
            'ssdp': {{ name: 'SSDP / UPnP Interface', icon: '🌐', desc: 'UPnP Device Description & Hersteller-Information' }},
            'fritzbox-tr064': {{ name: 'FRITZ!Box TR-064 Interface', icon: '🔀', desc: 'Router-Topologie, L2-Switch & DHCP-Eintrag' }},
            'matter': {{ name: 'Matter Operational Interface', icon: '✨', desc: 'Matter Node & Fabric-Information' }},
            'matter-mdns': {{ name: 'Matter DNS-SD Interface', icon: '✨', desc: 'Matter Operational Discovery' }},
            'home-assistant': {{ name: 'Home Assistant Interface', icon: '🏠', desc: 'Home Assistant Hub / API' }},
            'mdns_homeassistant': {{ name: 'Home Assistant mDNS', icon: '🏠', desc: 'Home Assistant Service Discovery' }},
            'http-probe': {{ name: 'HTTP Web Interface', icon: '🌐', desc: 'Weboberfläche auf Standardports' }},
            'configuration': {{ name: 'Konfiguration', icon: '⚙️', desc: 'Statisch konfigurierter Eintrag' }},
            'philips-hue': {{ name: 'Philips Hue Bridge Interface', icon: '💡', desc: 'Philips Hue Zigbee Bridge Leuchte / Sensor' }},
            'hue': {{ name: 'Philips Hue Bridge Interface', icon: '💡', desc: 'Philips Hue Zigbee Bridge Leuchte / Sensor' }},
            'documentation': {{ name: 'Benutzer-Notiz', icon: '📝', desc: 'Dokumentierter Geräteeintrag' }}
        }};

        let devSources = (dev.sources && dev.sources.length > 0) ? dev.sources : (dev.source ? [dev.source] : ['unknown']);
        let sourcesItems = devSources.map(s => {{
            const info = sourceMap[s] || {{ name: s, icon: '🔌', desc: 'Netzwerkbeobachtung' }};
            return `
                <div style="display:flex; align-items:flex-start; gap:8px; padding:4px 0; border-bottom:1px solid var(--border);">
                    <span style="font-size:14px; line-height:1.2;">${{info.icon}}</span>
                    <div style="flex:1;">
                        <div style="font-weight:600; font-size:11px;">${{info.name}}</div>
                        <div style="font-size:10px; color:var(--muted);">${{info.desc}}</div>
                    </div>
                </div>
            `;
        }}).join('');

        let scannerBox = `
            <div class="inspector-sec" style="background:var(--bg); border:1px solid var(--border); border-radius:6px; padding:8px 10px; margin-top:8px;">
                <div style="font-size:11px; font-weight:600; color:var(--muted); margin-bottom:4px;">🔌 Interfaces (${{devSources.length}})</div>
                ${{sourcesItems}}
            </div>
        `;

        let telemetryBox = '';
        const isBTHomeDevice = devSources.includes('bthome') || devSources.includes('bthome-v2') || (dev.metadata && (dev.metadata.source === 'bthome' || !!dev.metadata.shelly_gateway)) || dev.device_id.startsWith('bthome-');
        const isHueDevice = isZigbee || devSources.includes('philips-hue') || devSources.includes('hue') || (dev.metadata && (dev.metadata.source === 'philips-hue' || dev.metadata.bridge_ip || dev.metadata.hue_light_id)) || dev.device_id.startsWith('hue-');

        if (dev.metadata && (dev.metadata.battery || dev.metadata.button_event || dev.metadata.gateway || dev.metadata.shelly_gateway || dev.metadata.protocol || dev.metadata.temperature_c || dev.metadata.humidity_pct || dev.metadata.illuminance_lux || dev.metadata.contact_state || dev.metadata.rssi)) {{
            let rows = [];
            if (dev.metadata.button_event) {{
                let btnName = dev.metadata.button_event;
                if (btnName === 'press') btnName = 'Single Press (Einfachklick)';
                else if (btnName === 'double_press') btnName = 'Double Press (Doppelklick)';
                else if (btnName === 'triple_press') btnName = 'Triple Press (Dreifachklick)';
                else if (btnName === 'long_press') btnName = 'Long Press (Langer Druck)';
                else if (btnName === 'hold') btnName = 'Hold (Gehalten)';
                rows.push(`<div><span style="color:var(--muted);">Letzte Aktion:</span> <strong style="color:var(--primary); font-size:12px;">🔘 ${{btnName}}</strong></div>`);
            }}
            if (dev.metadata.battery) {{
                let bat = parseInt(dev.metadata.battery, 10) || 0;
                let batColor = bat > 50 ? 'var(--status-green)' : (bat > 20 ? 'var(--status-yellow)' : 'var(--status-red)');
                rows.push(`<div><span style="color:var(--muted);">Batterie:</span> <strong style="color:${{batColor}};">🔋 ${{bat}}%</strong></div>`);
            }}
            if (dev.metadata.contact_state) {{
                let open = dev.metadata.contact_state === 'open';
                rows.push(`<div><span style="color:var(--muted);">Kontakt:</span> <strong>${{open ? '🚪 Offen' : '🚪 Geschlossen'}}</strong></div>`);
            }}
            if (dev.metadata.temperature_c) {{
                rows.push(`<div><span style="color:var(--muted);">Temperatur:</span> <strong>🌡️ ${{dev.metadata.temperature_c}} °C</strong></div>`);
            }}
            if (dev.metadata.humidity_pct) {{
                rows.push(`<div><span style="color:var(--muted);">Luftfeuchtigkeit:</span> <strong>💧 ${{dev.metadata.humidity_pct}} %</strong></div>`);
            }}
            if (dev.metadata.illuminance_lux) {{
                rows.push(`<div><span style="color:var(--muted);">Helligkeit:</span> <strong>☀️ ${{dev.metadata.illuminance_lux}} Lux</strong></div>`);
            }}
            if (dev.metadata.rssi) {{
                rows.push(`<div><span style="color:var(--muted);">Signalstärke:</span> <strong>📶 ${{dev.metadata.rssi}} dBm</strong></div>`);
            }}
            if (isBTHomeDevice && (dev.metadata.shelly_gateway || dev.metadata.gateway)) {{
                let gw = dev.metadata.shelly_gateway || dev.metadata.gateway;
                rows.push(`<div><span style="color:var(--muted);">Shelly BLE Gateway:</span> <code>${{gw}}</code></div>`);
            }} else if (isHueDevice && (dev.metadata.bridge_ip || dev.metadata.gateway)) {{
                let gw = dev.metadata.bridge_ip || dev.metadata.gateway;
                rows.push(`<div><span style="color:var(--muted);">Hue Bridge:</span> <code>${{gw}}</code></div>`);
            }} else if (dev.metadata.gateway) {{
                rows.push(`<div><span style="color:var(--muted);">Gateway:</span> <code>${{dev.metadata.gateway}}</code></div>`);
            }}
            if (dev.metadata.protocol) {{
                let proto = dev.metadata.protocol;
                let badgeStyle = "background:#e2e8f0; color:#334155; font-size:10px; padding:1px 5px; border-radius:3px;";
                if (proto.toLowerCase() === 'zigbee') {{
                    badgeStyle = "background:rgba(234,179,8,0.2); color:#b45309; font-weight:600; font-size:10px; padding:1px 5px; border-radius:3px;";
                }} else if (proto.toLowerCase().includes('bthome') || proto.toLowerCase().includes('ble')) {{
                    badgeStyle = "background:rgba(59,130,246,0.15); color:#2563eb; font-weight:600; font-size:10px; padding:1px 5px; border-radius:3px;";
                }}
                rows.push(`<div><span style="color:var(--muted);">Protokoll:</span> <span class="badge" style="${{badgeStyle}}">${{proto}}</span></div>`);
            }}

            let boxTitle = '📶 BTHome Live Telemetrie';
            let boxBorderLeft = 'var(--primary)';
            let titleColor = 'var(--primary)';

            if (isHueDevice) {{
                boxTitle = '💡 Philips Hue / Zigbee Verbindung';
                boxBorderLeft = '#eab308';
                titleColor = '#b45309';
            }} else if (!isBTHomeDevice) {{
                boxTitle = '📊 Telemetrie & Verbindung';
                boxBorderLeft = '#64748b';
                titleColor = 'var(--text)';
            }}

            telemetryBox = `
                <div class="inspector-sec" style="background:var(--bg); border:1px solid var(--border); border-left:3px solid ${{boxBorderLeft}}; border-radius:6px; padding:8px 10px; margin-top:8px;">
                    <div style="font-size:11px; font-weight:700; color:${{titleColor}}; margin-bottom:6px; display:flex; align-items:center; gap:6px;">
                        <span>${{boxTitle}}</span>
                    </div>
                    <div style="display:flex; flex-direction:column; gap:4px; font-size:11px;">
                        ${{rows.join('')}}
                    </div>
                </div>
            `;
        }}

        let hueControlCard = '';
        const isHueActuatorOrLight = isHueDevice && (dev.category === 'lighting' || dev.kind === 'lighting' || dev.device_id.startsWith('hue-light-') || (dev.metadata && (dev.metadata.hue_light_id || dev.metadata.on !== undefined)));
        if (isHueActuatorOrLight && dev.metadata && (dev.metadata.hue_light_id || dev.device_id.startsWith('hue-light-') || dev.metadata.on !== undefined)) {{
            const lightId = dev.metadata.hue_light_id || dev.device_id.replace('hue-light-', '');
            const isOn = dev.metadata.on === 'true';
            const bri = dev.metadata.brightness ? `${{Math.round(parseInt(dev.metadata.brightness, 10) / 254 * 100)}}%` : '';
            const reachable = dev.metadata.reachable !== 'false';
            const isPlug = (dev.metadata.model && dev.metadata.model.toLowerCase().includes('plug')) || (dev.display_name && (dev.display_name.toLowerCase().includes('steckdose') || dev.display_name.toLowerCase().includes('plug') || dev.display_name.toLowerCase().includes('ventilator')));
            const cardIcon = isPlug ? '🔌' : '💡';
            const cardTitle = isPlug ? 'Philips Hue Aktor / Steckdose' : 'Philips Hue Lampe';
            hueControlCard = `
                <div class="inspector-sec" style="background:linear-gradient(135deg, rgba(234,179,8,0.12) 0%, rgba(245,158,11,0.05) 100%); border:1px solid rgba(234,179,8,0.35); border-radius:8px; padding:10px; margin-top:8px;">
                    <div style="display:flex; justify-content:space-between; align-items:center;">
                        <div>
                            <div style="font-weight:700; font-size:13px; color:#b45309; display:flex; align-items:center; gap:6px;">
                                <span>${{cardIcon}}</span> ${{cardTitle}}
                            </div>
                            <div style="font-size:11px; color:var(--muted); margin-top:2px;">
                                Zustand: <strong>${{isOn ? '🟢 Eingeschaltet' : '⚪ Ausgeschaltet'}}</strong> ${{bri ? '&bull; ' + bri : ''}} ${{!reachable ? '&bull; ⚠️ Offline' : ''}}
                            </div>
                        </div>
                        <button type="button" class="btn btn-sm ${{isOn ? '' : 'btn-primary'}}" style="font-size:11px; padding:4px 10px; cursor:pointer;" onclick="toggleHueLight('${{lightId}}')">
                            ${{isOn ? 'Ausschalten 🔌' : 'Einschalten 💡'}}
                        </button>
                    </div>
                </div>
            `;
        }}

        let goveeControlCard = '';
        let devIp = dev.ip;
        let devSecondaries = dev.secondaries || [];
        let goveeIp = null;
        if (devIp && allGoveeStates[devIp]) {{
            goveeIp = devIp;
        }} else {{
            for (let s of devSecondaries) {{
                if (s.ip && allGoveeStates[s.ip]) {{
                    goveeIp = s.ip;
                    break;
                }}
            }}
        }}
        if (!goveeIp && devIp && (
            (dev.hostname && dev.hostname.toLowerCase().includes('govee')) ||
            (dev.display_name && dev.display_name.toLowerCase().includes('govee')) ||
            (dev.vendor && dev.vendor.toLowerCase().includes('govee'))
        )) {{
            goveeIp = devIp;
        }}

        if (goveeIp) {{
            const gState = allGoveeStates[goveeIp] || {{ on: false, brightness: 100, color: {{ r: 255, g: 255, b: 255 }}, color_temp_kelvin: 0 }};
            const isOn = gState.on;
            const bri = gState.brightness || 100;
            const col = gState.color || {{ r: 255, g: 255, b: 255 }};
            const colorCss = `rgb(${{col.r}}, ${{col.g}}, ${{col.b}})`;
            const sku = gState.sku ? ` (${{escapeHtml(gState.sku)}})` : '';

            goveeControlCard = `
                <div class="inspector-sec" style="background:linear-gradient(135deg, rgba(37,99,235,0.08) 0%, rgba(59,130,246,0.04) 100%); border:1px solid rgba(37,99,235,0.3); border-radius:8px; padding:12px; margin-top:8px;">
                    <div style="display:flex; justify-content:space-between; align-items:center; margin-bottom:10px;">
                        <div>
                            <div style="font-weight:700; font-size:13px; color:#1e40af; display:flex; align-items:center; gap:6px;">
                                <span style="display:inline-block; width:12px; height:12px; border-radius:50%; background:${{colorCss}}; border:1px solid rgba(0,0,0,0.2);"></span>
                                <span>💡 Govee LAN Steuerung${{sku}}</span>
                            </div>
                            <div style="font-size:11px; color:var(--muted); margin-top:2px;">
                                IP: <code>${{escapeHtml(goveeIp)}}</code> &bull; UDP Port 4003 &bull; Zustand: <strong>${{isOn ? '🟢 Eingeschaltet' : '⚪ Ausgeschaltet'}}</strong>
                            </div>
                        </div>
                        <button type="button" class="btn btn-sm ${{isOn ? 'btn-primary' : ''}}" style="font-size:11px; padding:4px 12px; cursor:pointer;" onclick="toggleGoveeLight('${{goveeIp}}', this)">
                            ${{isOn ? '💡 An' : '⚪ Aus'}}
                        </button>
                    </div>

                    <div style="display:flex; align-items:center; gap:8px; margin-bottom:10px; font-size:12px;">
                        <span style="font-size:11px; color:var(--muted); min-width:55px;">Helligkeit:</span>
                        <input type="range" min="1" max="100" value="${{bri}}" style="flex:1; cursor:pointer;" onchange="setGoveeBrightness('${{goveeIp}}', this.value)" oninput="this.nextElementSibling.innerText = this.value + '%'" />
                        <span style="min-width:35px; text-align:right; font-weight:600; font-size:11px;">${{bri}}%</span>
                    </div>

                    <div style="display:flex; align-items:center; gap:6px; flex-wrap:wrap;">
                        <span style="font-size:11px; color:var(--muted); margin-right:4px;">Presets:</span>
                        <button type="button" class="btn-sm" style="background:#ffddaa; border:1px solid #f59e0b; font-size:10px; padding:2px 6px; cursor:pointer; border-radius:4px;" onclick="setGoveeTemp('${{goveeIp}}', 2700)" title="Warmweiß">Warm (2700K)</button>
                        <button type="button" class="btn-sm" style="background:#fff4e6; border:1px solid #cbd5e1; font-size:10px; padding:2px 6px; cursor:pointer; border-radius:4px;" onclick="setGoveeTemp('${{goveeIp}}', 4000)" title="Neutralweiß">Neutral (4000K)</button>
                        <button type="button" class="btn-sm" style="background:#f1f5f9; border:1px solid #cbd5e1; font-size:10px; padding:2px 6px; cursor:pointer; border-radius:4px;" onclick="setGoveeTemp('${{goveeIp}}', 6500)" title="Kaltweiß">Kalt (6500K)</button>
                        <button type="button" class="btn-sm" style="background:#ef4444; color:#fff; border:none; font-size:10px; padding:2px 6px; cursor:pointer; border-radius:4px;" onclick="setGoveeColor('${{goveeIp}}', 255, 0, 0)">Rot</button>
                        <button type="button" class="btn-sm" style="background:#10b981; color:#fff; border:none; font-size:10px; padding:2px 6px; cursor:pointer; border-radius:4px;" onclick="setGoveeColor('${{goveeIp}}', 0, 255, 100)">Grün</button>
                        <button type="button" class="btn-sm" style="background:#3b82f6; color:#fff; border:none; font-size:10px; padding:2px 6px; cursor:pointer; border-radius:4px;" onclick="setGoveeColor('${{goveeIp}}', 0, 100, 255)">Blau</button>
                        <button type="button" class="btn-sm" style="background:#8b5cf6; color:#fff; border:none; font-size:10px; padding:2px 6px; cursor:pointer; border-radius:4px;" onclick="setGoveeColor('${{goveeIp}}', 160, 32, 240)">Lila</button>
                        <input type="color" value='#ffffff' style="width:24px; height:24px; padding:0; border:none; border-radius:4px; cursor:pointer; margin-left:auto;" title="Eigene Farbe wählen" onchange="handleGoveeCustomColor('${{goveeIp}}', this.value)" />
                    </div>
                </div>
            `;
        }}

        let roomOptionsHtml = `<option value="">-- Kein Raum zugewiesen --</option>`;
        const floorList = ['Dachgeschoss', 'Obergeschoss', 'Erdgeschoss', 'Keller', 'Außenbereich', 'Sonstige'];
        floorList.forEach(fl => {{
            const inFloor = allRooms.filter(r => (r.floor || 'Sonstige') === fl);
            if (inFloor.length > 0) {{
                roomOptionsHtml += `<optgroup label="${{fl}}">`;
                inFloor.forEach(r => {{
                    const isSel = (dev.room && dev.room.toLowerCase() === r.name.toLowerCase()) ? 'selected' : '';
                    roomOptionsHtml += `<option value="${{escapeAttr(r.name)}}" ${{isSel}}>${{r.icon || '📍'}} ${{escapeHtml(r.name)}}</option>`;
                }});
                roomOptionsHtml += `</optgroup>`;
            }}
        }});
        if (dev.room && !allRooms.some(r => r.name.toLowerCase() === dev.room.toLowerCase())) {{
            roomOptionsHtml += `<option value="${{escapeAttr(dev.room)}}" selected>📍 ${{escapeHtml(dev.room)}} (Benutzerdefiniert)</option>`;
        }}

        panel.innerHTML = `
            <div>
                <div style="display:flex; align-items:center; gap:8px; margin-bottom:4px;">
                    <span style="font-size:24px;">${{dev.category_icon}}</span>
                    <div>
                        <h3 id="insp-header-title" style="font-size:16px;">${{dev.display_name}}</h3>
                        <span class="badge badge-kind">${{dev.category_title}}</span>
                    </div>
                </div>
                ${{statusBadge}}
                ${{hueControlCard}}
                ${{goveeControlCard}}
                ${{telemetryBox}}
                ${{historyBox}}
                ${{scannerBox}}
                ${{categorySelectorBox}}
                ${{webBtn}}
                ${{productCard}}
                ${{matterFabricsCard}}
                ${{assignBox}}
            </div>

            <!-- Network Interfaces -->
            <div class="inspector-sec">
                <div class="inspector-title">
                    <span>Network Interfaces (${{(dev.secondaries ? dev.secondaries.length : 0) + 1}})</span>
                </div>
                ${{ifacesHtml}}
                ${{candidateBox}}
                ${{manualLinkHtml}}
            </div>

            <!-- Device Identity & Documentation -->
            <div class="inspector-sec">
                <div class="inspector-title">
                    <span>Identity & Documentation</span>
                    <span id="doc-status" style="font-size:11px;"></span>
                </div>
                <div style="margin-bottom:8px;">
                    <label style="font-size:11px; color:var(--muted); font-weight:600;">Device Name (Optional override)</label>
                    <input type="text" id="insp-name" class="form-control" value="${{escapeAttr(dev.custom_name || '')}}" placeholder="${{escapeAttr(dev.display_name)}}" />
                </div>
                <div style="margin-bottom:8px;">
                    <div style="display:flex; justify-content:space-between; align-items:center; margin-bottom:4px;">
                        <label style="font-size:11px; color:var(--muted); font-weight:600;">Raum / Standort (Room / Location)</label>
                        <a href="javascript:void(0)" onclick="openRoomModal()" style="font-size:11px; color:var(--primary); text-decoration:none; font-weight:600;">⚙️ Räume verwalten</a>
                    </div>
                    <select id="insp-room" class="form-control" style="cursor:pointer;">
                        ${{roomOptionsHtml}}
                    </select>
                </div>
                <div style="margin-bottom:8px;">
                    <label style="font-size:11px; color:var(--muted); font-weight:600;">Manual / Documentation URL (Optional)</label>
                    <input type="url" id="insp-manual-url" class="form-control" value="${{escapeAttr(dev.manual_url)}}" placeholder="https://..." />
                </div>
                <div style="margin-bottom:8px;">
                    <label style="font-size:11px; color:var(--muted); font-weight:600;">Device Notes (Optional)</label>
                    <textarea id="insp-notes" class="form-control" placeholder="Installation location, credentials hint, firmware version...">${{escapeHtml(dev.notes)}}</textarea>
                </div>
                <button type="button" class="btn btn-sm btn-primary" onclick="saveInspectorNotes('${{dev.doc_key}}')">💾 Save Details</button>
            </div>

            <!-- Device Management & Privacy (Block Neighbor Devices) -->
            <div class="inspector-sec" style="background:var(--bg); border:1px solid var(--border); border-radius:6px; padding:10px; margin-top:10px;">
                <div class="inspector-title" style="margin-bottom:6px;">
                    <span>⚙️ Geräteverwaltung & Filter</span>
                </div>
                ${{dev.is_ignored ? `
                    <div style="background:rgba(239, 68, 68, 0.08); border:1px solid rgba(239, 68, 68, 0.25); border-radius:6px; padding:8px; margin-bottom:8px; font-size:11px;">
                        <strong style="color:var(--status-red);">🚫 Nachbargerät (Ignoriert)</strong>
                        <div style="color:var(--muted); margin-top:2px;">Dieses Gerät ist als Nachbargerät blockiert. Signale werden verworfen und das Gerät erscheint nicht in der Standardliste.</div>
                    </div>
                    <button type="button" class="btn btn-sm" style="background:var(--surface); border:1px solid var(--border); font-size:11px;" onclick="unignoreDevice('${{dev.device_id}}', '${{dev.doc_key}}')">
                        ✅ Nicht mehr ignorieren (Wiederherstellen)
                    </button>
                ` : `
                    <div style="font-size:11px; color:var(--muted); margin-bottom:8px;">
                        Signale fremder Geräte (z.B. BLE-Sensoren vom Nachbarn) können hier dauerhaft ignoriert werden:
                    </div>
                    <button type="button" class="btn btn-sm" style="background:#fee2e2; color:#b91c1c; border:1px solid #fca5a5; font-size:11px;" onclick="ignoreDevice('${{dev.device_id}}', '${{dev.doc_key}}', '${{escapeAttr(dev.display_name)}}')">
                        🚫 Als Nachbargerät ignorieren
                    </button>
                `}}
                <div style="margin-top:10px; padding-top:8px; border-top:1px solid var(--border);">
                    <button type="button" class="btn btn-sm" style="color:var(--status-red); border:1px solid var(--border); font-size:11px;" onclick="forgetDevice('${{dev.device_id}}', '${{escapeAttr(dev.display_name)}}')">
                        🗑️ Aus Verlauf löschen (Forget)
                    </button>
                </div>
            </div>

            <!-- Analyzer & Port Scan -->
            <div class="inspector-sec">
                <div class="inspector-title">
                    <span>Device Analyzer & Port Scan</span>
                </div>
                <button type="button" class="btn btn-sm" id="btn-run-analyzer" onclick="runInspectorAnalyzer('${{dev.device_id}}', '${{escapeAttr(dev.display_name)}}')">
                    🔍 Scan 19 Ports & Analyze
                </button>
                <div id="inspector-analysis-results" style="margin-top:10px;"></div>
            </div>
        `;
    }}

    function escapeHtml(text) {{
        return (text || '').replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
    }}

    function escapeAttr(text) {{
        return (text || '').replace(/"/g, "&quot;").replace(/'/g, "&#39;");
    }}

    async function updateDeviceCategory(docKey, deviceId) {{
        const sel = document.getElementById('insp-change-cat');
        if (!sel) return;
        const newCat = sel.value;
        const statusEl = document.getElementById('cat-status');
        if (statusEl) {{
            statusEl.innerText = 'Updating category...';
            statusEl.style.color = 'var(--muted)';
        }}

        try {{
            const key = docKey || deviceId;
            const res = await fetch('/api/devices/' + encodeURIComponent(key) + '/category', {{
                method: 'POST',
                headers: {{ 'Content-Type': 'application/json' }},
                body: JSON.stringify({{ category: newCat }})
            }});
            if (res.ok) {{
                if (statusEl) {{
                    statusEl.innerText = 'Category updated! Reloading...';
                    statusEl.style.color = 'var(--status-green)';
                }}
                setTimeout(() => {{
                    window.location.reload();
                }}, 400);
            }} else {{
                if (statusEl) {{
                    statusEl.innerText = 'Failed to update category';
                    statusEl.style.color = 'var(--status-red)';
                }}
            }}
        }} catch (e) {{
            if (statusEl) {{
                statusEl.innerText = 'Network error: ' + e;
                statusEl.style.color = 'var(--status-red)';
            }}
        }}
    }}

    async function saveInspectorNotes(docKey) {{
        const nameInput = document.getElementById('insp-name');
        const customName = nameInput ? nameInput.value.trim() : '';
        const roomInput = document.getElementById('insp-room');
        const room = roomInput ? roomInput.value.trim() : '';
        const notes = document.getElementById('insp-notes').value;
        const manualUrl = document.getElementById('insp-manual-url').value;
        const statusEl = document.getElementById('doc-status');

        statusEl.innerText = 'Saving...';
        statusEl.style.color = 'var(--muted)';
        try {{
            const res = await fetch('/api/devices/' + encodeURIComponent(docKey) + '/documentation', {{
                method: 'POST',
                headers: {{ 'Content-Type': 'application/json' }},
                body: JSON.stringify({{ name: customName, room: room, notes: notes, manual_url: manualUrl }})
            }});
            if (res.ok) {{
                statusEl.innerText = 'Saved!';
                statusEl.style.color = 'var(--status-green)';
                const dev = allDevices.find(d => d.doc_key === docKey || d.device_id === docKey);
                if (dev) {{
                    dev.custom_name = customName;
                    dev.room = room;
                    dev.display_name = customName || dev.original_name || dev.display_name;
                    dev.notes = notes;
                    dev.manual_url = manualUrl;
                    const titleEl = document.getElementById('insp-header-title');
                    if (titleEl) titleEl.innerText = dev.display_name;
                    const rowNameEl = document.querySelector(`tr[data-id="${{dev.device_id}}"] .dev-display-name`);
                    if (rowNameEl) rowNameEl.innerText = dev.display_name;
                    const tr = document.querySelector(`tr[data-id="${{dev.device_id}}"]`);
                    if (tr) {{
                        tr.setAttribute('data-room', room);
                        const matchedRoom = allRooms.find(r => r.name.toLowerCase() === room.toLowerCase());
                        tr.setAttribute('data-floor', matchedRoom ? (matchedRoom.floor || '') : '');
                    }}
                }}
                setTimeout(() => {{ statusEl.innerText = ''; }}, 2000);
            }} else {{
                statusEl.innerText = 'Error saving';
                statusEl.style.color = 'var(--status-red)';
            }}
        }} catch (e) {{
            statusEl.innerText = 'Network error: ' + e;
            statusEl.style.color = 'var(--status-red)';
        }}
    }}

    async function pingDevice(deviceId, btn) {{
        const originalText = btn ? btn.innerHTML : '📡 Ping';
        if (btn) {{
            btn.disabled = true;
            btn.innerHTML = '⏳ Pinging...';
        }}

        try {{
            const res = await fetch('/api/devices/' + encodeURIComponent(deviceId) + '/ping', {{
                method: 'POST'
            }});
            const data = await res.json();

            if (data.reachable) {{
                const rttText = data.rtt_ms ? (data.rtt_ms.toFixed(1) + ' ms') : data.method;
                if (btn) {{
                    btn.innerHTML = '✅ Online (' + rttText + ')';
                    btn.style.borderColor = '#10b981';
                    btn.style.color = '#065f46';
                    btn.style.background = '#ecfdf5';
                }}

                // Update in allDevices memory
                const dev = allDevices.find(d => d.device_id === deviceId || d.doc_key === deviceId);
                if (dev) {{
                    dev.is_active = true;
                    dev.status = 'active';
                    dev.last_seen = data.last_seen || new Date().toISOString();

                    // If inspector is open for this device, re-render it
                    if (selectedDeviceId === dev.device_id) {{
                        renderInspector(dev);
                    }}
                }}

                // Update table row indicators
                const tr = document.querySelector(`tr[data-id="${{deviceId}}"]`);
                if (tr) {{
                    tr.setAttribute('data-status', 'active');
                    tr.setAttribute('data-active', 'true');
                    const dot = tr.querySelector('.status-dot');
                    if (dot) {{
                        dot.style.background = '#10b981';
                        dot.title = 'Online / Active';
                    }}
                    const badges = tr.querySelectorAll('.badge');
                    badges.forEach(b => {{
                        if (b.innerText.includes('Offline')) b.remove();
                    }});
                }}

                setTimeout(() => {{
                    if (btn) {{
                        btn.disabled = false;
                        if (btn.classList.contains('btn-ping')) {{
                            btn.style.display = 'none';
                        }} else {{
                            btn.innerHTML = originalText;
                            btn.style.borderColor = '';
                            btn.style.color = '';
                            btn.style.background = '';
                        }}
                    }}
                }}, 3500);
            }} else {{
                if (btn) {{
                    btn.innerHTML = '❌ Offline';
                    btn.style.borderColor = '#f87171';
                    btn.style.color = '#991b1b';
                    btn.style.background = '#fef2f2';
                    setTimeout(() => {{
                        btn.disabled = false;
                        btn.innerHTML = originalText;
                        btn.style.borderColor = '';
                        btn.style.color = '';
                        btn.style.background = '';
                    }}, 2500);
                }}
            }}
        }} catch (err) {{
            if (btn) {{
                btn.disabled = false;
                btn.innerHTML = '⚠️ Error';
                setTimeout(() => {{ btn.innerHTML = originalText; }}, 2000);
            }}
        }}
    }}

    async function linkInterface(primaryId, linkedId) {{
        try {{
            const res = await fetch('/api/devices/link', {{
                method: 'POST',
                headers: {{ 'Content-Type': 'application/json' }},
                body: JSON.stringify({{ primary_id: primaryId, linked_id: linkedId }})
            }});
            if (res.ok) {{
                window.location.reload();
            }}
        }} catch (e) {{
            alert('Failed to link interfaces: ' + e);
        }}
    }}

    async function unlinkInterface(primaryId, linkedId) {{
        try {{
            const res = await fetch('/api/devices/unlink', {{
                method: 'POST',
                headers: {{ 'Content-Type': 'application/json' }},
                body: JSON.stringify({{ primary_id: primaryId, linked_id: linkedId }})
            }});
            if (res.ok) {{
                window.location.reload();
            }}
        }} catch (e) {{
            alert('Failed to unlink interfaces: ' + e);
        }}
    }}

    function linkManualInterface(primaryId) {{
        const sel = document.getElementById('insp-manual-link-select');
        if (!sel || !sel.value) {{
            alert('Bitte wähle ein Gerät aus der Liste aus.');
            return;
        }}
        linkInterface(primaryId, sel.value);
    }}

    async function runInspectorAnalyzer(deviceId, deviceName) {{
        const btn = document.getElementById('btn-run-analyzer');
        const resultsEl = document.getElementById('inspector-analysis-results');
        btn.disabled = true;
        resultsEl.innerHTML = '<div style="font-size:12px; color:var(--muted);"><span class="status-dot status-starting"></span>Probing 19 ports & service banners...</div>';

        try {{
            const res = await fetch('/api/devices/' + encodeURIComponent(deviceId) + '/analyze', {{ method: 'POST' }});
            if (!res.ok) {{
                resultsEl.innerHTML = '<div style="color:var(--status-red); font-size:12px;">Analysis failed.</div>';
                btn.disabled = false;
                return;
            }}
            const data = await res.json();

            let portsHtml = '';
            if (data.open_ports && data.open_ports.length > 0) {{
                portsHtml = data.open_ports.map(p => '<span class="badge" style="background:#dcfce7; color:#166534; font-weight:600; padding:2px 6px; margin:2px 3px 2px 0; font-size:10px;">✓ Port ' + p.port + ': ' + p.service + '</span>').join('');
            }} else {{
                portsHtml = '<span style="color:var(--muted); font-size:11px;">No open ports found.</span>';
            }}

            let httpInfo = '';
            if (data.http_title || data.http_server) {{
                httpInfo = '<div style="margin-top:6px; font-size:11px; color:var(--muted);">' +
                    (data.http_title ? '<strong>Title:</strong> ' + data.http_title + '<br>' : '') +
                    (data.http_server ? '<strong>Server:</strong> ' + data.http_server : '') +
                    '</div>';
            }}

            window._currentAnalyzedScript = {{
                filename: data.suggested_filename,
                content: data.suggested_rhai_script
            }};

            resultsEl.innerHTML = `
                <div style="background:var(--bg); border:1px solid var(--border); border-radius:6px; padding:8px; font-size:12px;">
                    <div style="font-weight:600; font-size:11px; margin-bottom:4px; display:flex; justify-content:space-between;">
                        <span>🔌 Port Scan Results</span>
                        <span class="badge" style="font-size:10px;">${{data.open_ports ? data.open_ports.length : 0}} / ${{data.total_ports_scanned || 19}} Open</span>
                    </div>
                    <div>${{portsHtml}}</div>
                    ${{httpInfo}}
                </div>
                <div style="display:flex; justify-content:space-between; align-items:center; margin-top:8px;">
                    <span style="font-size:11px; font-weight:600;">Draft Script (${{data.suggested_filename}}):</span>
                    <div>
                        <button class="btn-sm" style="font-size:10px;" onclick="copyRhaiScript()">📋 Copy</button>
                        <button class="btn-sm btn-web" style="font-size:10px;" onclick="saveAnalyzedDefinition()">💾 Save</button>
                    </div>
                </div>
                <pre class="code-box" style="margin-top:4px;"><code id="rhai-code-preview">${{escapeHtml(data.suggested_rhai_script)}}</code></pre>
                <div id="save-def-status" style="font-size:11px; margin-top:4px; color:var(--status-green);"></div>
            `;
            btn.disabled = false;
        }} catch (e) {{
            resultsEl.innerHTML = '<div style="color:var(--status-red); font-size:12px;">Analysis error: ' + e + '</div>';
            btn.disabled = false;
        }}
    }}

    function copyRhaiScript() {{
        if (window._currentAnalyzedScript) {{
            navigator.clipboard.writeText(window._currentAnalyzedScript.content);
            alert('Rhai script copied to clipboard!');
        }}
    }}

    async function saveAnalyzedDefinition() {{
        if (!window._currentAnalyzedScript) return;
        const status = document.getElementById('save-def-status');
        status.innerText = 'Saving definition script...';

        try {{
            const res = await fetch('/api/definitions/save', {{
                method: 'POST',
                headers: {{ 'Content-Type': 'application/json' }},
                body: JSON.stringify(window._currentAnalyzedScript)
            }});
            if (res.ok) {{
                status.innerText = 'Saved to definitions/devices/' + window._currentAnalyzedScript.filename + '! Click "Scan Network Now" to apply.';
            }} else {{
                status.innerText = 'Failed to save definition file.';
            }}
        }} catch (e) {{
            status.innerText = 'Error saving script: ' + e;
        }}
    }}

    async function assignProduct(deviceId, docKey) {{
        const prodId = document.getElementById('insp-assign-prod').value;
        if (!prodId) return;
        const res = await fetch(`/api/devices/${{deviceId}}/assign-product`, {{
            method: 'POST',
            headers: {{ 'Content-Type': 'application/json' }},
            body: JSON.stringify({{ doc_key: docKey, product_id: prodId }})
        }});
        if (res.ok) {{
            window.location.reload();
        }}
    }}

    async function renameMatterFabric(fabricId, currentName, currentIcon) {{
        const newName = prompt(`Enter friendly label for Matter Fabric (${{fabricId}}):`, currentName);
        if (!newName || !newName.trim()) return;
        const newIcon = prompt(`Enter emoji/icon for ${{newName}}:`, currentIcon || '✨') || '✨';

        try {{
            const res = await fetch('/api/matter/fabrics/' + encodeURIComponent(fabricId), {{
                method: 'POST',
                headers: {{ 'Content-Type': 'application/json' }},
                body: JSON.stringify({{ name: newName.trim(), icon: newIcon.trim() }})
            }});
            if (res.ok) {{
                window.location.reload();
            }} else {{
                alert('Failed to update Matter fabric label.');
            }}
        }} catch (e) {{
            alert('Network error: ' + e);
        }}
    }}

    async function forgetDevice(deviceId, deviceName) {{
        if (!confirm(`Are you sure you want to remove "${{deviceName || deviceId}}" from history?\n\nIf this device connects to the network again in the future, it will be rediscovered.`)) {{
            return;
        }}

        try {{
            const res = await fetch('/api/devices/' + encodeURIComponent(deviceId) + '/forget', {{
                method: 'POST'
            }});
            if (res.ok) {{
                window.location.reload();
            }} else {{
                alert('Failed to remove device from history.');
            }}
        }} catch (e) {{
            alert('Network error: ' + e);
        }}
    }}

    async function ignoreDevice(deviceId, docKey, name) {{
        if (!confirm(`Möchtest du "${{name || deviceId}}" ignorieren?\n\nSignale dieses Geräts (z.B. BLE-Sensoren vom Nachbarn) werden blockiert und das Gerät wird in der Hauptliste ausgeblendet.`)) {{
            return;
        }}
        try {{
            const id = docKey || deviceId;
            const res = await fetch('/api/devices/' + encodeURIComponent(id) + '/ignore', {{
                method: 'POST',
                headers: {{ 'Content-Type': 'application/json' }},
                body: JSON.stringify({{ name: name, reason: 'Nachbargerät' }})
            }});
            if (res.ok) {{
                window.location.reload();
            }} else {{
                alert('Fehler beim Ignorieren des Geräts.');
            }}
        }} catch (e) {{
            alert('Netzwerkfehler: ' + e);
        }}
    }}

    async function unignoreDevice(deviceId, docKey) {{
        try {{
            const id = docKey || deviceId;
            const res = await fetch('/api/devices/' + encodeURIComponent(id) + '/unignore', {{
                method: 'POST'
            }});
            if (res.ok) {{
                window.location.reload();
            }} else {{
                alert('Fehler beim Wiederherstellen des Geräts.');
            }}
        }} catch (e) {{
            alert('Netzwerkfehler: ' + e);
        }}
    }}

    async function toggleHueLight(lightId) {{
        try {{
            const res = await fetch('/api/hue/lights/' + encodeURIComponent(lightId) + '/toggle', {{ method: 'POST' }});
            const json = await res.json();
            if (res.ok && json.status === 'ok') {{
                setTimeout(() => window.location.reload(), 250);
            }} else {{
                alert('Hue Fehler: ' + (json.message || json.error || 'Schalten fehlgeschlagen'));
            }}
        }} catch(e) {{
            alert('Netzwerkfehler: ' + e);
        }}
    }}

    async function toggleGoveeLight(ip, btn) {{
        if (btn) btn.disabled = true;
        try {{
            const res = await fetch('/api/govee/lights/' + encodeURIComponent(ip) + '/toggle', {{ method: 'POST' }});
            const data = await res.json();
            if (res.ok && data.status === 'ok') {{
                if (allGoveeStates[ip]) {{
                    allGoveeStates[ip].on = data.on;
                }} else {{
                    allGoveeStates[ip] = {{ on: data.on, brightness: 100 }};
                }}
                const dev = allDevices.find(d => d.device_id === selectedDeviceId);
                if (dev) renderInspector(dev);
            }} else {{
                alert('Govee Fehler: ' + (data.error || 'Schalten fehlgeschlagen'));
            }}
        }} catch(e) {{
            alert('Govee UDP Fehler: ' + e);
        }} finally {{
            if (btn) btn.disabled = false;
        }}
    }}

    async function setGoveeBrightness(ip, val) {{
        try {{
            const b = parseInt(val, 10);
            await fetch('/api/govee/lights/' + encodeURIComponent(ip) + '/brightness', {{
                method: 'POST',
                headers: {{ 'Content-Type': 'application/json' }},
                body: JSON.stringify({{ brightness: b }})
            }});
            if (allGoveeStates[ip]) {{
                allGoveeStates[ip].brightness = b;
                allGoveeStates[ip].on = true;
            }}
        }} catch(e) {{
            console.error('Govee Brightness Error:', e);
        }}
    }}

    async function setGoveeColor(ip, r, g, b) {{
        try {{
            await fetch('/api/govee/lights/' + encodeURIComponent(ip) + '/color', {{
                method: 'POST',
                headers: {{ 'Content-Type': 'application/json' }},
                body: JSON.stringify({{ r: r, g: g, b: b }})
            }});
            if (allGoveeStates[ip]) {{
                allGoveeStates[ip].color = {{ r: r, g: g, b: b }};
                allGoveeStates[ip].on = true;
            }}
            const dev = allDevices.find(d => d.device_id === selectedDeviceId);
            if (dev) renderInspector(dev);
        }} catch(e) {{
            console.error('Govee Color Error:', e);
        }}
    }}

    async function setGoveeTemp(ip, kelvin) {{
        try {{
            await fetch('/api/govee/lights/' + encodeURIComponent(ip) + '/temperature', {{
                method: 'POST',
                headers: {{ 'Content-Type': 'application/json' }},
                body: JSON.stringify({{ kelvin: kelvin }})
            }});
            if (allGoveeStates[ip]) {{
                allGoveeStates[ip].color_temp_kelvin = kelvin;
                allGoveeStates[ip].on = true;
            }}
            const dev = allDevices.find(d => d.device_id === selectedDeviceId);
            if (dev) renderInspector(dev);
        }} catch(e) {{
            console.error('Govee Temp Error:', e);
        }}
    }}

    function handleGoveeCustomColor(ip, hex) {{
        if (!hex || hex.length !== 7) return;
        const r = parseInt(hex.slice(1, 3), 16);
        const g = parseInt(hex.slice(3, 5), 16);
        const b = parseInt(hex.slice(5, 7), 16);
        setGoveeColor(ip, r, g, b);
    }}

    // Room Manager Modal Functions
    function openRoomModal() {{
        renderModalRooms();
        const m = document.getElementById('room-manager-modal');
        if (m) m.style.display = 'block';
    }}

    function closeRoomModal() {{
        const m = document.getElementById('room-manager-modal');
        if (m) m.style.display = 'none';
    }}

    function renderModalRooms() {{
        const tbody = document.getElementById('modal-rooms-tbody');
        const countEl = document.getElementById('modal-room-count');
        if (!tbody) return;

        countEl.innerText = allRooms.length;
        if (allRooms.length === 0) {{
            tbody.innerHTML = '<tr><td colspan="6" style="text-align:center; padding:20px; color:var(--muted);">Noch keine Räume definiert. Klicke oben auf "Aus Philips Hue importieren" oder lege einen Raum an.</td></tr>';
            return;
        }}

        tbody.innerHTML = allRooms.map(r => {{
            const devCount = allDevices.filter(d => (d.room || '').toLowerCase() === r.name.toLowerCase()).length;
            const matterTag = r.matter_tag || 'CommonSpace';
            const hueBadge = r.hue_group_id 
                ? `<span class="badge" style="background:#fef3c7; color:#92400e; font-size:10px;" title="Hue Gruppe #${{r.hue_group_id}}">💡 #${{r.hue_group_id}}</span>`
                : '<span style="color:var(--muted); font-size:10px;">—</span>';

            return `
                <tr style="border-bottom:1px solid var(--border);">
                    <td style="padding:8px 10px; font-weight:600;">
                        <span style="font-size:16px; margin-right:6px;">${{r.icon || '📍'}}</span>
                        <span>${{escapeHtml(r.name)}}</span>
                    </td>
                    <td style="padding:8px 10px; color:var(--muted);">${{escapeHtml(r.floor || 'Ohne Etage')}}</td>
                    <td style="padding:8px 10px;">
                        <span class="badge" style="background:#e0e7ff; color:#3730a3; font-size:10px; padding:2px 6px;" title="Matter Location Tag: ${{matterTag}}">✨ ${{escapeHtml(matterTag)}}</span>
                    </td>
                    <td style="padding:8px 10px;">${{hueBadge}}</td>
                    <td style="padding:8px 10px; text-align:center;">
                        <span class="badge" style="background:var(--badge-bg); color:var(--text); font-size:11px; font-weight:600;">${{devCount}}</span>
                    </td>
                    <td style="padding:8px 10px; text-align:right; white-space:nowrap;">
                        <button type="button" class="btn-sm" style="font-size:11px; padding:3px 8px; margin-right:4px; cursor:pointer;" onclick="editRoom('${{r.id}}')">✏️ Edit</button>
                        <button type="button" class="btn-sm" style="color:var(--status-red); border:1px solid var(--border); font-size:11px; padding:3px 6px; cursor:pointer;" onclick="deleteRoom('${{r.id}}', '${{escapeAttr(r.name)}}')">🗑️</button>
                    </td>
                </tr>
            `;
        }}).join('');
    }}

    function editRoom(roomId) {{
        const r = allRooms.find(item => item.id === roomId);
        if (!r) return;

        document.getElementById('rm-id').value = r.id;
        document.getElementById('rm-name').value = r.name;
        document.getElementById('rm-floor').value = r.floor || 'Erdgeschoss';
        document.getElementById('rm-archetype').value = r.archetype || 'other';
        document.getElementById('rm-icon').value = r.icon || '📍';
        document.getElementById('rm-matter').value = r.matter_tag || 'CommonSpace';
        document.getElementById('rm-hue-group-id').value = r.hue_group_id || '';
        document.getElementById('rm-hue-class').value = r.hue_class || '';

        const hueSyncWrap = document.getElementById('rm-hue-sync-wrapper');
        if (hueSyncWrap) {{
            hueSyncWrap.style.display = r.hue_group_id ? 'block' : 'none';
        }}

        document.getElementById('room-editor-title').innerText = '✏️ Raum bearbeiten: ' + r.name;
        document.getElementById('btn-save-room').innerText = '💾 Änderungen speichern';
        document.getElementById('btn-cancel-room-edit').style.display = 'inline-block';
        document.getElementById('rm-name').focus();
    }}

    function cancelEditRoom() {{
        document.getElementById('rm-id').value = '';
        document.getElementById('rm-name').value = '';
        document.getElementById('rm-floor').value = 'Erdgeschoss';
        document.getElementById('rm-archetype').value = 'living_room';
        document.getElementById('rm-icon').value = '🛋️';
        document.getElementById('rm-matter').value = 'LivingRoom';
        document.getElementById('rm-hue-group-id').value = '';
        document.getElementById('rm-hue-class').value = '';

        const hueSyncWrap = document.getElementById('rm-hue-sync-wrapper');
        if (hueSyncWrap) hueSyncWrap.style.display = 'none';

        document.getElementById('room-editor-title').innerText = '➕ Neuen Raum anlegen';
        document.getElementById('btn-save-room').innerText = '💾 Raum speichern';
        document.getElementById('btn-cancel-room-edit').style.display = 'none';
    }}

    function setRoomIcon(emoji) {{
        const iconInput = document.getElementById('rm-icon');
        if (iconInput) iconInput.value = emoji;
    }}

    async function importHueRooms() {{
        const btn = document.getElementById('btn-import-hue');
        const statusEl = document.getElementById('room-op-status');
        if (btn) btn.disabled = true;
        if (statusEl) {{
            statusEl.innerText = 'Importiere Räume & Geräte aus Hue...';
            statusEl.style.color = 'var(--muted)';
        }}

        try {{
            const res = await fetch('/api/rooms/import-hue', {{ method: 'POST' }});
            const json = await res.json();
            if (res.ok && json.status === 'ok') {{
                if (statusEl) {{
                    statusEl.innerText = json.message || 'Import erfolgreich!';
                    statusEl.style.color = 'var(--status-green)';
                }}
                setTimeout(() => {{
                    window.location.reload();
                }}, 1000);
            }} else {{
                if (statusEl) {{
                    statusEl.innerText = 'Fehler: ' + (json.message || 'Import fehlgeschlagen');
                    statusEl.style.color = 'var(--status-red)';
                }}
                if (btn) btn.disabled = false;
            }}
        }} catch (e) {{
            if (statusEl) {{
                statusEl.innerText = 'Netzwerkfehler: ' + e;
                statusEl.style.color = 'var(--status-red)';
            }}
            if (btn) btn.disabled = false;
        }}
    }}

    async function submitNewRoom(event) {{
        event.preventDefault();
        const id = document.getElementById('rm-id').value.trim();
        const name = document.getElementById('rm-name').value.trim();
        if (!name) return;
        const floor = document.getElementById('rm-floor').value;
        const archetype = document.getElementById('rm-archetype').value;
        const icon = document.getElementById('rm-icon').value.trim();
        const matter = document.getElementById('rm-matter').value.trim();
        const hueGroupId = document.getElementById('rm-hue-group-id').value.trim();
        const hueClass = document.getElementById('rm-hue-class').value.trim();
        const statusEl = document.getElementById('room-op-status');

        try {{
            const res = await fetch('/api/rooms', {{
                method: 'POST',
                headers: {{ 'Content-Type': 'application/json' }},
                body: JSON.stringify({{
                    id: id,
                    name: name,
                    floor: floor,
                    archetype: archetype,
                    icon: icon,
                    matter_tag: matter,
                    hue_group_id: hueGroupId || null,
                    hue_class: hueClass || null,
                }})
            }});
            if (res.ok) {{
                const data = await res.json();
                if (statusEl) {{
                    statusEl.innerText = 'Raum gespeichert!';
                    statusEl.style.color = 'var(--status-green)';
                }}
                setTimeout(() => {{
                    window.location.reload();
                }}, 500);
            }} else {{
                alert('Fehler beim Speichern des Raums');
            }}
        }} catch (e) {{
            alert('Netzwerkfehler: ' + e);
        }}
    }}

    async function deleteRoom(id, name) {{
        if (!confirm(`Möchtest du den Raum "${{name}}" wirklich löschen?`)) {{
            return;
        }}
        try {{
            const res = await fetch('/api/rooms/' + encodeURIComponent(id), {{ method: 'DELETE' }});
            if (res.ok) {{
                window.location.reload();
            }} else {{
                alert('Fehler beim Löschen des Raums');
            }}
        }} catch (e) {{
            alert('Netzwerkfehler: ' + e);
        }}
    }}

    const archetypeMap = {{
        'living_room': {{ icon: '🛋️', tag: 'LivingRoom', floor: 'Erdgeschoss' }},
        'kitchen': {{ icon: '🍳', tag: 'Kitchen', floor: 'Erdgeschoss' }},
        'dining_room': {{ icon: '🍽️', tag: 'DiningRoom', floor: 'Erdgeschoss' }},
        'bedroom': {{ icon: '🛏️', tag: 'Bedroom', floor: 'Obergeschoss' }},
        'kids_room': {{ icon: '🧸', tag: 'KidsRoom', floor: 'Obergeschoss' }},
        'bathroom': {{ icon: '🛁', tag: 'Bathroom', floor: 'Obergeschoss' }},
        'toilet': {{ icon: '🚽', tag: 'Bathroom', floor: 'Erdgeschoss' }},
        'office': {{ icon: '💼', tag: 'Office', floor: 'Obergeschoss' }},
        'hallway': {{ icon: '🚪', tag: 'Hallway', floor: 'Erdgeschoss' }},
        'stairs': {{ icon: '🪜', tag: 'Hallway', floor: 'Erdgeschoss' }},
        'basement': {{ icon: '📦', tag: 'Basement', floor: 'Keller' }},
        'outdoor': {{ icon: '🌳', tag: 'Outdoor', floor: 'Außenbereich' }},
        'garage': {{ icon: '🚗', tag: 'Garage', floor: 'Außenbereich' }},
        'spa': {{ icon: '🧖', tag: 'Bathroom', floor: 'Keller' }},
        'other': {{ icon: '📍', tag: 'CommonSpace', floor: 'Erdgeschoss' }}
    }};

    function onArchetypeSelect(arch) {{
        const info = archetypeMap[arch];
        if (info) {{
            const iconEl = document.getElementById('rm-icon');
            const matterEl = document.getElementById('rm-matter');
            const floorEl = document.getElementById('rm-floor');
            if (iconEl) iconEl.value = info.icon;
            if (matterEl) matterEl.value = info.tag;
            if (floorEl) floorEl.value = info.floor;
        }}
    }}

    function onRoomNameInput(val) {{
        val = (val || '').toLowerCase();
        let detected = null;
        if (val.includes('wohn')) detected = 'living_room';
        else if (val.includes('küh') || val.includes('kueh')) detected = 'kitchen';
        else if (val.includes('ess')) detected = 'dining_room';
        else if (val.includes('schlaf')) detected = 'bedroom';
        else if (val.includes('kinder') || val.includes('tom') || val.includes('sophie')) detected = 'kids_room';
        else if (val.includes('bad') || val.includes('bath')) detected = 'bathroom';
        else if (val.includes('wc') || val.includes('toilet')) detected = 'toilet';
        else if (val.includes('büro') || val.includes('buero') || val.includes('arbeit')) detected = 'office';
        else if (val.includes('flur') || val.includes('windfang') || val.includes('diele')) detected = 'hallway';
        else if (val.includes('keller') || val.includes('ug')) detected = 'basement';
        else if (val.includes('garten') || val.includes('terrasse') || val.includes('balkon') || val.includes('vordach')) detected = 'outdoor';
        else if (val.includes('garage') || val.includes('carport')) detected = 'garage';
        else if (val.includes('sauna') || val.includes('wellness')) detected = 'spa';

        if (detected) {{
            const archEl = document.getElementById('rm-archetype');
            if (archEl) {{
                archEl.value = detected;
                onArchetypeSelect(detected);
            }}
        }}
    }}

    // Initial load: select device from hash or first available device
    window.addEventListener('DOMContentLoaded', () => {{
        const hash = (window.location.hash || '').replace('#', '');
        if (hash && allDevices.some(d => d.device_id === hash)) {{
            selectDevice(hash);
        }} else if (allDevices.length > 0) {{
            selectDevice(allDevices[0].device_id);
        }}
    }});
    </script>
    "#,
        client_devices_json,
        all_products_str,
        client_rooms_json,
        client_govee_json
    );

    let content = format!(
        r#"
        <div class="toolbar">
            <h2>Detected Devices ({})</h2>
            <div style="display:flex;gap:10px;align-items:center;">
                <input type="text" id="device-search" placeholder="Search devices (name, IP, MAC)..." class="search-input" oninput="filterDevices()" />
                <form action="/scan" method="POST" id="scan-form" style="margin:0;">
                    <button type="submit" id="scan-btn" class="btn btn-primary">
                        <span>🔄</span> <span id="scan-label">Scan Network Now</span>
                    </button>
                </form>
            </div>
        </div>
        <div class="status-pills" style="display:flex; flex-wrap:wrap; gap:8px; align-items:center; margin-bottom:12px;">
            <span style="font-size:12px; font-weight:600; color:var(--muted); margin-right:4px;">Status:</span>
            <button type="button" class="status-pill active" id="filter-status-all" onclick="setStatusFilter('all', this)">All ({})</button>
            <button type="button" class="status-pill" id="filter-status-active" onclick="setStatusFilter('active', this)">🟢 Active ({})</button>
            <button type="button" class="status-pill" id="filter-status-inactive" onclick="setStatusFilter('inactive', this)" title="Geräte, die HomeNode Server zuvor aktiv erkannt hat, aktuell offline">⚪ Former ({})</button>
            <button type="button" class="status-pill" id="filter-status-archive" onclick="setStatusFilter('archive', this)" title="Reine inaktive Alt-Leases / Geräte im FRITZ!Box Router-Archiv">📦 Router Archive ({})</button>
            <button type="button" class="status-pill" id="filter-status-ignored" onclick="setStatusFilter('ignored', this)" title="Ignorierte Nachbargeräte & blockierte BLE-Sender" style="color:var(--status-red);">🚫 Ignored ({})</button>

            <span style="font-size:12px; font-weight:600; color:var(--muted); margin-left:12px; margin-right:4px;">Interfaces:</span>
            <select id="scanner-filter" onchange="setScannerFilter(this.value)" style="background:var(--bg-card); color:var(--text); border:1px solid var(--border); border-radius:6px; font-size:12px; padding:3px 8px; cursor:pointer;">
                <option value="all">Alle Interfaces</option>
                <option value="arp">📡 ARP</option>
                <option value="ping">⚡ Ping / ICMP</option>
                <option value="mdns">🔍 mDNS / Bonjour</option>
                <option value="ssdp">🌐 SSDP / UPnP</option>
                <option value="fritzbox-tr064">🔀 FRITZ!Box TR-064</option>
                <option value="matter">✨ Matter</option>
                <option value="bthome">📶 BTHome (BLE Sensors & Buttons)</option>
                <option value="shelly-gateway">📡 Shelly BLE Gateways</option>
                <option value="philips-hue">💡 Philips Hue</option>
                <option value="mobile-scout">📱 mHomeNode Mobile Scout</option>
            </select>

            <span style="font-size:12px; font-weight:600; color:var(--muted); margin-left:12px; margin-right:4px;">Raum:</span>
            <select id="room-filter" onchange="setRoomFilter(this.value)" style="background:var(--bg-card); color:var(--text); border:1px solid var(--border); border-radius:6px; font-size:12px; padding:3px 8px; cursor:pointer;">
                {}
            </select>

            <span style="font-size:12px; font-weight:600; color:var(--muted); margin-left:8px; margin-right:4px;">Etage:</span>
            <select id="floor-filter" onchange="setFloorFilter(this.value)" style="background:var(--bg-card); color:var(--text); border:1px solid var(--border); border-radius:6px; font-size:12px; padding:3px 8px; cursor:pointer;">
                <option value="all">Alle Etagen</option>
                <option value="Dachgeschoss">Dachgeschoss</option>
                <option value="Obergeschoss">Obergeschoss</option>
                <option value="Erdgeschoss">Erdgeschoss</option>
                <option value="Keller">Keller</option>
                <option value="Außenbereich">Außenbereich</option>
            </select>

            <button type="button" class="btn-sm" style="margin-left:8px; font-size:11px; cursor:pointer; background:var(--surface); border:1px solid var(--border);" onclick="openRoomModal()">🚪 Räume verwalten</button>
        </div>
        <div class="pills">{}</div>

        <div style="display:flex; justify-content:space-between; align-items:center; margin-bottom:14px; padding:8px 14px; background:var(--surface); border:1px solid var(--border); border-radius:8px; font-size:12px;">
            <div style="display:flex; align-items:center; gap:8px;">
                <span>💡</span>
                <span style="color:var(--muted);">Echtzeit BLE-Events, Raumanwesenheit und Stromfluss findest du auf dem <a href="/" style="color:var(--primary); font-weight:600; text-decoration:none;">🏠 Dashboard</a>.</span>
            </div>
            <a href="/" class="btn btn-sm" style="font-size:11px; text-decoration:none; padding:3px 10px;">Zum Dashboard ↗</a>
        </div>
        
        <div class="workspace-grid">
            <!-- Left Inspector Panel -->
            <div class="inspector-panel" id="inspector-panel">
                <div id="inspector-content">
                    <div class="inspector-empty">
                        <span style="font-size:32px;">👈</span>
                        <h4 style="margin-top:10px;">Select a Device</h4>
                        <p style="font-size:12px; margin-top:6px;">Click on any device in the list to inspect its network interfaces, edit documentation, scan open ports, or link dual-homed connections.</p>
                    </div>
                </div>
            </div>

            <!-- Right Device List -->
            <div id="device-list-container">
                {}
            </div>
        </div>

        <!-- Room Manager Modal -->
        <div id="room-manager-modal" style="display:none; position:fixed; top:0; left:0; width:100%; height:100%; background:rgba(0,0,0,0.55); z-index:9999; overflow-y:auto; backdrop-filter:blur(2px);">
            <div class="card" style="max-width:760px; margin:40px auto; padding:24px; box-shadow:0 12px 35px rgba(0,0,0,0.3);">
                <div style="display:flex; justify-content:space-between; align-items:center; margin-bottom:16px; border-bottom:1px solid var(--border); padding-bottom:12px;">
                    <div>
                        <h3 style="margin:0; display:flex; align-items:center; gap:8px;">
                            <span>🚪</span> <span>Raum- & Standortverwaltung</span>
                        </h3>
                        <div style="font-size:11px; color:var(--muted); margin-top:3px;">
                            Strukturierte Räume & Etagen • Synchronisierbar mit Apple Home (HMRoom/HMFloor) & Matter 1.3+
                        </div>
                    </div>
                    <button type="button" class="btn-sm" onclick="closeRoomModal()" style="font-size:14px; font-weight:bold; cursor:pointer;">✕</button>
                </div>

                <div style="display:flex; justify-content:space-between; align-items:center; margin-bottom:16px; background:var(--surface); padding:10px 14px; border-radius:8px; border:1px solid var(--border);">
                    <div>
                        <div style="font-weight:600; font-size:12px;">💡 Philips Hue Bridge Räume</div>
                        <div style="font-size:11px; color:var(--muted);">Importiert alle konfigurierten Räume & ordnet Leuchten/Sensoren automatisch zu.</div>
                    </div>
                    <button type="button" id="btn-import-hue" class="btn btn-sm btn-primary" onclick="importHueRooms()">
                        <span>🔄</span> <span>Aus Philips Hue importieren</span>
                    </button>
                </div>

                <!-- Room Editor Form (Create or Edit) -->
                <div style="margin-bottom:16px; background:var(--bg); border:1px solid var(--border); border-radius:8px; padding:14px;">
                    <div style="display:flex; justify-content:space-between; align-items:center; margin-bottom:10px;">
                        <span id="room-editor-title" style="font-size:13px; font-weight:700; color:var(--text);">➕ Neuen Raum anlegen</span>
                        <button type="button" id="btn-cancel-room-edit" class="btn-sm" style="display:none; font-size:11px; cursor:pointer;" onclick="cancelEditRoom()">Abbrechen</button>
                    </div>
                    <form id="add-room-form" onsubmit="submitNewRoom(event)">
                        <input type="hidden" id="rm-id" value="" />
                        <input type="hidden" id="rm-hue-group-id" value="" />
                        <input type="hidden" id="rm-hue-class" value="" />

                        <div style="display:grid; grid-template-columns:1fr 1fr; gap:10px; margin-bottom:10px;">
                            <div>
                                <label style="font-size:11px; font-weight:600; color:var(--muted);">Raumname *</label>
                                <input type="text" id="rm-name" class="form-control" required placeholder="z.B. Wohnzimmer, Büro..." oninput="onRoomNameInput(this.value)" />
                            </div>
                            <div>
                                <label style="font-size:11px; font-weight:600; color:var(--muted);">Etage (Floor)</label>
                                <select id="rm-floor" class="form-control">
                                    <option value="Dachgeschoss">Dachgeschoss</option>
                                    <option value="Obergeschoss">Obergeschoss</option>
                                    <option value="Erdgeschoss" selected>Erdgeschoss</option>
                                    <option value="Keller">Keller</option>
                                    <option value="Außenbereich">Außenbereich</option>
                                    <option value="Sonstige">Sonstige</option>
                                </select>
                            </div>
                        </div>
                        <div style="display:grid; grid-template-columns:1fr 1fr 1fr; gap:10px; margin-bottom:12px;">
                            <div>
                                <label style="font-size:11px; font-weight:600; color:var(--muted);">Archetyp / Raumtyp</label>
                                <select id="rm-archetype" class="form-control" onchange="onArchetypeSelect(this.value)">
                                    <option value="living_room">Wohnzimmer (Living Room)</option>
                                    <option value="kitchen">Küche (Kitchen)</option>
                                    <option value="dining_room">Esszimmer (Dining)</option>
                                    <option value="bedroom">Schlafzimmer (Bedroom)</option>
                                    <option value="kids_room">Kinderzimmer (Kids)</option>
                                    <option value="bathroom">Badezimmer (Bathroom)</option>
                                    <option value="toilet">Toilette / Gäste-WC</option>
                                    <option value="office">Büro / Arbeitszimmer (Office)</option>
                                    <option value="hallway">Flur / Diele (Hallway)</option>
                                    <option value="stairs">Treppenhaus (Stairs)</option>
                                    <option value="basement">Keller / Lager (Basement)</option>
                                    <option value="outdoor">Außen / Garten / Terrasse</option>
                                    <option value="garage">Garage / Carport</option>
                                    <option value="spa">Sauna / Spa</option>
                                    <option value="other">Sonstiger Raum</option>
                                </select>
                            </div>
                            <div>
                                <label style="font-size:11px; font-weight:600; color:var(--muted);">Icon (Emoji)</label>
                                <div style="display:flex; gap:6px; align-items:center;">
                                    <input type="text" id="rm-icon" class="form-control" style="width:50px; text-align:center; font-size:16px;" value="🛋️" />
                                    <div style="display:flex; gap:4px; flex-wrap:wrap;">
                                        <span style="cursor:pointer; font-size:14px;" onclick="setRoomIcon('🛋️')" title="Wohnzimmer">🛋️</span>
                                        <span style="cursor:pointer; font-size:14px;" onclick="setRoomIcon('🍳')" title="Küche">🍳</span>
                                        <span style="cursor:pointer; font-size:14px;" onclick="setRoomIcon('🛏️')" title="Schlafzimmer">🛏️</span>
                                        <span style="cursor:pointer; font-size:14px;" onclick="setRoomIcon('🛁')" title="Bad">🛁</span>
                                        <span style="cursor:pointer; font-size:14px;" onclick="setRoomIcon('💼')" title="Büro">💼</span>
                                        <span style="cursor:pointer; font-size:14px;" onclick="setRoomIcon('🚪')" title="Flur">🚪</span>
                                        <span style="cursor:pointer; font-size:14px;" onclick="setRoomIcon('📦')" title="Keller">📦</span>
                                        <span style="cursor:pointer; font-size:14px;" onclick="setRoomIcon('🌳')" title="Garten">🌳</span>
                                    </div>
                                </div>
                            </div>
                            <div>
                                <label style="font-size:11px; font-weight:600; color:var(--muted);">Matter Area Tag</label>
                                <input type="text" id="rm-matter" class="form-control" value="LivingRoom" />
                            </div>
                        </div>

                        <div id="rm-hue-sync-wrapper" style="display:none; margin-bottom:12px; background:rgba(245,158,11,0.08); border:1px solid rgba(245,158,11,0.25); border-radius:6px; padding:8px 10px; font-size:11px;">
                            <label style="display:flex; align-items:center; gap:8px; cursor:pointer;">
                                <input type="checkbox" id="rm-sync-hue" checked />
                                <span>💡 Änderung (Name & Typ) auch direkt an die Philips Hue Bridge übertragen</span>
                            </label>
                        </div>

                        <div style="display:flex; justify-content:flex-end; gap:8px;">
                            <button type="submit" class="btn btn-sm btn-primary" id="btn-save-room">💾 Raum speichern</button>
                        </div>
                    </form>
                </div>

                <!-- Existing Rooms Table -->
                <div style="font-weight:600; font-size:12px; margin-bottom:8px; display:flex; justify-content:space-between; align-items:center;">
                    <span>Vorhandene Räume (<span id="modal-room-count">0</span>)</span>
                    <span id="room-op-status" style="font-size:11px; font-weight:normal;"></span>
                </div>
                <div style="max-height:360px; overflow-y:auto; border:1px solid var(--border); border-radius:6px;">
                    <table style="width:100%; border-collapse:collapse; font-size:12px;">
                        <thead>
                            <tr style="background:var(--surface); border-bottom:1px solid var(--border); text-align:left;">
                                <th style="padding:8px 10px;">Raum</th>
                                <th style="padding:8px 10px;">Etage</th>
                                <th style="padding:8px 10px;">Matter Tag</th>
                                <th style="padding:8px 10px;">Hue Bridge</th>
                                <th style="padding:8px 10px; text-align:center;">Geräte</th>
                                <th style="padding:8px 10px; text-align:right;">Aktionen</th>
                            </tr>
                        </thead>
                        <tbody id="modal-rooms-tbody">
                            <!-- Injected via JavaScript -->
                        </tbody>
                    </table>
                </div>

                <div style="display:flex; justify-content:flex-end; margin-top:16px;">
                    <button type="button" class="btn" style="background:var(--badge-bg); color:var(--text);" onclick="closeRoomModal()">Schließen</button>
                </div>
            </div>
        </div>

        {}"#,
        unified_devices.len(),
        total_count,
        active_count,
        former_count,
        archive_count,
        ignored_count,
        room_filter_options,
        pills_html,
        group_cards_html,
        script
    );

    page_layout(title, "devices", &content)
}

#[allow(dead_code)]
fn escape_html_str(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn render_dashboard_page(
    title: &str,
    snapshot: &RuntimeSnapshot,
    _docs: &HashMap<String, DeviceDocumentation>,
    _links: &HashMap<String, Vec<String>>,
    _verified_gateways: &[VerifiedShellyGateway],
) -> String {
    let total_devices = snapshot.devices.len();
    let active_devices = snapshot
        .devices
        .iter()
        .filter(|d| d.metadata.get("status").map(|s| s.as_str()).unwrap_or("active") == "active")
        .count();

    let content = format!(
        r#"
        <div style="margin-bottom:20px; display:flex; justify-content:space-between; align-items:center; flex-wrap:wrap; gap:12px;">
            <div>
                <h2 style="font-size:22px; font-weight:700; display:flex; align-items:center; gap:8px;">
                    <span>🏠 HomeNode Live Dashboard</span>
                    <span class="badge" style="background:#dcfce7; color:#166534; font-size:11px; font-weight:600; padding:3px 8px; border-radius:12px;">
                        <span class="status-dot status-ready" style="width:8px; height:8px; margin-right:4px;"></span>Live System Active
                    </span>
                </h2>
                <div style="font-size:12px; color:var(--muted); margin-top:3px;">
                    Zentrale Übersicht: Echtzeit-Energiefluss, BTHome BLE-Sensoren und Live-Ereignisse
                </div>
            </div>
            <div style="display:flex; gap:8px; align-items:center;">
                <span id="dash-last-updated" style="font-size:11px; color:var(--muted);">Connecting...</span>
                <form action="/scan" method="POST" style="margin:0;">
                    <button type="submit" class="btn btn-sm btn-primary">🔄 Scan Network</button>
                </form>
            </div>
        </div>

        <!-- Energy KPI Cards Grid -->
        <div style="display:grid; grid-template-columns:repeat(auto-fit, minmax(240px, 1fr)); gap:16px; margin-bottom:20px;">
            <!-- ☀️ Solar Generation -->
            <div class="card" style="margin-bottom:0; border-top:4px solid #f59e0b;">
                <div style="display:flex; justify-content:space-between; align-items:center; margin-bottom:8px;">
                    <span style="font-weight:600; font-size:12px; color:var(--muted);">☀️ SOLAR-ERZEUGUNG</span>
                    <span class="badge" style="font-size:10px;">Fronius</span>
                </div>
                <div style="display:flex; align-items:baseline; gap:6px; margin-bottom:10px;">
                    <span id="dash-solar-power-val" style="font-size:30px; font-weight:800; color:#d97706; font-family:monospace;">--</span>
                    <span style="font-size:15px; font-weight:600; color:var(--muted);">W</span>
                </div>
                <div style="border-top:1px solid var(--border); padding-top:8px; display:flex; justify-content:space-between; font-size:11px; color:var(--muted);">
                    <span>Heute: <strong id="dash-solar-day-val" style="color:var(--text);">-- kWh</strong></span>
                    <span>Jahr: <strong id="dash-solar-year-val" style="color:var(--text);">-- kWh</strong></span>
                </div>
            </div>

            <!-- 🌐 Grid Net Flow -->
            <div class="card" style="margin-bottom:0; border-top:4px solid #2563eb;">
                <div style="display:flex; justify-content:space-between; align-items:center; margin-bottom:8px;">
                    <span style="font-weight:600; font-size:12px; color:var(--muted);">🌐 NETZBEZUG / EINSPEISUNG</span>
                    <span class="badge" style="font-size:10px;">Shelly Pro 3EM</span>
                </div>
                <div style="display:flex; align-items:baseline; gap:6px; margin-bottom:10px;">
                    <span id="dash-grid-power-val" style="font-size:30px; font-weight:800; font-family:monospace;">--</span>
                    <span style="font-size:15px; font-weight:600; color:var(--muted);">W</span>
                    <span id="dash-grid-dir-badge" class="badge" style="margin-left:auto; font-size:10px; font-weight:600;">--</span>
                </div>
                <div style="border-top:1px solid var(--border); padding-top:8px; display:flex; justify-content:space-between; font-size:11px; color:var(--muted);">
                    <span>Import: <strong id="dash-grid-import-val" style="color:var(--text);">-- kWh</strong></span>
                    <span>Export: <strong id="dash-grid-export-val" style="color:var(--text);">-- kWh</strong></span>
                </div>
            </div>

            <!-- 🏠 House Load -->
            <div class="card" style="margin-bottom:0; border-top:4px solid #10b981;">
                <div style="display:flex; justify-content:space-between; align-items:center; margin-bottom:8px;">
                    <span style="font-weight:600; font-size:12px; color:var(--muted);">🏠 HAUSVERBRAUCH</span>
                    <span class="badge" style="font-size:10px;">Berechnet</span>
                </div>
                <div style="display:flex; align-items:baseline; gap:6px; margin-bottom:10px;">
                    <span id="dash-house-power-val" style="font-size:30px; font-weight:800; color:#059669; font-family:monospace;">--</span>
                    <span style="font-size:15px; font-weight:600; color:var(--muted);">W</span>
                </div>
                <div style="border-top:1px solid var(--border); padding-top:8px; display:flex; justify-content:space-between; font-size:11px; color:var(--muted);">
                    <span>Autarkie: <strong id="dash-autarky-val" style="color:var(--status-green);">-- %</strong></span>
                    <span>Eigenverbrauch: <strong id="dash-self-val" style="color:var(--text);">-- %</strong></span>
                </div>
            </div>

            <!-- 🔋 Balcony Storage -->
            <div class="card" style="margin-bottom:0; border-top:4px solid #8b5cf6;">
                <div style="display:flex; justify-content:space-between; align-items:center; margin-bottom:8px;">
                    <span style="font-weight:600; font-size:12px; color:var(--muted);">🔋 SPEICHER & BALKON</span>
                    <span class="badge" style="font-size:10px;">EcoFlow</span>
                </div>
                <div style="margin-bottom:10px;">
                    <div style="font-size:13px; font-weight:700;">2 Wechselrichter im LAN</div>
                    <div style="font-size:11px; color:var(--muted); margin-top:2px;">ecoflow1 (.96) & ecoflow2 (.105)</div>
                </div>
                <div style="border-top:1px solid var(--border); padding-top:8px; font-size:11px; color:var(--muted); display:flex; justify-content:space-between;">
                    <span>Status: <strong style="color:var(--status-green);">Verbunden</strong></span>
                    <a href="/energy" style="color:var(--primary); text-decoration:none; font-weight:600;">Details ↗</a>
                </div>
            </div>
        </div>

        <!-- Power Flow Diagram (Compact) -->
        <div class="card" style="padding:16px 20px; margin-bottom:20px;">
            <div style="display:flex; justify-content:space-between; align-items:center; margin-bottom:12px;">
                <h3 style="font-size:14px; font-weight:600;">⚡ Live Energiefluss</h3>
                <a href="/energy" style="font-size:11px; color:var(--primary); text-decoration:none; font-weight:600;">Ausführliche 3-Phasen Analyse ↗</a>
            </div>
            <div style="display:flex; justify-content:space-around; align-items:center; flex-wrap:wrap; gap:14px;">
                <div style="text-align:center; min-width:120px; padding:12px; background:var(--bg); border:2px solid #f59e0b; border-radius:10px;">
                    <div style="font-size:26px;">☀️</div>
                    <div style="font-weight:700; font-size:12px; margin-top:2px;">Solar PV</div>
                    <div id="dash-flow-solar" style="font-weight:700; font-size:15px; color:#d97706; margin-top:1px;">-- W</div>
                </div>

                <div style="display:flex; flex-direction:column; align-items:center; min-width:70px;">
                    <span style="font-size:18px;">➡️</span>
                    <span id="dash-flow-solar-text" style="font-size:10px; font-weight:600; color:var(--muted);">-- W</span>
                </div>

                <div style="text-align:center; min-width:140px; padding:14px; background:var(--bg); border:2px solid #10b981; border-radius:10px; box-shadow:0 2px 6px rgba(0,0,0,0.04);">
                    <div style="font-size:28px;">🏠</div>
                    <div style="font-weight:700; font-size:13px; margin-top:2px;">Hausverbrauch</div>
                    <div id="dash-flow-house" style="font-weight:800; font-size:18px; color:#059669; margin-top:1px;">-- W</div>
                </div>

                <div style="display:flex; flex-direction:column; align-items:center; min-width:70px;">
                    <span style="font-size:18px;" id="dash-arrow-grid">⬅️</span>
                    <span id="dash-flow-grid-text" style="font-size:10px; font-weight:600; color:var(--muted);">-- W</span>
                </div>

                <div style="text-align:center; min-width:120px; padding:12px; background:var(--bg); border:2px solid #2563eb; border-radius:10px;">
                    <div style="font-size:26px;">🌐</div>
                    <div style="font-weight:700; font-size:12px; margin-top:2px;">Stromnetz</div>
                    <div id="dash-flow-grid" style="font-weight:700; font-size:15px; margin-top:1px;">-- W</div>
                    <div id="dash-flow-grid-dir" style="font-size:10px; color:var(--muted);">--</div>
                </div>
            </div>
        </div>

        <!-- BLE Sensors & Live Event Log -->
        <div style="display:grid; grid-template-columns: 320px 1fr; gap:20px; align-items:start; margin-bottom:20px;">
            <!-- Left column: Active BLE Sensors -->
            <div>
                <div class="card" style="margin-bottom:0;">
                    <div style="display:flex; justify-content:space-between; align-items:center; margin-bottom:10px;">
                        <h3 style="font-size:14px; font-weight:700; display:flex; align-items:center; gap:6px; margin:0;">
                            <span>🔘 BLE Sensoren & Taster</span>
                        </h3>
                        <a href="/devices" style="font-size:11px; color:var(--primary); text-decoration:none;">Alle Geräte ↗</a>
                    </div>
                    <div style="font-size:11px; color:var(--muted); margin-bottom:12px;">
                        Erkannte BTHome Taster & Sensoren mit aktuellem Batteriestand und Standort.
                    </div>
                    <div id="dash-sensor-chips" style="display:flex; flex-direction:column; gap:8px;">
                        <span style="font-size:11px; color:var(--muted); text-align:center; padding:16px 0;">Warte auf Sensor-Meldungen...</span>
                    </div>
                </div>
            </div>

            <!-- Right column: Live Event Feed -->
            <div class="card" style="margin-bottom:0; border-left:4px solid #2563eb; min-height:360px;">
                <div style="display:flex; justify-content:space-between; align-items:center; flex-wrap:wrap; gap:8px; margin-bottom:12px; border-bottom:1px solid var(--border); padding-bottom:10px;">
                    <div>
                        <div style="font-weight:700; font-size:14px; display:flex; align-items:center; gap:6px;">
                            <span>⚡ Echtzeit Event-Log & Raum-Tracking</span>
                            <span id="dash-event-count" class="badge" style="background:#2563eb; color:#fff; font-size:10px; padding:1px 6px; border-radius:10px;">0 Events</span>
                        </div>
                        <div style="font-size:11px; color:var(--muted); margin-top:2px;">
                            Live-Meldungen von Buttons, Sensoren und Gateways inklusive Raumerkennung
                        </div>
                    </div>
                    <div style="display:flex; gap:6px; align-items:center;">
                        <button type="button" class="btn btn-sm" onclick="clearDashboardEvents()" style="background:var(--bg); border:1px solid var(--border); font-size:11px;">Clear Log</button>
                    </div>
                </div>

                <!-- Event feed items list -->
                <div id="dash-log-container" style="max-height:420px; overflow-y:auto; background:var(--code-bg); border:1px solid var(--border); border-radius:8px; padding:8px 12px; font-family:monospace; font-size:11px; line-height:1.7;">
                    <div id="dash-log-empty" style="color:var(--muted); text-align:center; padding:30px 0; font-size:12px;">
                        Warte auf BLE Events (drücke z.B. den Shelly BLU Button)...
                    </div>
                    <div id="dash-log-items"></div>
                </div>
            </div>
        </div>
                <div style="display:flex; justify-content:space-between; align-items:center; flex-wrap:wrap; gap:8px; margin-bottom:12px; border-bottom:1px solid var(--border); padding-bottom:10px;">
                    <div>
                        <div style="font-weight:700; font-size:14px; display:flex; align-items:center; gap:6px;">
                            <span>⚡ Echtzeit Event-Log & Raum-Tracking</span>
                            <span id="dash-event-count" class="badge" style="background:#2563eb; color:#fff; font-size:10px; padding:1px 6px; border-radius:10px;">0 Events</span>
                        </div>
                        <div style="font-size:11px; color:var(--muted); margin-top:2px;">
                            Live-Meldungen von Buttons, Sensoren und Gateways inklusive Raumerkennung
                        </div>
                    </div>
                    <div style="display:flex; gap:6px; align-items:center;">
                        <button type="button" class="btn btn-sm" onclick="clearDashboardEvents()" style="background:var(--bg); border:1px solid var(--border); font-size:11px;">Clear Log</button>
                    </div>
                </div>

                <!-- Event feed items list -->
                <div id="dash-log-container" style="max-height:420px; overflow-y:auto; background:var(--code-bg); border:1px solid var(--border); border-radius:8px; padding:8px 12px; font-family:monospace; font-size:11px; line-height:1.7;">
                    <div id="dash-log-empty" style="color:var(--muted); text-align:center; padding:30px 0; font-size:12px;">
                        Warte auf BLE Events (drücke z.B. den Shelly BLU Button)...
                    </div>
                    <div id="dash-log-items"></div>
                </div>
            </div>
        </div>

        <!-- Bottom Navigation Quick Jump -->
        <div style="display:grid; grid-template-columns:repeat(auto-fit, minmax(220px, 1fr)); gap:14px;">
            <a href="/devices" class="card" style="text-decoration:none; color:inherit; margin-bottom:0; transition:border-color 0.15s; display:flex; align-items:center; gap:12px;">
                <span style="font-size:24px;">📱</span>
                <div>
                    <div style="font-weight:700; font-size:13px; color:var(--primary);">Geräteinventar ({} Geräte)</div>
                    <div style="font-size:11px; color:var(--muted);">Filter, Dokumentation & Port-Scanner ({} aktiv)</div>
                </div>
            </a>
            <a href="/energy" class="card" style="text-decoration:none; color:inherit; margin-bottom:0; transition:border-color 0.15s; display:flex; align-items:center; gap:12px;">
                <span style="font-size:24px;">⚡</span>
                <div>
                    <div style="font-weight:700; font-size:13px; color:var(--primary);">Energie & 3-Phasen</div>
                    <div style="font-size:11px; color:var(--muted);">Fronius Inverter & Shelly Pro 3EM Phasen</div>
                </div>
            </a>
            <a href="/matter" class="card" style="text-decoration:none; color:inherit; margin-bottom:0; transition:border-color 0.15s; display:flex; align-items:center; gap:12px;">
                <span style="font-size:24px;">✨</span>
                <div>
                    <div style="font-weight:700; font-size:13px; color:var(--primary);">Matter Fabrics</div>
                    <div style="font-size:11px; color:var(--muted);">Apple Home, Google, Alexa & Multi-Admin</div>
                </div>
            </a>
            <a href="/catalog" class="card" style="text-decoration:none; color:inherit; margin-bottom:0; transition:border-color 0.15s; display:flex; align-items:center; gap:12px;">
                <span style="font-size:24px;">🏢</span>
                <div>
                    <div style="font-weight:700; font-size:13px; color:var(--primary);">Hardware Katalog</div>
                    <div style="font-size:11px; color:var(--muted);">Modelle, Handbücher & Rhai-Treiber</div>
                </div>
            </a>
        </div>

        <script>
        function escapeHtml(text) {{
            return (text || '').replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
        }}
        function escapeAttr(text) {{
            return (text || '').replace(/"/g, "&quot;").replace(/'/g, "&#39;");
        }}

        function clearDashboardEvents() {{
            const items = document.getElementById('dash-log-items');
            if (items) items.innerHTML = '';
            const empty = document.getElementById('dash-log-empty');
            if (empty) empty.style.display = 'block';
            const count = document.getElementById('dash-event-count');
            if (count) count.innerText = '0 Events';
        }}

        async function fetchDashboardEnergy() {{
            try {{
                const res = await fetch('/api/energy/live');
                if (!res.ok) return;
                const data = await res.json();

                document.getElementById('dash-solar-power-val').innerText = Math.round(data.solar_power_w).toLocaleString();
                document.getElementById('dash-solar-day-val').innerText = (data.solar_day_kwh || 0).toFixed(2) + ' kWh';
                document.getElementById('dash-solar-year-val').innerText = Math.round(data.solar_year_kwh || 0).toLocaleString() + ' kWh';

                const gridVal = Math.round(data.grid_power_w);
                const absGrid = Math.abs(gridVal);
                document.getElementById('dash-grid-power-val').innerText = absGrid.toLocaleString();
                const gridBadge = document.getElementById('dash-grid-dir-badge');
                if (gridVal < -1) {{
                    gridBadge.innerText = '🟢 EINSPEISUNG';
                    gridBadge.style.background = '#dcfce7';
                    gridBadge.style.color = '#166534';
                }} else {{
                    gridBadge.innerText = '🟠 NETZBEZUG';
                    gridBadge.style.background = '#ffedd5';
                    gridBadge.style.color = '#9a3412';
                }}
                document.getElementById('dash-grid-import-val').innerText = (data.grid_import_kwh || 0).toLocaleString() + ' kWh';
                document.getElementById('dash-grid-export-val').innerText = (data.grid_export_kwh || 0).toLocaleString() + ' kWh';

                document.getElementById('dash-house-power-val').innerText = Math.round(data.house_consumption_w).toLocaleString();
                document.getElementById('dash-autarky-val').innerText = (data.autarky_pct || 0).toFixed(1) + ' %';
                document.getElementById('dash-self-val').innerText = (data.self_consumption_pct || 0).toFixed(1) + ' %';

                document.getElementById('dash-flow-solar').innerText = Math.round(data.solar_power_w).toLocaleString() + ' W';
                document.getElementById('dash-flow-solar-text').innerText = Math.round(data.solar_power_w) + ' W';
                document.getElementById('dash-flow-house').innerText = Math.round(data.house_consumption_w).toLocaleString() + ' W';
                document.getElementById('dash-flow-grid').innerText = absGrid.toLocaleString() + ' W';
                document.getElementById('dash-flow-grid-dir').innerText = (gridVal < -1) ? 'Einspeisung ins Netz' : 'Netzbezug';

                if (gridVal < -1) {{
                    document.getElementById('dash-arrow-grid').innerText = '➡️';
                    document.getElementById('dash-flow-grid-text').innerText = 'Export ' + absGrid + ' W';
                }} else {{
                    document.getElementById('dash-arrow-grid').innerText = '⬅️';
                    document.getElementById('dash-flow-grid-text').innerText = 'Import ' + absGrid + ' W';
                }}

                const updatedEl = document.getElementById('dash-last-updated');
                if (updatedEl) updatedEl.innerText = 'Aktualisiert: ' + new Date().toLocaleTimeString();
            }} catch (e) {{
                console.debug('Dashboard energy error:', e);
            }}
        }}

        async function fetchDashboardEvents() {{
            try {{
                const res = await fetch('/api/events');
                if (!res.ok) return;
                const events = await res.json();
                if (!Array.isArray(events)) return;

                const countEl = document.getElementById('dash-event-count');
                if (countEl) countEl.innerText = `${{events.length}} Event${{events.length === 1 ? '' : 's'}}`;

                const container = document.getElementById('dash-log-items');
                const emptyEl = document.getElementById('dash-log-empty');
                if (container) {{
                    if (events.length === 0) {{
                        if (emptyEl) emptyEl.style.display = 'block';
                        container.innerHTML = '';
                    }} else {{
                        if (emptyEl) emptyEl.style.display = 'none';
                        container.innerHTML = events.slice(0, 40).map(ev => {{
                            const time = ev.timestamp ? new Date(ev.timestamp).toLocaleTimeString() : '';
                            let badgeBg = '#2563eb';
                            if (ev.event_type.includes('press') || ev.event_type.includes('push')) badgeBg = '#d97706';
                            if (ev.event_type.includes('door') || ev.event_type.includes('open')) badgeBg = '#059669';
                            if (ev.event_type.includes('battery')) badgeBg = '#10b981';

                            const gwLabel = ev.gateway_name || ev.gateway_ip || ev.gateway || 'Shelly Gateway';
                            let locationBadge = '';
                            if (ev.room) {{
                                locationBadge = `<span class="badge" style="background:#fef3c7; color:#92400e; font-size:10px; font-weight:700; padding:1px 6px; border-radius:4px; display:inline-flex; align-items:center; gap:3px;">📍 ${{escapeHtml(ev.room)}} <small style="opacity:0.8; font-weight:normal;">(${{escapeHtml(gwLabel)}})</small></span>`;
                            }} else {{
                                locationBadge = `<span class="badge" style="background:#f1f5f9; color:#475569; font-size:10px; padding:1px 6px; border-radius:4px;">📡 ${{escapeHtml(gwLabel)}}</span>`;
                            }}

                            return `
                                <div style="display:flex; align-items:center; justify-content:space-between; gap:10px; padding:4px 0; border-bottom:1px solid rgba(148,163,184,0.15);">
                                    <div style="display:flex; align-items:center; gap:6px; overflow:hidden;">
                                        <span style="color:var(--muted); font-size:10px;">[${{time}}]</span>
                                        <span>${{ev.icon || '🔘'}}</span>
                                        <strong style="color:var(--text);">${{escapeHtml(ev.device_name)}}</strong>
                                        <span class="badge" style="background:${{badgeBg}}; color:#fff; font-size:10px; padding:1px 6px; border-radius:3px;">${{escapeHtml(ev.description)}}</span>
                                        ${{locationBadge}}
                                    </div>
                                    <div style="display:flex; align-items:center; gap:8px; font-size:10px; color:var(--muted); flex-shrink:0;">
                                        <span>📶 ${{ev.rssi}} dBm</span>
                                        <a href="/devices#bthome-${{escapeAttr(ev.mac.replace(/[:\-]/g, ''))}}" style="color:var(--primary); text-decoration:none; font-weight:600;">Inspect ↗</a>
                                    </div>
                                </div>
                            `;
                        }}).join('');
                    }}
                }}

                // Sensor cards update
                const chipsEl = document.getElementById('dash-sensor-chips');
                if (chipsEl) {{
                    const seenMacs = new Map();
                    events.forEach(ev => {{
                        if (!seenMacs.has(ev.mac)) {{
                            seenMacs.set(ev.mac, ev);
                        }}
                    }});

                    if (seenMacs.size === 0) {{
                        chipsEl.innerHTML = '<span style="font-size:11px; color:var(--muted);">Keine aktiven Sensoren empfangen.</span>';
                    }} else {{
                        chipsEl.innerHTML = Array.from(seenMacs.values()).map(ev => {{
                            const bat = ev.battery ? `🔋 ${{ev.battery}}%` : '';
                            const roomTag = ev.room ? `📍 ${{escapeHtml(ev.room)}}` : (ev.gateway_name || 'Standort offen');
                            return `
                                <div style="background:var(--bg); border:1px solid var(--border); border-radius:6px; padding:8px 10px; display:flex; justify-content:space-between; align-items:center;">
                                    <div style="display:flex; align-items:center; gap:6px;">
                                        <span>${{ev.icon || '🔘'}}</span>
                                        <div>
                                            <div style="font-weight:600; font-size:12px;">${{escapeHtml(ev.device_name)}}</div>
                                            <div style="font-size:10px; color:var(--muted);">${{bat}} &bull; 📶 ${{ev.rssi}} dBm</div>
                                        </div>
                                    </div>
                                    <div style="text-align:right;">
                                        <span class="badge" style="background:#fef3c7; color:#92400e; font-size:10px; font-weight:600;">${{roomTag}}</span>
                                    </div>
                                </div>
                            `;
                        }}).join('');
                    }}
                }}
            }} catch (e) {{
                console.debug('Dashboard events error:', e);
            }}
        }}

        window.addEventListener('DOMContentLoaded', () => {{
            fetchDashboardEnergy();
            fetchDashboardEvents();
            setInterval(fetchDashboardEnergy, 2500);
            setInterval(fetchDashboardEvents, 1200);
        }});
        </script>
        "#,
        total_devices,
        active_devices
    );

    page_layout(title, "dashboard", &content)
}

fn render_energy_page(title: &str) -> String {
    let content = r#"
    <div style="margin-bottom:20px; display:flex; justify-content:space-between; align-items:center; flex-wrap:wrap; gap:12px;">
        <div>
            <h2 style="font-size:20px; font-weight:700; display:flex; align-items:center; gap:8px;">
                <span>⚡ Real-Time Energy Dashboard</span>
                <span id="energy-live-pulse" class="badge" style="background:#dcfce7; color:#166534; font-size:11px; font-weight:600; padding:3px 8px; border-radius:12px;">
                    <span class="status-dot status-ready" style="width:8px; height:8px; margin-right:4px;"></span>Live Polling
                </span>
            </h2>
            <div style="font-size:12px; color:var(--muted); margin-top:3px;">
                Monitoring Fronius Solar Inverter, Shelly Pro 3EM 3-Phase Grid Meter, and EcoFlow Storage
            </div>
        </div>
        <div style="display:flex; gap:8px; align-items:center;">
            <span id="energy-last-updated" style="font-size:11px; color:var(--muted);">Connecting...</span>
            <button class="btn btn-sm btn-primary" onclick="fetchLiveEnergy()">🔄 Refresh</button>
        </div>
    </div>

    <!-- Top KPI Overview Grid -->
    <div style="display:grid; grid-template-columns:repeat(auto-fit, minmax(260px, 1fr)); gap:16px; margin-bottom:20px;">
        <!-- ☀️ Solar Inverter (Fronius) -->
        <div class="card" style="margin-bottom:0; border-top: 4px solid #f59e0b;">
            <div style="display:flex; justify-content:space-between; align-items:center; margin-bottom:8px;">
                <div style="display:flex; align-items:center; gap:6px; font-weight:600; font-size:13px; color:var(--muted);">
                    <span>☀️ SOLAR GENERATION</span>
                </div>
                <span id="solar-badge" class="badge" style="font-size:10px;">Fronius Inverter</span>
            </div>
            <div style="display:flex; align-items:baseline; gap:6px; margin-bottom:12px;">
                <span id="solar-power-val" style="font-size:32px; font-weight:800; color:#d97706; font-family:monospace;">--</span>
                <span style="font-size:16px; font-weight:600; color:var(--muted);">W</span>
            </div>
            <div style="border-top:1px solid var(--border); padding-top:10px; display:grid; grid-template-columns:1fr 1fr 1fr; gap:6px; font-size:11px;">
                <div>
                    <span style="color:var(--muted); display:block;">Today</span>
                    <strong id="solar-day-val">-- kWh</strong>
                </div>
                <div>
                    <span style="color:var(--muted); display:block;">Year</span>
                    <strong id="solar-year-val">-- kWh</strong>
                </div>
                <div>
                    <span style="color:var(--muted); display:block;">Lifetime</span>
                    <strong id="solar-total-val">-- MWh</strong>
                </div>
            </div>
        </div>

        <!-- 🌐 Grid Net Flow (Shelly Pro 3EM) -->
        <div class="card" style="margin-bottom:0; border-top: 4px solid #2563eb;">
            <div style="display:flex; justify-content:space-between; align-items:center; margin-bottom:8px;">
                <div style="display:flex; align-items:center; gap:6px; font-weight:600; font-size:13px; color:var(--muted);">
                    <span>🌐 GRID NET FLOW</span>
                </div>
                <span id="grid-badge" class="badge" style="font-size:10px;">Shelly Pro 3EM</span>
            </div>
            <div style="display:flex; align-items:baseline; gap:6px; margin-bottom:12px;">
                <span id="grid-power-val" style="font-size:32px; font-weight:800; font-family:monospace;">--</span>
                <span style="font-size:16px; font-weight:600; color:var(--muted);">W</span>
                <span id="grid-dir-badge" class="badge" style="margin-left:auto; font-size:11px; font-weight:600;">--</span>
            </div>
            <div style="border-top:1px solid var(--border); padding-top:10px; display:grid; grid-template-columns:1fr 1fr; gap:8px; font-size:11px;">
                <div>
                    <span style="color:var(--muted); display:block;">Grid Import Total</span>
                    <strong id="grid-import-val">-- kWh</strong>
                </div>
                <div>
                    <span style="color:var(--muted); display:block;">Feed-in Export Total</span>
                    <strong id="grid-export-val">-- kWh</strong>
                </div>
            </div>
        </div>

        <!-- 🏠 House Consumption -->
        <div class="card" style="margin-bottom:0; border-top: 4px solid #10b981;">
            <div style="display:flex; justify-content:space-between; align-items:center; margin-bottom:8px;">
                <div style="display:flex; align-items:center; gap:6px; font-weight:600; font-size:13px; color:var(--muted);">
                    <span>🏠 HOUSE CONSUMPTION</span>
                </div>
                <span class="badge" style="font-size:10px;">Calculated Load</span>
            </div>
            <div style="display:flex; align-items:baseline; gap:6px; margin-bottom:12px;">
                <span id="house-power-val" style="font-size:32px; font-weight:800; color:#059669; font-family:monospace;">--</span>
                <span style="font-size:16px; font-weight:600; color:var(--muted);">W</span>
            </div>
            <div style="border-top:1px solid var(--border); padding-top:10px; display:grid; grid-template-columns:1fr 1fr; gap:8px; font-size:11px;">
                <div>
                    <span style="color:var(--muted); display:block;">Autarky (Self-Sufficiency)</span>
                    <strong id="autarky-val" style="color:var(--status-green);">-- %</strong>
                </div>
                <div>
                    <span style="color:var(--muted); display:block;">Self-Consumption</span>
                    <strong id="self-consumption-val">-- %</strong>
                </div>
            </div>
        </div>

        <!-- 🔋 Batteries & EcoFlow Systems -->
        <div class="card" style="margin-bottom:0; border-top: 4px solid #8b5cf6;">
            <div style="display:flex; justify-content:space-between; align-items:center; margin-bottom:8px;">
                <div style="display:flex; align-items:center; gap:6px; font-weight:600; font-size:13px; color:var(--muted);">
                    <span>🔋 BATTERY STORAGE</span>
                </div>
                <span class="badge" style="font-size:10px;">EcoFlow PowerStream</span>
            </div>
            <div style="margin-bottom:10px;">
                <div style="font-size:13px; font-weight:600; margin-bottom:4px;" id="battery-summary-title">2 Balcony Inverters Detected</div>
                <div style="font-size:11px; color:var(--muted);">ecoflow1 (.96) & ecoflow2 (.105)</div>
            </div>
            <div style="border-top:1px solid var(--border); padding-top:8px; font-size:11px; color:var(--muted);">
                <span>Integrate via Home Assistant (`synologynas:8123`) or EcoFlow Developer API for live SOC % telemetry.</span>
            </div>
        </div>
    </div>

    <!-- Interactive Power Flow Diagram -->
    <div class="card" style="padding:24px 20px; margin-bottom:20px;">
        <h3 style="font-size:14px; font-weight:600; margin-bottom:16px;">Power Flow Diagram</h3>
        <div style="display:flex; justify-content:space-around; align-items:center; flex-wrap:wrap; gap:20px;">
            <!-- Node: Solar -->
            <div style="text-align:center; min-width:140px; padding:16px; background:var(--bg); border:2px solid #f59e0b; border-radius:12px;">
                <div style="font-size:32px;">☀️</div>
                <div style="font-weight:700; font-size:13px; margin-top:4px;">Solar PV</div>
                <div id="flow-solar-val" style="font-weight:700; font-size:16px; color:#d97706; margin-top:2px;">-- W</div>
                <div style="font-size:10px; color:var(--muted);">Fronius Inverter</div>
            </div>

            <div id="flow-line-solar-house" style="display:flex; flex-direction:column; align-items:center; min-width:80px;">
                <span style="font-size:20px;" id="arrow-solar-house">➡️</span>
                <span id="flow-text-solar-house" style="font-size:10px; font-weight:600; color:var(--muted);">-- W</span>
            </div>

            <!-- Node: House -->
            <div style="text-align:center; min-width:160px; padding:20px; background:var(--bg); border:2px solid #10b981; border-radius:12px; box-shadow:0 2px 8px rgba(0,0,0,0.05);">
                <div style="font-size:36px;">🏠</div>
                <div style="font-weight:700; font-size:14px; margin-top:4px;">Home Load</div>
                <div id="flow-house-val" style="font-weight:800; font-size:20px; color:#059669; margin-top:2px;">-- W</div>
                <div style="font-size:10px; color:var(--muted);">Current Total Consumption</div>
            </div>

            <div id="flow-line-grid-house" style="display:flex; flex-direction:column; align-items:center; min-width:80px;">
                <span style="font-size:20px;" id="arrow-grid-house">⬅️</span>
                <span id="flow-text-grid-house" style="font-size:10px; font-weight:600; color:var(--muted);">-- W</span>
            </div>

            <!-- Node: Grid -->
            <div style="text-align:center; min-width:140px; padding:16px; background:var(--bg); border:2px solid #2563eb; border-radius:12px;">
                <div style="font-size:32px;">🌐</div>
                <div style="font-weight:700; font-size:13px; margin-top:4px;">Public Grid</div>
                <div id="flow-grid-val" style="font-weight:700; font-size:16px; color:#2563eb; margin-top:2px;">-- W</div>
                <div id="flow-grid-state" style="font-size:10px; color:var(--muted);">Shelly Pro 3EM</div>
            </div>
        </div>
    </div>

    <!-- Shelly Pro 3EM 3-Phase Precision Table -->
    <div class="card" style="margin-bottom:20px;">
        <div style="display:flex; justify-content:space-between; align-items:center; margin-bottom:14px; flex-wrap:wrap; gap:8px;">
            <div>
                <h3 style="font-size:15px; font-weight:600; display:flex; align-items:center; gap:6px;">
                    <span>⚡ Shelly Pro 3EM (3-Phase Electrical Monitor)</span>
                    <span id="shelly-conn-dot" class="status-dot status-ready"></span>
                </h3>
                <div style="font-size:12px; color:var(--muted);">Target host: <span id="shelly-host-label">shellypro3em.fritz.box</span></div>
            </div>
            <div style="display:flex; gap:8px; font-size:12px;">
                <span class="badge" style="background:var(--bg); border:1px solid var(--border);">Grid Frequency: <strong>50.0 Hz</strong></span>
                <span class="badge" style="background:var(--bg); border:1px solid var(--border);">Total Current: <strong id="shelly-total-curr">-- A</strong></span>
                <span class="badge" style="background:var(--bg); border:1px solid var(--border);">Apparent Power: <strong id="shelly-total-aprt">-- VA</strong></span>
            </div>
        </div>

        <div style="overflow-x:auto;">
            <table>
                <thead>
                    <tr>
                        <th>Phase</th>
                        <th>Active Power (W)</th>
                        <th>Voltage (V)</th>
                        <th>Current (A)</th>
                        <th>Apparent Power (VA)</th>
                        <th>Power Factor (cos φ)</th>
                        <th>Flow Direction</th>
                    </tr>
                </thead>
                <tbody>
                    <tr>
                        <td><strong>Phase A (L1)</strong></td>
                        <td id="phase-a-act" style="font-family:monospace; font-weight:700;">-- W</td>
                        <td id="phase-a-volt">-- V</td>
                        <td id="phase-a-curr">-- A</td>
                        <td id="phase-a-aprt">-- VA</td>
                        <td id="phase-a-pf">--</td>
                        <td id="phase-a-dir">--</td>
                    </tr>
                    <tr>
                        <td><strong>Phase B (L2)</strong></td>
                        <td id="phase-b-act" style="font-family:monospace; font-weight:700;">-- W</td>
                        <td id="phase-b-volt">-- V</td>
                        <td id="phase-b-curr">-- A</td>
                        <td id="phase-b-aprt">-- VA</td>
                        <td id="phase-b-pf">--</td>
                        <td id="phase-b-dir">--</td>
                    </tr>
                    <tr>
                        <td><strong>Phase C (L3)</strong></td>
                        <td id="phase-c-act" style="font-family:monospace; font-weight:700;">-- W</td>
                        <td id="phase-c-volt">-- V</td>
                        <td id="phase-c-curr">-- A</td>
                        <td id="phase-c-aprt">-- VA</td>
                        <td id="phase-c-pf">--</td>
                        <td id="phase-c-dir">--</td>
                    </tr>
                </tbody>
            </table>
        </div>
    </div>

    <!-- JavaScript for Live Telemetry Polling -->
    <script>
    async function fetchLiveEnergy() {
        try {
            const res = await fetch('/api/energy/live');
            if (!res.ok) throw new Error('API returned ' + res.status);
            const data = await res.json();

            // Solar UI
            document.getElementById('solar-power-val').innerText = Math.round(data.solar_power_w).toLocaleString();
            document.getElementById('solar-day-val').innerText = (data.solar_day_kwh || 0).toFixed(2) + ' kWh';
            document.getElementById('solar-year-val').innerText = Math.round(data.solar_year_kwh || 0).toLocaleString() + ' kWh';
            document.getElementById('solar-total-val').innerText = ((data.solar_total_kwh || 0) / 1000).toFixed(2) + ' MWh';

            // Grid UI
            const gridVal = Math.round(data.grid_power_w);
            const absGrid = Math.abs(gridVal);
            document.getElementById('grid-power-val').innerText = absGrid.toLocaleString();
            const gridDirBadge = document.getElementById('grid-dir-badge');
            if (gridVal < -1) {
                gridDirBadge.innerText = '🟢 FEED-IN EXPORT';
                gridDirBadge.style.background = '#dcfce7';
                gridDirBadge.style.color = '#166534';
            } else {
                gridDirBadge.innerText = '🟠 GRID DRAW';
                gridDirBadge.style.background = '#ffedd5';
                gridDirBadge.style.color = '#9a3412';
            }
            document.getElementById('grid-import-val').innerText = (data.grid_import_kwh || 0).toLocaleString() + ' kWh';
            document.getElementById('grid-export-val').innerText = (data.grid_export_kwh || 0).toLocaleString() + ' kWh';

            // House UI
            document.getElementById('house-power-val').innerText = Math.round(data.house_consumption_w).toLocaleString();
            document.getElementById('autarky-val').innerText = (data.autarky_pct || 0).toFixed(1) + ' %';
            document.getElementById('self-consumption-val').innerText = (data.self_consumption_pct || 0).toFixed(1) + ' %';

            // Flow Diagram
            document.getElementById('flow-solar-val').innerText = Math.round(data.solar_power_w).toLocaleString() + ' W';
            document.getElementById('flow-house-val').innerText = Math.round(data.house_consumption_w).toLocaleString() + ' W';
            document.getElementById('flow-grid-val').innerText = absGrid.toLocaleString() + ' W';
            document.getElementById('flow-grid-state').innerText = (gridVal < -1) ? 'Feeding Surplus to Grid' : 'Drawing from Grid';

            if (gridVal < -1) {
                document.getElementById('arrow-grid-house').innerText = '➡️';
                document.getElementById('flow-text-grid-house').innerText = 'Exporting ' + absGrid + ' W';
            } else {
                document.getElementById('arrow-grid-house').innerText = '⬅️';
                document.getElementById('flow-text-grid-house').innerText = 'Importing ' + absGrid + ' W';
            }

            document.getElementById('flow-text-solar-house').innerText = Math.round(data.solar_power_w) + ' W';

            // 3-Phase Shelly details
            if (data.shelly) {
                document.getElementById('shelly-host-label').innerText = data.shelly_host;
                document.getElementById('shelly-total-curr').innerText = data.shelly.total_current + ' A';
                document.getElementById('shelly-total-aprt').innerText = Math.round(data.shelly.total_aprt_power) + ' VA';

                const updatePhase = (prefix, p) => {
                    if (!p) return;
                    const elAct = document.getElementById(prefix + '-act');
                    elAct.innerText = p.act_power.toFixed(1) + ' W';
                    if (p.act_power < 0) {
                        elAct.style.color = '#16a34a';
                        document.getElementById(prefix + '-dir').innerHTML = '<span class="badge" style="background:#dcfce7; color:#166534; font-size:10px;">🟢 Feed-in</span>';
                    } else {
                        elAct.style.color = '#d97706';
                        document.getElementById(prefix + '-dir').innerHTML = '<span class="badge" style="background:#ffedd5; color:#9a3412; font-size:10px;">🟠 Draw</span>';
                    }
                    document.getElementById(prefix + '-volt').innerText = p.voltage + ' V';
                    document.getElementById(prefix + '-curr').innerText = p.current + ' A';
                    document.getElementById(prefix + '-aprt').innerText = p.aprt_power + ' VA';
                    document.getElementById(prefix + '-pf').innerText = p.pf;
                };

                updatePhase('phase-a', data.shelly.phase_a);
                updatePhase('phase-b', data.shelly.phase_b);
                updatePhase('phase-c', data.shelly.phase_c);
            }

            document.getElementById('energy-last-updated').innerText = 'Updated ' + data.timestamp;
        } catch (e) {
            document.getElementById('energy-last-updated').innerText = 'Polling error: ' + e.message;
        }
    }

    // Initial fetch on page load + periodic polling every 2500ms
    window.addEventListener('DOMContentLoaded', () => {
        fetchLiveEnergy();
        setInterval(fetchLiveEnergy, 2500);
    });
    </script>
    "#;

    page_layout(title, "energy", content)
}

fn render_status_page(title: &str, snapshot: &RuntimeSnapshot) -> String {
    let rows = snapshot
        .modules
        .iter()
        .map(|module| {
            let manifest = module.manifest.as_ref();
            let health = module.health.as_ref();
            let name = manifest.map(|m| m.display_name.as_str()).unwrap_or("Unknown");
            let id = manifest.map(|m| m.id.as_str()).unwrap_or("n/a");
            let version = manifest.map(|m| m.version.as_str()).unwrap_or("-");
            let (status_text, dot_class) = if !module.connected {
                ("Disconnected", "status-error")
            } else if let Some(h) = health {
                format_health_state(h.state)
            } else {
                ("Unknown", "status-error")
            };
            let message = health.map(|h| h.message.as_str()).unwrap_or("-");

            format!(
                "<tr><td><strong>{name}</strong><br><small style=\"color:var(--muted)\">{id}</small></td><td><span class=\"status-dot {dot_class}\"></span>{status_text}</td><td>{version}</td><td>{message}</td></tr>"
            )
        })
        .collect::<Vec<_>>()
        .join("");

    let has_hue = snapshot
        .modules
        .iter()
        .any(|m| m.manifest.as_ref().map(|man| man.id == "philips-hue").unwrap_or(false));

    let hue_card = if has_hue {
        r#"
        <div class="card" style="margin-top:20px;">
            <div style="display:flex; justify-content:space-between; align-items:center; margin-bottom:12px;">
                <div style="display:flex; align-items:center; gap:8px;">
                    <span style="font-size:22px;">💡</span>
                    <h3 style="margin:0;">Philips Hue Bridge Integration</h3>
                </div>
                <span class="badge" style="background:#eab308; color:#000; font-weight:700; padding:3px 8px; border-radius:4px;">Hue Zigbee Gateway</span>
            </div>
            <p style="color:var(--muted); font-size:13px; line-height:1.5; margin-bottom:14px;">
                Verbindung zur Philips Hue Bridge (<code>192.168.178.12</code>). Um HomeNode mit deiner Hue Bridge zu verbinden: drücke den runden Knopf oben auf der Bridge und klicke anschließend auf <strong>"Jetzt koppeln"</strong>.
            </p>
            <div style="display:flex; align-items:center; gap:12px;">
                <button type="button" class="btn btn-primary" id="hue-pair-btn" onclick="triggerHuePairing()">
                    🔗 Philips Hue Bridge jetzt koppeln
                </button>
                <span id="hue-pair-msg" style="font-size:13px; font-weight:600;"></span>
            </div>
            <script>
            async function triggerHuePairing() {
                const btn = document.getElementById('hue-pair-btn');
                const msg = document.getElementById('hue-pair-msg');
                btn.disabled = true;
                msg.innerText = 'Kopplung wird angefragt...';
                msg.style.color = '#0284c7';
                try {
                    const res = await fetch('/api/hue/pair', { method: 'POST' });
                    const data = await res.json();
                    if (res.ok && data.status === 'ok') {
                        msg.innerText = '🎉 ' + (data.message || 'Erfolgreich gekoppelt!');
                        msg.style.color = 'var(--status-green)';
                        setTimeout(() => window.location.reload(), 1500);
                    } else if (data.status === 'waiting_for_button') {
                        msg.innerText = '🔘 ' + data.message;
                        msg.style.color = '#f59e0b';
                        btn.disabled = false;
                    } else {
                        msg.innerText = '⚠️ ' + (data.message || data.error || 'Fehler');
                        msg.style.color = 'var(--status-red)';
                        btn.disabled = false;
                    }
                } catch(e) {
                    msg.innerText = 'Netzwerkfehler: ' + e;
                    msg.style.color = 'var(--status-red)';
                    btn.disabled = false;
                }
            }
            </script>
        </div>
        "#
    } else {
        ""
    };

    let content = format!(
        r#"<div class="card"><h2>Integration Modules ({})</h2><div style="overflow-x:auto"><table><thead><tr><th>Module</th><th>Status</th><th>Version</th><th>Health / Details</th></tr></thead><tbody>{}</tbody></table></div></div>{}"#,
        snapshot.modules.len(),
        rows,
        hue_card
    );

    page_layout(title, "status", &content)
}

fn render_matter_page(
    title: &str,
    snapshot: &RuntimeSnapshot,
    _docs: &HashMap<String, DeviceDocumentation>,
    links: &HashMap<String, Vec<String>>,
    fabric_metas: &HashMap<String, MatterFabricMeta>,
) -> String {
    let unified_devices = build_unified_devices(&snapshot.devices, links);

    struct MatterDeviceEntry {
        display_name: String,
        ip: String,
        mac: String,
        vendor: String,
        web_url: Option<String>,
        fabrics: Vec<ClientMatterFabric>,
    }

    let mut matter_devices = Vec::new();
    let mut fabric_members: HashMap<String, Vec<(String, String, u16, String)>> = HashMap::new();

    for udev in &unified_devices {
        let dev = &udev.primary;
        let fabrics = extract_device_matter_fabrics(dev, &udev.secondary_interfaces, fabric_metas);
        if !fabrics.is_empty() {
            let ip = dev.metadata.get("ip").cloned().unwrap_or_else(|| "-".to_string());
            let mac = dev.metadata.get("mac").cloned().unwrap_or_default();
            let vendor = dev.metadata.get("vendor").cloned().unwrap_or_default();
            let web_url = dev.metadata.get("web_url").cloned();

            for fab in &fabrics {
                fabric_members.entry(fab.fabric_id.clone()).or_default().push((
                    dev.display_name.clone(),
                    fab.node_id.clone(),
                    fab.port,
                    fab.interface.clone(),
                ));
            }

            matter_devices.push(MatterDeviceEntry {
                display_name: dev.display_name.clone(),
                ip,
                mac,
                vendor,
                web_url,
                fabrics,
            });
        }
    }

    matter_devices.sort_by(|a, b| a.display_name.cmp(&b.display_name));

    let mut fabric_keys: Vec<String> = fabric_metas.keys().cloned().collect();
    for fid in fabric_members.keys() {
        if !fabric_keys.contains(fid) {
            fabric_keys.push(fid.clone());
        }
    }
    fabric_keys.sort_by_key(|k| match k.as_str() {
        "6ABEDCB982EC2223" => 1,
        "4518A03EC84FB6E7" => 2,
        "A38D674BFAAFF432" => 3,
        _ => 10,
    });

    let total_nodes: usize = fabric_members.values().map(|v| v.len()).sum();
    let multi_admin_count = matter_devices.iter().filter(|d| d.fabrics.len() > 1).count();

    let summary_cards = format!(
        r#"<div style="display:grid; grid-template-columns:repeat(auto-fit, minmax(240px, 1fr)); gap:16px; margin-bottom:20px;">
            <div class="card" style="margin:0;">
                <div style="font-size:12px; font-weight:600; color:var(--muted); text-transform:uppercase;">Operational Fabrics</div>
                <div style="font-size:28px; font-weight:700; color:var(--primary); margin:4px 0;">{}</div>
                <div style="font-size:12px; color:var(--muted);">Multi-ecosystem Matter fabrics active</div>
            </div>
            <div class="card" style="margin:0;">
                <div style="font-size:12px; font-weight:600; color:var(--muted); text-transform:uppercase;">Matter Endpoints</div>
                <div style="font-size:28px; font-weight:700; color:var(--status-green); margin:4px 0;">{} Nodes / {} Devices</div>
                <div style="font-size:12px; color:var(--muted);">Active operational node announcements</div>
            </div>
            <div class="card" style="margin:0;">
                <div style="font-size:12px; font-weight:600; color:var(--muted); text-transform:uppercase;">Multi-Admin Coverage</div>
                <div style="font-size:28px; font-weight:700; color:#8b5cf6; margin:4px 0;">{} Devices ({:.0}%)</div>
                <div style="font-size:12px; color:var(--muted);">Co-managed across Apple Home &amp; Home Assistant</div>
            </div>
        </div>"#,
        fabric_keys.len(),
        total_nodes,
        matter_devices.len(),
        multi_admin_count,
        if matter_devices.is_empty() { 0.0 } else { (multi_admin_count as f64 / matter_devices.len() as f64) * 100.0 }
    );

    let mut fabric_cards = String::new();
    for fid in &fabric_keys {
        let meta = fabric_metas.get(fid);
        let name = meta.map(|m| m.name.as_str()).unwrap_or("Unknown Fabric");
        let icon = meta.map(|m| m.icon.as_str()).unwrap_or("✨");
        let desc = meta.map(|m| m.description.as_str()).unwrap_or("Operational Matter fabric membership.");

        let members = fabric_members.get(fid).cloned().unwrap_or_default();
        let mut member_chips = String::new();
        for (m_name, node_id, port, iface) in &members {
            let iface_icon = if iface == "thread" { "🧵" } else { "📶" };
            member_chips.push_str(&format!(
                r#"<span class="badge" style="margin:3px 4px 3px 0; padding:4px 8px; font-size:11px; background:var(--bg); border:1px solid var(--border);">{} <strong>{}</strong> <code>Node: {}</code> (:{} {})</span>"#,
                icon, m_name, node_id, port, iface_icon
            ));
        }
        if member_chips.is_empty() {
            member_chips = r#"<span style="color:var(--muted); font-size:12px;">No active members currently advertising.</span>"#.to_string();
        }

        fabric_cards.push_str(&format!(
            r#"<div class="card" style="margin-bottom:16px;">
                <div style="display:flex; justify-content:space-between; align-items:flex-start; flex-wrap:wrap; gap:10px;">
                    <div>
                        <div style="display:flex; align-items:center; gap:8px;">
                            <span style="font-size:24px;">{}</span>
                            <div>
                                <h3 style="margin:0; font-size:17px;">{}</h3>
                                <div style="font-size:11px; color:var(--muted); font-family:monospace; margin-top:2px;">
                                    Compressed Fabric ID: <strong>{}</strong> &bull; <span class="badge" style="background:#dcfce7; color:#166534; font-weight:600;">{} Commissioned Nodes</span>
                                </div>
                            </div>
                        </div>
                        <p style="font-size:12px; color:var(--muted); margin-top:6px;">{}</p>
                    </div>
                    <button type="button" class="btn btn-sm" onclick="renameMatterFabric('{}', '{}', '{}')">✏️ Rename Fabric</button>
                </div>
                <div style="margin-top:12px; padding-top:10px; border-top:1px solid var(--border);">
                    <div style="font-size:11px; font-weight:600; text-transform:uppercase; color:var(--muted); margin-bottom:6px;">Member Devices &amp; Node Assignments</div>
                    <div style="display:flex; flex-wrap:wrap;">{}</div>
                </div>
            </div>"#,
            icon, name, fid, members.len(), desc, fid, name.replace('\'', "\\'"), icon.replace('\'', "\\'"), member_chips
        ));
    }

    let mut matrix_header = String::new();
    matrix_header.push_str("<th>Matter Device</th><th>Network</th>");
    for fid in &fabric_keys {
        let meta = fabric_metas.get(fid);
        let name = meta.map(|m| m.name.as_str()).unwrap_or("Fabric");
        let icon = meta.map(|m| m.icon.as_str()).unwrap_or("✨");
        matrix_header.push_str(&format!("<th style=\"text-align:center;\">{} {}</th>", icon, name));
    }
    matrix_header.push_str("<th style=\"text-align:right;\">Quick Action</th>");

    let mut matrix_rows = String::new();
    for dev in &matter_devices {
        let mut cols = String::new();
        for fid in &fabric_keys {
            if let Some(entry) = dev.fabrics.iter().find(|f| &f.fabric_id == fid) {
                let iface_icon = if entry.interface == "thread" { "🧵 Thread" } else { "📶 Wi-Fi" };
                cols.push_str(&format!(
                    r#"<td style="text-align:center;"><span class="badge" style="background:#dcfce7; color:#166534; font-weight:600; padding:3px 7px;">✓ <code>0x{}</code></span><br><small style="color:var(--muted); font-size:10px;">:{} &bull; {}</small></td>"#,
                    entry.node_id, entry.port, iface_icon
                ));
            } else {
                cols.push_str(r#"<td style="text-align:center; color:var(--muted); opacity:0.35;">—</td>"#);
            }
        }

        let web_btn = if let Some(ref url) = dev.web_url {
            format!(r#"<a href="{url}" target="_blank" class="btn-sm btn-web" style="text-decoration:none;">🌐 Web UI</a>"#)
        } else {
            String::new()
        };

        let net_info = if dev.mac.is_empty() {
            format!("<code>{}</code>", dev.ip)
        } else {
            format!("<code>{}</code><br><small style=\"color:var(--muted);\">{}</small>", dev.ip, dev.mac)
        };

        matrix_rows.push_str(&format!(
            r#"<tr>
                <td><strong>{}</strong><br><small style="color:var(--muted);">{}</small></td>
                <td>{}</td>
                {cols}
                <td style="text-align:right;">{}</td>
            </tr>"#,
            dev.display_name, dev.vendor, net_info, web_btn
        ));
    }

    let script = r#"
    <script>
    async function renameMatterFabric(fabricId, currentName, currentIcon) {
        const newName = prompt(`Enter friendly label for Matter Fabric (${fabricId}):`, currentName);
        if (!newName || !newName.trim()) return;
        const newIcon = prompt(`Enter emoji/icon for ${newName}:`, currentIcon || '✨') || '✨';

        try {
            const res = await fetch('/api/matter/fabrics/' + encodeURIComponent(fabricId), {
                method: 'POST',
                headers: { 'Content-Type': 'application/json' },
                body: JSON.stringify({ name: newName.trim(), icon: newIcon.trim() })
            });
            if (res.ok) {
                window.location.reload();
            } else {
                alert('Failed to update Matter fabric label.');
            }
        } catch (e) {
            alert('Network error: ' + e);
        }
    }
    </script>
    "#;

    let content = format!(
        r#"
        <div class="toolbar">
            <div>
                <h2>✨ Matter Fabrics &amp; Ecosystems</h2>
                <p style="font-size:12px; color:var(--muted); margin-top:2px;">
                    Multi-Admin operational fabrics, node credentials, and cross-ecosystem synchronization discovered via mDNS (<code>_matter._tcp</code>).
                </p>
            </div>
            <form action="/scan" method="POST" style="margin:0;">
                <button type="submit" class="btn btn-primary">
                    <span>🔄</span> <span>Scan Matter Fabrics Now</span>
                </button>
            </form>
        </div>

        {}

        <div style="margin-bottom:24px;">
            <h3 style="font-size:15px; margin-bottom:12px;">Active Operational Fabrics ({})</h3>
            {}
        </div>

        <div class="card" style="margin-bottom:24px;">
            <div style="display:flex; justify-content:space-between; align-items:center; margin-bottom:12px;">
                <div>
                    <h3 style="font-size:15px;">Multi-Admin Cross-Ecosystem Matrix</h3>
                    <p style="font-size:12px; color:var(--muted); margin-top:2px;">
                        Status of all Matter devices co-commissioned across Apple Home, Home Assistant, and vendor fabrics.
                    </p>
                </div>
                <span class="badge" style="font-size:11px;">{} Matter Devices</span>
            </div>
            <div style="overflow-x:auto;">
                <table>
                    <thead><tr>{}</tr></thead>
                    <tbody>{}</tbody>
                </table>
            </div>
        </div>

        <div class="card" style="background:var(--bg); border:1px dashed var(--border);">
            <div style="font-size:13px; font-weight:700; color:var(--text); margin-bottom:4px;">🧵 Thread Mesh &amp; Border Router Intelligence</div>
            <p style="font-size:12px; color:var(--muted);">
                Matter-over-Thread devices on your network communicate across the Thread mesh network coordinated by your Apple TV / HomePod Thread Border Router. They advertise IPv6 link-local addresses without requiring standard IPv4 DHCP leases and are automatically correlated by HomeNode.
            </p>
        </div>
        {}
        "#,
        summary_cards,
        fabric_keys.len(),
        fabric_cards,
        matter_devices.len(),
        matrix_header,
        matrix_rows,
        script
    );

    page_layout(title, "matter", &content)
}

fn render_catalog_page(
    title: &str,
    catalog: &homenode_definitions::CatalogDatabase,
) -> String {
    let matter_count = catalog
        .products
        .iter()
        .filter(|p| p.matter_device_type.is_some())
        .count();

    let mut vendor_cards = String::new();
    let mut vendor_options = String::new();

    for v in &catalog.vendors {
        vendor_options.push_str(&format!(
            r#"<option value="{}">{} {}</option>"#,
            v.id, v.icon, v.name
        ));

        let prods = catalog.products_for_vendor(&v.id);
        let website_link = if let Some(w) = &v.website {
            format!(r#"<a href="{w}" target="_blank" style="color:var(--primary); text-decoration:none; font-size:13px; font-weight:600;">🌐 Website ↗</a>"#)
        } else {
            String::new()
        };

        let support_link = if let Some(s) = &v.support_url {
            format!(r#" <a href="{s}" target="_blank" style="color:var(--muted); text-decoration:none; font-size:13px;">[Support] ↗</a>"#)
        } else {
            String::new()
        };

        let mut protocols_badges = String::new();
        for proto in &v.protocols {
            protocols_badges.push_str(&format!(r#"<span class="badge" style="font-size:11px;">{proto}</span> "#));
        }

        let mut oui_str = String::new();
        if !v.oui_prefixes.is_empty() {
            let prefixes = v.oui_prefixes.iter().map(|p| format!("<code>{p}</code>")).collect::<Vec<_>>().join(", ");
            oui_str = format!(r#"<div style="font-size:11px; color:var(--muted); margin-top:4px;">MAC Prefixes: {prefixes}</div>"#);
        }

        let desc_html = if let Some(d) = &v.description {
            format!(r#"<p style="font-size:13px; color:var(--muted); margin-top:6px; margin-bottom:12px;">{d}</p>"#)
        } else {
            String::new()
        };

        let mut prod_rows = String::new();
        for p in &prods {
            let matter_badge = if let Some(m) = &p.matter_device_type {
                format!(r#"<br><span class="badge" style="background:#059669; color:#fff; font-size:10px; margin-top:3px;">✨ Matter: {m}</span>"#)
            } else {
                String::new()
            };

            let doc_link = if let Some(d) = &p.documentation_url {
                format!(r#"<a href="{d}" target="_blank" class="btn-sm btn-web" style="text-decoration:none;">📖 Manual ↗</a>"#)
            } else {
                "-".to_string()
            };

            let conn_badges = p.connectivity.iter().map(|c| format!(r#"<span class="badge" style="font-size:10px;">{c}</span>"#)).collect::<Vec<_>>().join(" ");
            let ports_str = if p.default_ports.is_empty() {
                "-".to_string()
            } else {
                p.default_ports.iter().map(|port| port.to_string()).collect::<Vec<_>>().join(", ")
            };

            let model_str = p.model_number.as_deref().unwrap_or("-");
            let specs_str = p.specs.as_deref().unwrap_or("-");

            prod_rows.push_str(&format!(
                r#"<tr>
                    <td><strong>{} {}</strong>{}<br><small style="color:var(--muted)">Model: <code>{}</code></small></td>
                    <td><span class="badge badge-kind">{}</span></td>
                    <td>{}</td>
                    <td><code>{}</code></td>
                    <td style="max-width:320px; font-size:12px; color:var(--muted);">{}</td>
                    <td style="text-align:right;">{}</td>
                </tr>"#,
                p.category_icon, p.name, matter_badge, model_str, p.category, conn_badges, ports_str, specs_str, doc_link
            ));
        }

        let products_table = if prod_rows.is_empty() {
            r#"<div style="font-size:12px; color:var(--muted); font-style:italic;">No registered products under this vendor yet.</div>"#.to_string()
        } else {
            format!(
                r#"<div style="overflow-x:auto;">
                    <table>
                        <thead>
                            <tr>
                                <th>Product Model</th>
                                <th>Category</th>
                                <th>Connectivity</th>
                                <th>Default Ports</th>
                                <th>Specifications</th>
                                <th style="text-align:right;">Documentation</th>
                            </tr>
                        </thead>
                        <tbody>{}</tbody>
                    </table>
                </div>"#,
                prod_rows
            )
        };

        vendor_cards.push_str(&format!(
            r#"<div class="card vendor-card" data-name="{}" data-id="{}">
                <div style="display:flex; justify-content:space-between; align-items:flex-start; flex-wrap:wrap; gap:10px; margin-bottom:8px;">
                    <div>
                        <h3 style="font-size:18px; display:flex; align-items:center; gap:8px;">
                            <span>{}</span> <span>{}</span>
                        </h3>
                        <div style="margin-top:4px;">{} {}</div>
                        {}
                    </div>
                    <div>{}</div>
                </div>
                {}
                <div style="margin-top:10px;">
                    <div style="font-size:12px; font-weight:600; color:var(--muted); text-transform:uppercase; margin-bottom:6px;">
                        Known Hardware Models ({}):
                    </div>
                    {}
                </div>
            </div>"#,
            v.name.to_lowercase(), v.id,
            v.icon, v.name,
            website_link, support_link,
            oui_str,
            protocols_badges,
            desc_html,
            prods.len(),
            products_table
        ));
    }

    let content = format!(
        r#"
        <div class="toolbar" style="align-items:flex-start; margin-bottom:20px;">
            <div>
                <h2>Vendor & Product Database</h2>
                <p style="color:var(--muted); font-size:13px;">
                    Central smart hardware profiles, IEEE OUI manufacturer mappings, Matter profiles, and product specifications.
                </p>
                <div style="display:flex; gap:10px; margin-top:8px; flex-wrap:wrap;">
                    <span class="badge" style="font-size:12px; padding:4px 10px;">🏢 {} Vendors</span>
                    <span class="badge" style="font-size:12px; padding:4px 10px;">🔌 {} Hardware Models</span>
                    <span class="badge" style="font-size:12px; padding:4px 10px; background:#059669; color:#fff;">✨ {} Matter-Certified</span>
                </div>
            </div>
            <div style="display:flex; gap:8px; flex-wrap:wrap;">
                <button type="button" class="btn btn-primary" onclick="openModal('add-product-modal')">➕ Add Product</button>
                <button type="button" class="btn" style="background:var(--badge-bg); color:var(--text);" onclick="openModal('add-vendor-modal')">🏢 Add Vendor</button>
            </div>
        </div>

        <div style="margin-bottom:20px;">
            <input type="text" id="catalog-search" class="form-control" placeholder="🔍 Filter vendors, products, protocols, categories..." oninput="filterCatalog()" />
        </div>

        <div id="vendor-list">
            {}
        </div>

        <!-- Add Vendor Modal -->
        <div id="add-vendor-modal" style="display:none; position:fixed; top:0; left:0; width:100%; height:100%; background:rgba(0,0,0,0.5); z-index:999; overflow-y:auto;">
            <div class="card" style="max-width:560px; margin:40px auto; padding:24px;">
                <div style="display:flex; justify-content:space-between; align-items:center; margin-bottom:16px;">
                    <h3>🏢 Add New Hardware Vendor</h3>
                    <button type="button" class="btn-sm" onclick="closeModal('add-vendor-modal')">✕</button>
                </div>
                <form id="add-vendor-form" onsubmit="submitVendor(event)">
                    <div style="margin-bottom:10px;">
                        <label style="font-size:12px; font-weight:600;">Vendor ID (slug, e.g. shelly, avm)</label>
                        <input type="text" id="v-id" class="form-control" required placeholder="acme" />
                    </div>
                    <div style="margin-bottom:10px;">
                        <label style="font-size:12px; font-weight:600;">Vendor Name</label>
                        <input type="text" id="v-name" class="form-control" required placeholder="Acme Smart Devices Corp" />
                    </div>
                    <div style="display:grid; grid-template-columns:1fr 1fr; gap:10px; margin-bottom:10px;">
                        <div>
                            <label style="font-size:12px; font-weight:600;">Website URL</label>
                            <input type="url" id="v-web" class="form-control" placeholder="https://..." />
                        </div>
                        <div>
                            <label style="font-size:12px; font-weight:600;">Icon Emoji</label>
                            <input type="text" id="v-icon" class="form-control" value="🏢" />
                        </div>
                    </div>
                    <div style="margin-bottom:10px;">
                        <label style="font-size:12px; font-weight:600;">IEEE MAC OUI Prefixes (comma-separated)</label>
                        <input type="text" id="v-oui" class="form-control" placeholder="00:11:22, 33:44:55" />
                    </div>
                    <div style="margin-bottom:10px;">
                        <label style="font-size:12px; font-weight:600;">Supported Protocols (comma-separated)</label>
                        <input type="text" id="v-proto" class="form-control" placeholder="wifi, matter, lan, ble" />
                    </div>
                    <div style="margin-bottom:14px;">
                        <label style="font-size:12px; font-weight:600;">Description</label>
                        <textarea id="v-desc" class="form-control" placeholder="Vendor summary..."></textarea>
                    </div>
                    <div style="display:flex; justify-content:flex-end; gap:8px;">
                        <button type="button" class="btn" style="background:var(--badge-bg); color:var(--text);" onclick="closeModal('add-vendor-modal')">Cancel</button>
                        <button type="submit" class="btn btn-primary">Save Vendor</button>
                    </div>
                </form>
            </div>
        </div>

        <!-- Add Product Modal -->
        <div id="add-product-modal" style="display:none; position:fixed; top:0; left:0; width:100%; height:100%; background:rgba(0,0,0,0.5); z-index:999; overflow-y:auto;">
            <div class="card" style="max-width:620px; margin:40px auto; padding:24px;">
                <div style="display:flex; justify-content:space-between; align-items:center; margin-bottom:16px;">
                    <h3>🔌 Add New Hardware Product</h3>
                    <button type="button" class="btn-sm" onclick="closeModal('add-product-modal')">✕</button>
                </div>
                <form id="add-product-form" onsubmit="submitProduct(event)">
                    <div style="display:grid; grid-template-columns:1fr 1fr; gap:10px; margin-bottom:10px;">
                        <div>
                            <label style="font-size:12px; font-weight:600;">Manufacturer / Vendor</label>
                            <select id="p-vendor" class="form-control" required>
                                {}
                            </select>
                        </div>
                        <div>
                            <label style="font-size:12px; font-weight:600;">Product ID (slug)</label>
                            <input type="text" id="p-id" class="form-control" required placeholder="shelly_plus_1" />
                        </div>
                    </div>
                    <div style="display:grid; grid-template-columns:2fr 1fr; gap:10px; margin-bottom:10px;">
                        <div>
                            <label style="font-size:12px; font-weight:600;">Product Commercial Name</label>
                            <input type="text" id="p-name" class="form-control" required placeholder="Shelly Plus 1" />
                        </div>
                        <div>
                            <label style="font-size:12px; font-weight:600;">Model / Part Number</label>
                            <input type="text" id="p-model" class="form-control" placeholder="SNSW-001X16EU" />
                        </div>
                    </div>
                    <div style="display:grid; grid-template-columns:1fr 1fr; gap:10px; margin-bottom:10px;">
                        <div>
                            <label style="font-size:12px; font-weight:600;">Category</label>
                            <select id="p-category" class="form-control">
                                <option value="energy">Solar & Energy (energy)</option>
                                <option value="smart-plug">Smart Plugs & Sockets (smart-plug)</option>
                                <option value="router">Routers & Gateways (router)</option>
                                <option value="sensor">Sensors & Detectors (sensor)</option>
                                <option value="lighting">Smart Lighting (lighting)</option>
                                <option value="hub">Smart Home Hubs (hub)</option>
                                <option value="nas">Network Storage & NAS (nas)</option>
                                <option value="display">Smart Displays & Clocks (display)</option>
                                <option value="computer">Computers & Laptops (computer)</option>
                                <option value="appliance">Home Appliances (appliance)</option>
                                <option value="audio">Audio & Speakers (audio)</option>
                                <option value="vpn">VPN & Virtual Devices (vpn)</option>
                                <option value="network-device">Network & Other Devices</option>
                            </select>
                        </div>
                        <div>
                            <label style="font-size:12px; font-weight:600;">Category Icon</label>
                            <input type="text" id="p-icon" class="form-control" value="⚡" />
                        </div>
                    </div>
                    <div style="margin-bottom:10px;">
                        <label style="font-size:12px; font-weight:600;">Matter Device Type Profile (Optional)</label>
                        <input type="text" id="p-matter" class="form-control" placeholder="e.g. On/Off Light (0x0100), Electrical Sensor (0x0503)" />
                    </div>
                    <div style="display:grid; grid-template-columns:1fr 1fr; gap:10px; margin-bottom:10px;">
                        <div>
                            <label style="font-size:12px; font-weight:600;">Connectivity (comma-separated)</label>
                            <input type="text" id="p-conn" class="form-control" placeholder="wifi, lan, bluetooth" />
                        </div>
                        <div>
                            <label style="font-size:12px; font-weight:600;">Default Listening Ports</label>
                            <input type="text" id="p-ports" class="form-control" placeholder="80, 443" />
                        </div>
                    </div>
                    <div style="margin-bottom:10px;">
                        <label style="font-size:12px; font-weight:600;">Hostname Patterns (comma-separated substrings for auto-match)</label>
                        <input type="text" id="p-patterns" class="form-control" placeholder="shellyplus1, shelly-1" />
                    </div>
                    <div style="margin-bottom:10px;">
                        <label style="font-size:12px; font-weight:600;">Official Manual / Documentation URL</label>
                        <input type="url" id="p-doc" class="form-control" placeholder="https://..." />
                    </div>
                    <div style="margin-bottom:14px;">
                        <label style="font-size:12px; font-weight:600;">Hardware Specifications</label>
                        <textarea id="p-specs" class="form-control" placeholder="Key technical specifications, voltage, relays..."></textarea>
                    </div>
                    <div style="display:flex; justify-content:flex-end; gap:8px;">
                        <button type="button" class="btn" style="background:var(--badge-bg); color:var(--text);" onclick="closeModal('add-product-modal')">Cancel</button>
                        <button type="submit" class="btn btn-primary">Save Product</button>
                    </div>
                </form>
            </div>
        </div>

        <script>
        function filterCatalog() {{
            const q = (document.getElementById('catalog-search').value || '').toLowerCase();
            const cards = document.querySelectorAll('.vendor-card');
            cards.forEach(card => {{
                const text = card.innerText.toLowerCase();
                if (!q || text.includes(q)) {{
                    card.style.display = '';
                }} else {{
                    card.style.display = 'none';
                }}
            }});
        }}

        function openModal(id) {{
            const el = document.getElementById(id);
            if (el) el.style.display = 'block';
        }}

        function closeModal(id) {{
            const el = document.getElementById(id);
            if (el) el.style.display = 'none';
        }}

        async function submitVendor(e) {{
            e.preventDefault();
            const ouiRaw = document.getElementById('v-oui').value || '';
            const protoRaw = document.getElementById('v-proto').value || '';
            const vendor = {{
                id: document.getElementById('v-id').value.trim(),
                name: document.getElementById('v-name').value.trim(),
                website: document.getElementById('v-web').value.trim() || null,
                support_url: null,
                icon: document.getElementById('v-icon').value.trim() || '🏢',
                oui_prefixes: ouiRaw.split(',').map(s => s.trim().toLowerCase()).filter(Boolean),
                protocols: protoRaw.split(',').map(s => s.trim().toLowerCase()).filter(Boolean),
                description: document.getElementById('v-desc').value.trim() || null
            }};

            const res = await fetch('/api/catalog/vendor', {{
                method: 'POST',
                headers: {{ 'Content-Type': 'application/json' }},
                body: JSON.stringify(vendor)
            }});

            if (res.ok) {{
                window.location.reload();
            }} else {{
                alert('Failed to save vendor');
            }}
        }}

        async function submitProduct(e) {{
            e.preventDefault();
            const connRaw = document.getElementById('p-conn').value || '';
            const portsRaw = document.getElementById('p-ports').value || '';
            const patRaw = document.getElementById('p-patterns').value || '';

            const product = {{
                id: document.getElementById('p-id').value.trim(),
                vendor_id: document.getElementById('p-vendor').value,
                name: document.getElementById('p-name').value.trim(),
                model_number: document.getElementById('p-model').value.trim() || null,
                category: document.getElementById('p-category').value,
                category_icon: document.getElementById('p-icon').value.trim() || '🔌',
                connectivity: connRaw.split(',').map(s => s.trim().toLowerCase()).filter(Boolean),
                matter_device_type: document.getElementById('p-matter').value.trim() || null,
                default_ports: portsRaw.split(',').map(s => parseInt(s.trim())).filter(n => !isNaN(n)),
                hostname_patterns: patRaw.split(',').map(s => s.trim().toLowerCase()).filter(Boolean),
                documentation_url: document.getElementById('p-doc').value.trim() || null,
                specs: document.getElementById('p-specs').value.trim() || null,
                rhai_script_ref: null
            }};

            const res = await fetch('/api/catalog/product', {{
                method: 'POST',
                headers: {{ 'Content-Type': 'application/json' }},
                body: JSON.stringify(product)
            }});

            if (res.ok) {{
                window.location.reload();
            }} else {{
                alert('Failed to save product');
            }}
        }}
        </script>
        "#,
        catalog.vendors.len(),
        catalog.products.len(),
        matter_count,
        vendor_cards,
        vendor_options
    );

    page_layout(title, "catalog", &content)
}

fn format_health_state(state: i32) -> (&'static str, &'static str) {
    match state {
        1 => ("Starting", "status-starting"),
        2 => ("Ready", "status-ready"),
        3 => ("Degraded", "status-starting"),
        4 => ("Stopped", "status-error"),
        5 => ("Failed", "status-error"),
        _ => ("Unknown", "status-error"),
    }
}

fn render_error(title: &str, current_tab: &str, error: &str) -> String {
    let content = format!(
        r#"<div class="card"><div class="empty-state"><h3 style="color:var(--status-red)">Error loading runtime snapshot</h3><p>{error}</p></div></div>"#
    );
    page_layout(title, current_tab, &content)
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

    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("failed to connect web module")))
}

fn load_config(path: &Path) -> Result<WebConfig> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config at {}", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(WebConfig::default());
    }
    toml::from_str(&raw).with_context(|| format!("failed to parse TOML config at {}", path.display()))
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

fn default_listen_addr() -> String {
    String::from("0.0.0.0:8080")
}

fn default_status_title() -> String {
    String::from("HomeNode Server")
}
