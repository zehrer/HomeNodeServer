use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use tempfile::tempdir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::oneshot;

use homenode_sdk::proto::Empty;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn boot_without_modules_works() -> Result<()> {
    let temp = tempdir()?;
    let socket_path = temp.path().join("homenode.sock");
    let config_path = temp.path().join("server.toml");
    std::fs::write(
        &config_path,
        format!(
            "[server]\nsocket_path = \"{}\"\nlog_filter = \"info\"\n",
            socket_path.display()
        ),
    )?;

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let handle = tokio::spawn(homenode_server::run_with_shutdown(
        config_path.clone(),
        async move {
            let _ = shutdown_rx.await;
        },
    ));

    let snapshot = wait_for_snapshot(&socket_path, |snapshot| snapshot.modules.is_empty()).await?;
    assert!(snapshot.modules.is_empty());
    assert!(snapshot.devices.is_empty());

    let _ = shutdown_tx.send(());
    handle.await??;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn config_validation_rejects_unknown_and_duplicate_ids() -> Result<()> {
    let temp = tempdir()?;
    let unknown_path = temp.path().join("unknown.toml");
    std::fs::write(
        &unknown_path,
        r#"
[modules.foo]
enabled = false
module_id = "unknown-module"
"#,
    )?;
    let error = homenode_server::config::load_from_path(&unknown_path).unwrap_err();
    assert!(error.to_string().contains("unknown module_id"));

    let duplicate_path = temp.path().join("duplicate.toml");
    std::fs::write(
        &duplicate_path,
        r#"
[modules.web]
enabled = false
module_id = "web"

[modules.web_copy]
enabled = false
module_id = "web"
"#,
    )?;
    let error = homenode_server::config::load_from_path(&duplicate_path).unwrap_err();
    assert!(error.to_string().contains("duplicate module_id"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn supervisor_starts_stub_modules_and_web_serves_status() -> Result<()> {
    let temp = tempdir()?;
    let socket_path = temp.path().join("supervisor.sock");
    let web_config = temp.path().join("web.toml");
    let network_config = temp.path().join("network.toml");
    let config_path = temp.path().join("server.toml");
    let listen_addr = reserve_tcp_port()?;
    let web_program = built_binary("homenode-module-web")?;
    let network_program = built_binary("homenode-module-network-discovery")?;

    std::fs::write(
        &web_config,
        format!(
            "listen_addr = \"{listen_addr}\"\nstatus_title = \"HomeNode Test\"\n"
        ),
    )?;
    std::fs::write(
        &network_config,
        r#"
health_message = "Network discovery stub active"

[[demo_devices]]
device_id = "switch-01"
display_name = "Network Switch"
kind = "network-switch"
capabilities = ["lldp"]
"#,
    )?;
    std::fs::write(
        &config_path,
        format!(
            r#"[server]
socket_path = "{socket}"
log_filter = "info"

[modules.web]
enabled = true
module_id = "web"
program = "{web_program}"
config = "{web_config}"

[modules.network_discovery]
enabled = true
module_id = "network-discovery"
program = "{network_program}"
config = "{network_config}"
"#,
            socket = socket_path.display(),
            web_program = web_program.display(),
            web_config = web_config.display(),
            network_program = network_program.display(),
            network_config = network_config.display(),
        ),
    )?;

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let handle = tokio::spawn(homenode_server::run_with_shutdown(
        config_path.clone(),
        async move {
            let _ = shutdown_rx.await;
        },
    ));

    let snapshot = wait_for_snapshot(&socket_path, |snapshot| {
        snapshot.modules.iter().any(|module| {
            module
                .manifest
                .as_ref()
                .map(|manifest| manifest.id == "web")
                .unwrap_or(false)
        }) && snapshot.modules.iter().any(|module| {
            module
                .manifest
                .as_ref()
                .map(|manifest| manifest.id == "network-discovery")
                .unwrap_or(false)
        }) && snapshot
            .devices
            .iter()
            .any(|device| device.device_id == "switch-01")
    })
    .await?;

    assert_eq!(snapshot.modules.len(), 2);
    let page = fetch_http_page(&listen_addr).await?;
    assert!(page.contains("HomeNode Test"));
    assert!(page.contains("network-discovery"));
    assert!(page.contains("Network Switch"));

    let _ = shutdown_tx.send(());
    handle.await??;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fake_module_contract_works_over_sdk() -> Result<()> {
    let temp = tempdir()?;
    let socket_path = temp.path().join("contract.sock");
    let server_config = temp.path().join("server.toml");
    let fake_config = temp.path().join("fake.toml");
    std::fs::write(
        &server_config,
        format!(
            "[server]\nsocket_path = \"{}\"\nlog_filter = \"info\"\n",
            socket_path.display()
        ),
    )?;
    std::fs::write(&fake_config, "")?;

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let handle = tokio::spawn(homenode_server::run_with_shutdown(
        server_config.clone(),
        async move {
            let _ = shutdown_rx.await;
        },
    ));

    wait_for_snapshot(&socket_path, |_| true).await?;

    let fake_bin = PathBuf::from(env!("CARGO_BIN_EXE_homenode-fake-module"));
    let mut child = tokio::process::Command::new(fake_bin)
        .env("HOMENODE_SOCKET_PATH", &socket_path)
        .env("HOMENODE_MODULE_CONFIG", &fake_config)
        .env("HOMENODE_MODULE_ID", "switchbot")
        .env("HOMENODE_SERVER_CONFIG", &server_config)
        .spawn()
        .context("failed to spawn fake module")?;

    let snapshot = wait_for_snapshot(&socket_path, |snapshot| {
        snapshot.modules.iter().any(|module| {
            module
                .manifest
                .as_ref()
                .map(|manifest| manifest.id == "switchbot")
                .unwrap_or(false)
        }) && snapshot
            .devices
            .iter()
            .any(|device| device.device_id == "fake-device-01")
    })
    .await?;

    assert_eq!(snapshot.modules.len(), 1);
    assert_eq!(snapshot.devices.len(), 1);

    child.kill().await?;
    let _ = shutdown_tx.send(());
    handle.await??;
    Ok(())
}

async fn wait_for_snapshot(
    socket_path: &Path,
    predicate: impl Fn(&homenode_sdk::proto::RuntimeSnapshot) -> bool,
) -> Result<homenode_sdk::proto::RuntimeSnapshot> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match homenode_sdk::connect_control_client(socket_path).await {
            Ok(mut client) => {
                let snapshot = client.get_runtime_snapshot(Empty {}).await?.into_inner();
                if predicate(&snapshot) {
                    return Ok(snapshot);
                }
            }
            Err(_) => {}
        }

        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for runtime snapshot");
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn fetch_http_page(listen_addr: &str) -> Result<String> {
    let mut stream = tokio::net::TcpStream::connect(listen_addr).await?;
    let request = format!("GET / HTTP/1.1\r\nHost: {listen_addr}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await?;

    let mut response = Vec::new();
    stream.read_to_end(&mut response).await?;
    Ok(String::from_utf8_lossy(&response).to_string())
}

fn reserve_tcp_port() -> Result<String> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    drop(listener);
    Ok(address.to_string())
}

fn built_binary(binary_name: &str) -> Result<PathBuf> {
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .context("failed to resolve workspace root")?
        .to_path_buf();
    let target_dir = workspace_root.join("target").join("debug");
    let binary_path = target_dir.join(binary_name);
    if binary_path.exists() {
        return Ok(binary_path);
    }

    let status = std::process::Command::new("cargo")
        .current_dir(&workspace_root)
        .args(["build", "-p", binary_name])
        .status()
        .context("failed to invoke cargo build for module binary")?;
    if !status.success() {
        anyhow::bail!("cargo build -p {binary_name} failed");
    }

    if binary_path.exists() {
        Ok(binary_path)
    } else {
        anyhow::bail!("failed to locate built binary {}", binary_path.display())
    }
}
