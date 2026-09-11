use std::path::PathBuf;

fn parse_config_path() -> PathBuf {
    let mut args = std::env::args().skip(1);
    let mut custom_config = None;

    while let Some(arg) = args.next() {
        if arg == "--config" {
            if let Some(value) = args.next() {
                custom_config = Some(PathBuf::from(value));
            }
        }
    }

    if let Some(path) = custom_config {
        return path;
    }

    let default_config = PathBuf::from("config/server.toml");
    if default_config.exists() {
        default_config
    } else {
        PathBuf::from("config/server.example.toml")
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    homenode_server::run_from_path(parse_config_path()).await
}
