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
];

pub const RESERVED_MODULE_IDS: &[&str] = &[
    "web",
    "matter-controller",
    "matter-bridge",
    "network-discovery",
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
