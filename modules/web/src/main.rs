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

use homenode_sdk::proto::{Empty, HealthState, ModuleRegistration, RuntimeSnapshot};
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
    definitions_dir: PathBuf,
    docs_store: Arc<RwLock<HashMap<String, DeviceDocumentation>>>,
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
    let definitions_dir = workspace_root.join("definitions").join("devices");

    let initial_docs = load_documentation(&docs_path);
    let docs_store = Arc::new(RwLock::new(initial_docs));

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
        .route("/api/definitions/save", post(save_definition_handler))
        .with_state(WebState {
            socket_path: env.socket_path,
            status_title: config.status_title,
            docs_path,
            definitions_dir,
            docs_store,
        });

    axum::serve(listener, app).await?;
    Ok(())
}

fn load_documentation(path: &Path) -> HashMap<String, DeviceDocumentation> {
    if !path.exists() {
        return HashMap::new();
    }
    match std::fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
        Err(err) => {
            warn!("Failed to read device documentation from {}: {err}", path.display());
            HashMap::new()
        }
    }
}

fn persist_documentation(path: &Path, docs: &HashMap<String, DeviceDocumentation>) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let data = serde_json::to_string_pretty(docs)?;
    std::fs::write(path, data)?;
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
    let body = match load_snapshot(&state.socket_path).await {
        Ok(snapshot) => render_devices_page(&state.status_title, &snapshot, &docs),
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
// Documentation API handlers
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
    if let Err(err) = persist_documentation(&state.docs_path, &store) {
        error!("Failed to persist device documentation: {err}");
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": err.to_string()})),
        )
            .into_response();
    }
    Json(serde_json::json!({"status": "saved"})).into_response()
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

    // Probe common IoT, web, and server ports
    let probe_ports = [21, 22, 23, 53, 80, 443, 554, 1883, 5000, 5001, 5060, 8080, 8443, 8883, 9000];
    let mut join_set = tokio::task::JoinSet::new();
    for port in probe_ports {
        join_set.spawn(async move {
            let addr = SocketAddr::new(IpAddr::V4(ip), port);
            let open = tokio::time::timeout(Duration::from_millis(180), TcpStream::connect(addr))
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

    // Inspect HTTP banners on open web ports
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

    // Suggest category and generate Rhai script
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
        22 => "SSH Terminal",
        23 => "Telnet",
        53 => "DNS Server",
        80 => "HTTP Web Interface",
        443 => "HTTPS Web Interface",
        554 => "RTSP Video Stream",
        1883 => "MQTT Broker/Client",
        5000 => "Synology DSM Web UI",
        5001 => "Synology DSM HTTPS",
        5060 => "SIP VoIP Telephony",
        8080 => "HTTP Alt Web Interface",
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
// HTML Rendering
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
            padding: 24px;
        }}
        .container {{ max-width: 1040px; margin: 0 auto; }}
        header {{
            display: flex;
            align-items: center;
            justify-content: space-between;
            flex-wrap: wrap;
            gap: 16px;
            margin-bottom: 24px;
            padding-bottom: 16px;
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
            padding: 20px;
            margin-bottom: 20px;
        }}
        h2 {{ font-size: 16px; font-weight: 600; margin-bottom: 16px; }}
        table {{
            width: 100%;
            border-collapse: collapse;
            font-size: 14px;
            text-align: left;
        }}
        th, td {{
            padding: 10px 12px;
            border-bottom: 1px solid var(--border);
            vertical-align: middle;
        }}
        th {{
            font-size: 12px;
            font-weight: 600;
            color: var(--muted);
            text-transform: uppercase;
        }}
        tr:last-child td {{ border-bottom: none; }}
        .badge {{
            display: inline-block;
            padding: 2px 8px;
            font-size: 12px;
            border-radius: 9999px;
            background: var(--badge-bg);
            color: var(--badge-text);
        }}
        .badge-kind {{
            background: #e0e7ff;
            color: #3730a3;
        }}
        @media (prefers-color-scheme: dark) {{
            .badge-kind {{ background: #312e81; color: #c7d2fe; }}
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
        .empty-state {{
            text-align: center;
            padding: 40px 16px;
            color: var(--muted);
        }}
        .toolbar {{
            display: flex;
            align-items: center;
            justify-content: space-between;
            flex-wrap: wrap;
            gap: 12px;
            margin-bottom: 16px;
        }}
        .btn {{
            display: inline-flex;
            align-items: center;
            gap: 6px;
            padding: 8px 16px;
            font-size: 14px;
            font-weight: 500;
            border-radius: 6px;
            border: 1px solid transparent;
            cursor: pointer;
            text-decoration: none;
            transition: background-color 0.15s ease, opacity 0.15s ease;
        }}
        .btn-primary {{
            background: var(--primary);
            color: #ffffff;
        }}
        .btn-primary:hover {{
            filter: brightness(1.1);
        }}
        .btn-sm {{
            display: inline-flex;
            align-items: center;
            gap: 4px;
            padding: 4px 8px;
            font-size: 12px;
            font-weight: 500;
            border-radius: 4px;
            border: 1px solid var(--border);
            background: var(--surface);
            color: var(--text);
            cursor: pointer;
            text-decoration: none;
            transition: all 0.15s ease;
            white-space: nowrap;
        }}
        .btn-sm:hover {{
            border-color: var(--primary);
            color: var(--primary);
        }}
        .btn-web {{
            background: #dbeafe;
            color: #1d4ed8;
            border-color: #bfdbfe;
        }}
        .btn-web:hover {{
            background: #bfdbfe;
            color: #1e40af;
        }}
        @media (prefers-color-scheme: dark) {{
            .btn-web {{
                background: #1e3a8a;
                color: #bfdbfe;
                border-color: #1e40af;
            }}
        }}
        .btn-doc-active {{
            border-color: var(--status-green);
            color: var(--status-green);
            font-weight: 600;
        }}
        .search-input {{
            padding: 8px 14px;
            font-size: 14px;
            border-radius: 6px;
            border: 1px solid var(--border);
            background: var(--surface);
            color: var(--text);
            width: 260px;
            max-width: 100%;
        }}
        .search-input:focus {{
            outline: none;
            border-color: var(--primary);
        }}
        .pills {{
            display: flex;
            flex-wrap: wrap;
            gap: 6px;
            margin-bottom: 20px;
        }}
        .pill {{
            display: inline-flex;
            align-items: center;
            gap: 6px;
            padding: 6px 12px;
            font-size: 13px;
            border-radius: 9999px;
            border: 1px solid var(--border);
            background: var(--surface);
            color: var(--muted);
            cursor: pointer;
            transition: all 0.15s ease;
        }}
        .pill:hover {{
            border-color: var(--primary);
            color: var(--text);
        }}
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
            margin-bottom: 12px;
        }}
        .group-header h3 {{
            font-size: 15px;
            font-weight: 600;
            display: flex;
            align-items: center;
            gap: 8px;
        }}
        .action-cell {{
            display: flex;
            gap: 6px;
            align-items: center;
            flex-wrap: wrap;
        }}
        /* Modal Styles */
        .modal-overlay {{
            position: fixed;
            top: 0; left: 0; right: 0; bottom: 0;
            background: rgba(0,0,0,0.5);
            display: none;
            align-items: center;
            justify-content: center;
            z-index: 1000;
            padding: 16px;
        }}
        .modal-overlay.active {{ display: flex; }}
        .modal {{
            background: var(--surface);
            border: 1px solid var(--border);
            border-radius: 10px;
            width: 600px;
            max-width: 100%;
            max-height: 90vh;
            overflow-y: auto;
            box-shadow: 0 10px 25px rgba(0,0,0,0.2);
            padding: 24px;
        }}
        .modal-header {{
            display: flex;
            align-items: center;
            justify-content: space-between;
            margin-bottom: 16px;
            border-bottom: 1px solid var(--border);
            padding-bottom: 12px;
        }}
        .modal-close {{
            background: none;
            border: none;
            font-size: 20px;
            cursor: pointer;
            color: var(--muted);
        }}
        .form-group {{
            margin-bottom: 14px;
        }}
        .form-group label {{
            display: block;
            font-size: 13px;
            font-weight: 600;
            color: var(--muted);
            margin-bottom: 6px;
        }}
        .form-control {{
            width: 100%;
            padding: 8px 12px;
            border-radius: 6px;
            border: 1px solid var(--border);
            background: var(--bg);
            color: var(--text);
            font-family: inherit;
            font-size: 14px;
        }}
        textarea.form-control {{
            min-height: 120px;
            resize: vertical;
        }}
        .code-box {{
            background: var(--code-bg);
            border: 1px solid var(--border);
            border-radius: 6px;
            padding: 12px;
            font-family: monospace;
            font-size: 12px;
            overflow-x: auto;
            white-space: pre;
            margin-top: 10px;
            max-height: 260px;
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

<!-- Documentation Modal -->
<div id="docs-modal" class="modal-overlay">
    <div class="modal">
        <div class="modal-header">
            <h3 id="docs-modal-title">Device Documentation</h3>
            <button class="modal-close" onclick="closeModal('docs-modal')">&times;</button>
        </div>
        <form id="docs-form" onsubmit="saveDeviceDocs(event)">
            <input type="hidden" id="docs-device-id" />
            <div class="form-group">
                <label for="docs-notes">Notes & Documentation (Markdown supported)</label>
                <textarea id="docs-notes" class="form-control" placeholder="Installation location, credentials hint, firmware version, serial number..."></textarea>
            </div>
            <div class="form-group">
                <label for="docs-manual-url">Manual or Web Documentation URL</label>
                <input type="url" id="docs-manual-url" class="form-control" placeholder="https://..." />
            </div>
            <div style="display:flex; justify-content:space-between; align-items:center; margin-top:16px;">
                <span id="docs-status" style="font-size:13px; color:var(--status-green);"></span>
                <div style="display:flex; gap:8px;">
                    <button type="button" class="btn btn-sm" onclick="closeModal('docs-modal')">Cancel</button>
                    <button type="submit" class="btn btn-primary">Save Notes</button>
                </div>
            </div>
        </form>
    </div>
</div>

<!-- Device Analyzer Modal -->
<div id="analyze-modal" class="modal-overlay">
    <div class="modal">
        <div class="modal-header">
            <h3 id="analyze-modal-title">Device Analyzer</h3>
            <button class="modal-close" onclick="closeModal('analyze-modal')">&times;</button>
        </div>
        <div id="analyze-body">
            <div style="text-align:center; padding:30px 0;">
                <p>🔍 Probing ports, HTTP banners, and device fingerprints...</p>
            </div>
        </div>
    </div>
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
    devices: Vec<homenode_sdk::proto::DeviceRecord>,
}

fn render_devices_page(
    title: &str,
    snapshot: &RuntimeSnapshot,
    docs: &HashMap<String, DeviceDocumentation>,
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

    let mut category_map: HashMap<String, DynamicCategory> = HashMap::new();

    for device in &snapshot.devices {
        let cat_key = device
            .metadata
            .get("category")
            .cloned()
            .unwrap_or_else(|| device.kind.clone());

        let (fallback_title, fallback_icon) = default_category_presentation(&cat_key);
        let title = device
            .metadata
            .get("category_title")
            .cloned()
            .unwrap_or_else(|| fallback_title.to_string());
        let icon = device
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

        entry.devices.push(device.clone());
    }

    let mut categories: Vec<_> = category_map.into_values().collect();
    categories.sort_by_key(|c| (category_sort_order(&c.key), c.title.clone()));

    // Filter pills
    let mut pills_html = format!(
        r#"<button type="button" class="pill active" onclick="selectCategory('all', this)">All ({})</button>"#,
        snapshot.devices.len()
    );

    for cat in &categories {
        pills_html.push_str(&format!(
            r#"<button type="button" class="pill" onclick="selectCategory('{}', this)">{} {} ({})</button>"#,
            cat.key, cat.icon, cat.title, cat.devices.len()
        ));
    }

    // Group cards
    let mut group_cards_html = String::new();
    for cat in &categories {
        let rows = cat
            .devices
            .iter()
            .map(|device| {
                let ip = device
                    .metadata
                    .get("ip")
                    .cloned()
                    .unwrap_or_else(|| "-".to_string());
                let mac = device.metadata.get("mac").cloned().unwrap_or_default();
                let vendor = device.metadata.get("vendor").cloned().unwrap_or_default();
                let web_url = device.metadata.get("web_url").cloned().or_else(|| {
                    let cat = device.metadata.get("category").map(|s| s.as_str()).unwrap_or("");
                    if cat == "nas" {
                        Some(format!("http://{ip}:5000"))
                    } else if cat == "router"
                        || cat == "smart-plug"
                        || cat == "display"
                        || cat == "radio"
                        || cat == "energy"
                        || cat == "appliance"
                        || cat == "sensor"
                    {
                        Some(format!("http://{ip}"))
                    } else {
                        None
                    }
                });

                let doc_key = if !mac.is_empty() {
                    mac.clone()
                } else {
                    device.device_id.clone()
                };
                let has_docs = docs.get(&doc_key).is_some_and(|d| !d.notes.trim().is_empty());

                let caps = device
                    .capabilities
                    .iter()
                    .map(|c| format!("<span class=\"badge\">{c}</span>"))
                    .collect::<Vec<_>>()
                    .join(" ");

                let network_info = if mac.is_empty() {
                    format!("<code>{ip}</code>")
                } else if vendor.is_empty() {
                    format!("<code>{ip}</code><br><small style=\"color:var(--muted)\">{mac}</small>")
                } else {
                    format!("<code>{ip}</code><br><small style=\"color:var(--muted)\">{mac} &bull; {vendor}</small>")
                };

                let escaped_name = device.display_name.replace('\'', "\\'");
                let mut actions_html = String::new();

                if let Some(url) = web_url {
                    actions_html.push_str(&format!(
                        r#"<a href="{url}" target="_blank" class="btn-sm btn-web" title="Open Web Interface">🌐 Web UI</a> "#
                    ));
                }

                let doc_btn_class = if has_docs {
                    "btn-sm btn-doc-active"
                } else {
                    "btn-sm"
                };
                let doc_label = if has_docs { "📝 Docs ✓" } else { "📝 Docs" };

                actions_html.push_str(&format!(
                    r#"<button type="button" class="{doc_btn_class}" onclick="openDocsModal('{doc_key}', '{escaped_name}')">{doc_label}</button> "#
                ));

                actions_html.push_str(&format!(
                    r#"<button type="button" class="btn-sm" onclick="analyzeDevice('{}', '{}')">🔍 Analyze</button>"#,
                    device.device_id, escaped_name
                ));

                format!(
                    "<tr><td><strong>{}</strong><br><small style=\"color:var(--muted)\">{}</small></td><td><span class=\"badge badge-kind\">{}</span></td><td>{}</td><td>{}</td><td><div class=\"action-cell\">{}</div></td></tr>",
                    device.display_name,
                    device.device_id,
                    device.kind,
                    network_info,
                    if caps.is_empty() { String::from("-") } else { caps },
                    actions_html,
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
                                <th>Capabilities</th>
                                <th>Actions</th>
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

    let script = r#"
    <script>
    let currentCategory = 'all';

    function selectCategory(cat, el) {
        currentCategory = cat;
        document.querySelectorAll('.pill').forEach(p => p.classList.remove('active'));
        el.classList.add('active');
        filterDevices();
    }

    function filterDevices() {
        const q = (document.getElementById('device-search').value || '').toLowerCase();
        const groups = document.querySelectorAll('.device-group');

        groups.forEach(group => {
            const cat = group.getAttribute('data-category');
            const matchesCat = (currentCategory === 'all' || currentCategory === cat);

            let visibleRows = 0;
            const rows = group.querySelectorAll('tbody tr');
            rows.forEach(row => {
                const text = row.innerText.toLowerCase();
                const matchesSearch = !q || text.includes(q);
                if (matchesSearch) {
                    row.style.display = '';
                    visibleRows++;
                } else {
                    row.style.display = 'none';
                }
            });

            if (matchesCat && visibleRows > 0) {
                group.style.display = '';
            } else {
                group.style.display = 'none';
            }
        });
    }

    const scanForm = document.getElementById('scan-form');
    if (scanForm) {
        scanForm.onsubmit = function() {
            const btn = document.getElementById('scan-btn');
            const label = document.getElementById('scan-label');
            if (btn && label) {
                btn.disabled = true;
                label.innerText = 'Scanning Network...';
                btn.style.opacity = '0.7';
            }
        };
    }

    function openModal(id) {
        document.getElementById(id).classList.add('active');
    }

    function closeModal(id) {
        document.getElementById(id).classList.remove('active');
    }

    async function openDocsModal(deviceId, deviceName) {
        document.getElementById('docs-modal-title').innerText = 'Documentation: ' + deviceName;
        document.getElementById('docs-device-id').value = deviceId;
        document.getElementById('docs-notes').value = '';
        document.getElementById('docs-manual-url').value = '';
        document.getElementById('docs-status').innerText = 'Loading...';
        openModal('docs-modal');

        try {
            const res = await fetch('/api/devices/' + encodeURIComponent(deviceId) + '/documentation');
            if (res.ok) {
                const data = await res.json();
                document.getElementById('docs-notes').value = data.notes || '';
                document.getElementById('docs-manual-url').value = data.manual_url || '';
                document.getElementById('docs-status').innerText = data.updated_at ? 'Last updated: ' + new Date(data.updated_at).toLocaleString() : '';
            } else {
                document.getElementById('docs-status').innerText = '';
            }
        } catch (e) {
            document.getElementById('docs-status').innerText = '';
        }
    }

    async function saveDeviceDocs(event) {
        event.preventDefault();
        const deviceId = document.getElementById('docs-device-id').value;
        const notes = document.getElementById('docs-notes').value;
        const manualUrl = document.getElementById('docs-manual-url').value;
        const statusEl = document.getElementById('docs-status');

        statusEl.innerText = 'Saving...';
        try {
            const res = await fetch('/api/devices/' + encodeURIComponent(deviceId) + '/documentation', {
                method: 'POST',
                headers: { 'Content-Type': 'application/json' },
                body: JSON.stringify({ notes: notes, manual_url: manualUrl })
            });
            if (res.ok) {
                statusEl.innerText = 'Saved successfully! Reload page to update badge.';
                setTimeout(() => closeModal('docs-modal'), 1200);
            } else {
                statusEl.innerText = 'Error saving documentation';
            }
        } catch (e) {
            statusEl.innerText = 'Network error while saving';
        }
    }

    async function analyzeDevice(deviceId, deviceName) {
        document.getElementById('analyze-modal-title').innerText = 'Device Analyzer: ' + deviceName;
        const bodyEl = document.getElementById('analyze-body');
        bodyEl.innerHTML = '<div style="text-align:center; padding:30px 0;"><p>🔍 Probing open ports, service banners, and generating Rhai definition for <strong>' + deviceName + '</strong>...</p></div>';
        openModal('analyze-modal');

        try {
            const res = await fetch('/api/devices/' + encodeURIComponent(deviceId) + '/analyze', { method: 'POST' });
            if (!res.ok) {
                const err = await res.json();
                bodyEl.innerHTML = '<div class="card"><p style="color:var(--status-red)">Analysis failed: ' + (err.error || 'Server error') + '</p></div>';
                return;
            }
            const data = await res.json();

            let portsHtml = '';
            if (data.open_ports && data.open_ports.length > 0) {
                portsHtml = data.open_ports.map(p => '<span class="badge" style="margin-right:4px;">Port ' + p.port + ' (' + p.service + ')</span>').join('');
            } else {
                portsHtml = '<em>No standard TCP ports open</em>';
            }

            let httpInfo = '';
            if (data.http_title || data.http_server) {
                httpInfo = '<div style="margin-top:10px; font-size:13px;">' +
                    (data.http_title ? '<strong>Page Title:</strong> ' + data.http_title + '<br>' : '') +
                    (data.http_server ? '<strong>HTTP Server:</strong> ' + data.http_server + '<br>' : '') +
                    '</div>';
            }

            window._currentAnalyzedScript = {
                filename: data.suggested_filename,
                content: data.suggested_rhai_script
            };

            bodyEl.innerHTML = `
                <div style="margin-bottom:14px;">
                    <strong>Host:</strong> <code>${data.ip}</code> ${data.hostname ? '(' + data.hostname + ')' : ''}<br>
                    <strong>Hardware / Vendor:</strong> ${data.vendor || 'Unknown'} ${data.mac ? '<code>' + data.mac + '</code>' : ''}<br>
                    <strong>Open Services:</strong> <div style="margin-top:6px;">${portsHtml}</div>
                    ${httpInfo}
                    <div style="margin-top:10px;">
                        <strong>Suggested Category:</strong> ${data.suggested_icon} ${data.suggested_title} (<code>${data.suggested_category}</code>)
                    </div>
                </div>
                <div style="display:flex; justify-content:space-between; align-items:center; margin-top:14px;">
                    <strong>Draft Rhai Definition Script (${data.suggested_filename}):</strong>
                    <div>
                        <button class="btn-sm" onclick="copyRhaiScript()">📋 Copy</button>
                        <button class="btn-sm btn-web" id="save-def-btn" onclick="saveAnalyzedDefinition()">💾 Save Definition</button>
                    </div>
                </div>
                <pre class="code-box"><code id="rhai-code-preview">${escapeHtml(data.suggested_rhai_script)}</code></pre>
                <div id="save-def-status" style="font-size:12px; margin-top:6px; color:var(--status-green);"></div>
            `;
        } catch (e) {
            bodyEl.innerHTML = '<div class="card"><p style="color:var(--status-red)">Network error while analyzing device.</p></div>';
        }
    }

    function escapeHtml(text) {
        return text.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
    }

    function copyRhaiScript() {
        if (window._currentAnalyzedScript) {
            navigator.clipboard.writeText(window._currentAnalyzedScript.content);
            alert('Rhai script copied to clipboard!');
        }
    }

    async function saveAnalyzedDefinition() {
        if (!window._currentAnalyzedScript) return;
        const btn = document.getElementById('save-def-btn');
        const status = document.getElementById('save-def-status');
        btn.disabled = true;
        status.innerText = 'Saving definition script...';

        try {
            const res = await fetch('/api/definitions/save', {
                method: 'POST',
                headers: { 'Content-Type': 'application/json' },
                body: JSON.stringify(window._currentAnalyzedScript)
            });
            if (res.ok) {
                status.innerText = 'Definition saved to definitions/devices/' + window._currentAnalyzedScript.filename + '! Click "Scan Network Now" to apply.';
            } else {
                status.innerText = 'Failed to save definition file.';
                btn.disabled = false;
            }
        } catch (e) {
            status.innerText = 'Error saving definition script.';
            btn.disabled = false;
        }
    }
    </script>
    "#;

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
        {}
        {}"#,
        snapshot.devices.len(),
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
