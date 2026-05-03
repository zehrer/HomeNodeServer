pub mod config;
mod service;

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use config::{enabled_modules, load_from_path, HomeNodeConfig, ModuleLaunchSpec};
use tokio::process::{Child, Command};
use tokio::sync::{oneshot, Mutex, RwLock};
use tokio::time::MissedTickBehavior;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::Server;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use homenode_sdk::proto::home_node_control_server::HomeNodeControlServer;

use crate::service::{ControlService, SharedState};

#[derive(Debug)]
struct ManagedChild {
    alias: String,
    module_id: String,
    child: Child,
    exited: bool,
}

type SharedChildren = Arc<Mutex<Vec<ManagedChild>>>;

pub async fn run_from_path(config_path: impl AsRef<Path>) -> Result<()> {
    run_with_shutdown(config_path, async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
}

pub async fn run_with_shutdown<F>(config_path: impl AsRef<Path>, shutdown: F) -> Result<()>
where
    F: Future<Output = ()> + Send,
{
    let config_path = config_path.as_ref().to_path_buf();
    let config = load_from_path(&config_path)?;
    init_tracing(&config.server.log_filter);
    run_config_with_shutdown(config_path, config, shutdown).await
}

pub async fn run_config_with_shutdown<F>(
    config_path: PathBuf,
    config: HomeNodeConfig,
    shutdown: F,
) -> Result<()>
where
    F: Future<Output = ()> + Send,
{
    let socket_path = config.server.socket_path.clone();
    if let Some(parent) = socket_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create socket directory {}", parent.display()))?;
    }

    match tokio::fs::remove_file(&socket_path).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("failed to remove stale socket"),
    }

    let listener = tokio::net::UnixListener::bind(&socket_path)
        .with_context(|| format!("failed to bind {}", socket_path.display()))?;
    let incoming = UnixListenerStream::new(listener);

    let state: SharedState = Arc::new(RwLock::new(Default::default()));
    let service = ControlService::new(state.clone());

    let (grpc_stop_tx, grpc_stop_rx) = oneshot::channel::<()>();
    let grpc_task = tokio::spawn(async move {
        Server::builder()
            .add_service(HomeNodeControlServer::new(service))
            .serve_with_incoming_shutdown(incoming, async {
                let _ = grpc_stop_rx.await;
            })
            .await
    });

    let children = Arc::new(Mutex::new(Vec::new()));
    launch_modules(&config_path, &config, children.clone()).await?;

    let (monitor_stop_tx, monitor_stop_rx) = oneshot::channel::<()>();
    let monitor_task = tokio::spawn(monitor_children(
        children.clone(),
        state.clone(),
        monitor_stop_rx,
    ));

    shutdown.await;
    let _ = grpc_stop_tx.send(());
    let _ = monitor_stop_tx.send(());
    stop_children(children.clone()).await;

    let _ = monitor_task.await;
    grpc_task.await.context("gRPC task join failed")??;

    match tokio::fs::remove_file(&socket_path).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("failed to remove socket during shutdown"),
    }

    Ok(())
}

pub fn resolve_program_path(program: &Path) -> PathBuf {
    if program.components().count() > 1 || program.is_absolute() {
        return program.to_path_buf();
    }

    let current_exe = std::env::current_exe().ok();
    if let Some(current_exe) = current_exe {
        if let Some(parent) = current_exe.parent() {
            for candidate_dir in [Some(parent), parent.parent()] {
                if let Some(candidate_dir) = candidate_dir {
                    let candidate = candidate_dir.join(program);
                    if candidate.exists() {
                        return candidate;
                    }
                }
            }
        }
    }

    program.to_path_buf()
}

async fn launch_modules(
    config_path: &Path,
    config: &HomeNodeConfig,
    children: SharedChildren,
) -> Result<()> {
    for module in enabled_modules(config)? {
        let child = spawn_module(config_path, &config.server.socket_path, &module).await?;
        info!(
            alias = %module.alias,
            module_id = %module.module_id,
            program = %module.program.display(),
            "started module process"
        );
        children.lock().await.push(ManagedChild {
            alias: module.alias,
            module_id: module.module_id,
            child,
            exited: false,
        });
    }

    Ok(())
}

async fn spawn_module(
    server_config_path: &Path,
    socket_path: &Path,
    module: &ModuleLaunchSpec,
) -> Result<Child> {
    let program = resolve_program_path(&module.program);
    let mut command = Command::new(&program);
    command
        .args(&module.args)
        .env("HOMENODE_SOCKET_PATH", socket_path)
        .env("HOMENODE_MODULE_CONFIG", &module.config_path)
        .env("HOMENODE_MODULE_ID", &module.module_id)
        .env("HOMENODE_SERVER_CONFIG", server_config_path)
        .kill_on_drop(true);

    for (key, value) in &module.env {
        command.env(key, value);
    }

    command
        .spawn()
        .with_context(|| format!("failed to spawn module {}", program.display()))
}

async fn monitor_children(
    children: SharedChildren,
    state: SharedState,
    mut stop: oneshot::Receiver<()>,
) {
    let mut interval = tokio::time::interval(Duration::from_millis(500));
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = &mut stop => break,
            _ = interval.tick() => {
                let mut guard = children.lock().await;
                for child in guard.iter_mut() {
                    if child.exited {
                        continue;
                    }

                    match child.child.try_wait() {
                        Ok(Some(status)) => {
                            child.exited = true;
                            warn!(
                                alias = %child.alias,
                                module_id = %child.module_id,
                                status = %status,
                                "module process exited"
                            );
                            state.write().await.mark_disconnected(
                                &child.module_id,
                                format!("process exited with status {status}"),
                            );
                        }
                        Ok(None) => {}
                        Err(error) => {
                            child.exited = true;
                            warn!(
                                alias = %child.alias,
                                module_id = %child.module_id,
                                error = %error,
                                "failed to poll module process"
                            );
                            state
                                .write()
                                .await
                                .mark_disconnected(&child.module_id, format!("poll error: {error}"));
                        }
                    }
                }
            }
        }
    }
}

async fn stop_children(children: SharedChildren) {
    let mut guard = children.lock().await;
    for child in guard.iter_mut() {
        if child.exited {
            continue;
        }

        match child.child.kill().await {
            Ok(()) => child.exited = true,
            Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => {
                child.exited = true;
            }
            Err(error) => warn!(
                alias = %child.alias,
                module_id = %child.module_id,
                error = %error,
                "failed to stop module process"
            ),
        }
    }
}

fn init_tracing(log_filter: &str) {
    let filter = EnvFilter::try_new(log_filter).unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}
