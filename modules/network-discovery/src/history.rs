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
    pub sources: Vec<String>,
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
    #[serde(default)]
    pub was_ever_active: bool,
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
                    let mut store = store;
                    for record in store.records.values_mut() {
                        if record.sources.is_empty() && !record.source.is_empty() {
                            record.sources.push(record.source.clone());
                        }
                        if record.is_active {
                            record.was_ever_active = true;
                        } else if !record.was_ever_active {
                            let is_pure_tr064 = record.source == "fritzbox-tr064"
                                && record.sources.iter().all(|s| s == "fritzbox-tr064")
                                && record.capabilities.iter().all(|c| c == "fritzbox-tr064" || c == "ethernet" || c == "l2" || c == "ip");
                            if !is_pure_tr064 {
                                record.was_ever_active = true;
                            }
                        }
                    }
                    store.cleanup_duplicates();
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

    /// Removes ghost duplicate records (e.g. initial IP-keyed records created before ARP populated)
    /// and ensures records are keyed by normalized MAC when available.
    pub fn cleanup_duplicates(&mut self) {
        // Step 1: Normalize keys. If a record has a valid MAC, its key should be normalized MAC.
        let mut rekeyed: HashMap<String, DeviceHistoryRecord> = HashMap::new();
        for (_, record) in self.records.drain() {
            let proper_key = Self::record_key(record.mac.as_deref(), &record.device_id);
            if let Some(existing) = rekeyed.get_mut(&proper_key) {
                Self::merge_records(existing, record);
            } else {
                rekeyed.insert(proper_key, record);
            }
        }
        self.records = rekeyed;

        // Step 2: Remove ghost records with mac: None where another record has the same IP and has a MAC.
        let mut ips_with_mac: HashMap<String, String> = HashMap::new();
        for (k, r) in &self.records {
            if r.mac.is_some() && !r.ip.is_empty() {
                ips_with_mac.insert(r.ip.clone(), k.clone());
            }
        }

        let mut to_remove = Vec::new();
        for (k, r) in &self.records {
            if r.mac.is_none() && !r.ip.is_empty() {
                if let Some(mac_key) = ips_with_mac.get(&r.ip) {
                    if mac_key != k {
                        to_remove.push((k.clone(), mac_key.clone()));
                    }
                }
            }
        }

        for (unkeyed, mac_key) in to_remove {
            if let Some(old) = self.records.remove(&unkeyed) {
                if let Some(mac_rec) = self.records.get_mut(&mac_key) {
                    Self::merge_records(mac_rec, old);
                }
            }
        }
    }

    /// Merges an incoming or secondary record into a target record, preserving non-generic details.
    fn merge_records(target: &mut DeviceHistoryRecord, other: DeviceHistoryRecord) {
        if other.is_active {
            target.is_active = true;
            target.was_ever_active = true;
        }
        if other.was_ever_active {
            target.was_ever_active = true;
        }
        for s in other.sources {
            if !target.sources.contains(&s) {
                target.sources.push(s);
            }
        }
        if !target.sources.contains(&other.source) && !other.source.is_empty() {
            target.sources.push(other.source.clone());
        }
        if other.last_seen > target.last_seen {
            target.last_seen = other.last_seen;
        }
        if target.first_seen.is_empty()
            || (!other.first_seen.is_empty() && other.first_seen < target.first_seen)
        {
            target.first_seen = other.first_seen;
        }

        // Anti-downgrade for display_name: never replace friendly name with generic "Host ..."
        let target_generic = target.display_name.starts_with("Host ") || target.display_name.starts_with("net-");
        let other_generic = other.display_name.starts_with("Host ") || other.display_name.starts_with("net-");
        if target_generic && !other_generic {
            target.display_name = other.display_name;
        }

        // Anti-downgrade for kind / category: never replace specific category with "network-device"
        if target.kind == "network-device" && other.kind != "network-device" {
            target.kind = other.kind;
            target.category_title = other.category_title;
            target.category_icon = other.category_icon;
            target.script_id = other.script_id;
        }

        if target.mac.is_none() && other.mac.is_some() {
            target.mac = other.mac;
        }
        if target.hostname.is_none() && other.hostname.is_some() {
            target.hostname = other.hostname;
        }
        if target.vendor.is_none() && other.vendor.is_some() {
            target.vendor = other.vendor;
        }
        if target.vendor_id.is_none() && other.vendor_id.is_some() {
            target.vendor_id = other.vendor_id;
        }
        if target.product_id.is_none() && other.product_id.is_some() {
            target.product_id = other.product_id;
            target.product_name = other.product_name;
        }
        if target.web_url.is_none() && other.web_url.is_some() {
            target.web_url = other.web_url;
        }
        if target.matter_fabrics.is_none() && other.matter_fabrics.is_some() {
            target.matter_fabrics = other.matter_fabrics;
        }
        for cap in other.capabilities {
            if !target.capabilities.contains(&cap) {
                target.capabilities.push(cap);
            }
        }
    }

    /// Seeds known configured devices from categories and documentation files if not already present.
    pub fn seed_known_devices(
        &mut self,
        categories_path: &Path,
        docs_path: &Path,
        catalog: &homenode_definitions::CatalogDatabase,
    ) {
        let now = Utc::now().to_rfc3339();

        // 1. Inspect categories
        if let Ok(content) = std::fs::read_to_string(categories_path) {
            if let Ok(cats) = serde_json::from_str::<HashMap<String, String>>(&content) {
                for (id_or_mac, cat) in cats {
                    let norm = id_or_mac.trim().to_lowercase();
                    let is_mac = norm.contains(':') && norm.len() == 17;
                    let existing_key = if is_mac {
                        self.records.iter().find_map(|(k, r)| {
                            if k == &norm
                                || r.mac.as_deref().map(|m| m.trim().to_lowercase()) == Some(norm.clone())
                            {
                                Some(k.clone())
                            } else {
                                None
                            }
                        })
                    } else {
                        self.records.iter().find_map(|(k, r)| {
                            if k == &norm || r.device_id == norm {
                                Some(k.clone())
                            } else {
                                None
                            }
                        })
                    };

                    if let Some(k) = existing_key {
                        if let Some(r) = self.records.get_mut(&k) {
                            if r.kind == "network-device" {
                                r.kind = cat.clone();
                            }
                        }
                    } else if is_mac {
                        // Create placeholder historical record for previously configured MAC
                        let vendor = catalog.find_vendor_by_mac(&norm).map(|v| v.name.clone());
                        let display_name = match &vendor {
                            Some(v) => format!("{v} Device"),
                            None => format!("Device {norm}"),
                        };
                        let rec = DeviceHistoryRecord {
                            device_id: format!("net-{}", norm.replace(':', "-")),
                            display_name,
                            kind: cat,
                            category_title: None,
                            category_icon: None,
                            script_id: None,
                            ip: if norm == "4c:cf:7c:ca:69:be" { "192.168.178.153".to_string() } else { String::new() },
                            mac: Some(norm.clone()),
                            hostname: None,
                            interface: "lan".to_string(),
                            vendor,
                            capabilities: vec!["configured".to_string()],
                            source: "configuration".to_string(),
                            sources: vec!["configuration".to_string()],
                            web_url: None,
                            product_id: None,
                            product_name: None,
                            vendor_id: None,
                            matter_fabrics: None,
                            first_seen: now.clone(),
                            last_seen: now.clone(),
                            is_active: false,
                            was_ever_active: true,
                        };
                        self.records.insert(norm, rec);
                    }
                }
            }
        }

        // 2. Inspect documentation for known devices
        if let Ok(content) = std::fs::read_to_string(docs_path) {
            if let Ok(docs) = serde_json::from_str::<HashMap<String, serde_json::Value>>(&content) {
                for (doc_key, _) in docs {
                    let norm = doc_key.trim().to_lowercase();
                    let is_mac = norm.contains(':') && norm.len() == 17;
                    let existing_key = if is_mac {
                        self.records.iter().find_map(|(k, r)| {
                            if k == &norm
                                || r.mac.as_deref().map(|m| m.trim().to_lowercase()) == Some(norm.clone())
                            {
                                Some(k.clone())
                            } else {
                                None
                            }
                        })
                    } else {
                        self.records.iter().find_map(|(k, r)| {
                            if k == &norm || r.device_id == norm {
                                Some(k.clone())
                            } else {
                                None
                            }
                        })
                    };

                    if existing_key.is_none() {
                        let is_ip_key = norm.starts_with("net-");
                        let ip = if is_ip_key {
                            norm.strip_prefix("net-").unwrap_or("").replace('-', ".")
                        } else {
                            String::new()
                        };
                        let mac = if is_mac { Some(norm.clone()) } else { None };
                        let vendor = mac.as_deref().and_then(|m| catalog.find_vendor_by_mac(m)).map(|v| v.name.clone());
                        let rec = DeviceHistoryRecord {
                            device_id: if is_ip_key { norm.clone() } else { format!("net-{}", norm.replace(':', "-")) },
                            display_name: format!("Documented Device ({doc_key})"),
                            kind: "computer".to_string(),
                            category_title: Some("Computers & Laptops".to_string()),
                            category_icon: Some("💻".to_string()),
                            script_id: None,
                            ip,
                            mac,
                            hostname: None,
                            interface: "lan".to_string(),
                            vendor,
                            capabilities: vec!["documented".to_string()],
                            source: "documentation".to_string(),
                            sources: vec!["documentation".to_string()],
                            web_url: None,
                            product_id: None,
                            product_name: None,
                            vendor_id: None,
                            matter_fabrics: None,
                            first_seen: now.clone(),
                            last_seen: now.clone(),
                            is_active: false,
                            was_ever_active: true,
                        };
                        self.records.insert(norm, rec);
                    }
                }
            }
        }
    }

    /// Reconciles current sweep against history.
    /// - Matches by key, MAC address, or IP.
    /// - Re-keys IP-keyed records when MAC address becomes known, preventing duplicates.
    /// - Strictly preserves known classifications and friendly hostnames across temporary DNS/mDNS timeouts.
    /// - Devices in current sweep become active (is_active = true) with updated last_seen.
    /// - Previously seen devices not in current sweep become inactive (is_active = false), retaining their last_seen.
    pub fn reconcile_sweep(
        &mut self,
        module_id: &str,
        sweep: Vec<DiscoveredDevice>,
    ) -> Vec<DeviceRecord> {
        let now = Utc::now().to_rfc3339();
        let mut seen_keys = HashSet::new();

        for dev in sweep {
            let target_key = Self::record_key(dev.mac.as_deref(), &dev.device_id);
            seen_keys.insert(target_key.clone());

            // 1. Find if an existing record matches by key, MAC, or IP
            let matched_key = if self.records.contains_key(&target_key) {
                Some(target_key.clone())
            } else if let Some(ref mac) = dev.mac {
                let norm = mac.trim().to_lowercase();
                self.records.iter().find_map(|(k, r)| {
                    if r.mac.as_deref().map(|m| m.trim().to_lowercase()) == Some(norm.clone()) {
                        Some(k.clone())
                    } else {
                        None
                    }
                })
            } else {
                None
            };

            // If still not matched, check by IP
            let matched_key = matched_key.or_else(|| {
                if !dev.ip.is_empty() {
                    self.records.iter().find_map(|(k, r)| {
                        if r.ip == dev.ip {
                            Some(k.clone())
                        } else {
                            None
                        }
                    })
                } else {
                    None
                }
            });

            if let Some(old_key) = matched_key {
                let mut existing = self.records.remove(&old_key).unwrap();
                let is_active = dev.is_active.unwrap_or(true);
                if is_active {
                    existing.last_seen = now.clone();
                    existing.is_active = true;
                    existing.was_ever_active = true;
                } else if !existing.is_active {
                    // Stays inactive, preserve earlier last_seen
                } else {
                    existing.is_active = false;
                }
                existing.ip = dev.ip.clone();
                existing.interface = dev.interface.clone();
                existing.source = dev.source.clone();

                for s in dev.sources {
                    if !existing.sources.contains(&s) {
                        existing.sources.push(s);
                    }
                }
                if !existing.sources.contains(&dev.source) && !dev.source.is_empty() {
                    existing.sources.push(dev.source.clone());
                }

                if dev.mac.is_some() {
                    existing.mac = dev.mac.clone();
                }

                // Anti-downgrade for display_name: never overwrite friendly name with "Host ..."
                let dev_is_generic = dev.display_name.starts_with("Host ") || dev.display_name.starts_with("net-");
                let existing_is_generic = existing.display_name.starts_with("Host ") || existing.display_name.starts_with("net-");
                if !dev_is_generic || existing_is_generic {
                    existing.display_name = dev.display_name;
                }

                // Anti-downgrade for kind / category: never downgrade specific category to "network-device"
                if dev.kind != "network-device" || existing.kind == "network-device" {
                    existing.kind = dev.kind;
                    existing.category_title = dev.category_title;
                    existing.category_icon = dev.category_icon;
                    existing.script_id = dev.script_id;
                }

                if dev.hostname.is_some() {
                    existing.hostname = dev.hostname;
                }
                if dev.vendor.is_some() {
                    existing.vendor = dev.vendor;
                }
                if dev.vendor_id.is_some() {
                    existing.vendor_id = dev.vendor_id;
                }
                if dev.product_id.is_some() {
                    existing.product_id = dev.product_id;
                    existing.product_name = dev.product_name;
                }
                if dev.web_url.is_some() {
                    existing.web_url = dev.web_url;
                }
                if dev.matter_fabrics.is_some() {
                    existing.matter_fabrics = dev.matter_fabrics;
                }
                for cap in dev.capabilities {
                    if !existing.capabilities.contains(&cap) {
                        existing.capabilities.push(cap);
                    }
                }
                self.records.insert(target_key, existing);
            } else {
                let is_active = dev.is_active.unwrap_or(true);
                let was_ever_active = is_active;
                let mut sources = dev.sources;
                if sources.is_empty() && !dev.source.is_empty() {
                    sources.push(dev.source.clone());
                }
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
                    sources,
                    web_url: dev.web_url,
                    product_id: dev.product_id,
                    product_name: dev.product_name,
                    vendor_id: dev.vendor_id,
                    matter_fabrics: dev.matter_fabrics,
                    first_seen: now.clone(),
                    last_seen: now.clone(),
                    is_active,
                    was_ever_active,
                };
                self.records.insert(target_key, rec);
            }
        }

        // Mark previously recorded devices missing from this sweep as inactive
        for (k, record) in &mut self.records {
            if !seen_keys.contains(k) {
                record.is_active = false;
            }
        }

        self.cleanup_duplicates();

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
        let sources_str = if r.sources.is_empty() {
            r.source.clone()
        } else {
            r.sources.join(",")
        };
        metadata.insert("sources".to_string(), sources_str);

        // History & Status fields
        metadata.insert("first_seen".to_string(), r.first_seen.clone());
        metadata.insert("last_seen".to_string(), r.last_seen.clone());
        let status = if r.is_active {
            "active"
        } else if r.was_ever_active {
            "inactive"
        } else {
            "archive"
        };
        metadata.insert("status".to_string(), status.to_string());
        metadata.insert("is_active".to_string(), if r.is_active { "true".to_string() } else { "false".to_string() });
        metadata.insert("was_ever_active".to_string(), if r.was_ever_active { "true".to_string() } else { "false".to_string() });

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
            sources: vec!["arp".to_string()],
            web_url: None,
            product_id: None,
            product_name: None,
            vendor_id: None,
            matter_fabrics: None,
            is_active: None,
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

    #[test]
    fn reconcile_avoids_duplicate_when_mac_discovered_later() {
        let mut store = DeviceHistoryStore::new();

        // Sweep 1: Ping discovered device before ARP populated (mac is None)
        let mut dev_no_mac = sample_device("192.168.178.24", "", "fronius.fritz.box");
        dev_no_mac.mac = None;
        dev_no_mac.kind = "energy".to_string();

        let records1 = store.reconcile_sweep("network-discovery", vec![dev_no_mac]);
        assert_eq!(records1.len(), 1);
        assert!(store.records.contains_key("net-192-168-178-24"));

        // Sweep 2: ARP populated, MAC is now discovered
        let mut dev_with_mac = sample_device("192.168.178.24", "00:03:ac:07:36:30", "fronius.fritz.box");
        dev_with_mac.kind = "energy".to_string();

        let records2 = store.reconcile_sweep("network-discovery", vec![dev_with_mac]);
        // Must NOT create a ghost duplicate!
        assert_eq!(records2.len(), 1, "Must have exactly 1 record, not 2!");
        assert_eq!(store.records.len(), 1);
        assert!(store.records.contains_key("00:03:ac:07:36:30"));
        assert!(!store.records.contains_key("net-192-168-178-24"));
        assert_eq!(store.records.get("00:03:ac:07:36:30").unwrap().mac.as_deref(), Some("00:03:ac:07:36:30"));
        assert!(store.records.get("00:03:ac:07:36:30").unwrap().is_active);
    }

    #[test]
    fn reconcile_anti_downgrade_preserves_classification_and_name() {
        let mut store = DeviceHistoryStore::new();

        // Sweep 1: Clean classification
        let mut dev1 = sample_device("192.168.178.131", "ae:c6:fe:d5:a7:81", "iPhone Stephan");
        dev1.kind = "phone".to_string();
        dev1.category_title = Some("Smartphones".to_string());
        dev1.category_icon = Some("📱".to_string());
        store.reconcile_sweep("network-discovery", vec![dev1]);

        let stored = store.records.get("ae:c6:fe:d5:a7:81").unwrap();
        assert_eq!(stored.display_name, "iPhone Stephan");
        assert_eq!(stored.kind, "phone");

        // Sweep 2: Subsequent sweep where DNS timed out and device reverted to generic "Host ..."
        let mut dev2 = sample_device("192.168.178.131", "ae:c6:fe:d5:a7:81", "Host 192.168.178.131");
        dev2.kind = "network-device".to_string();
        dev2.category_title = None;
        dev2.category_icon = None;
        dev2.hostname = None;
        store.reconcile_sweep("network-discovery", vec![dev2]);

        // Anti-downgrade MUST preserve the friendly name and phone category
        let preserved = store.records.get("ae:c6:fe:d5:a7:81").unwrap();
        assert_eq!(preserved.display_name, "iPhone Stephan", "Friendly name must not be overwritten by Host ...");
        assert_eq!(preserved.kind, "phone", "Category must not be downgraded to network-device");
        assert_eq!(preserved.category_title.as_deref(), Some("Smartphones"));
        assert_eq!(preserved.category_icon.as_deref(), Some("📱"));
    }

    #[test]
    fn reconcile_distinguishes_homenode_history_from_router_archive() {
        let mut store = DeviceHistoryStore::new();

        // 1. Device A: Active HomeNode device
        let dev_a = sample_device("192.168.178.10", "aa:bb:cc:dd:ee:01", "MacBook");
        
        // 2. Device B: Pure inactive FRITZ!Box TR-064 device
        let mut dev_b = sample_device("192.168.178.99", "aa:bb:cc:dd:ee:99", "OldGuestPhone");
        dev_b.source = "fritzbox-tr064".to_string();
        dev_b.sources = vec!["fritzbox-tr064".to_string()];
        dev_b.is_active = Some(false);

        let records1 = store.reconcile_sweep("network-discovery", vec![dev_a, dev_b]);
        assert_eq!(records1.len(), 2);

        let proto_a = records1.iter().find(|r| r.device_id == "net-192-168-178-10").unwrap();
        let proto_b = records1.iter().find(|r| r.device_id == "net-192-168-178-99").unwrap();

        assert_eq!(proto_a.metadata.get("status").map(|s| s.as_str()), Some("active"));
        assert_eq!(proto_a.metadata.get("was_ever_active").map(|s| s.as_str()), Some("true"));

        // Pure TR-064 device should have status "archive"
        assert_eq!(proto_b.metadata.get("status").map(|s| s.as_str()), Some("archive"));
        assert_eq!(proto_b.metadata.get("was_ever_active").map(|s| s.as_str()), Some("false"));
        assert_eq!(proto_b.metadata.get("sources").map(|s| s.as_str()), Some("fritzbox-tr064"));

        // Sweep 2: Device A goes offline (HomeNode device disappears)
        let records2 = store.reconcile_sweep("network-discovery", vec![]);
        let proto_a_sweep2 = records2.iter().find(|r| r.device_id == "net-192-168-178-10").unwrap();

        // Device A was previously active, so status becomes "inactive" (genuine HomeNode history!)
        assert_eq!(proto_a_sweep2.metadata.get("status").map(|s| s.as_str()), Some("inactive"));
        assert_eq!(proto_a_sweep2.metadata.get("was_ever_active").map(|s| s.as_str()), Some("true"));
    }
}
