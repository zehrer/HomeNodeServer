use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RoomRecord {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub floor: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub archetype: Option<String>,
    #[serde(default)]
    pub matter_tag: Option<String>,
    #[serde(default)]
    pub hue_group_id: Option<String>,
    #[serde(default)]
    pub hue_class: Option<String>,
    #[serde(default)]
    pub sort_order: Option<i32>,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
}

impl RoomRecord {
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        floor: Option<String>,
        icon: Option<String>,
        archetype: Option<String>,
    ) -> Self {
        let name_str = name.into();
        let arch = archetype.or_else(|| deduce_archetype_from_name(&name_str));
        let deduced_icon = icon.or_else(|| arch.as_deref().map(default_icon_for_archetype).map(|s| s.to_string()));
        let deduced_floor = floor.or_else(|| deduce_floor_from_name(&name_str).map(|s| s.to_string()));
        let matter_tag = arch.as_deref().map(matter_area_tag_for_archetype).map(|s| s.to_string());
        let now = Utc::now().to_rfc3339();

        Self {
            id: id.into(),
            name: name_str,
            floor: deduced_floor,
            icon: deduced_icon,
            archetype: arch,
            matter_tag,
            hue_group_id: None,
            hue_class: None,
            sort_order: None,
            created_at: now.clone(),
            updated_at: now,
        }
    }
}

pub fn slugify_room_id(name: &str) -> String {
    name.trim()
        .to_lowercase()
        .replace(['ä', 'Ä'], "ae")
        .replace(['ö', 'Ö'], "oe")
        .replace(['ü', 'Ü'], "ue")
        .replace('ß', "ss")
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

pub fn deduce_archetype_from_name(name: &str) -> Option<String> {
    let lower = name.to_lowercase();
    if lower.contains("wohn") || lower.contains("living") {
        Some("living_room".to_string())
    } else if lower.contains("küh") || lower.contains("kueh") || lower.contains("kitchen") {
        Some("kitchen".to_string())
    } else if lower.contains("ess") || lower.contains("dining") {
        Some("dining_room".to_string())
    } else if lower.contains("schlaf") || lower.contains("bedroom") {
        Some("bedroom".to_string())
    } else if lower.contains("kinder") || lower.contains("tom") || lower.contains("sophie") || lower.contains("kids") {
        Some("kids_room".to_string())
    } else if lower.contains("bad") || lower.contains("bath") {
        Some("bathroom".to_string())
    } else if lower.contains("wc") || lower.contains("toilet") {
        Some("toilet".to_string())
    } else if lower.contains("büro") || lower.contains("buero") || lower.contains("office") || lower.contains("arbeit") {
        Some("office".to_string())
    } else if lower.contains("windfang") || lower.contains("flur") || lower.contains("hall") || lower.contains("corridor") || lower.contains("diele") {
        Some("hallway".to_string())
    } else if lower.contains("treppe") || lower.contains("stairs") {
        Some("stairs".to_string())
    } else if lower.contains("sauna") || lower.contains("wellness") || lower.contains("spa") {
        Some("spa".to_string())
    } else if lower.contains("keller") || lower.contains("basement") || lower.contains("storage") || lower.contains("lager") {
        Some("basement".to_string())
    } else if lower.contains("vordach") || lower.contains("garten") || lower.contains("outdoor") || lower.contains("balkon") || lower.contains("terrasse") {
        Some("outdoor".to_string())
    } else if lower.contains("garage") || lower.contains("carport") {
        Some("garage".to_string())
    } else {
        Some("other".to_string())
    }
}

pub fn default_icon_for_archetype(archetype_or_class: &str) -> &'static str {
    match archetype_or_class.to_lowercase().replace(['_', '-', ' '], "").as_str() {
        "livingroom" | "living" | "wohnzimmer" => "🛋️",
        "kitchen" | "kueche" | "küche" => "🍳",
        "dining" | "diningroom" | "esszimmer" => "🍽️",
        "bedroom" | "schlafzimmer" => "🛏️",
        "kidsbedroom" | "kidsroom" | "kinderzimmer" => "🧸",
        "bathroom" | "bath" | "bad" | "badezimmer" => "🛁",
        "toilet" | "wc" | "gästewc" => "🚽",
        "office" | "büro" | "buero" | "arbeitszimmer" => "💼",
        "hallway" | "corridor" | "flur" | "windfang" | "diele" => "🚪",
        "stairs" | "treppenhaus" | "treppe" => "🪜",
        "balcony" | "terrace" | "balkon" | "terrasse" => "🪴",
        "garden" | "outdoor" | "garten" | "vordach" | "hof" | "frontdoor" => "🌳",
        "garage" | "carport" => "🚗",
        "basement" | "cellar" | "keller" | "storage" => "📦",
        "spa" | "sauna" | "wellness" => "🧖",
        "hobbyroom" | "hobby" | "serverroom" | "technik" => "🖥️",
        "attic" | "topfloor" | "dachgeschoss" | "dg" => "🏠",
        _ => "📍",
    }
}

pub fn deduce_floor_from_name(name: &str) -> Option<&'static str> {
    let lower = name.to_lowercase();
    if lower.contains("vordach")
        || lower.contains("garten")
        || lower.contains("terrasse")
        || lower.contains("balkon")
        || lower.contains("garage")
        || lower.contains("carport")
        || lower.contains("outdoor")
        || lower.contains("hof")
    {
        Some("Außenbereich")
    } else if lower.contains("keller")
        || lower.contains("sauna")
        || lower.contains("untergeschoss")
        || lower.contains("serverraum")
        || lower.contains("hobbyraum")
        || lower.contains(" ug")
        || lower.starts_with("ug")
        || lower.ends_with("ug")
    {
        Some("Keller")
    } else if lower.contains("erdgeschoss")
        || lower.contains(" eg")
        || lower.starts_with("eg ")
        || lower.ends_with("eg")
        || lower.contains("wohn")
        || lower.contains("küche")
        || lower.contains("kueche")
        || lower.contains("ess")
        || lower.contains("windfang")
        || lower.contains("flur")
        || lower.contains("wc")
    {
        Some("Erdgeschoss")
    } else if lower.contains("obergeschoss")
        || lower.contains(" og")
        || lower.starts_with("og ")
        || lower.ends_with("og")
        || lower.contains("1.og")
        || lower.contains("2.og")
        || lower.contains("schlaf")
        || lower.contains("kinder")
        || lower.contains("tom")
        || lower.contains("sophie")
    {
        Some("Obergeschoss")
    } else if lower.contains("dachgeschoss")
        || lower.contains(" dg")
        || lower.starts_with("dg ")
        || lower.ends_with("dg")
        || lower.contains("dach")
        || lower.contains("speicher")
        || lower.contains("attic")
    {
        Some("Dachgeschoss")
    } else {
        None
    }
}

pub fn matter_area_tag_for_archetype(archetype: &str) -> &'static str {
    match archetype.to_lowercase().as_str() {
        "living_room" => "LivingRoom",
        "kitchen" => "Kitchen",
        "dining_room" => "DiningRoom",
        "bedroom" => "Bedroom",
        "kids_room" => "KidsRoom",
        "bathroom" => "Bathroom",
        "toilet" => "Bathroom",
        "office" => "Office",
        "hallway" | "stairs" => "Hallway",
        "basement" => "Basement",
        "outdoor" => "Outdoor",
        "garage" => "Garage",
        "attic" => "Attic",
        _ => "CommonSpace",
    }
}

#[derive(Clone)]
pub struct RoomsStore {
    path: PathBuf,
    rooms: Arc<RwLock<HashMap<String, RoomRecord>>>,
}

impl RoomsStore {
    pub fn load_or_create(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref().to_path_buf();
        let mut map = HashMap::new();

        if path.exists() {
            match fs::read_to_string(&path) {
                Ok(content) if !content.trim().is_empty() => {
                    match serde_json::from_str::<Vec<RoomRecord>>(&content) {
                        Ok(list) => {
                            info!("Loaded {} room(s) from {}", list.len(), path.display());
                            for r in list {
                                map.insert(r.id.clone(), r);
                            }
                        }
                        Err(err) => {
                            warn!("Failed to parse rooms from {}: {}", path.display(), err);
                        }
                    }
                }
                _ => {}
            }
        }

        Self {
            path,
            rooms: Arc::new(RwLock::new(map)),
        }
    }

    pub fn list(&self) -> Vec<RoomRecord> {
        let guard = match self.rooms.read() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let mut list: Vec<RoomRecord> = guard.values().cloned().collect();

        list.sort_by(|a, b| {
            let floor_order = |floor_opt: &Option<String>| match floor_opt.as_deref() {
                Some("Dachgeschoss") => 1,
                Some("Obergeschoss") => 2,
                Some("Erdgeschoss") => 3,
                Some("Keller") => 4,
                Some("Außenbereich") => 5,
                _ => 6,
            };

            let ord_a = a.sort_order.unwrap_or(0);
            let ord_b = b.sort_order.unwrap_or(0);
            if ord_a != ord_b {
                return ord_a.cmp(&ord_b);
            }

            let fa = floor_order(&a.floor);
            let fb = floor_order(&b.floor);
            if fa != fb {
                return fa.cmp(&fb);
            }

            a.name.cmp(&b.name)
        });

        list
    }

    pub fn get(&self, id: &str) -> Option<RoomRecord> {
        let guard = match self.rooms.read() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        guard.get(id).cloned()
    }

    pub fn find_by_name(&self, name: &str) -> Option<RoomRecord> {
        let guard = match self.rooms.read() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let clean = name.trim().to_lowercase();
        guard.values().find(|room| room.name.trim().to_lowercase() == clean).cloned()
    }

    pub fn upsert(&self, mut room: RoomRecord) -> Result<RoomRecord> {
        if room.id.trim().is_empty() {
            room.id = slugify_room_id(&room.name);
        }
        if room.icon.is_none() {
            room.icon = room.archetype.as_deref().map(default_icon_for_archetype).map(|s| s.to_string());
        }
        if room.floor.is_none() {
            room.floor = deduce_floor_from_name(&room.name).map(|s| s.to_string());
        }
        if room.matter_tag.is_none() {
            room.matter_tag = room.archetype.as_deref().map(matter_area_tag_for_archetype).map(|s| s.to_string());
        }
        room.updated_at = Utc::now().to_rfc3339();
        if room.created_at.is_empty() {
            room.created_at = room.updated_at.clone();
        }

        let saved = room.clone();
        {
            let mut guard = match self.rooms.write() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            guard.insert(room.id.clone(), room);
        }
        self.persist()?;
        Ok(saved)
    }

    pub fn delete(&self, id: &str) -> Result<bool> {
        let removed = {
            let mut guard = match self.rooms.write() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            guard.remove(id).is_some()
        };
        if removed {
            self.persist()?;
        }
        Ok(removed)
    }

    pub fn persist(&self) -> Result<()> {
        let list = self.list();
        if let Some(parent) = self.path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let json_data = serde_json::to_string_pretty(&list)
            .context("failed to serialize rooms to JSON")?;
        fs::write(&self.path, json_data)
            .with_context(|| format!("failed to write rooms file to {}", self.path.display()))?;
        Ok(())
    }

    pub fn distinct_floors(&self) -> Vec<String> {
        let mut floors = vec![
            "Dachgeschoss".to_string(),
            "Obergeschoss".to_string(),
            "Erdgeschoss".to_string(),
            "Keller".to_string(),
            "Außenbereich".to_string(),
        ];
        let rooms = self.list();
        for r in rooms {
            if let Some(f) = r.floor {
                if !f.trim().is_empty() && !floors.iter().any(|x| x.eq_ignore_ascii_case(&f)) {
                    floors.push(f);
                }
            }
        }
        floors
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_rooms_store_crud() {
        let tmp = NamedTempFile::new().unwrap();
        let store = RoomsStore::load_or_create(tmp.path());

        let r1 = RoomRecord::new("living-room", "Wohnzimmer", None, None, None);
        assert_eq!(r1.floor.as_deref(), Some("Erdgeschoss"));
        assert_eq!(r1.icon.as_deref(), Some("🛋️"));
        assert_eq!(r1.matter_tag.as_deref(), Some("LivingRoom"));

        store.upsert(r1).unwrap();

        let list = store.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "Wohnzimmer");

        assert!(store.get("living-room").is_some());
        assert!(store.find_by_name("wohnzimmer").is_some());

        assert!(store.delete("living-room").unwrap());
        assert_eq!(store.list().len(), 0);
    }
}
