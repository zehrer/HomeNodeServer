use std::path::PathBuf;

fn parse_config_path() -> PathBuf {
    let mut args = std::env::args().skip(1);
    let mut config = PathBuf::from("config/server.toml");

    while let Some(arg) = args.next() {
        if arg == "--config" {
            if let Some(value) = args.next() {
                config = PathBuf::from(value);
            }
        }
    }

    config
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    homenode_server::run_from_path(parse_config_path()).await
}
