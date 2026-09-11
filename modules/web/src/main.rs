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
        .with_state(WebState {
            socket_path: env.socket_path,
            status_title: config.status_title,
        });

    axum::serve(listener, app).await?;
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

fn render_devices_page(title: &str, snapshot: &RuntimeSnapshot) -> String {
    let content = if snapshot.devices.is_empty() {
        r#"<div class="card"><div class="empty-state"><h3>No devices detected yet</h3><p>Connected integration modules will automatically list discovered devices here.</p></div></div>"#.to_string()
    } else {
        let rows = snapshot.devices.iter().map(|device| {
            let caps = device.capabilities.iter().map(|c| format!("<span class=\"badge\">{c}</span>")).collect::<Vec<_>>().join(" ");
            format!(
                "<tr><td><strong>{}</strong><br><small style=\"color:var(--muted)\">{}</small></td><td><span class=\"badge badge-kind\">{}</span></td><td><span class=\"badge\">{}</span></td><td>{}</td></tr>",
                device.display_name,
                device.device_id,
                device.kind,
                device.module_id,
                if caps.is_empty() { String::from("-") } else { caps },
            )
        }).collect::<Vec<_>>().join("");

        format!(
            r#"<div class="card"><h2>Detected Devices ({})</h2><div style="overflow-x:auto"><table><thead><tr><th>Device</th><th>Kind</th><th>Source Module</th><th>Capabilities</th></tr></thead><tbody>{}</tbody></table></div></div>"#,
            snapshot.devices.len(),
            rows
        )
    };

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
