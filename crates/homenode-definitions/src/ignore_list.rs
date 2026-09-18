use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IgnoredDeviceRecord {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default = "default_reason")]
    pub reason: String,
    #[serde(default = "default_ignored_at")]
    pub ignored_at: String,
}

fn default_reason() -> String {
    "Nachbargerät (Neighbor device)".to_string()
}

fn default_ignored_at() -> String {
    Utc::now().to_rfc3339()
}

impl IgnoredDeviceRecord {
    pub fn new(id: impl Into<String>, name: Option<String>, reason: Option<String>) -> Self {
        Self {
            id: id.into(),
            name,
            reason: reason.unwrap_or_else(default_reason),
            ignored_at: default_ignored_at(),
        }
    }

    pub fn normalized_id(&self) -> String {
        normalize_identifier(&self.id)
    }
}

pub fn normalize_identifier(raw: &str) -> String {
    raw.trim().to_lowercase().replace([':', '-'], "")
}

#[derive(Clone)]
pub struct IgnoredDevicesStore {
    path: PathBuf,
    records: Arc<RwLock<HashMap<String, IgnoredDeviceRecord>>>,
}

impl IgnoredDevicesStore {
    pub fn load_or_create(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref().to_path_buf();
        let mut map = HashMap::new();

        if path.exists() {
            match fs::read_to_string(&path) {
                Ok(content) if !content.trim().is_empty() => {
                    match serde_json::from_str::<Vec<IgnoredDeviceRecord>>(&content) {
                        Ok(list) => {
                            info!("Loaded {} ignored device(s) from {}", list.len(), path.display());
                            for r in list {
                                let norm = r.normalized_id();
                                map.insert(norm, r);
                            }
                        }
                        Err(err) => {
                            error!("Failed to parse ignored devices from {}: {}", path.display(), err);
                        }
                    }
                }
                Ok(_) => {}
                Err(err) => {
                    error!("Failed to read ignored devices file {}: {}", path.display(), err);
                }
            }
        }

        Self {
            path,
            records: Arc::new(RwLock::new(map)),
        }
    }

    /// Checks if a device ID or MAC address is in the ignore list.
    pub fn is_ignored(&self, id_or_mac: &str) -> bool {
        if id_or_mac.trim().is_empty() {
            return false;
        }
        let norm = normalize_identifier(id_or_mac);
        let guard = match self.records.read() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };

        if guard.contains_key(&norm) {
            return true;
        }

        // Also check if any ignored MAC/ID is a substring of the target or vice-versa
        for key in guard.keys() {
            if key.len() >= 8 && (norm.contains(key) || key.contains(&norm)) {
                return true;
            }
        }

        false
    }

    /// Add a device to the ignore list and persist to disk.
    pub fn ignore(&self, record: IgnoredDeviceRecord) -> Result<()> {
        let norm = record.normalized_id();
        {
            let mut guard = match self.records.write() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.insert(norm, record);
        }
        self.save()
    }

    /// Remove a device from the ignore list and persist to disk.
    pub fn unignore(&self, id_or_mac: &str) -> Result<bool> {
        let norm = normalize_identifier(id_or_mac);
        let removed = {
            let mut guard = match self.records.write() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.remove(&norm).is_some()
        };
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    /// List all currently ignored devices.
    pub fn list(&self) -> Vec<IgnoredDeviceRecord> {
        let guard = match self.records.read() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut list: Vec<IgnoredDeviceRecord> = guard.values().cloned().collect();
        list.sort_by(|a, b| b.ignored_at.cmp(&a.ignored_at));
        list
    }

    /// Count of currently ignored devices.
    pub fn count(&self) -> usize {
        let guard = match self.records.read() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.len()
    }

    fn save(&self) -> Result<()> {
        let list = self.list();
        let serialized = serde_json::to_string_pretty(&list)
            .context("failed to serialize ignored devices")?;

        if let Some(parent) = self.path.parent() {
            let _ = fs::create_dir_all(parent);
        }

        fs::write(&self.path, serialized)
            .with_context(|| format!("failed to write ignored devices to {}", self.path.display()))?;

        debug!("Persisted {} ignored devices to {}", list.len(), self.path.display());
        Ok(())
    }
}
