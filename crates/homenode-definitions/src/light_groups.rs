use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LightGroup {
    pub id: String,
    pub name: String,
    pub room: String,
    pub device_ids: Vec<String>,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
}

impl LightGroup {
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        room: impl Into<String>,
        device_ids: Vec<String>,
        icon: Option<String>,
    ) -> Self {
        let now = Utc::now().to_rfc3339();
        Self {
            id: id.into(),
            name: name.into(),
            room: room.into(),
            device_ids,
            icon,
            created_at: now.clone(),
            updated_at: now,
        }
    }
}

#[derive(Clone)]
pub struct LightGroupsStore {
    path: PathBuf,
    groups: Arc<RwLock<HashMap<String, LightGroup>>>,
}

impl LightGroupsStore {
    pub fn new(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref().to_path_buf();
        let mut store = Self {
            path: path.clone(),
            groups: Arc::new(RwLock::new(HashMap::new())),
        };

        if let Err(e) = store.load() {
            warn!("Failed to load light groups from {}: {e}", path.display());
        }

        store
    }

    pub fn load(&mut self) -> Result<()> {
        if !self.path.exists() {
            return Ok(());
        }

        let content = fs::read_to_string(&self.path)
            .with_context(|| format!("failed to read light groups from {}", self.path.display()))?;

        if content.trim().is_empty() {
            return Ok(());
        }

        let groups: Vec<LightGroup> = serde_json::from_str(&content)
            .with_context(|| format!("failed to parse light groups from {}", self.path.display()))?;

        let mut map = self.groups.write().unwrap();
        map.clear();
        for g in groups {
            map.insert(g.id.clone(), g);
        }

        info!("Loaded {} light group(s) from {}", map.len(), self.path.display());
        Ok(())
    }

    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create directory {}", parent.display()))?;
        }

        let groups: Vec<LightGroup> = {
            let map = self.groups.read().unwrap();
            let mut list: Vec<LightGroup> = map.values().cloned().collect();
            list.sort_by(|a, b| a.name.cmp(&b.name));
            list
        };

        let json = serde_json::to_string_pretty(&groups)?;
        fs::write(&self.path, json)
            .with_context(|| format!("failed to write light groups to {}", self.path.display()))?;

        Ok(())
    }

    pub fn list(&self) -> Vec<LightGroup> {
        let map = self.groups.read().unwrap();
        let mut list: Vec<LightGroup> = map.values().cloned().collect();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        list
    }

    pub fn list_for_room(&self, room: &str) -> Vec<LightGroup> {
        let map = self.groups.read().unwrap();
        let room_lower = room.trim().to_lowercase();
        let mut list: Vec<LightGroup> = map
            .values()
            .filter(|g| g.room.trim().to_lowercase() == room_lower)
            .cloned()
            .collect();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        list
    }

    pub fn get(&self, id: &str) -> Option<LightGroup> {
        let map = self.groups.read().unwrap();
        map.get(id).cloned()
    }

    pub fn upsert(&self, mut group: LightGroup) -> Result<()> {
        group.updated_at = Utc::now().to_rfc3339();
        if group.created_at.is_empty() {
            group.created_at = group.updated_at.clone();
        }

        {
            let mut map = self.groups.write().unwrap();
            map.insert(group.id.clone(), group);
        }

        self.save()
    }

    pub fn delete(&self, id: &str) -> Result<bool> {
        let removed = {
            let mut map = self.groups.write().unwrap();
            map.remove(id).is_some()
        };

        if removed {
            self.save()?;
        }

        Ok(removed)
    }
}
