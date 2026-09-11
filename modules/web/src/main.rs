use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::extract::State;
use axum::response::Html;
use axum::routing::get;
use axum::Router;
use serde::Deserialize;
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

#[derive(Clone)]
struct WebState {
    socket_path: std::path::PathBuf,
    status_title: String,
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
        .route("/scan", axum::routing::post(scan_trigger_form_handler))
        .route("/api/scan", axum::routing::post(scan_trigger_api_handler))
        .with_state(WebState {
            socket_path: env.socket_path,
            status_title: config.status_title,
        });

    axum::serve(listener, app).await?;
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
    let body = match load_snapshot(&state.socket_path).await {
        Ok(snapshot) => render_devices_page(&state.status_title, &snapshot),
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
        .container {{ max-width: 960px; margin: 0 auto; }}
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
            transition: background-color 0.15s ease, opacity 0.15s ease;
        }}
        .btn-primary {{
            background: var(--primary);
            color: #ffffff;
        }}
        .btn-primary:hover {{
            filter: brightness(1.1);
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

struct CategoryDef {
    key: &'static str,
    title: &'static str,
    icon: &'static str,
}

const CATEGORIES: &[CategoryDef] = &[
    CategoryDef { key: "phone", title: "Phones", icon: "☎️" },
    CategoryDef { key: "camera", title: "Cameras", icon: "📷" },
    CategoryDef { key: "computer", title: "Computers & Laptops", icon: "💻" },
    CategoryDef { key: "mobile", title: "Mobile Devices", icon: "📱" },
    CategoryDef { key: "wearable", title: "Wearables", icon: "⌚" },
    CategoryDef { key: "audio", title: "Audio & Speakers", icon: "🔊" },
    CategoryDef { key: "iot", title: "Smart Home & IoT", icon: "💡" },
    CategoryDef { key: "printer", title: "Printers", icon: "🖨️" },
    CategoryDef { key: "streaming", title: "TV & Streaming", icon: "📺" },
    CategoryDef { key: "router", title: "Routers & Gateways", icon: "🌐" },
    CategoryDef { key: "network-device", title: "Network Infrastructure & Other", icon: "🔌" },
];

fn render_devices_page(title: &str, snapshot: &RuntimeSnapshot) -> String {
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

    // Group devices by category
    let mut groups: std::collections::HashMap<&'static str, Vec<&homenode_sdk::proto::DeviceRecord>> =
        std::collections::HashMap::new();

    for device in &snapshot.devices {
        let cat_key = CATEGORIES
            .iter()
            .find(|c| c.key == device.kind)
            .map(|c| c.key)
            .unwrap_or("network-device");
        groups.entry(cat_key).or_default().push(device);
    }

    // Render filter pills
    let mut pills_html = format!(
        r#"<button type="button" class="pill active" onclick="selectCategory('all', this)">All ({})</button>"#,
        snapshot.devices.len()
    );

    for cat in CATEGORIES {
        if let Some(list) = groups.get(cat.key) {
            pills_html.push_str(&format!(
                r#"<button type="button" class="pill" onclick="selectCategory('{}', this)">{} {} ({})</button>"#,
                cat.key, cat.icon, cat.title, list.len()
            ));
        }
    }

    // Render group cards
    let mut group_cards_html = String::new();
    for cat in CATEGORIES {
        if let Some(list) = groups.get(cat.key) {
            let rows = list
                .iter()
                .map(|device| {
                    let ip = device
                        .metadata
                        .get("ip")
                        .cloned()
                        .unwrap_or_else(|| "-".to_string());
                    let mac = device.metadata.get("mac").cloned().unwrap_or_default();
                    let vendor = device.metadata.get("vendor").cloned().unwrap_or_default();
                    let iface = device.metadata.get("interface").cloned().unwrap_or_default();
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

                    let iface_badge = if iface.is_empty() {
                        format!("<span class=\"badge\">{}</span>", device.module_id)
                    } else {
                        format!(
                            "<span class=\"badge\">{}</span> <small style=\"color:var(--muted)\">({iface})</small>",
                            device.module_id
                        )
                    };

                    format!(
                        "<tr><td><strong>{}</strong><br><small style=\"color:var(--muted)\">{}</small></td><td><span class=\"badge badge-kind\">{}</span></td><td>{}</td><td>{}</td><td>{}</td></tr>",
                        device.display_name,
                        device.device_id,
                        device.kind,
                        network_info,
                        iface_badge,
                        if caps.is_empty() { String::from("-") } else { caps },
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
                                    <th>Kind</th>
                                    <th>Network (IP / MAC)</th>
                                    <th>Source</th>
                                    <th>Capabilities</th>
                                </tr>
                            </thead>
                            <tbody>{}</tbody>
                        </table>
                    </div>
                </div>"#,
                cat.key,
                cat.icon,
                cat.title,
                list.len(),
                rows
            ));
        }
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
    let rows = snapshot.modules.iter().map(|module| {
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
    }).collect::<Vec<_>>().join("");

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
