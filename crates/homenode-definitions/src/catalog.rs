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
        let host_lower = hostname.to_lowercase();
        let vendor_lower = vendor_hint.unwrap_or("").to_lowercase();

        // 1. High confidence: exact hostname substring matches
        for product in &self.products {
            for pattern in &product.hostname_patterns {
                let pat = pattern.to_lowercase();
                if !pat.is_empty() && host_lower.contains(&pat) {
                    return Some(product);
                }
            }
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
        }
    }
}
