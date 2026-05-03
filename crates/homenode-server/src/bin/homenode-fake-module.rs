use std::collections::HashMap;
use std::time::Duration;

use anyhow::Result;
use homenode_sdk::proto::{HealthState, ModuleRegistration, UpsertDevicesRequest};
use homenode_sdk::{connect_control_client, device_record, module_health, module_manifest, ModuleEnvironment};

#[tokio::main]
async fn main() -> Result<()> {
    let env = ModuleEnvironment::from_env()?;
    let mut client = wait_for_client(&env.socket_path).await?;

    let registration = ModuleRegistration {
        manifest: Some(module_manifest(
            env.module_id.clone(),
            "Fake Module",
            env!("CARGO_PKG_VERSION"),
            ["test/fake-module"],
        )),
        initial_health: Some(module_health(
            env.module_id.clone(),
            HealthState::Ready,
            "Fake module connected",
        )),
    };

    client.register_module(registration).await?;
    client
        .upsert_devices(UpsertDevicesRequest {
            module_id: env.module_id.clone(),
            devices: vec![device_record(
                env.module_id.clone(),
                "fake-device-01",
                "Fake Device",
                "test-device",
                ["test"],
                HashMap::<String, String>::new(),
            )],
        })
        .await?;
    client
        .report_health(module_health(
            env.module_id,
            HealthState::Ready,
            "Fake module published a device",
        ))
        .await?;
    std::future::pending::<()>().await;
    Ok(())
}

async fn wait_for_client(
    socket_path: &std::path::Path,
) -> Result<homenode_sdk::proto::home_node_control_client::HomeNodeControlClient<tonic::transport::Channel>>
{
    let mut last_error = None;
    for _ in 0..20 {
        match connect_control_client(socket_path).await {
            Ok(client) => return Ok(client),
            Err(error) => {
                last_error = Some(error);
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("failed to connect fake module")))
}
