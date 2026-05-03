use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use serde::Deserialize;
use tracing_subscriber::EnvFilter;

use homenode_sdk::proto::{HealthState, ModuleRegistration};
use homenode_sdk::{connect_control_client, module_health, module_manifest, ModuleEnvironment};

#[derive(Debug, Deserialize)]
struct MatterControllerConfig {
    #[serde(default = "default_health_message")]
    health_message: String,
}

impl Default for MatterControllerConfig {
    fn default() -> Self {
        Self {
            health_message: default_health_message(),
        }
    }
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
                "Matter Controller",
                env!("CARGO_PKG_VERSION"),
                ["matter/controller"],
            )),
            initial_health: Some(module_health(
                env.module_id.clone(),
                HealthState::Starting,
                "Starting Matter controller stub",
            )),
        })
        .await?;
    client
        .report_health(module_health(
            env.module_id,
            HealthState::Ready,
            config.health_message,
        ))
        .await?;

    std::future::pending::<()>().await;
    Ok(())
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

    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("failed to connect matter controller")))
}

fn load_config(path: &Path) -> Result<MatterControllerConfig> {
    let raw = std::fs::read_to_string(path)?;
    if raw.trim().is_empty() {
        return Ok(MatterControllerConfig::default());
    }
    Ok(toml::from_str(&raw)?)
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

fn default_health_message() -> String {
    String::from("Matter controller stub active")
}
