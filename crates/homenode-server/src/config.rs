use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use homenode_sdk::is_known_module_id;

#[derive(Debug, Clone, Deserialize, Default)]
pub struct HomeNodeConfig {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub modules: BTreeMap<String, ModuleConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_socket_path")]
    pub socket_path: PathBuf,
    #[serde(default = "default_log_filter")]
    pub log_filter: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            socket_path: default_socket_path(),
            log_filter: default_log_filter(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ModuleConfig {
    #[serde(default)]
    pub enabled: bool,
    pub module_id: Option<String>,
    pub program: Option<String>,
    pub config: Option<PathBuf>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct ModuleLaunchSpec {
    pub alias: String,
    pub module_id: String,
    pub program: PathBuf,
    pub config_path: PathBuf,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

pub fn load_from_path(path: &Path) -> Result<HomeNodeConfig> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    let mut config: HomeNodeConfig = if raw.trim().is_empty() {
        HomeNodeConfig::default()
    } else {
        toml::from_str(&raw).context("failed to parse HomeNode TOML config")?
    };

    let base_dir = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    normalize_paths(&mut config, &base_dir);
    validate(&config)?;
    Ok(config)
}

pub fn enabled_modules(config: &HomeNodeConfig) -> Result<Vec<ModuleLaunchSpec>> {
    validate(config)?;

    let mut modules = Vec::new();
    for (alias, module) in &config.modules {
        if !module.enabled {
            continue;
        }

        let module_id = module_id(alias, module);
        let program = module
            .program
            .clone()
            .with_context(|| format!("enabled module `{alias}` is missing `program`"))?;
        let config_path = module
            .config
            .clone()
            .with_context(|| format!("enabled module `{alias}` is missing `config`"))?;

        modules.push(ModuleLaunchSpec {
            alias: alias.clone(),
            module_id,
            program: program.into(),
            config_path,
            args: module.args.clone(),
            env: module.env.clone(),
        });
    }

    Ok(modules)
}

fn normalize_paths(config: &mut HomeNodeConfig, base_dir: &Path) {
    if config.server.socket_path.is_relative() {
        config.server.socket_path = base_dir.join(&config.server.socket_path);
    }

    for module in config.modules.values_mut() {
        if let Some(path) = &module.config {
            if path.is_relative() {
                module.config = Some(base_dir.join(path));
            }
        }
    }
}

fn validate(config: &HomeNodeConfig) -> Result<()> {
    let mut seen_ids = BTreeSet::new();

    for (alias, module) in &config.modules {
        let module_id = module_id(alias, module);
        if !is_known_module_id(&module_id) {
            bail!("module `{alias}` uses unknown module_id `{module_id}`");
        }

        if !seen_ids.insert(module_id.clone()) {
            bail!("duplicate module_id `{module_id}` in config");
        }

        if module.enabled && module.program.is_none() {
            bail!("enabled module `{alias}` must set `program`");
        }

        if module.enabled && module.config.is_none() {
            bail!("enabled module `{alias}` must set `config`");
        }
    }

    Ok(())
}

fn module_id(alias: &str, module: &ModuleConfig) -> String {
    module
        .module_id
        .clone()
        .unwrap_or_else(|| alias.replace('_', "-"))
}

fn default_socket_path() -> PathBuf {
    PathBuf::from("/tmp/homenode-server.sock")
}

fn default_log_filter() -> String {
    String::from("info")
}
