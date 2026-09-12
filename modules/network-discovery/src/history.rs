use std::collections::{HashMap, HashSet};
use std::path::Path;
use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::scanner::DiscoveredDevice;
use homenode_sdk::device_record;
use homenode_sdk::proto::DeviceRecord;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceHistoryRecord {
    pub device_id: String,
    pub display_name: String,
    pub kind: String,
    #[serde(default)]
    pub category_title: Option<String>,
    #[serde(default)]
    pub category_icon: Option<String>,
    #[serde(default)]
    pub script_id: Option<String>,
    pub ip: String,
    #[serde(default)]
    pub mac: Option<String>,
    #[serde(default)]
    pub hostname: Option<String>,
    pub interface: String,
    #[serde(default)]
    pub vendor: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    pub source: String,
    #[serde(default)]
    pub web_url: Option<String>,
    #[serde(default)]
    pub product_id: Option<String>,
    #[serde(default)]
    pub product_name: Option<String>,
    #[serde(default)]
    pub vendor_id: Option<String>,
    #[serde(default)]
    pub matter_fabrics: Option<String>,
    pub first_seen: String,
    pub last_seen: String,
    pub is_active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DeviceHistoryStore {
    // Keyed by normalized MAC address if available, otherwise device_id
    pub records: HashMap<String, DeviceHistoryRecord>,
}

impl DeviceHistoryStore {
    pub fn new() -> Self {
        Self {
            records: HashMap::new(),
        }
    }

    pub fn load_from_path(path: &Path) -> Self {
        if !path.exists() {
            info!("Device history file not found at {}; starting fresh", path.display());
            return Self::new();
        }

        match std::fs::read_to_string(path) {
            Ok(content) => match serde_json::from_str::<DeviceHistoryStore>(&content) {
                Ok(store) => {
                    info!(
                        "Loaded {} device history records from {}",
                        store.records.len(),
                        path.display()
                    );
                    store
                }
                Err(err) => {
                    warn!(
                        "Failed to parse device history JSON at {}: {err}; starting fresh",
                        path.display()
                    );
                    Self::new()
                }
            },
            Err(err) => {
                warn!(
                    "Failed to read device history file at {}: {err}; starting fresh",
                    path.display()
                );
                Self::new()
            }
        }
    }

    pub fn save_to_path(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create dir {}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(self)
            .context("failed to serialize device history")?;
        std::fs::write(path, json)
            .with_context(|| format!("failed to write device history to {}", path.display()))?;
        Ok(())
    }

    pub fn record_key(mac: Option<&str>, device_id: &str) -> String {
        if let Some(m) = mac {
            let norm = m.trim().to_lowercase();
            if !norm.is_empty() && !norm.contains("incomplete") {
                return norm;
            }
        }
        device_id.to_string()
    }

    /// Reconciles current sweep against history.
    /// - Devices in current sweep become active (is_active = true) with updated last_seen.
    /// - Previously seen devices not in current sweep become inactive (is_active = false), retaining their last_seen.
    /// Returns full set of active and inactive devices converted to proto DeviceRecord.
    pub fn reconcile_sweep(
        &mut self,
        module_id: &str,
        sweep: Vec<DiscoveredDevice>,
    ) -> Vec<DeviceRecord> {
        let now = Utc::now().to_rfc3339();
        let mut seen_keys = HashSet::new();

        for dev in sweep {
            let key = Self::record_key(dev.mac.as_deref(), &dev.device_id);
            seen_keys.insert(key.clone());

            if let Some(existing) = self.records.get_mut(&key) {
                existing.last_seen = now.clone();
                existing.is_active = true;
                existing.ip = dev.ip;
                existing.interface = dev.interface;
                existing.display_name = dev.display_name;
                existing.kind = dev.kind;
                if dev.mac.is_some() {
                    existing.mac = dev.mac;
                }
                if dev.hostname.is_some() {
                    existing.hostname = dev.hostname;
                }
                if dev.vendor.is_some() {
                    existing.vendor = dev.vendor;
                }
                if dev.category_title.is_some() {
                    existing.category_title = dev.category_title;
                }
                if dev.category_icon.is_some() {
                    existing.category_icon = dev.category_icon;
                }
                if dev.script_id.is_some() {
                    existing.script_id = dev.script_id;
                }
                if dev.web_url.is_some() {
                    existing.web_url = dev.web_url;
                }
                if dev.product_id.is_some() {
                    existing.product_id = dev.product_id;
                }
                if dev.product_name.is_some() {
                    existing.product_name = dev.product_name;
                }
                if dev.vendor_id.is_some() {
                    existing.vendor_id = dev.vendor_id;
                }
                if dev.matter_fabrics.is_some() {
                    existing.matter_fabrics = dev.matter_fabrics;
                }
                for cap in dev.capabilities {
                    if !existing.capabilities.contains(&cap) {
                        existing.capabilities.push(cap);
                    }
                }
            } else {
                let rec = DeviceHistoryRecord {
                    device_id: dev.device_id,
                    display_name: dev.display_name,
                    kind: dev.kind,
                    category_title: dev.category_title,
                    category_icon: dev.category_icon,
                    script_id: dev.script_id,
                    ip: dev.ip,
                    mac: dev.mac,
                    hostname: dev.hostname,
                    interface: dev.interface,
                    vendor: dev.vendor,
                    capabilities: dev.capabilities,
                    source: dev.source,
                    web_url: dev.web_url,
                    product_id: dev.product_id,
                    product_name: dev.product_name,
                    vendor_id: dev.vendor_id,
                    matter_fabrics: dev.matter_fabrics,
                    first_seen: now.clone(),
                    last_seen: now.clone(),
                    is_active: true,
                };
                self.records.insert(key, rec);
            }
        }

        // Mark previously recorded devices missing from this sweep as inactive
        for (k, record) in &mut self.records {
            if !seen_keys.contains(k) {
                record.is_active = false;
            }
        }

        // Convert to gRPC DeviceRecord instances
        self.records
            .values()
            .map(|r| self.to_proto_device(module_id, r))
            .collect()
    }

    /// Forget / remove a device from history
    pub fn forget_device(&mut self, device_id_or_mac: &str) -> bool {
        let needle = device_id_or_mac.trim().to_lowercase();
        let initial_len = self.records.len();
        self.records.retain(|k, v| {
            let key_match = k.to_lowercase() == needle;
            let id_match = v.device_id.to_lowercase() == needle;
            let mac_match = v.mac.as_deref().map(|m| m.to_lowercase() == needle).unwrap_or(false);
            !key_match && !id_match && !mac_match
        });
        self.records.len() < initial_len
    }

    fn to_proto_device(&self, module_id: &str, r: &DeviceHistoryRecord) -> DeviceRecord {
        let mut metadata = HashMap::new();
        metadata.insert("ip".to_string(), r.ip.clone());
        metadata.insert("interface".to_string(), r.interface.clone());
        if let Some(ref mac) = r.mac {
            metadata.insert("mac".to_string(), mac.clone());
        }
        if let Some(ref host) = r.hostname {
            metadata.insert("hostname".to_string(), host.clone());
        }
        if let Some(ref vendor) = r.vendor {
            metadata.insert("vendor".to_string(), vendor.clone());
        }
        if let Some(ref title) = r.category_title {
            metadata.insert("category_title".to_string(), title.clone());
        }
        if let Some(ref icon) = r.category_icon {
            metadata.insert("category_icon".to_string(), icon.clone());
        }
        if let Some(ref script_id) = r.script_id {
            metadata.insert("script_id".to_string(), script_id.clone());
        }
        if let Some(ref web_url) = r.web_url {
            metadata.insert("web_url".to_string(), web_url.clone());
        }
        if let Some(ref product_id) = r.product_id {
            metadata.insert("product_id".to_string(), product_id.clone());
        }
        if let Some(ref product_name) = r.product_name {
            metadata.insert("product_name".to_string(), product_name.clone());
        }
        if let Some(ref vendor_id) = r.vendor_id {
            metadata.insert("vendor_id".to_string(), vendor_id.clone());
        }
        if let Some(ref matter_fabrics) = r.matter_fabrics {
            metadata.insert("matter_fabrics".to_string(), matter_fabrics.clone());
        }
        metadata.insert("category".to_string(), r.kind.clone());
        metadata.insert("source".to_string(), r.source.clone());

        // History & Status fields
        metadata.insert("first_seen".to_string(), r.first_seen.clone());
        metadata.insert("last_seen".to_string(), r.last_seen.clone());
        metadata.insert("status".to_string(), if r.is_active { "active".to_string() } else { "inactive".to_string() });
        metadata.insert("is_active".to_string(), if r.is_active { "true".to_string() } else { "false".to_string() });

        device_record(
            module_id,
            r.device_id.clone(),
            r.display_name.clone(),
            r.kind.clone(),
            r.capabilities.clone(),
            metadata,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_device(ip: &str, mac: &str, name: &str) -> DiscoveredDevice {
        DiscoveredDevice {
            device_id: format!("net-{}", ip.replace('.', "-")),
            display_name: name.to_string(),
            kind: "computer".to_string(),
            category_title: Some("Computers & Laptops".to_string()),
            category_icon: Some("💻".to_string()),
            script_id: None,
            ip: ip.to_string(),
            mac: Some(mac.to_string()),
            hostname: Some(format!("{name}.local")),
            interface: "en0".to_string(),
            vendor: Some("Apple Inc.".to_string()),
            capabilities: vec!["ip".to_string()],
            source: "arp".to_string(),
            web_url: None,
            product_id: None,
            product_name: None,
            vendor_id: None,
            matter_fabrics: None,
        }
    }

    #[test]
    fn reconcile_tracks_active_and_inactive_devices() {
        let mut store = DeviceHistoryStore::new();

        // Sweep 1: Devices A and B are active
        let sweep1 = vec![
            sample_device("192.168.178.10", "aa:bb:cc:dd:ee:01", "MacBook"),
            sample_device("192.168.178.20", "aa:bb:cc:dd:ee:02", "iPhone"),
        ];

        let records1 = store.reconcile_sweep("network-discovery", sweep1);
        assert_eq!(records1.len(), 2);
        assert!(records1.iter().all(|r| r.metadata.get("status").map(|s| s.as_str()) == Some("active")));

        let macbook_first_seen = store.records.get("aa:bb:cc:dd:ee:01").unwrap().first_seen.clone();
        let macbook_last_seen = store.records.get("aa:bb:cc:dd:ee:01").unwrap().last_seen.clone();
        assert!(!macbook_first_seen.is_empty());

        // Sweep 2: MacBook went to sleep / offline. Only iPhone is in sweep2.
        let sweep2 = vec![
            sample_device("192.168.178.20", "aa:bb:cc:dd:ee:02", "iPhone"),
        ];

        let records2 = store.reconcile_sweep("network-discovery", sweep2);
        assert_eq!(records2.len(), 2, "MacBook must NOT disappear from records");

        let mb = store.records.get("aa:bb:cc:dd:ee:01").unwrap();
        assert!(!mb.is_active, "MacBook must now be inactive");
        assert_eq!(mb.first_seen, macbook_first_seen, "First seen must be preserved");
        assert_eq!(mb.last_seen, macbook_last_seen, "Last seen must be preserved when offline");

        let iphone = store.records.get("aa:bb:cc:dd:ee:02").unwrap();
        assert!(iphone.is_active, "iPhone remains active");

        // Sweep 3: MacBook wakes up and reconnects (possibly with new IP via DHCP)
        let sweep3 = vec![
            sample_device("192.168.178.11", "aa:bb:cc:dd:ee:01", "MacBook"),
            sample_device("192.168.178.20", "aa:bb:cc:dd:ee:02", "iPhone"),
        ];

        let records3 = store.reconcile_sweep("network-discovery", sweep3);
        assert_eq!(records3.len(), 2);
        let mb3 = store.records.get("aa:bb:cc:dd:ee:01").unwrap();
        assert!(mb3.is_active, "MacBook is active again");
        assert_eq!(mb3.ip, "192.168.178.11", "IP updated to new DHCP lease");
        assert_eq!(mb3.first_seen, macbook_first_seen, "First seen preserved across offline period");
    }

    #[test]
    fn forget_device_removes_from_history() {
        let mut store = DeviceHistoryStore::new();
        let sweep = vec![sample_device("192.168.178.50", "11:22:33:44:55:66", "OldPrinter")];
        store.reconcile_sweep("network-discovery", sweep);
        assert_eq!(store.records.len(), 1);

        assert!(store.forget_device("11:22:33:44:55:66"));
        assert_eq!(store.records.len(), 0);
    }
}
