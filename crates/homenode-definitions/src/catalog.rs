use std::path::Path;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Vendor {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub website: Option<String>,
    #[serde(default)]
    pub support_url: Option<String>,
    #[serde(default = "default_vendor_icon")]
    pub icon: String,
    #[serde(default)]
    pub oui_prefixes: Vec<String>,
    #[serde(default)]
    pub protocols: Vec<String>,
    #[serde(default)]
    pub description: Option<String>,
}

fn default_vendor_icon() -> String {
    "🏢".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Product {
    pub id: String,
    pub vendor_id: String,
    pub name: String,
    #[serde(default)]
    pub model_number: Option<String>,
    pub category: String,
    #[serde(default = "default_product_icon")]
    pub category_icon: String,
    #[serde(default)]
    pub connectivity: Vec<String>,
    #[serde(default)]
    pub matter_device_type: Option<String>,
    #[serde(default)]
    pub default_ports: Vec<u16>,
    #[serde(default)]
    pub hostname_patterns: Vec<String>,
    #[serde(default)]
    pub documentation_url: Option<String>,
    #[serde(default)]
    pub specs: Option<String>,
    #[serde(default)]
    pub rhai_script_ref: Option<String>,
}

fn default_product_icon() -> String {
    "🔌".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct CatalogDatabase {
    #[serde(default)]
    pub vendors: Vec<Vendor>,
    #[serde(default)]
    pub products: Vec<Product>,
}

impl CatalogDatabase {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn load_from_str(content: &str) -> Result<Self> {
        let db: CatalogDatabase = serde_json::from_str(content)
            .context("failed to parse catalog JSON")?;
        Ok(db)
    }

    pub fn load_from_path(path: impl AsRef<Path>) -> Result<Self> {
        let p = path.as_ref();
        let content = std::fs::read_to_string(p)
            .with_context(|| format!("failed to read catalog file {}", p.display()))?;
        Self::load_from_str(&content)
    }

    pub fn save_to_path(&self, path: impl AsRef<Path>) -> Result<()> {
        let p = path.as_ref();
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let json = serde_json::to_string_pretty(self)
            .context("failed to serialize catalog database")?;
        std::fs::write(p, json)
            .with_context(|| format!("failed to write catalog to {}", p.display()))?;
        Ok(())
    }

    pub fn merge(&mut self, other: CatalogDatabase) {
        for v in other.vendors {
            self.add_or_update_vendor(v);
        }
        for p in other.products {
            self.add_or_update_product(p);
        }
    }

    pub fn add_or_update_vendor(&mut self, vendor: Vendor) {
        if let Some(existing) = self.vendors.iter_mut().find(|v| v.id == vendor.id) {
            *existing = vendor;
        } else {
            self.vendors.push(vendor);
        }
    }

    pub fn add_or_update_product(&mut self, product: Product) {
        if let Some(existing) = self.products.iter_mut().find(|p| p.id == product.id) {
            *existing = product;
        } else {
            self.products.push(product);
        }
    }

    pub fn find_vendor(&self, vendor_id: &str) -> Option<&Vendor> {
        self.vendors.iter().find(|v| v.id.eq_ignore_ascii_case(vendor_id))
    }

    pub fn find_product(&self, product_id: &str) -> Option<&Product> {
        self.products.iter().find(|p| p.id.eq_ignore_ascii_case(product_id))
    }

    pub fn products_for_vendor<'a>(&'a self, vendor_id: &str) -> Vec<&'a Product> {
        self.products
            .iter()
            .filter(|p| p.vendor_id.eq_ignore_ascii_case(vendor_id))
            .collect()
    }

    /// Resolve vendor name and profile using MAC OUI prefix lookup
    pub fn find_vendor_by_mac(&self, mac: &str) -> Option<&Vendor> {
        let norm = normalize_mac(mac);
        let parts: Vec<&str> = norm.split(':').collect();
        if parts.len() < 3 {
            return None;
        }

        // Check 3-octet prefix (e.g. "b4:fc:7d")
        let prefix3 = format!("{}:{}:{}", parts[0], parts[1], parts[2]).to_lowercase();
        for vendor in &self.vendors {
            if vendor.oui_prefixes.iter().any(|p| p.to_lowercase() == prefix3) {
                return Some(vendor);
            }
        }

        // Check 4-octet prefix if present (e.g. "b4:fc:7d:00")
        if parts.len() >= 4 {
            let prefix4 = format!("{}:{}:{}:{}", parts[0], parts[1], parts[2], parts[3]).to_lowercase();
            for vendor in &self.vendors {
                if vendor.oui_prefixes.iter().any(|p| p.to_lowercase() == prefix4) {
                    return Some(vendor);
                }
            }
        }

        None
    }

    /// Find product matching given hostname, optional vendor hint, or open ports
    pub fn match_product(
        &self,
        hostname: &str,
        vendor_hint: Option<&str>,
        open_ports: &[u16],
    ) -> Option<&Product> {
        let raw_host = hostname.to_lowercase();
        let vendor_lower = vendor_hint.unwrap_or("").to_lowercase();

        // Strip local DNS suffixes (.fritz.box, .local, .lan, .home.arpa, .home, etc.)
        let mut host_clean = raw_host.as_str();
        for suffix in &[
            ".fritz.box",
            ".fritz.box.",
            ".local",
            ".local.",
            ".lan",
            ".lan.",
            ".home",
            ".home.arpa",
            ".internal",
        ] {
            if let Some(stripped) = host_clean.strip_suffix(suffix) {
                host_clean = stripped;
                break;
            }
        }

        // 1. High confidence: hostname pattern matching
        let mut best_match: Option<(&Product, usize, bool)> = None;

        for product in &self.products {
            let v_name = self.find_vendor(&product.vendor_id).map(|v| v.name.to_lowercase()).unwrap_or_default();
            let norm_vendor_id = product.vendor_id.replace('_', " ");
            let vendor_matches = !vendor_lower.is_empty()
                && (product.vendor_id.eq_ignore_ascii_case(&vendor_lower)
                    || vendor_lower.contains(&product.vendor_id)
                    || product.vendor_id.contains(&vendor_lower)
                    || vendor_lower.contains(&norm_vendor_id)
                    || (!v_name.is_empty() && (vendor_lower.contains(&v_name) || v_name.contains(&vendor_lower))));

            // If a specific vendor hint was provided, do not match products with conflicting vendors or generic switches
            if !vendor_lower.is_empty() && !vendor_matches {
                if product.vendor_id == "generic_switch" || !product.vendor_id.starts_with("generic") {
                    continue;
                }
            }

            for pattern in &product.hostname_patterns {
                let pat = pattern.to_lowercase();
                if pat.is_empty() {
                    continue;
                }
                let is_match = if pat.contains('.') {
                    raw_host == pat || raw_host == format!("{pat}.")
                } else if pat.len() <= 6 && (pat == "switch" || pat == "hub" || pat == "router" || pat == "bridge") {
                    host_clean == pat || host_clean.starts_with(&format!("{pat}-")) || host_clean.ends_with(&format!("-{pat}"))
                } else {
                    host_clean.contains(&pat)
                };

                if is_match {
                    let score = pat.len();
                    match best_match {
                        None => {
                            best_match = Some((product, score, vendor_matches));
                        }
                        Some((_, prev_score, prev_vendor_match)) => {
                            if (vendor_matches && !prev_vendor_match)
                                || (vendor_matches == prev_vendor_match && score > prev_score)
                            {
                                best_match = Some((product, score, vendor_matches));
                            }
                        }
                    }
                }
            }
        }

        if let Some((prod, _, _)) = best_match {
            return Some(prod);
        }

        // 2. Medium confidence: vendor match + port/service cues
        for product in &self.products {
            if !vendor_lower.is_empty() && product.vendor_id.eq_ignore_ascii_case(&vendor_lower) {
                if !product.default_ports.is_empty()
                    && product.default_ports.iter().any(|dp| open_ports.contains(dp))
                {
                    return Some(product);
                }
            }
        }

        None
    }
}

fn normalize_mac(mac: &str) -> String {
    mac.split(':')
        .map(|part| {
            if part.len() == 1 {
                format!("0{part}")
            } else {
                part.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(":")
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_catalog_load_and_lookup() {
        let json = r#"{
            "vendors": [
                {
                    "id": "shelly",
                    "name": "Shelly",
                    "website": "https://www.shelly.com",
                    "icon": "⚡",
                    "oui_prefixes": ["c4:5b:be", "08:3a:88"],
                    "protocols": ["wifi", "matter"]
                }
            ],
            "products": [
                {
                    "id": "shelly_pro_3em",
                    "vendor_id": "shelly",
                    "name": "Shelly Pro 3EM",
                    "model_number": "S3EM-002CEBEU",
                    "category": "energy",
                    "category_icon": "⚡",
                    "connectivity": ["lan", "wifi"],
                    "matter_device_type": "Electrical Sensor (0x0503)",
                    "default_ports": [80],
                    "hostname_patterns": ["shellypro3em"],
                    "specs": "3-phase DIN-rail energy meter"
                }
            ]
        }"#;

        let catalog = CatalogDatabase::load_from_str(json).expect("valid json");
        assert_eq!(catalog.vendors.len(), 1);
        assert_eq!(catalog.products.len(), 1);

        // OUI lookup
        let vendor = catalog.find_vendor_by_mac("c4:5b:be:12:34:56");
        assert!(vendor.is_some());
        assert_eq!(vendor.unwrap().name, "Shelly");

        // Product match
        let prod = catalog.match_product("shellypro3em.fritz.box", Some("Shelly"), &[80]);
        assert!(prod.is_some());
        assert_eq!(prod.unwrap().id, "shelly_pro_3em");
        assert_eq!(prod.unwrap().matter_device_type.as_deref(), Some("Electrical Sensor (0x0503)"));
    }

    #[test]
    fn test_load_bundled_catalog() {
        let path = std::path::Path::new("../../definitions/catalog.json");
        if path.exists() {
            let catalog = CatalogDatabase::load_from_path(path).expect("valid bundled catalog.json");
            assert!(catalog.vendors.len() >= 15);
            assert!(catalog.products.len() >= 20);

            // Test AVM lookup
            let avm = catalog.find_vendor_by_mac("b4:fc:7d:12:34:56");
            assert!(avm.is_some());
            assert_eq!(avm.unwrap().id, "avm");

            // Test EcoFlow lookup
            let ecoflow = catalog.find_vendor_by_mac("a0:85:e3:de:01:10");
            assert!(ecoflow.is_some());
            assert_eq!(ecoflow.unwrap().id, "ecoflow");

            // Test Shelly Pro 3EM match
            let prod = catalog.match_product("shellypro3em.fritz.box", None, &[80]);
            assert!(prod.is_some());
            assert_eq!(prod.unwrap().id, "shelly_pro_3em");

            // Test fritz.box exact match vs fritz.box suffix
            let fritz = catalog.match_product("fritz.box", None, &[]);
            assert!(fritz.is_some());
            assert_eq!(fritz.unwrap().id, "fritzbox_gateway");

            let non_fritz = catalog.match_product("espressif2.fritz.box", None, &[]);
            assert!(non_fritz.is_none() || non_fritz.unwrap().id != "fritzbox_gateway");

            // Test iPad models
            let ipad_air = catalog.match_product("ipadair.fritz.box", Some("Apple Inc."), &[]);
            assert!(ipad_air.is_some());
            assert_eq!(ipad_air.unwrap().id, "apple_ipad_air");

            let ipad_pro_m2 = catalog.match_product("ipadm2.fritz.box", Some("Apple Inc."), &[]);
            assert!(ipad_pro_m2.is_some());
            assert_eq!(ipad_pro_m2.unwrap().id, "apple_ipad_pro_m2");

            let ipad_pro_m5 = catalog.match_product("ipadm5.fritz.box", Some("Apple Inc."), &[]);
            assert!(ipad_pro_m5.is_some());
            assert_eq!(ipad_pro_m5.unwrap().id, "apple_ipad_pro_m5");

            // Test iPhone product variants
            let iphone13pro = catalog.match_product("iphone13pro.fritz.box", Some("Apple Inc."), &[]);
            assert!(iphone13pro.is_some());
            assert_eq!(iphone13pro.unwrap().id, "apple_iphone_13_pro");

            let iphone16pro = catalog.match_product("iphone16pro.fritz.box", Some("Apple Inc."), &[]);
            assert!(iphone16pro.is_some());
            assert_eq!(iphone16pro.unwrap().id, "apple_iphone_16_pro");

            let iphone16e = catalog.match_product("iphone16e.fritz.box", Some("Apple Inc."), &[]);
            assert!(iphone16e.is_some());
            assert_eq!(iphone16e.unwrap().id, "apple_iphone_16e");

            let iphone17pro = catalog.match_product("iphone17pro.fritz.box", Some("Apple Inc."), &[]);
            assert!(iphone17pro.is_some());
            assert_eq!(iphone17pro.unwrap().id, "apple_iphone_17_pro");

            let iphonemini = catalog.match_product("iphone-mini.fritz.box", Some("Apple Inc."), &[]);
            assert!(iphonemini.is_some());
            assert_eq!(iphonemini.unwrap().id, "apple_iphone_mini");

            let iphonese2 = catalog.match_product("iphonese2.fritz.box", Some("Apple Inc."), &[]);
            assert!(iphonese2.is_some());
            assert_eq!(iphonese2.unwrap().id, "apple_iphone_se2");

            let iphone_generic = catalog.match_product("iphone.fritz.box", Some("Apple Inc."), &[]);
            assert!(iphone_generic.is_some());
            assert_eq!(iphone_generic.unwrap().id, "apple_iphone");

            // Test HP corporate laptop lookup & product match
            let hp_vendor = catalog.find_vendor_by_mac("4c:cf:7c:ca:69:be");
            assert!(hp_vendor.is_some());
            assert_eq!(hp_vendor.unwrap().id, "hp");

            let hp_laptop = catalog.match_product("hensoldt-steffi.fritz.box", Some("HP Inc."), &[]);
            assert!(hp_laptop.is_some());
            assert_eq!(hp_laptop.unwrap().id, "hp_business_laptop");

            // Test Discovergy Smart Meter Gateway lookup & product match
            let disc_vendor = catalog.find_vendor_by_mac("2c:dd:0c:a5:94:64");
            assert!(disc_vendor.is_some());
            assert_eq!(disc_vendor.unwrap().id, "discovergy");

            let disc_gateway = catalog.match_product("edgy0020071074.fritz.box", Some("Discovergy GmbH"), &[]);
            assert!(disc_gateway.is_some());
            assert_eq!(disc_gateway.unwrap().id, "discovergy_edgy_gateway");

            // Test WireGuard VPN peer match
            let vpn_peer = catalog.match_product("iphonestephan.fritz.box", Some("WireGuard / FRITZ!Box VPN"), &[]);
            assert!(vpn_peer.is_some());
            assert_eq!(vpn_peer.unwrap().id, "wireguard_vpn_peer");

            // Test Philips Hue Wall Switch / Dimmer vs Network Switch
            let hue_switch = catalog.match_product("Switch Sophie", Some("Philips Hue (Signify)"), &[]);
            assert!(hue_switch.is_none() || hue_switch.unwrap().id != "unmanaged_switch_l2");

            let hue_dimmer = catalog.match_product("Hue Dimmer Switch", Some("Philips Hue (Signify)"), &[]);
            assert!(hue_dimmer.is_some());
            assert_eq!(hue_dimmer.unwrap().id, "philips_hue_dimmer_switch");
        }
    }
}
