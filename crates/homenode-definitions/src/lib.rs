use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use rhai::{Dynamic, Engine, Scope, AST};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceMeta {
    pub id: String,
    pub name: String,
    pub category: String,
    pub category_title: String,
    pub category_icon: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ObservationContext {
    pub ip: String,
    pub mac: Option<String>,
    pub hostname: Option<String>,
    pub name: String,
    pub vendor: Option<String>,
    pub interface: String,
}

#[derive(Debug, Clone)]
pub struct DeviceMatch {
    pub meta: DeviceMeta,
    pub script_id: String,
}

#[derive(Clone)]
pub struct RhaiDeviceEngine {
    engine: Arc<Engine>,
    scripts: Vec<(DeviceMeta, AST)>,
}

impl RhaiDeviceEngine {
    pub fn new() -> Self {
        let mut engine = Engine::new();
        // Strict safety limits to prevent runaway scripts or memory bloat
        engine.set_max_operations(20_000);
        engine.set_max_string_size(4_096);
        engine.set_max_array_size(256);
        engine.set_max_map_size(256);

        Self {
            engine: Arc::new(engine),
            scripts: Vec::new(),
        }
    }

    pub fn load_from_dir(&mut self, dir_path: impl AsRef<Path>) -> Result<usize> {
        let path = dir_path.as_ref();
        if !path.exists() {
            debug!("Device definitions directory {} does not exist", path.display());
            return Ok(0);
        }

        let mut loaded = 0;
        let read_dir = std::fs::read_dir(path)
            .with_context(|| format!("failed to read directory {}", path.display()))?;

        for entry in read_dir.flatten() {
            let file_path = entry.path();
            if file_path.extension().is_some_and(|ext| ext == "rhai") {
                match self.load_script_file(&file_path) {
                    Ok(meta) => {
                        info!("Loaded device definition: {} ({}) [{}]", meta.name, meta.id, meta.category);
                        loaded += 1;
                    }
                    Err(err) => {
                        warn!("Failed to load device script {}: {err}", file_path.display());
                    }
                }
            }
        }

        Ok(loaded)
    }

    pub fn load_script_str(&mut self, script_content: &str) -> Result<DeviceMeta> {
        let ast = self
            .engine
            .compile(script_content)
            .context("failed to compile Rhai script")?;

        let meta = self.eval_meta(&ast)?;
        self.scripts.push((meta.clone(), ast));
        Ok(meta)
    }

    pub fn load_script_file(&mut self, path: &Path) -> Result<DeviceMeta> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read script {}", path.display()))?;
        self.load_script_str(&content)
    }

    fn eval_meta(&self, ast: &AST) -> Result<DeviceMeta> {
        let mut scope = Scope::new();
        let result: Dynamic = self
            .engine
            .call_fn(&mut scope, ast, "meta", ())
            .context("failed to call meta() function in script")?;

        if !result.is_map() {
            anyhow::bail!("meta() must return a map");
        }

        let map = result.cast::<rhai::Map>();
        let id = map
            .get("id")
            .and_then(|v| v.clone().try_cast::<String>())
            .unwrap_or_else(|| "unknown".to_string());
        let name = map
            .get("name")
            .and_then(|v| v.clone().try_cast::<String>())
            .unwrap_or_else(|| "Unknown Device".to_string());
        let category = map
            .get("category")
            .and_then(|v| v.clone().try_cast::<String>())
            .unwrap_or_else(|| "network-device".to_string());
        let category_title = map
            .get("category_title")
            .and_then(|v| v.clone().try_cast::<String>())
            .unwrap_or_else(|| "Network Devices".to_string());
        let category_icon = map
            .get("category_icon")
            .and_then(|v| v.clone().try_cast::<String>())
            .unwrap_or_else(|| "🔌".to_string());

        let capabilities = if let Some(caps) = map.get("capabilities") {
            if caps.is_array() {
                caps.clone()
                    .into_typed_array::<String>()
                    .unwrap_or_default()
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };

        Ok(DeviceMeta {
            id,
            name,
            category,
            category_title,
            category_icon,
            capabilities,
        })
    }

    pub fn identify(&self, obs: &ObservationContext) -> Option<DeviceMatch> {
        let mut scope = Scope::new();
        let mut obs_map = rhai::Map::new();
        obs_map.insert("ip".into(), Dynamic::from(obs.ip.clone()));
        obs_map.insert(
            "mac".into(),
            Dynamic::from(obs.mac.clone().unwrap_or_default()),
        );
        obs_map.insert(
            "hostname".into(),
            Dynamic::from(obs.hostname.clone().unwrap_or_default()),
        );
        obs_map.insert("name".into(), Dynamic::from(obs.name.clone()));
        obs_map.insert(
            "vendor".into(),
            Dynamic::from(obs.vendor.clone().unwrap_or_default()),
        );
        obs_map.insert("interface".into(), Dynamic::from(obs.interface.clone()));

        let obs_dynamic = Dynamic::from(obs_map);

        for (meta, ast) in &self.scripts {
            match self
                .engine
                .call_fn::<bool>(&mut scope, ast, "identify", (obs_dynamic.clone(),))
            {
                Ok(true) => {
                    return Some(DeviceMatch {
                        meta: meta.clone(),
                        script_id: meta.id.clone(),
                    });
                }
                Ok(false) => {}
                Err(err) => {
                    debug!("identify error in script {}: {err}", meta.id);
                }
            }
        }

        None
    }

    pub fn loaded_scripts_count(&self) -> usize {
        self.scripts.len()
    }
}

impl Default for RhaiDeviceEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiles_and_evaluates_meta_and_identify() {
        let script = r#"
        fn meta() {
            #{
                id: "apple_ipad",
                name: "Apple iPad",
                category: "tablet",
                category_title: "Tablets",
                category_icon: "📟",
                capabilities: ["wifi", "airplay"]
            }
        }

        fn identify(obs) {
            let name = obs.name.to_lower();
            name.contains("ipad")
        }
        "#;

        let mut engine = RhaiDeviceEngine::new();
        let meta = engine.load_script_str(script).expect("failed to load script");
        assert_eq!(meta.id, "apple_ipad");
        assert_eq!(meta.category, "tablet");
        assert_eq!(meta.category_title, "Tablets");
        assert_eq!(meta.category_icon, "📟");

        let ipad_obs = ObservationContext {
            ip: "192.168.1.50".into(),
            mac: Some("00:11:22:33:44:55".into()),
            hostname: Some("stephans-ipad.fritz.box".into()),
            name: "stephans-ipad.fritz.box".into(),
            vendor: Some("Apple Inc.".into()),
            interface: "en0".into(),
        };

        let matched = engine.identify(&ipad_obs);
        assert!(matched.is_some());
        let m = matched.unwrap();
        assert_eq!(m.meta.category, "tablet");

        let iphone_obs = ObservationContext {
            ip: "192.168.1.51".into(),
            mac: Some("00:11:22:33:44:56".into()),
            hostname: Some("stephans-iphone.fritz.box".into()),
            name: "stephans-iphone.fritz.box".into(),
            vendor: Some("Apple Inc.".into()),
            interface: "en0".into(),
        };

        assert!(engine.identify(&iphone_obs).is_none());
    }
}
