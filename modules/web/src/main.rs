use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::extract::{Path as AxumPath, State};
use axum::response::{Html, IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::RwLock;
use tracing::{error, info, warn};
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
    pub notes: String,
    #[serde(default)]
    pub manual_url: Option<String>,
    #[serde(default)]
    pub updated_at: String,
}

#[derive(Clone)]
struct WebState {
    socket_path: PathBuf,
    status_title: String,
    docs_path: PathBuf,
    links_path: PathBuf,
    definitions_dir: PathBuf,
    docs_store: Arc<RwLock<HashMap<String, DeviceDocumentation>>>,
    links_store: Arc<RwLock<HashMap<String, Vec<String>>>>,
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
    let definitions_dir = workspace_root.join("definitions").join("devices");

    let initial_docs = load_json_map(&docs_path);
    let initial_links = load_json_map(&links_path);
    let docs_store = Arc::new(RwLock::new(initial_docs));
    let links_store = Arc::new(RwLock::new(initial_links));

    let listener = tokio::net::TcpListener::bind(&config.listen_addr).await?;
    client
        .report_health(module_health(
            env.module_id,
            HealthState::Ready,
            format!("Serving status page on {}", config.listen_addr),
        ))
        .await?;

    let app = Router::new()
        .route("/", get(devices_handler))
        .route("/status", get(status_handler))
        .route("/scan", post(scan_trigger_form_handler))
        .route("/api/scan", post(scan_trigger_api_handler))
        .route(
            "/api/devices/:id/documentation",
            get(get_device_doc_handler).post(save_device_doc_handler),
        )
        .route("/api/devices/:id/analyze", post(analyze_device_handler))
        .route("/api/devices/link", post(link_devices_handler))
        .route("/api/devices/unlink", post(unlink_devices_handler))
        .route("/api/definitions/save", post(save_definition_handler))
        .with_state(WebState {
            socket_path: env.socket_path,
            status_title: config.status_title,
            docs_path,
            links_path,
            definitions_dir,
            docs_store,
            links_store,
        });

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

async fn devices_handler(State(state): State<WebState>) -> Html<String> {
    let docs = state.docs_store.read().await.clone();
    let links = state.links_store.read().await.clone();
    let body = match load_snapshot(&state.socket_path).await {
        Ok(snapshot) => render_devices_page(&state.status_title, &snapshot, &docs, &links),
        Err(error) => render_error(&state.status_title, "devices", &error.to_string()),
    };
    Html(body)
}

async fn status_handler(State(state): State<WebState>) -> Html<String> {
    let body = match load_snapshot(&state.socket_path).await {
        Ok(snapshot) => render_status_page(&state.status_title, &snapshot),
        Err(error) => render_error(&state.status_title, "status", &error.to_string()),
    };
    Html(body)
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
    notes: String,
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
    let entry = DeviceDocumentation {
        notes: payload.notes,
        manual_url: payload.manual_url.filter(|u| !u.trim().is_empty()),
        updated_at: now,
    };
    store.insert(device_id, entry);
    if let Err(err) = persist_json(&state.docs_path, &*store) {
        error!("Failed to persist device documentation: {err}");
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": err.to_string()})),
        )
            .into_response();
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
    let mut links = state.links_store.write().await;
    let list = links.entry(payload.primary_id.clone()).or_default();
    if !list.contains(&payload.linked_id) {
        list.push(payload.linked_id);
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
struct UnifiedDevice {
    primary: DeviceRecord,
    secondary_interfaces: Vec<DeviceRecord>,
    merge_candidate: Option<DeviceRecord>,
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

    // Skip if already linked
    if links.values().any(|v| v.contains(&dev.device_id) || (!mac.is_empty() && v.contains(&mac))) {
        return None;
    }
    if let Some(secondaries) = links.get(&dev.device_id).or_else(|| links.get(&mac)) {
        if !secondaries.is_empty() {
            return None;
        }
    }

    for other in all_devices {
        if other.device_id == dev.device_id {
            continue;
        }
        let other_name = other.display_name.to_lowercase();
        let other_host = other.metadata.get("hostname").cloned().unwrap_or_default().to_lowercase();
        let other_ip = other.metadata.get("ip").cloned().unwrap_or_default();

        if other_ip == ip {
            continue;
        }

        // Heuristic 1: MacBook Pro abbreviation match (e.g. macbookprom2 vs mbp-m2-2)
        let is_mbp_match = (name.contains("macbook") || host.contains("macbook"))
            && (other_name.contains("mbp") || other_host.contains("mbp"));

        // Heuristic 2: Suffix match (e.g. host and host-2 or host-wlan)
        let clean_name1 = name.replace("-2", "").replace(".fritz.box", "");
        let clean_name2 = other_name.replace("-2", "").replace(".fritz.box", "");
        let is_suffix_match = clean_name1 == clean_name2 && (name.contains("-2") || other_name.contains("-2"));

        if is_mbp_match || is_suffix_match {
            return Some(other.clone());
        }
    }

    None
}

fn build_unified_devices(
    devices: &[DeviceRecord],
    links: &HashMap<String, Vec<String>>,
) -> Vec<UnifiedDevice> {
    let mut dev_map: HashMap<String, DeviceRecord> = HashMap::new();
    let mut mac_map: HashMap<String, String> = HashMap::new();

    for d in devices {
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
    for d in devices {
        if secondary_ids.contains(&d.device_id) {
            continue;
        }
        let mac = d.metadata.get("mac").cloned().unwrap_or_default();
        if !mac.is_empty() && secondary_ids.contains(&mac) {
            continue;
        }

        let mut secondaries = Vec::new();
        let configured_secs = links.get(&d.device_id).or_else(|| links.get(&mac));
        if let Some(list) = configured_secs {
            for sec_id in list {
                let actual_id = mac_map.get(sec_id).unwrap_or(sec_id);
                if let Some(sec_dev) = dev_map.get(actual_id) {
                    secondaries.push(sec_dev.clone());
                }
            }
        }

        let candidate = if secondaries.is_empty() {
            detect_merge_candidate(d, devices, links)
        } else {
            None
        };

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

fn page_layout(title: &str, current_tab: &str, content: &str) -> String {
    let devices_active = if current_tab == "devices" { "class=\"active\"" } else { "" };
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
            flex-wrap: wrap;
            gap: 16px;
            margin-bottom: 20px;
            padding-bottom: 14px;
            border-bottom: 1px solid var(--border);
        }}
        h1 {{ font-size: 22px; font-weight: 700; }}
        nav {{ display: flex; gap: 8px; }}
        nav a {{
            text-decoration: none;
            padding: 6px 14px;
            border-radius: 6px;
            font-size: 14px;
            font-weight: 500;
            color: var(--muted);
        }}
        nav a.active {{
            background: var(--primary);
            color: #ffffff;
        }}
        nav a:hover:not(.active) {{
            background: var(--badge-bg);
            color: var(--text);
        }}
        .card {{
            background: var(--surface);
            border: 1px solid var(--border);
            border-radius: 8px;
            padding: 16px 20px;
            margin-bottom: 16px;
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
        .btn {{
            display: inline-flex;
            align-items: center;
            gap: 6px;
            padding: 6px 14px;
            font-size: 13px;
            font-weight: 500;
            border-radius: 6px;
            border: 1px solid transparent;
            cursor: pointer;
            text-decoration: none;
            transition: all 0.15s ease;
        }}
        .btn-primary {{
            background: var(--primary);
            color: #ffffff;
        }}
        .btn-primary:hover {{ filter: brightness(1.1); }}
        .btn-sm {{
            padding: 3px 8px;
            font-size: 11px;
            border-radius: 4px;
            border: 1px solid var(--border);
            background: var(--surface);
            color: var(--text);
            cursor: pointer;
            text-decoration: none;
            display: inline-flex;
            align-items: center;
            gap: 4px;
        }}
        .btn-sm:hover {{ border-color: var(--primary); color: var(--primary); }}
        .btn-web {{
            background: #dbeafe;
            color: #1d4ed8;
            border-color: #bfdbfe;
        }}
        .btn-web:hover {{ background: #bfdbfe; color: #1e40af; }}
        @media (prefers-color-scheme: dark) {{
            .btn-web {{ background: #1e3a8a; color: #bfdbfe; border-color: #1e40af; }}
        }}
        
        .search-input {{
            padding: 6px 12px;
            font-size: 13px;
            border-radius: 6px;
            border: 1px solid var(--border);
            background: var(--surface);
            color: var(--text);
            width: 240px;
        }}
        .search-input:focus {{ outline: none; border-color: var(--primary); }}
        
        .pills {{
            display: flex;
            flex-wrap: wrap;
            gap: 5px;
            margin-bottom: 16px;
        }}
        .pill {{
            display: inline-flex;
            align-items: center;
            gap: 5px;
            padding: 4px 10px;
            font-size: 12px;
            border-radius: 9999px;
            border: 1px solid var(--border);
            background: var(--surface);
            color: var(--muted);
            cursor: pointer;
            transition: all 0.15s ease;
        }}
        .pill:hover {{ border-color: var(--primary); color: var(--text); }}
        .pill.active {{
            background: var(--primary);
            border-color: var(--primary);
            color: #ffffff;
            font-weight: 500;
        }}
        
        .group-header {{
            display: flex;
            align-items: center;
            justify-content: space-between;
            margin-bottom: 8px;
        }}
        .group-header h3 {{
            font-size: 14px;
            display: flex;
            align-items: center;
            gap: 6px;
        }}
        
        /* Inspector Styles */
        .inspector-sec {{
            margin-top: 14px;
            padding-top: 12px;
            border-top: 1px solid var(--border);
        }}
        .inspector-title {{
            font-size: 12px;
            font-weight: 600;
            color: var(--muted);
            text-transform: uppercase;
            margin-bottom: 8px;
            display: flex;
            align-items: center;
            justify-content: space-between;
        }}
        .iface-card {{
            background: var(--bg);
            border: 1px solid var(--border);
            border-radius: 6px;
            padding: 8px 10px;
            margin-bottom: 6px;
            font-size: 12px;
        }}
        .merge-box {{
            background: #fefce8;
            border: 1px solid #fef08a;
            color: #854d0e;
            border-radius: 6px;
            padding: 10px;
            margin-top: 8px;
            font-size: 12px;
        }}
        @media (prefers-color-scheme: dark) {{
            .merge-box {{ background: #422006; border-color: #854d0e; color: #fef08a; }}
        }}
        .form-control {{
            width: 100%;
            padding: 6px 10px;
            border-radius: 6px;
            border: 1px solid var(--border);
            background: var(--bg);
            color: var(--text);
            font-family: inherit;
            font-size: 13px;
        }}
        textarea.form-control {{ min-height: 80px; resize: vertical; }}
        .code-box {{
            background: var(--code-bg);
            border: 1px solid var(--border);
            border-radius: 6px;
            padding: 10px;
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
            <a href="/" {devices_active}>Detected Devices</a>
            <a href="/status" {status_active}>System Status</a>
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
        "phone" => ("Smartphones", "📱"),
        "tablet" => ("Tablets", "📟"),
        "voip-phone" => ("VoIP Phones", "☎️"),
        "computer" => ("Computers & Laptops", "💻"),
        "nas" => ("Network Storage & NAS", "🗄️"),
        "router" => ("Routers & Gateways", "🌐"),
        "lighting" => ("Smart Lighting", "💡"),
        "smart-plug" => ("Smart Plugs & Sockets", "🔌"),
        "display" => ("Smart Clocks & Displays", "⏰"),
        "sensor" => ("Sensors & Detectors", "👁️"),
        "energy" => ("Solar & Energy Systems", "☀️"),
        "appliance" => ("Home Appliances", "🧺"),
        "radio" => ("LoRa & Mesh Radios", "📻"),
        "hub" => ("Smart Home Hubs", "🎛️"),
        "iot" => ("Smart Home & IoT", "💡"),
        "wearable" => ("Wearables", "⌚"),
        "camera" => ("Cameras", "📷"),
        "audio" => ("Audio & Speakers", "🔊"),
        "printer" => ("Printers", "🖨️"),
        "streaming" => ("TV & Streaming", "📺"),
        _ => ("Network & Other Devices", "🔌"),
    }
}

fn category_sort_order(key: &str) -> u32 {
    match key {
        "phone" => 1,
        "tablet" => 2,
        "voip-phone" => 3,
        "computer" => 4,
        "nas" => 5,
        "router" => 6,
        "lighting" => 7,
        "smart-plug" => 8,
        "display" => 9,
        "sensor" => 10,
        "energy" => 11,
        "appliance" => 12,
        "radio" => 13,
        "hub" => 14,
        "iot" => 15,
        "wearable" => 16,
        "camera" => 17,
        "audio" => 18,
        "printer" => 19,
        "streaming" => 20,
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

fn render_devices_page(
    title: &str,
    snapshot: &RuntimeSnapshot,
    docs: &HashMap<String, DeviceDocumentation>,
    links: &HashMap<String, Vec<String>>,
) -> String {
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

    let mut category_map: HashMap<String, DynamicCategory> = HashMap::new();
    for udev in &unified_devices {
        let dev = &udev.primary;
        let cat_key = dev
            .metadata
            .get("category")
            .cloned()
            .unwrap_or_else(|| dev.kind.clone());

        let (fallback_title, fallback_icon) = default_category_presentation(&cat_key);
        let title = dev
            .metadata
            .get("category_title")
            .cloned()
            .unwrap_or_else(|| fallback_title.to_string());
        let icon = dev
            .metadata
            .get("category_icon")
            .cloned()
            .unwrap_or_else(|| fallback_icon.to_string());

        let entry = category_map
            .entry(cat_key.clone())
            .or_insert_with(|| DynamicCategory {
                key: cat_key,
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
            let category = p.metadata.get("category").cloned().unwrap_or_else(|| p.kind.clone());
            let (fallback_title, fallback_icon) = default_category_presentation(&category);
            let category_title = p.metadata.get("category_title").cloned().unwrap_or_else(|| fallback_title.to_string());
            let category_icon = p.metadata.get("category_icon").cloned().unwrap_or_else(|| fallback_icon.to_string());

            let doc_key = if !mac.is_empty() { mac.clone() } else { p.device_id.clone() };
            let doc = docs.get(&doc_key).cloned().unwrap_or_default();

            let secondaries_json: Vec<serde_json::Value> = udev.secondary_interfaces.iter().map(|s| {
                serde_json::json!({
                    "device_id": s.device_id,
                    "name": s.display_name,
                    "ip": s.metadata.get("ip").cloned().unwrap_or_default(),
                    "mac": s.metadata.get("mac").cloned().unwrap_or_default(),
                    "vendor": s.metadata.get("vendor").cloned().unwrap_or_default(),
                    "hostname": s.metadata.get("hostname").cloned().unwrap_or_default(),
                })
            }).collect();

            let candidate_json = udev.merge_candidate.as_ref().map(|c| {
                serde_json::json!({
                    "device_id": c.device_id,
                    "name": c.display_name,
                    "ip": c.metadata.get("ip").cloned().unwrap_or_default(),
                    "mac": c.metadata.get("mac").cloned().unwrap_or_default(),
                    "hostname": c.metadata.get("hostname").cloned().unwrap_or_default(),
                })
            });

            serde_json::json!({
                "device_id": p.device_id,
                "display_name": p.display_name,
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
                "manual_url": doc.manual_url.unwrap_or_default(),
                "updated_at": doc.updated_at,
                "secondaries": secondaries_json,
                "candidate": candidate_json,
            })
        })
        .collect();

    let client_devices_json = serde_json::to_string(&client_devices).unwrap_or_else(|_| "[]".to_string());

    // Right side device list groups
    let mut group_cards_html = String::new();
    for cat in &categories {
        let rows = cat
            .devices
            .iter()
            .map(|udev| {
                let device = &udev.primary;
                let ip = device.metadata.get("ip").cloned().unwrap_or_else(|| "-".to_string());
                let mac = device.metadata.get("mac").cloned().unwrap_or_default();
                let vendor = device.metadata.get("vendor").cloned().unwrap_or_default();
                let web_url = device.metadata.get("web_url").cloned();

                let doc_key = if !mac.is_empty() { mac.clone() } else { device.device_id.clone() };
                let has_docs = docs.get(&doc_key).is_some_and(|d| !d.notes.trim().is_empty());

                let mut iface_badges = String::new();
                if !udev.secondary_interfaces.is_empty() {
                    iface_badges.push_str(&format!(
                        r#" <span class="badge badge-dual" title="Multi-interface device ({} interfaces)">LAN + WLAN ({})</span>"#,
                        udev.secondary_interfaces.len() + 1,
                        udev.secondary_interfaces.len() + 1
                    ));
                }

                let network_info = if mac.is_empty() {
                    format!("<code>{ip}</code>{iface_badges}")
                } else if vendor.is_empty() {
                    format!("<code>{ip}</code>{iface_badges}<br><small style=\"color:var(--muted)\">{mac}</small>")
                } else {
                    format!("<code>{ip}</code>{iface_badges}<br><small style=\"color:var(--muted)\">{mac} &bull; {vendor}</small>")
                };

                let web_button = if let Some(url) = web_url {
                    format!(r#"<a href="{url}" target="_blank" class="btn-sm btn-web" onclick="event.stopPropagation()" title="Open Web Interface">🌐 Web UI</a>"#)
                } else {
                    String::new()
                };

                let doc_icon = if has_docs { r#" <span style="color:var(--status-green); font-size:11px;" title="Documentation saved">📝✓</span>"# } else { "" };

                format!(
                    r#"<tr class="device-item" data-id="{}" onclick="selectDevice('{}')">
                        <td><strong>{}</strong>{doc_icon}<br><small style="color:var(--muted)">{} &bull; {}</small></td>
                        <td><span class="badge badge-kind">{}</span></td>
                        <td>{}</td>
                        <td style="text-align:right;">{}</td>
                    </tr>"#,
                    device.device_id,
                    device.device_id,
                    device.display_name,
                    device.device_id,
                    device.module_id,
                    device.kind,
                    network_info,
                    web_button,
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
                                <th>Category</th>
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

    let script = format!(
        r#"
    <script>
    const allDevices = {};
    let currentCategory = 'all';
    let selectedDeviceId = null;

    function selectCategory(cat, el) {{
        currentCategory = cat;
        document.querySelectorAll('.pill').forEach(p => p.classList.remove('active'));
        el.classList.add('active');
        filterDevices();
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
                if (matchesSearch) {{
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

        // Secondary / Linked Interfaces
        let ifacesHtml = `
            <div class="iface-card">
                <strong>Primary Interface (LAN/Main)</strong><br>
                <code>${{dev.ip}}</code> ${{dev.mac ? '&bull; <small>' + dev.mac + '</small>' : ''}}<br>
                <small style="color:var(--muted)">${{dev.vendor || 'Unknown Vendor'}} ${{dev.hostname ? '&bull; ' + dev.hostname : ''}}</small>
            </div>
        `;

        if (dev.secondaries && dev.secondaries.length > 0) {{
            dev.secondaries.forEach(sec => {{
                ifacesHtml += `
                    <div class="iface-card">
                        <div style="display:flex; justify-content:space-between; align-items:center;">
                            <strong>Linked Interface (WLAN/Secondary)</strong>
                            <button class="btn-sm" style="color:var(--status-red); border:none; padding:2px;" onclick="unlinkInterface('${{dev.device_id}}', '${{sec.device_id}}')">Unlink</button>
                        </div>
                        <code>${{sec.ip}}</code> ${{sec.mac ? '&bull; <small>' + sec.mac + '</small>' : ''}}<br>
                        <small style="color:var(--muted)">${{sec.vendor || 'Unknown Vendor'}} ${{sec.hostname ? '&bull; ' + sec.hostname : ''}}</small>
                    </div>
                `;
            }});
        }}

        // Candidate merge box
        let candidateBox = '';
        if (dev.candidate) {{
            candidateBox = `
                <div class="merge-box">
                    <strong>💡 Dual-Homed Candidate:</strong><br>
                    <span>${{dev.candidate.name}} (<code>${{dev.candidate.ip}}</code>)</span><br>
                    <button class="btn btn-sm btn-primary" style="margin-top:6px;" onclick="linkInterface('${{dev.device_id}}', '${{dev.candidate.device_id}}')">
                        🔗 Merge Interfaces (LAN + WLAN)
                    </button>
                </div>
            `;
        }}

        panel.innerHTML = `
            <div>
                <div style="display:flex; align-items:center; gap:8px; margin-bottom:4px;">
                    <span style="font-size:24px;">${{dev.category_icon}}</span>
                    <div>
                        <h3 style="font-size:16px;">${{dev.display_name}}</h3>
                        <span class="badge badge-kind">${{dev.category_title}}</span>
                    </div>
                </div>
                ${{webBtn}}
            </div>

            <!-- Network Interfaces -->
            <div class="inspector-sec">
                <div class="inspector-title">
                    <span>Network Interfaces (${{(dev.secondaries ? dev.secondaries.length : 0) + 1}})</span>
                </div>
                ${{ifacesHtml}}
                ${{candidateBox}}
            </div>

            <!-- Documentation & Notes -->
            <div class="inspector-sec">
                <div class="inspector-title">
                    <span>Documentation & Notes</span>
                    <span id="doc-status" style="color:var(--status-green); font-size:11px;"></span>
                </div>
                <div style="margin-bottom:8px;">
                    <label style="font-size:11px; color:var(--muted); font-weight:600;">Manual / Documentation URL</label>
                    <input type="url" id="insp-manual-url" class="form-control" value="${{escapeAttr(dev.manual_url)}}" placeholder="https://..." />
                </div>
                <div style="margin-bottom:8px;">
                    <label style="font-size:11px; color:var(--muted); font-weight:600;">Device Notes (Markdown)</label>
                    <textarea id="insp-notes" class="form-control" placeholder="Installation location, credentials hint, firmware version...">${{escapeHtml(dev.notes)}}</textarea>
                </div>
                <button type="button" class="btn btn-sm btn-primary" onclick="saveInspectorNotes('${{dev.doc_key}}')">💾 Save Notes</button>
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

    async function saveInspectorNotes(docKey) {{
        const notes = document.getElementById('insp-notes').value;
        const manualUrl = document.getElementById('insp-manual-url').value;
        const statusEl = document.getElementById('doc-status');

        statusEl.innerText = 'Saving...';
        try {{
            const res = await fetch('/api/devices/' + encodeURIComponent(docKey) + '/documentation', {{
                method: 'POST',
                headers: {{ 'Content-Type': 'application/json' }},
                body: JSON.stringify({{ notes: notes, manual_url: manualUrl }})
            }});
            if (res.ok) {{
                statusEl.innerText = 'Saved!';
                const dev = allDevices.find(d => d.doc_key === docKey);
                if (dev) {{
                    dev.notes = notes;
                    dev.manual_url = manualUrl;
                }}
                setTimeout(() => {{ statusEl.innerText = ''; }}, 2000);
            }} else {{
                statusEl.innerText = 'Error saving';
            }}
        }} catch (e) {{
            statusEl.innerText = 'Network error';
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
        client_devices_json
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
        <div class="pills">{}</div>
        
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

        {}"#,
        unified_devices.len(),
        pills_html,
        group_cards_html,
        script
    );

    page_layout(title, "devices", &content)
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

    let content = format!(
        r#"<div class="card"><h2>Integration Modules ({})</h2><div style="overflow-x:auto"><table><thead><tr><th>Module</th><th>Status</th><th>Version</th><th>Health / Details</th></tr></thead><tbody>{}</tbody></table></div></div>"#,
        snapshot.modules.len(),
        rows
    );

    page_layout(title, "status", &content)
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
    String::from("127.0.0.1:8080")
}

fn default_status_title() -> String {
    String::from("HomeNode Server")
}
