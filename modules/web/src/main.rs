use std::path::Path;
use std::time::Duration;

use anyhow::Result;
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

    let app = Router::new().route("/", get(index)).with_state(WebState {
        socket_path: env.socket_path,
        status_title: config.status_title,
    });

    axum::serve(listener, app).await?;
    Ok(())
}

async fn index(State(state): State<WebState>) -> Html<String> {
    let body = match snapshot_markup(&state.socket_path, &state.status_title).await {
        Ok(markup) => markup,
        Err(error) => format!(
            "<html><body><h1>{}</h1><p>failed to load runtime snapshot: {error}</p></body></html>",
            state.status_title
        ),
    };
    Html(body)
}

async fn snapshot_markup(socket_path: &Path, title: &str) -> Result<String> {
    let mut client = connect_control_client(socket_path).await?;
    let snapshot = client.get_runtime_snapshot(Empty {}).await?.into_inner();
    Ok(render_snapshot(title, &snapshot))
}

fn render_snapshot(title: &str, snapshot: &RuntimeSnapshot) -> String {
    let modules = snapshot
        .modules
        .iter()
        .map(|module| {
            let manifest = module.manifest.as_ref();
            let health = module.health.as_ref();
            format!(
                "<li><strong>{}</strong> ({}) - {}{}</li>",
                manifest
                    .map(|manifest| manifest.display_name.clone())
                    .unwrap_or_else(|| String::from("unknown")),
                manifest
                    .map(|manifest| manifest.id.clone())
                    .unwrap_or_else(|| String::from("n/a")),
                health
                    .map(|health| health.message.clone())
                    .unwrap_or_else(|| String::from("no health")),
                if module.connected { "" } else { " [disconnected]" },
            )
        })
        .collect::<Vec<_>>()
        .join("");
    let devices = snapshot
        .devices
        .iter()
        .map(|device| {
            format!(
                "<li><strong>{}</strong> ({}) via {}</li>",
                device.display_name, device.kind, device.module_id
            )
        })
        .collect::<Vec<_>>()
        .join("");

    format!(
        "<html><body><h1>{title}</h1><h2>Modules</h2><ul>{modules}</ul><h2>Devices</h2><ul>{devices}</ul></body></html>"
    )
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
    let raw = std::fs::read_to_string(path)?;
    if raw.trim().is_empty() {
        return Ok(WebConfig::default());
    }
    Ok(toml::from_str(&raw)?)
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
