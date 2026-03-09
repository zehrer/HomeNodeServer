use std::env;
use std::fs;
use std::thread;
use std::time::Duration;

use log::info;
use rs_matter::MATTER_PORT;

fn main() -> std::io::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let state_dir = env::var("RS_MATTERD_STATE_DIR")
        .unwrap_or_else(|_| String::from("/var/lib/rs-matterd"));
    let heartbeat_secs = env::var("RS_MATTERD_HEARTBEAT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(60);

    fs::create_dir_all(&state_dir)?;

    info!("starting rs-matterd skeleton");
    info!("state directory: {state_dir}");
    info!("default Matter port reserved for future integration: {MATTER_PORT}");
    info!("heartbeat interval: {heartbeat_secs}s");

    loop {
        info!("rs-matterd is alive");
        thread::sleep(Duration::from_secs(heartbeat_secs));
    }
}
