use std::collections::HashMap;
use homenode_definitions::{LightGroup, RoomRecord};
use crate::govee::GoveeDeviceState;
use crate::{page_layout, DeviceDocumentation, MatterFabricMeta, UnifiedDevice};

pub fn render_rooms_page(
    title: &str,
    rooms: &[RoomRecord],
    unified_devices: &[UnifiedDevice],
    docs: &HashMap<String, DeviceDocumentation>,
    govee_states: &HashMap<String, GoveeDeviceState>,
    _matter_fabric_metas: &HashMap<String, MatterFabricMeta>,
    light_groups: &[LightGroup],
) -> String {
    let mut floor_rooms: HashMap<String, Vec<&RoomRecord>> = HashMap::new();
    let standard_floors = ["Dachgeschoss", "Obergeschoss", "Erdgeschoss", "Keller", "Außenbereich", "Sonstige"];
    
    for f in &standard_floors {
        floor_rooms.insert(f.to_string(), Vec::new());
    }

    for r in rooms {
        let f = r.floor.as_deref().unwrap_or("Sonstige");
        floor_rooms.entry(f.to_string()).or_default().push(r);
    }

    // Sort rooms inside each floor
    for list in floor_rooms.values_mut() {
        list.sort_by(|a, b| a.name.cmp(&b.name));
    }

    // Map devices to rooms
    // Key is room name (case-insensitive or exact)
    let mut room_devices: HashMap<String, Vec<&UnifiedDevice>> = HashMap::new();
    let mut unassigned_devices: Vec<&UnifiedDevice> = Vec::new();

    for udev in unified_devices {
        let p = &udev.primary;
        let mac = p.metadata.get("mac").cloned().unwrap_or_default();
        let mac_lower = mac.to_lowercase();
        let doc_key = if !mac.is_empty() { mac.clone() } else { p.device_id.clone() };
        
        let mut assigned_room = docs.get(&doc_key)
            .or_else(|| docs.get(&mac))
            .or_else(|| docs.get(&mac_lower))
            .or_else(|| docs.get(&p.device_id))
            .and_then(|d| d.room.as_deref())
            .or_else(|| p.metadata.get("room").map(|s| s.as_str()));

        if assigned_room.is_none() {
            for sec in &udev.secondary_interfaces {
                let s_mac = sec.metadata.get("mac").cloned().unwrap_or_default();
                let s_mac_lower = s_mac.to_lowercase();
                if let Some(r) = docs.get(&s_mac)
                    .or_else(|| docs.get(&s_mac_lower))
                    .or_else(|| docs.get(&sec.device_id))
                    .and_then(|d| d.room.as_deref())
                    .or_else(|| sec.metadata.get("room").map(|s| s.as_str())) {
                    assigned_room = Some(r);
                    break;
                }
            }
        }

        if let Some(r_name) = assigned_room.filter(|s| !s.trim().is_empty()) {
            room_devices.entry(r_name.to_string()).or_default().push(udev);
        } else {
            unassigned_devices.push(udev);
        }
    }

    // Precalculate counts
    let total_rooms = rooms.len();
    let total_assigned_devices: usize = room_devices.values().map(|v| v.len()).sum();
    let total_unassigned_devices = unassigned_devices.len();

    // Prepare JSON for client-side interactivity
    let rooms_json: Vec<serde_json::Value> = rooms.iter().map(|r| {
        let count = room_devices.get(&r.name).map(|v| v.len()).unwrap_or(0);
        serde_json::json!({
            "id": r.id,
            "name": r.name,
            "floor": r.floor.as_deref().unwrap_or("Sonstige"),
            "icon": r.icon.as_deref().unwrap_or("📍"),
            "archetype": r.archetype.as_deref().unwrap_or(""),
            "matter_tag": r.matter_tag.as_deref().unwrap_or(""),
            "hue_group_id": r.hue_group_id.as_deref().unwrap_or(""),
            "hue_class": r.hue_class.as_deref().unwrap_or(""),
            "device_count": count,
        })
    }).collect();

    let devices_json: Vec<serde_json::Value> = unified_devices.iter().map(|udev| {
        let p = &udev.primary;
        let mac = p.metadata.get("mac").cloned().unwrap_or_default();
        let mac_lower = mac.to_lowercase();
        let mut ip = p.metadata.get("ip").cloned().unwrap_or_default();
        if ip.is_empty() || ip == "-" {
            for sec in &udev.secondary_interfaces {
                if let Some(sip) = sec.metadata.get("ip") {
                    if !sip.is_empty() && sip != "-" && !sip.contains('(') {
                        ip = sip.clone();
                        break;
                    }
                }
            }
        }

        let doc_key = if !mac.is_empty() { mac.clone() } else { p.device_id.clone() };
        let mut doc = docs.get(&doc_key)
            .or_else(|| docs.get(&mac))
            .or_else(|| docs.get(&mac_lower))
            .or_else(|| docs.get(&p.device_id));

        if doc.is_none() {
            for sec in &udev.secondary_interfaces {
                let s_mac = sec.metadata.get("mac").cloned().unwrap_or_default();
                let s_mac_lower = s_mac.to_lowercase();
                if let Some(d) = docs.get(&s_mac)
                    .or_else(|| docs.get(&s_mac_lower))
                    .or_else(|| docs.get(&sec.device_id)) {
                    doc = Some(d);
                    break;
                }
            }
        }

        let room = doc.and_then(|d| d.room.as_deref())
            .or_else(|| p.metadata.get("room").map(|s| s.as_str()))
            .unwrap_or("");

        let custom_name = doc.and_then(|d| d.name.as_deref()).unwrap_or("");
        let effective_name = if !custom_name.is_empty() { custom_name } else { &p.display_name };

        let mut is_govee = p.metadata.get("vendor").map(|v| v.to_lowercase().contains("govee")).unwrap_or(false)
            || p.display_name.to_lowercase().contains("govee")
            || p.metadata.get("hostname").map(|h| h.to_lowercase().contains("govee")).unwrap_or(false);

        if !is_govee {
            for sec in &udev.secondary_interfaces {
                if sec.metadata.get("vendor").map(|v| v.to_lowercase().contains("govee")).unwrap_or(false)
                    || sec.display_name.to_lowercase().contains("govee")
                    || sec.metadata.get("hostname").map(|h| h.to_lowercase().contains("govee")).unwrap_or(false) {
                    is_govee = true;
                    break;
                }
            }
        }

        let is_hue = p.module_id == "philips-hue"
            || p.metadata.get("source").map(|s| s.as_str()) == Some("philips-hue")
            || p.metadata.get("protocol").map(|s| s.as_str()) == Some("zigbee");

        let hue_id = p.metadata.get("hue_light_id").cloned().unwrap_or_default();
        let hue_on = p.metadata.get("hue_on").map(|s| s == "true").unwrap_or(true);
        let hue_bri = p.metadata.get("hue_bri").and_then(|s| s.parse::<u64>().ok()).unwrap_or(254);

        let is_shelly = p.display_name.to_lowercase().contains("shelly")
            || p.metadata.get("hostname").map(|h| h.to_lowercase().contains("shelly")).unwrap_or(false);

        let g_state = if !ip.is_empty() {
            govee_states.get(&ip)
        } else {
            udev.secondary_interfaces.iter().find_map(|sec| {
                sec.metadata.get("ip").and_then(|sip| govee_states.get(sip))
            })
        };

        if let Some(gs) = g_state {
            if ip.is_empty() {
                ip = gs.ip.clone();
            }
            is_govee = true;
        }

        serde_json::json!({
            "device_id": p.device_id,
            "display_name": p.display_name,
            "custom_name": custom_name,
            "effective_name": effective_name,
            "kind": p.kind,
            "category": p.metadata.get("category").cloned().unwrap_or_else(|| p.kind.clone()),
            "category_icon": p.metadata.get("category_icon").cloned().unwrap_or_else(|| "📱".to_string()),
            "ip": ip,
            "mac": mac,
            "room": room,
            "is_active": p.metadata.get("status").map(|s| s.as_str()).unwrap_or("active") == "active",
            "is_govee": is_govee,
            "is_hue": is_hue,
            "hue_id": hue_id,
            "hue_on": hue_on,
            "hue_bri": hue_bri,
            "is_shelly": is_shelly,
            "govee_on": g_state.map(|s| s.on_off).unwrap_or(false),
            "govee_brightness": g_state.map(|s| s.brightness).unwrap_or(100),
            "govee_color": g_state.map(|s| s.color).unwrap_or((255, 255, 255)),
            "govee_kelvin": g_state.map(|s| s.color_tem_kelvin).unwrap_or(0),
            "govee_sku": g_state.map(|s| s.sku.clone()).unwrap_or_default(),
            "web_url": p.metadata.get("web_url").cloned(),
            "metadata": p.metadata,
        })
    }).collect();

    let light_groups_json: Vec<serde_json::Value> = light_groups.iter().map(|g| {
        serde_json::json!({
            "id": g.id,
            "name": g.name,
            "room": g.room,
            "device_ids": g.device_ids,
            "icon": g.icon.as_deref().unwrap_or("💡"),
            "created_at": g.created_at,
            "updated_at": g.updated_at,
        })
    }).collect();

    let content = format!(r##"
    <style>
        .rooms-header {{
            display: flex;
            justify-content: space-between;
            align-items: center;
            margin-bottom: 20px;
            flex-wrap: wrap;
            gap: 12px;
        }}
        .floor-tabs {{
            display: flex;
            gap: 8px;
            flex-wrap: wrap;
            margin-bottom: 16px;
        }}
        .floor-tab {{
            padding: 6px 14px;
            border-radius: 8px;
            border: 1px solid var(--border);
            background: var(--surface);
            color: var(--muted);
            font-size: 13px;
            font-weight: 500;
            cursor: pointer;
            transition: all 0.15s;
            display: inline-flex;
            align-items: center;
            gap: 6px;
        }}
        .floor-tab:hover {{ color: var(--text); border-color: var(--muted); }}
        .floor-tab.active {{
            background: var(--primary);
            color: #fff;
            border-color: var(--primary);
            font-weight: 600;
        }}
        .room-pills-container {{
            display: flex;
            gap: 8px;
            flex-wrap: wrap;
            margin-bottom: 20px;
            padding: 12px;
            background: var(--surface);
            border: 1px solid var(--border);
            border-radius: 10px;
        }}
        .room-pill {{
            padding: 6px 12px;
            border-radius: 6px;
            border: 1px solid var(--border);
            background: var(--bg);
            color: var(--text);
            font-size: 12px;
            cursor: pointer;
            transition: all 0.15s;
            display: inline-flex;
            align-items: center;
            gap: 6px;
        }}
        .room-pill:hover {{ border-color: var(--primary); }}
        .room-pill.active {{
            background: var(--primary-bg);
            border-color: var(--primary);
            color: var(--primary);
            font-weight: 600;
        }}
        .room-pill .badge-count {{
            background: var(--badge-bg);
            color: var(--muted);
            font-size: 10px;
            padding: 1px 6px;
            border-radius: 10px;
        }}
        .room-pill.active .badge-count {{
            background: var(--primary);
            color: #fff;
        }}
        
        .room-banner {{
            display: flex;
            flex-direction: column;
            gap: 12px;
            padding: 18px 20px;
            background: var(--surface);
            border: 1px solid var(--border);
            border-left: 4px solid var(--primary);
            border-radius: 10px;
            margin-bottom: 20px;
            box-shadow: 0 1px 3px rgba(0,0,0,0.03);
        }}
        .room-banner-top {{
            display: flex;
            justify-content: space-between;
            align-items: center;
            flex-wrap: wrap;
            gap: 12px;
        }}
        .room-banner-title {{
            display: flex;
            align-items: center;
            gap: 10px;
            font-size: 18px;
            font-weight: 700;
        }}
        
        .room-telemetry-strip {{
            display: flex;
            flex-wrap: wrap;
            gap: 8px;
            align-items: center;
        }}
        .telemetry-pill {{
            display: inline-flex;
            align-items: center;
            gap: 5px;
            font-size: 11px;
            font-weight: 600;
            padding: 3px 9px;
            border-radius: 20px;
            background: var(--bg);
            border: 1px solid var(--border);
            color: var(--text);
        }}
        
        .room-scenes-bar {{
            display: flex;
            gap: 8px;
            flex-wrap: wrap;
            align-items: center;
            padding-top: 10px;
            border-top: 1px dashed var(--border);
        }}
        .scene-chip {{
            padding: 5px 11px;
            font-size: 11px;
            font-weight: 600;
            border-radius: 20px;
            border: 1px solid var(--border);
            background: var(--bg);
            color: var(--text);
            cursor: pointer;
            transition: all 0.15s;
            display: inline-flex;
            align-items: center;
            gap: 5px;
        }}
        .scene-chip:hover {{
            background: var(--primary);
            color: #fff;
            border-color: var(--primary);
            transform: translateY(-1px);
        }}
        
        .section-header {{
            font-size: 14px;
            font-weight: 700;
            margin: 20px 0 12px 0;
            display: flex;
            align-items: center;
            justify-content: space-between;
            color: var(--text);
        }}

        .device-cards-grid {{
            display: grid;
            grid-template-columns: repeat(auto-fill, minmax(320px, 1fr));
            gap: 16px;
        }}
        
        .room-dev-card {{
            background: var(--surface);
            border: 1px solid var(--border);
            border-radius: 10px;
            padding: 14px 16px;
            display: flex;
            flex-direction: column;
            justify-content: space-between;
            box-shadow: 0 1px 3px rgba(0,0,0,0.03);
            transition: transform 0.1s, box-shadow 0.15s;
        }}
        .room-dev-card:hover {{
            box-shadow: 0 3px 8px rgba(0,0,0,0.07);
        }}

        .light-group-card {{
            background: linear-gradient(135deg, rgba(245, 158, 11, 0.05) 0%, rgba(217, 119, 6, 0.03) 100%);
            border: 1.5px solid rgba(245, 158, 11, 0.35);
            box-shadow: 0 2px 6px rgba(245, 158, 11, 0.08);
        }}
        .light-group-card:hover {{
            border-color: #f59e0b;
        }}

        .dev-card-header {{
            display: flex;
            justify-content: space-between;
            align-items: flex-start;
            margin-bottom: 10px;
        }}
        .dev-card-info {{
            display: flex;
            align-items: flex-start;
            gap: 10px;
        }}
        .dev-card-icon {{
            font-size: 24px;
            line-height: 1;
            padding: 6px;
            background: var(--bg);
            border-radius: 8px;
            border: 1px solid var(--border);
        }}
        .dev-card-name {{
            font-weight: 600;
            font-size: 14px;
            line-height: 1.3;
        }}
        .dev-card-meta {{
            font-size: 11px;
            color: var(--muted);
            margin-top: 2px;
        }}
        .dev-control-widget {{
            margin-top: 12px;
            padding-top: 12px;
            border-top: 1px solid var(--border);
        }}
        .govee-widget {{
            background: linear-gradient(135deg, rgba(59, 130, 246, 0.05) 0%, rgba(99, 102, 241, 0.05) 100%);
            border: 1px solid rgba(59, 130, 246, 0.2);
            border-radius: 8px;
            padding: 10px;
        }}
        .slider-row {{
            display: flex;
            align-items: center;
            gap: 8px;
            margin-top: 8px;
            font-size: 11px;
            color: var(--muted);
        }}
        .color-chips {{
            display: flex;
            gap: 6px;
            margin-top: 8px;
            align-items: center;
            flex-wrap: wrap;
        }}
        .color-chip {{
            width: 20px;
            height: 20px;
            border-radius: 50%;
            border: 2px solid #fff;
            box-shadow: 0 1px 3px rgba(0,0,0,0.2);
            cursor: pointer;
            transition: transform 0.1s;
        }}
        .color-chip:hover {{
            transform: scale(1.18);
        }}

        /* Subordinate Sensors and Gateways */
        .secondary-section {{
            margin-top: 32px;
            border: 1px solid var(--border);
            border-radius: 10px;
            background: var(--surface);
            overflow: hidden;
        }}
        .secondary-summary {{
            padding: 14px 18px;
            font-size: 13px;
            font-weight: 600;
            cursor: pointer;
            user-select: none;
            display: flex;
            justify-content: space-between;
            align-items: center;
            background: rgba(0,0,0,0.015);
        }}
        .secondary-summary:hover {{
            background: rgba(0,0,0,0.03);
        }}
        .secondary-grid {{
            display: grid;
            grid-template-columns: repeat(auto-fill, minmax(280px, 1fr));
            gap: 12px;
            padding: 16px;
            background: var(--bg);
            border-top: 1px solid var(--border);
        }}
        .secondary-card {{
            background: var(--surface);
            border: 1px solid var(--border);
            border-radius: 8px;
            padding: 10px 12px;
            font-size: 12px;
        }}

        /* Modal styling */
        .modal-backdrop {{
            position: fixed;
            top: 0; left: 0; right: 0; bottom: 0;
            background: rgba(0, 0, 0, 0.5);
            z-index: 1000;
            display: none;
            align-items: center;
            justify-content: center;
            padding: 20px;
        }}
        .modal-dialog {{
            background: var(--surface);
            border: 1px solid var(--border);
            border-radius: 12px;
            width: 100%;
            max-width: 520px;
            max-height: 90vh;
            overflow-y: auto;
            padding: 24px;
            box-shadow: 0 10px 25px rgba(0,0,0,0.2);
        }}
    </style>

    <div class="rooms-header">
        <div>
            <h2>🚪 Room & Area View</h2>
            <p style="font-size:12px; color:var(--muted); margin-top:2px;">
                Apple Home kompatible Raumsteuerung: Lichtgruppen, Lichtszenen & native lokale Govee / Philips Hue LAN-Steuerung.
            </p>
        </div>
        <div style="display:flex; gap:8px; flex-wrap:wrap;">
            <input type="text" id="room-search-input" class="search-input" placeholder="🔍 Gerät oder Raum suchen..." oninput="filterRoomsAndDevices()" />
            <button type="button" class="btn btn-sm" onclick="openLightGroupModal()">🔗 Lichter gruppieren</button>
            <button type="button" class="btn btn-sm" onclick="openRoomModal()">⚙️ Räume verwalten</button>
        </div>
    </div>

    <!-- Floor Filter Tabs -->
    <div class="floor-tabs" id="floor-tabs-bar">
        <button class="floor-tab active" onclick="selectFloor('ALL', this)">🏢 Alle Etagen</button>
        <button class="floor-tab" onclick="selectFloor('Dachgeschoss', this)">🏠 Dachgeschoss</button>
        <button class="floor-tab" onclick="selectFloor('Obergeschoss', this)">🛏️ Obergeschoss</button>
        <button class="floor-tab" onclick="selectFloor('Erdgeschoss', this)">🛋️ Erdgeschoss</button>
        <button class="floor-tab" onclick="selectFloor('Keller', this)">📦 Keller</button>
        <button class="floor-tab" onclick="selectFloor('Außenbereich', this)">🌳 Außenbereich</button>
        <button class="floor-tab" onclick="selectFloor('UNASSIGNED', this)">❓ Ohne Raum ({total_unassigned_devices})</button>
    </div>

    <!-- Room Pills Container -->
    <div class="room-pills-container" id="room-pills-list">
        <!-- Injected via JavaScript -->
    </div>

    <!-- Active Room Banner with Telemetry and Light Scenes -->
    <div class="room-banner" id="active-room-banner">
        <div class="room-banner-top">
            <div>
                <div class="room-banner-title" id="room-banner-title">
                    <span>📍 Alle Räume</span>
                </div>
                <div style="font-size:11px; color:var(--muted); margin-top:3px;" id="room-banner-meta">
                    Gesamt: {total_assigned_devices} zugeordnete Geräte in {total_rooms} Räumen
                </div>
            </div>
            <!-- Live room sensor telemetry strip -->
            <div class="room-telemetry-strip" id="room-telemetry-strip">
                <!-- Dynamically populated if room has sensor telemetry -->
            </div>
        </div>

        <!-- Room Light Scenes Bar -->
        <div class="room-scenes-bar" id="room-scenes-bar">
            <span style="font-size:11px; font-weight:700; color:var(--muted); margin-right:4px;">✨ Lichtszenen:</span>
            <button type="button" class="scene-chip" onclick="activateRoomScene('all_on', this)">💡 Alles An</button>
            <button type="button" class="scene-chip" onclick="activateRoomScene('all_off', this)">🔌 Alles Aus</button>
            <button type="button" class="scene-chip" onclick="activateRoomScene('relax', this)">🛋️ Gemütlich</button>
            <button type="button" class="scene-chip" onclick="activateRoomScene('focus', this)">💼 Hell / Fokus</button>
            <button type="button" class="scene-chip" onclick="activateRoomScene('night', this)">🌙 Nachtlicht</button>
            <button type="button" class="scene-chip" onclick="activateRoomScene('movie', this)">🎬 Heimkino</button>
        </div>
    </div>

    <!-- Controllable Actuators & Lights (Hero Section) -->
    <div id="controllable-section-title" class="section-header" style="display:none;">
        <span>💡 Beleuchtung & Aktoren</span>
        <span style="font-size:11px; color:var(--muted);" id="controllable-count-badge"></span>
    </div>
    <div class="device-cards-grid" id="room-device-cards">
        <!-- Rendered by JavaScript based on selected room/floor -->
    </div>

    <!-- Subordinate Sensors & Gateways Section -->
    <div id="secondary-devices-wrapper" style="display:none;">
        <details class="secondary-section" id="secondary-devices-details">
            <summary class="secondary-summary">
                <span id="secondary-summary-title">📡 Sensoren, Taster & Gateways (0)</span>
                <span style="font-size:11px; color:var(--muted);">Details ein-/ausklappen ▾</span>
            </summary>
            <div class="secondary-grid" id="secondary-devices-grid">
                <!-- Populated by JavaScript -->
            </div>
        </details>
    </div>

    <!-- Modal for Merging / Grouping Lights (Apple Home Style) -->
    <div class="modal-backdrop" id="light-group-modal" onclick="if(event.target===this) closeLightGroupModal()">
        <div class="modal-dialog">
            <div style="display:flex; justify-content:space-between; align-items:center; margin-bottom:16px;">
                <h3 style="margin:0; font-size:16px;">🔗 Lichter zusammenführen (Apple Home Style)</h3>
                <button type="button" class="btn-sm" onclick="closeLightGroupModal()" style="border:none; background:none; font-size:16px; cursor:pointer;">✖</button>
            </div>
            <p style="font-size:12px; color:var(--muted); margin-bottom:16px;">
                Fasse mehrere Lampen desselben Raumes zusammen. Sie werden im Dashboard als eine einzelne Hauptkachel mit gemeinsamer Ein/Aus-, Dimm- und Farbsteuerung dargestellt.
            </p>
            <form onsubmit="submitCreateLightGroup(event)">
                <div style="margin-bottom:12px;">
                    <label style="font-size:12px; font-weight:600; display:block; margin-bottom:4px;">Gruppenname:</label>
                    <input type="text" id="group-name-input" class="search-input" style="width:100%;" placeholder="z. B. Wohnzimmer Vorhänge, Couchlicht" required />
                </div>
                <div style="margin-bottom:12px;">
                    <label style="font-size:12px; font-weight:600; display:block; margin-bottom:4px;">Raum:</label>
                    <select id="group-room-select" class="search-input" style="width:100%;" onchange="updateGroupModalDeviceList()" required>
                        <!-- Injected via JS -->
                    </select>
                </div>
                <div style="margin-bottom:16px;">
                    <label style="font-size:12px; font-weight:600; display:block; margin-bottom:4px;">Verknüpfte Lampen auswählen (mind. 2):</label>
                    <div id="group-lights-checklist" style="max-height:160px; overflow-y:auto; border:1px solid var(--border); border-radius:6px; padding:8px; background:var(--bg);">
                        <!-- Injected via JS -->
                    </div>
                </div>
                <div style="display:flex; justify-content:flex-end; gap:8px;">
                    <button type="button" class="btn btn-sm" onclick="closeLightGroupModal()">Abbrechen</button>
                    <button type="submit" class="btn btn-sm btn-primary">🔗 Gruppe erstellen</button>
                </div>
            </form>
        </div>
    </div>

    <!-- Modal for Room Management & Editor -->
    <div class="modal-backdrop" id="room-manager-modal" onclick="if(event.target===this) closeRoomModal()">
        <div class="modal-dialog" style="max-width: 680px;">
            <div style="display:flex; justify-content:space-between; align-items:center; margin-bottom:16px; border-bottom:1px solid var(--border); padding-bottom:12px;">
                <div>
                    <h3 style="margin:0; font-size:16px; display:flex; align-items:center; gap:8px;">
                        <span>🚪</span> <span>Raum- & Standortverwaltung</span>
                    </h3>
                    <div style="font-size:11px; color:var(--muted); margin-top:3px;">
                        Name, Struktur (Etage / Floor) & Icons anpassen &bull; Optional mit Philips Hue Bridge synchronisieren
                    </div>
                </div>
                <button type="button" class="btn-sm" onclick="closeRoomModal()" style="border:none; background:none; font-size:16px; cursor:pointer;">✖</button>
            </div>

            <!-- Hue Bridge Quick Sync Bar -->
            <div style="display:flex; justify-content:space-between; align-items:center; margin-bottom:16px; background:var(--surface); padding:10px 14px; border-radius:8px; border:1px solid var(--border);">
                <div>
                    <div style="font-weight:600; font-size:12px;">💡 Philips Hue Bridge</div>
                    <div style="font-size:11px; color:var(--muted);">Räume und Lampenzuordnungen aus der Hue Bridge abrufen.</div>
                </div>
                <button type="button" id="btn-import-hue" class="btn btn-sm" onclick="importHueRooms()">
                    <span>🔄</span> <span>Aus Hue importieren</span>
                </button>
            </div>

            <!-- Room Editor Form (Create or Edit) -->
            <div style="margin-bottom:16px; background:var(--bg); border:1px solid var(--border); border-radius:8px; padding:14px;">
                <div style="display:flex; justify-content:space-between; align-items:center; margin-bottom:10px;">
                    <span id="room-editor-title" style="font-size:13px; font-weight:700; color:var(--text);">➕ Neuen Raum anlegen</span>
                    <button type="button" id="btn-cancel-room-edit" class="btn-sm" style="display:none; font-size:11px; cursor:pointer;" onclick="cancelEditRoom()">Abbrechen</button>
                </div>
                <form id="room-editor-form" onsubmit="submitRoomForm(event)">
                    <input type="hidden" id="rm-id" value="" />
                    <input type="hidden" id="rm-hue-group-id" value="" />
                    <input type="hidden" id="rm-hue-class" value="" />

                    <div style="display:grid; grid-template-columns:1fr 1fr; gap:10px; margin-bottom:10px;">
                        <div>
                            <label style="font-size:11px; font-weight:600; color:var(--muted); display:block; margin-bottom:4px;">Raumname *</label>
                            <input type="text" id="rm-name" class="search-input" style="width:100%;" required placeholder="z. B. Wohnzimmer, Büro..." oninput="onRoomNameInput(this.value)" />
                        </div>
                        <div>
                            <label style="font-size:11px; font-weight:600; color:var(--muted); display:block; margin-bottom:4px;">Etage (Floor)</label>
                            <select id="rm-floor" class="search-input" style="width:100%;">
                                <option value="Dachgeschoss">Dachgeschoss</option>
                                <option value="Obergeschoss">Obergeschoss</option>
                                <option value="Erdgeschoss" selected>Erdgeschoss</option>
                                <option value="Keller">Keller</option>
                                <option value="Außenbereich">Außenbereich</option>
                                <option value="Sonstige">Sonstige</option>
                            </select>
                        </div>
                    </div>

                    <div style="display:grid; grid-template-columns:1fr 1fr 1fr; gap:10px; margin-bottom:10px;">
                        <div>
                            <label style="font-size:11px; font-weight:600; color:var(--muted); display:block; margin-bottom:4px;">Raumtyp / Archetyp</label>
                            <select id="rm-archetype" class="search-input" style="width:100%;" onchange="onArchetypeSelect(this.value)">
                                <option value="living_room">Wohnzimmer</option>
                                <option value="kitchen">Küche</option>
                                <option value="dining_room">Esszimmer</option>
                                <option value="bedroom">Schlafzimmer</option>
                                <option value="kids_room">Kinderzimmer</option>
                                <option value="bathroom">Badezimmer</option>
                                <option value="toilet">Gäste-WC</option>
                                <option value="office">Büro / Arbeit</option>
                                <option value="hallway">Flur / Diele</option>
                                <option value="stairs">Treppenhaus</option>
                                <option value="basement">Keller / Lager</option>
                                <option value="outdoor">Außen / Garten</option>
                                <option value="garage">Garage / Carport</option>
                                <option value="spa">Sauna / Spa</option>
                                <option value="other">Sonstiger Raum</option>
                            </select>
                        </div>
                        <div>
                            <label style="font-size:11px; font-weight:600; color:var(--muted); display:block; margin-bottom:4px;">Icon (Emoji)</label>
                            <div style="display:flex; gap:6px; align-items:center;">
                                <input type="text" id="rm-icon" class="search-input" style="width:50px; text-align:center; font-size:16px;" value="🛋️" />
                                <div style="display:flex; gap:4px; flex-wrap:wrap;">
                                    <span style="cursor:pointer; font-size:14px;" onclick="setRoomIcon('🛋️')" title="Wohnzimmer">🛋️</span>
                                    <span style="cursor:pointer; font-size:14px;" onclick="setRoomIcon('🍳')" title="Küche">🍳</span>
                                    <span style="cursor:pointer; font-size:14px;" onclick="setRoomIcon('🛏️')" title="Schlafzimmer">🛏️</span>
                                    <span style="cursor:pointer; font-size:14px;" onclick="setRoomIcon('🛁')" title="Bad">🛁</span>
                                    <span style="cursor:pointer; font-size:14px;" onclick="setRoomIcon('💼')" title="Büro">💼</span>
                                    <span style="cursor:pointer; font-size:14px;" onclick="setRoomIcon('🚪')" title="Flur">🚪</span>
                                    <span style="cursor:pointer; font-size:14px;" onclick="setRoomIcon('📦')" title="Keller">📦</span>
                                    <span style="cursor:pointer; font-size:14px;" onclick="setRoomIcon('🌳')" title="Garten">🌳</span>
                                    <span style="cursor:pointer; font-size:14px;" onclick="setRoomIcon('🚗')" title="Garage">🚗</span>
                                </div>
                            </div>
                        </div>
                        <div>
                            <label style="font-size:11px; font-weight:600; color:var(--muted); display:block; margin-bottom:4px;">Matter Area Tag</label>
                            <input type="text" id="rm-matter" class="search-input" style="width:100%;" value="LivingRoom" />
                        </div>
                    </div>

                    <div id="rm-hue-sync-wrapper" style="display:none; margin-bottom:10px; background:rgba(245,158,11,0.08); border:1px solid rgba(245,158,11,0.25); border-radius:6px; padding:8px 10px; font-size:11px;">
                        <label style="display:flex; align-items:center; gap:8px; cursor:pointer;">
                            <input type="checkbox" id="rm-sync-hue" checked />
                            <span>💡 Änderung (Name & Typ) auch direkt an die Philips Hue Bridge übertragen</span>
                        </label>
                    </div>

                    <div style="display:flex; justify-content:flex-end; gap:8px;">
                        <button type="submit" id="btn-save-room" class="btn btn-sm btn-primary">💾 Raum speichern</button>
                    </div>
                </form>
            </div>

            <!-- Existing Rooms Table -->
            <div style="display:flex; justify-content:space-between; align-items:center; margin-bottom:8px;">
                <span style="font-weight:600; font-size:12px;">Vorhandene Räume (<span id="modal-room-count">0</span>)</span>
                <span id="room-op-status" style="font-size:11px;"></span>
            </div>
            <div style="max-height:260px; overflow-y:auto; border:1px solid var(--border); border-radius:6px;">
                <table style="width:100%; border-collapse:collapse; font-size:12px;">
                    <thead>
                        <tr style="background:var(--surface); border-bottom:1px solid var(--border); text-align:left;">
                            <th style="padding:8px 10px;">Raum</th>
                            <th style="padding:8px 10px;">Etage</th>
                            <th style="padding:8px 10px;">Matter Tag</th>
                            <th style="padding:8px 10px;">Hue Bridge</th>
                            <th style="padding:8px 10px; text-align:center;">Geräte</th>
                            <th style="padding:8px 10px; text-align:right;">Aktionen</th>
                        </tr>
                    </thead>
                    <tbody id="modal-rooms-tbody">
                        <!-- Injected via JavaScript -->
                    </tbody>
                </table>
            </div>
        </div>
    </div>

    <script>
        const allRoomsData = {rooms_json_str};
        const allDevicesData = {devices_json_str};
        let allLightGroupsData = {light_groups_json_str};
        
        let currentFloor = 'ALL';
        let currentRoomId = 'ALL';

        // 1. URL & LocalStorage State Persistence
        function saveActiveRoomState() {{
            try {{
                localStorage.setItem('homenode_active_room', currentRoomId);
                localStorage.setItem('homenode_active_floor', currentFloor);
                const hash = 'room=' + encodeURIComponent(currentRoomId) + '&floor=' + encodeURIComponent(currentFloor);
                if (window.location.hash.slice(1) !== hash) {{
                    history.replaceState(null, '', '#' + hash);
                }}
            }} catch(e) {{
                console.error('Failed to save room state:', e);
            }}
        }}

        function restoreActiveRoomState() {{
            let targetRoom = 'ALL';
            let targetFloor = 'ALL';

            if (window.location.hash && window.location.hash.length > 1) {{
                const params = new URLSearchParams(window.location.hash.slice(1));
                if (params.has('room')) targetRoom = params.get('room');
                if (params.has('floor')) targetFloor = params.get('floor');
            }} else {{
                targetRoom = localStorage.getItem('homenode_active_room') || 'ALL';
                targetFloor = localStorage.getItem('homenode_active_floor') || 'ALL';
            }}

            if (targetFloor && targetFloor !== 'ALL') {{
                currentFloor = targetFloor;
            }}
            if (targetRoom && targetRoom !== 'ALL') {{
                currentRoomId = targetRoom;
            }}

            // Sync floor tabs active class
            document.querySelectorAll('.floor-tab').forEach(b => {{
                b.classList.toggle('active', b.getAttribute('data-floor') === currentFloor || (currentFloor === 'ALL' && b.innerText.includes('Alle Etagen')));
            }});
        }}

        function initRoomView() {{
            restoreActiveRoomState();
            renderRoomPills();
            renderDeviceCards();

            window.addEventListener('hashchange', () => {{
                restoreActiveRoomState();
                renderRoomPills();
                renderDeviceCards();
            }});
        }}

        function selectFloor(floor, btn) {{
            currentFloor = floor;
            currentRoomId = 'ALL';
            document.querySelectorAll('.floor-tab').forEach(b => b.classList.remove('active'));
            if (btn) btn.classList.add('active');
            saveActiveRoomState();
            renderRoomPills();
            renderDeviceCards();
        }}

        function selectRoom(roomId) {{
            currentRoomId = roomId;
            saveActiveRoomState();
            renderRoomPills();
            renderDeviceCards();
        }}

        function renderRoomPills() {{
            const container = document.getElementById('room-pills-list');
            if (!container) return;

            let filteredRooms = allRoomsData;
            if (currentFloor === 'UNASSIGNED') {{
                container.innerHTML = `
                    <button class="room-pill active" onclick="selectRoom('UNASSIGNED')">
                        ❓ Ohne Raumzuweisung <span class="badge-count">${{allDevicesData.filter(d => !d.room).length}}</span>
                    </button>
                `;
                return;
            }}

            if (currentFloor !== 'ALL') {{
                filteredRooms = allRoomsData.filter(r => r.floor === currentFloor);
            }}

            let html = `
                <button class="room-pill ${{currentRoomId === 'ALL' ? 'active' : ''}}" onclick="selectRoom('ALL')">
                    🌟 Alle Räume (${{filteredRooms.length}})
                </button>
            `;

            filteredRooms.forEach(r => {{
                let isAct = (currentRoomId === r.id);
                html += `
                    <button class="room-pill ${{isAct ? 'active' : ''}}" onclick="selectRoom('${{r.id}}')">
                        <span>${{r.icon || '📍'}}</span>
                        <span>${{r.name}}</span>
                        <span class="badge-count">${{r.device_count}}</span>
                    </button>
                `;
            }});

            container.innerHTML = html;
        }}

        // Check if a device is a light/actuator vs subordinate sensor/gateway
        function isControllableActuator(dev) {{
            const kind = (dev.kind || '').toLowerCase();
            const cat = (dev.category || '').toLowerCase();
            const name = (dev.effective_name || dev.display_name || '').toLowerCase();
            const devId = (dev.device_id || '').toLowerCase();

            // 1. Explicitly sensors, buttons, remotes, and hubs are NEVER controllable actuators:
            if (kind === 'button' || cat === 'button' || kind.includes('remote') || cat.includes('remote')) return false;
            if (kind.includes('sensor') || cat.includes('sensor')) return false;
            if (devId.startsWith('hue-sensor-')) return false;
            if (kind.includes('gateway') || cat.includes('gateway') || kind.includes('bridge') || cat.includes('bridge') || kind.includes('hub') || cat.includes('hub')) return false;

            // Network switches (L2/L3 infrastructure) are not controllable home actuators
            if (kind === 'switch' && (name.startsWith('switch-') || name === 'switch' || (dev.mac && dev.mac.toLowerCase().startsWith('fa:ce:')))) return false;

            // 2. Philips Hue: Only actual lights (having a valid hue_id) are controllable actuators
            if (dev.is_hue && dev.hue_id) return true;
            if (dev.is_hue && !dev.hue_id) return false;

            // 3. Govee LAN lights:
            if (dev.is_govee && dev.ip) return true;

            // 4. Lighting & Power Actuators:
            if (kind.includes('light') || cat.includes('light') || cat.includes('beleuchtung') || cat.includes('lamp')) return true;
            if (kind.includes('plug') || cat.includes('plug') || kind.includes('socket') || cat.includes('steckdose') || kind.includes('relay') || cat.includes('relais')) return true;
            if (kind.includes('actuator') || cat.includes('actuator') || kind.includes('dimmer')) return true;

            // Shelly smart switches/plugs with web_url or relay
            if (dev.is_shelly && (kind.includes('relay') || kind.includes('plug') || cat.includes('relay') || cat.includes('plug') || dev.web_url)) {{
                return true;
            }}

            return false;
        }}

        function renderDeviceCards() {{
            const container = document.getElementById('room-device-cards');
            const bannerTitle = document.getElementById('room-banner-title');
            const bannerMeta = document.getElementById('room-banner-meta');
            const scenesBar = document.getElementById('room-scenes-bar');
            const telemetryStrip = document.getElementById('room-telemetry-strip');
            const controllableTitle = document.getElementById('controllable-section-title');
            const controllableBadge = document.getElementById('controllable-count-badge');
            const secondaryWrapper = document.getElementById('secondary-devices-wrapper');
            const secondaryGrid = document.getElementById('secondary-devices-grid');
            const secondarySummaryTitle = document.getElementById('secondary-summary-title');
            const searchVal = (document.getElementById('room-search-input')?.value || '').toLowerCase().trim();

            let targetDevices = [];
            let bannerIcon = '🌟';
            let bannerText = 'Alle Räume';
            let bannerSub = '';
            let currentRoomName = '';

            if (currentFloor === 'UNASSIGNED' || currentRoomId === 'UNASSIGNED') {{
                targetDevices = allDevicesData.filter(d => !d.room);
                bannerIcon = '❓';
                bannerText = 'Geräte ohne Raum';
                bannerSub = `${{targetDevices.length}} Geräte bisher keinem Raum zugewiesen`;
                if (scenesBar) scenesBar.style.display = 'none';
            }} else if (currentRoomId !== 'ALL') {{
                const r = allRoomsData.find(rm => rm.id === currentRoomId);
                if (r) {{
                    currentRoomName = r.name;
                    targetDevices = allDevicesData.filter(d => d.room && d.room.toLowerCase() === r.name.toLowerCase());
                    bannerIcon = r.icon || '📍';
                    bannerText = `${{r.name}} (${{r.floor}})`;
                    bannerSub = `${{targetDevices.length}} Geräte im Raum &bull; Matter Area Tag: ${{r.matter_tag || 'Standard'}}`;
                    if (scenesBar) scenesBar.style.display = 'flex';
                }}
            }} else if (currentFloor !== 'ALL') {{
                const floorRooms = allRoomsData.filter(rm => rm.floor === currentFloor).map(rm => rm.name.toLowerCase());
                targetDevices = allDevicesData.filter(d => d.room && floorRooms.includes(d.room.toLowerCase()));
                bannerIcon = '🏢';
                bannerText = `Etage: ${{currentFloor}}`;
                bannerSub = `${{targetDevices.length}} Geräte auf dieser Etage`;
                if (scenesBar) scenesBar.style.display = 'none';
            }} else {{
                targetDevices = allDevicesData;
                bannerIcon = '🌟';
                bannerText = 'Alle Geräte & Räume';
                bannerSub = `${{targetDevices.length}} Geräte im Netzwerk gefunden`;
                if (scenesBar) scenesBar.style.display = 'none';
            }}

            if (searchVal) {{
                targetDevices = targetDevices.filter(d => 
                    d.effective_name.toLowerCase().includes(searchVal) ||
                    d.display_name.toLowerCase().includes(searchVal) ||
                    d.ip.toLowerCase().includes(searchVal) ||
                    (d.room && d.room.toLowerCase().includes(searchVal))
                );
            }}

            bannerTitle.innerHTML = `<span>${{bannerIcon}}</span> <span>${{bannerText}}</span>`;
            bannerMeta.innerHTML = bannerSub;

            // Extract sensor telemetry for compact chip pills in the active banner
            let telemetryHtml = '';
            let temps = [], hums = [], bats = [];
            targetDevices.forEach(d => {{
                const m = d.metadata || {{}};
                if (m.temperature) temps.push(parseFloat(m.temperature));
                if (m.temp) temps.push(parseFloat(m.temp));
                if (m.humidity) hums.push(parseFloat(m.humidity));
                if (m.battery) bats.push(parseInt(m.battery, 10));
            }});

            if (temps.length > 0) {{
                const avgTemp = (temps.reduce((a, b) => a + b, 0) / temps.length).toFixed(1);
                telemetryHtml += `<span class="telemetry-pill">🌡️ ${{avgTemp}} °C</span>`;
            }}
            if (hums.length > 0) {{
                const avgHum = Math.round(hums.reduce((a, b) => a + b, 0) / hums.length);
                telemetryHtml += `<span class="telemetry-pill">💧 ${{avgHum}}%</span>`;
            }}
            if (bats.length > 0) {{
                const minBat = Math.min(...bats);
                telemetryHtml += `<span class="telemetry-pill">🔋 ${{minBat}}%</span>`;
            }}
            if (currentRoomName) {{
                telemetryHtml += `<span class="telemetry-pill" style="background:#f0fdf4; color:#166534; border-color:#bbf7d0;">🔒 OK</span>`;
            }}
            telemetryStrip.innerHTML = telemetryHtml;

            // Partition devices: Controllable vs Secondary Sensors/Gateways
            const activeGroups = allLightGroupsData.filter(g => {{
                if (currentRoomName) return g.room.toLowerCase() === currentRoomName.toLowerCase();
                return true;
            }});

            // Collect IDs of devices belonging to active groups
            const groupedDevIds = new Set();
            activeGroups.forEach(g => g.device_ids.forEach(id => groupedDevIds.add(id)));

            const controllableDevs = [];
            const secondaryDevs = [];

            targetDevices.forEach(dev => {{
                // If it is in a group and we are viewing this room/all rooms, group card renders it!
                if (groupedDevIds.has(dev.device_id) || (dev.ip && groupedDevIds.has(dev.ip))) {{
                    return; // Rendered inside light group card
                }}
                if (isControllableActuator(dev)) {{
                    controllableDevs.push(dev);
                }} else {{
                    secondaryDevs.push(dev);
                }}
            }});

            // Render Hero Controllable Cards
            let cardsHtml = '';

            // 1. Render Group Cards first (Apple Home style)
            activeGroups.forEach(grp => {{
                cardsHtml += renderLightGroupCard(grp);
            }});

            // 2. Render remaining standalone controllable devices
            controllableDevs.forEach(dev => {{
                cardsHtml += renderSingleDeviceCard(dev);
            }});

            if (cardsHtml.trim() === '') {{
                container.innerHTML = `
                    <div style="grid-column: 1 / -1; padding: 40px; text-align:center; background:var(--surface); border:1px dashed var(--border); border-radius:10px; color:var(--muted);">
                        <h3>Keine steuerbaren Lampen oder Aktoren</h3>
                        <p style="font-size:12px; margin-top:4px;">In dieser Auswahl befinden sich momentan keine aktiven Schalter oder Leuchten.</p>
                    </div>
                `;
            }} else {{
                container.innerHTML = cardsHtml;
            }}

            if (controllableTitle && controllableBadge) {{
                const totalControllable = activeGroups.length + controllableDevs.length;
                if (totalControllable > 0 && currentRoomId !== 'ALL') {{
                    controllableTitle.style.display = 'flex';
                    controllableBadge.innerText = `${{totalControllable}} aktiv`;
                }} else {{
                    controllableTitle.style.display = 'none';
                }}
            }}

            // 3. Render Subordinate Sensors & Gateways Section
            if (secondaryDevs.length > 0) {{
                secondaryWrapper.style.display = 'block';
                secondarySummaryTitle.innerText = `📡 Sensoren, Taster & Gateways (${{secondaryDevs.length}})`;
                secondaryGrid.innerHTML = secondaryDevs.map(dev => renderSecondaryDeviceCard(dev)).join('');
            }} else {{
                secondaryWrapper.style.display = 'none';
            }}
        }}

        // Render Apple Home style Merged Light Card
        function renderLightGroupCard(grp) {{
            const memberDevs = grp.device_ids.map(id => {{
                return allDevicesData.find(d => d.device_id === id || d.ip === id);
            }}).filter(Boolean);

            const anyOn = memberDevs.some(d => d.govee_on || d.hue_on);
            let avgBri = 100;
            if (memberDevs.length > 0) {{
                const sumBri = memberDevs.reduce((acc, d) => acc + (d.govee_brightness || 100), 0);
                avgBri = Math.round(sumBri / memberDevs.length);
            }}

            const memberNames = memberDevs.map(d => d.effective_name).join(', ');

            return `
                <div class="room-dev-card light-group-card" id="group-card-${{grp.id}}">
                    <div>
                        <div class="dev-card-header">
                            <div class="dev-card-info">
                                <div class="dev-card-icon">${{grp.icon || '💡'}}</div>
                                <div>
                                    <div class="dev-card-name" style="color:#d97706;">${{grp.name}}</div>
                                    <div class="dev-card-meta">${{memberDevs.length}} gekoppelte Leuchten &bull; ${{grp.room}}</div>
                                </div>
                            </div>
                            <span class="badge" style="background:#fef3c7; color:#92400e; font-size:10px; font-weight:700;">Apple Home Gruppe</span>
                        </div>
                        <div style="font-size:11px; color:var(--muted); margin-bottom:6px;">
                            ${{memberNames || 'Lampen werden geladen...'}}
                        </div>
                    </div>

                    <!-- Master Group Control Widget -->
                    <div class="dev-control-widget" style="background:rgba(245, 158, 11, 0.08); border:1px solid rgba(245, 158, 11, 0.25); border-radius:8px; padding:10px;">
                        <div style="display:flex; justify-content:space-between; align-items:center;">
                            <div style="font-weight:700; font-size:12px; color:#92400e;">
                                <span>Master-Schalter (Alle)</span>
                            </div>
                            <button type="button" class="btn btn-sm ${{anyOn ? 'btn-primary' : ''}}" style="font-size:11px; padding:4px 12px;" onclick="toggleLightGroupPower('${{grp.id}}', ${{!anyOn}}, this)">
                                ${{anyOn ? '💡 Alle An' : '⚪ Alle Aus'}}
                            </button>
                        </div>
                        <div class="slider-row">
                            <span style="font-weight:600;">Helligkeit:</span>
                            <input type="range" min="1" max="100" value="${{avgBri}}" style="flex:1; cursor:pointer;" onchange="setLightGroupBrightness('${{grp.id}}', this.value)" oninput="this.nextElementSibling.innerText = this.value + '%'" />
                            <span style="min-width:30px; text-align:right; font-weight:600;">${{avgBri}}%</span>
                        </div>
                        <div class="color-chips">
                            <span style="font-size:10px; color:var(--muted); margin-right:2px;">Farbe:</span>
                            <span class="color-chip" style="background:#ffddaa;" title="Warmweiß (2700K)" onclick="setLightGroupTemp('${{grp.id}}', 2700)"></span>
                            <span class="color-chip" style="background:#fff4e6;" title="Neutralweiß (4000K)" onclick="setLightGroupTemp('${{grp.id}}', 4000)"></span>
                            <span class="color-chip" style="background:#ef4444;" title="Rot" onclick="setLightGroupColor('${{grp.id}}', 255, 0, 0)"></span>
                            <span class="color-chip" style="background:#10b981;" title="Grün" onclick="setLightGroupColor('${{grp.id}}', 0, 255, 100)"></span>
                            <span class="color-chip" style="background:#3b82f6;" title="Blau" onclick="setLightGroupColor('${{grp.id}}', 0, 100, 255)"></span>
                            <span class="color-chip" style="background:#8b5cf6;" title="Lila" onclick="setLightGroupColor('${{grp.id}}', 160, 32, 240)"></span>
                            <input type="color" value='#ffffff' style="width:20px; height:20px; padding:0; border:none; border-radius:50%; cursor:pointer; margin-left:auto;" title="Eigene Gruppenfarbe wählen" onchange="handleLightGroupCustomColor('${{grp.id}}', this.value)" />
                        </div>

                        <!-- Accordion for individual member light fine-tuning & ungrouping -->
                        <details style="margin-top:12px; border-top:1px dashed rgba(245, 158, 11, 0.3); padding-top:8px;">
                            <summary style="font-size:11px; cursor:pointer; color:#b45309; font-weight:600;">
                                ▼ Einzelne Lampen anpassen (${{memberDevs.length}})
                            </summary>
                            <div style="margin-top:8px; display:flex; flex-direction:column; gap:8px;">
                                ${{memberDevs.map(mDev => `
                                    <div style="display:flex; justify-content:space-between; align-items:center; background:var(--surface); padding:6px 8px; border-radius:6px; border:1px solid var(--border); font-size:11px;">
                                        <span>${{mDev.effective_name}}</span>
                                        <div style="display:flex; gap:6px; align-items:center;">
                                            ${{mDev.is_govee ? `
                                                <button type="button" class="btn btn-sm" style="font-size:10px; padding:2px 6px;" onclick="toggleGoveePower('${{mDev.ip}}', this)">
                                                    ${{mDev.govee_on ? 'An' : 'Aus'}}
                                                </button>
                                            ` : ''}}
                                        </div>
                                    </div>
                                `).join('')}}
                                <div style="display:flex; justify-content:flex-end; margin-top:4px;">
                                    <button type="button" class="btn btn-sm" style="font-size:10px; color:#dc2626; border-color:#fca5a5;" onclick="deleteLightGroup('${{grp.id}}')">
                                        ✖ Gruppe auflösen
                                    </button>
                                </div>
                            </div>
                        </details>
                    </div>
                </div>
            `;
        }}

        // Render standard Hero Light / Actuator Card
        function renderSingleDeviceCard(dev) {{
            let statusDot = dev.is_active 
                ? '<span style="display:inline-block; width:8px; height:8px; border-radius:50%; background:#10b981;" title="Online"></span>'
                : '<span style="display:inline-block; width:8px; height:8px; border-radius:50%; background:#94a3b8;" title="Offline"></span>';

            let controlsHtml = '';

            // 1. Govee Smart Light Control Widget
            if (dev.is_govee && dev.ip) {{
                let isOn = dev.govee_on;
                let bri = dev.govee_brightness || 100;
                let [r, g, b] = dev.govee_color || [255, 255, 255];
                let colorCss = `rgb(${{r}}, ${{g}}, ${{b}})`;

                controlsHtml = `
                    <div class="dev-control-widget govee-widget">
                        <div style="display:flex; justify-content:space-between; align-items:center;">
                            <div style="font-weight:600; font-size:12px; color:#1e40af; display:flex; align-items:center; gap:6px;">
                                <span style="display:inline-block; width:12px; height:12px; border-radius:50%; background:${{colorCss}}; border:1px solid rgba(0,0,0,0.2);"></span>
                                <span>Govee LAN (${{dev.govee_sku || 'Light'}})</span>
                            </div>
                            <button type="button" class="btn btn-sm ${{isOn ? 'btn-primary' : ''}}" style="font-size:11px; padding:3px 10px;" onclick="toggleGoveePower('${{dev.ip}}', this)">
                                ${{isOn ? '💡 An' : '⚪ Aus'}}
                            </button>
                        </div>
                        <div class="slider-row">
                            <span>Dimmen:</span>
                            <input type="range" min="1" max="100" value="${{bri}}" style="flex:1; cursor:pointer;" onchange="setGoveeBrightness('${{dev.ip}}', this.value)" oninput="this.nextElementSibling.innerText = this.value + '%'" />
                            <span style="min-width:30px; text-align:right; font-weight:600;">${{bri}}%</span>
                        </div>
                        <div class="color-chips">
                            <span style="font-size:10px; color:var(--muted); margin-right:2px;">Farbe:</span>
                            <span class="color-chip" style="background:#ffddaa;" title="Warmweiß (2700K)" onclick="setGoveeTemp('${{dev.ip}}', 2700)"></span>
                            <span class="color-chip" style="background:#fff4e6;" title="Neutralweiß (4000K)" onclick="setGoveeTemp('${{dev.ip}}', 4000)"></span>
                            <span class="color-chip" style="background:#f1f5f9;" title="Kaltweiß (6500K)" onclick="setGoveeTemp('${{dev.ip}}', 6500)"></span>
                            <span class="color-chip" style="background:#ef4444;" title="Rot" onclick="setGoveeColor('${{dev.ip}}', 255, 0, 0)"></span>
                            <span class="color-chip" style="background:#10b981;" title="Grün" onclick="setGoveeColor('${{dev.ip}}', 0, 255, 100)"></span>
                            <span class="color-chip" style="background:#3b82f6;" title="Blau" onclick="setGoveeColor('${{dev.ip}}', 0, 100, 255)"></span>
                            <span class="color-chip" style="background:#8b5cf6;" title="Lila" onclick="setGoveeColor('${{dev.ip}}', 160, 32, 240)"></span>
                            <input type="color" value='#ffffff' style="width:20px; height:20px; padding:0; border:none; border-radius:50%; cursor:pointer; margin-left:auto;" title="Eigene Farbe wählen" onchange="handleGoveeCustomColor('${{dev.ip}}', this.value)" />
                        </div>
                    </div>
                `;
            }} else if (dev.is_hue && dev.hue_id) {{
                let isOn = dev.hue_on;
                controlsHtml = `
                    <div class="dev-control-widget" style="display:flex; justify-content:space-between; align-items:center;">
                        <span style="font-size:11px; color:#b45309; font-weight:600;">💡 Philips Hue Lampe</span>
                        <button type="button" class="btn btn-sm ${{isOn ? 'btn-primary' : ''}}" style="font-size:11px; padding:3px 10px;" onclick="toggleHueLight('${{dev.hue_id}}', this)">
                            ${{isOn ? '💡 An' : '⚪ Aus'}}
                        </button>
                    </div>
                `;
            }} else if (dev.web_url) {{
                controlsHtml = `
                    <div class="dev-control-widget" style="display:flex; justify-content:space-between; align-items:center;">
                        <span style="font-size:11px; color:var(--muted);">Webinterface verfügbar</span>
                        <a href="${{dev.web_url}}" target="_blank" class="btn-sm btn-web" style="font-size:11px;">🌐 Web UI</a>
                    </div>
                `;
            }}

            return `
                <div class="room-dev-card" id="dev-card-${{dev.device_id}}">
                    <div>
                        <div class="dev-card-header">
                            <div class="dev-card-info">
                                <div class="dev-card-icon">${{dev.category_icon}}</div>
                                <div>
                                    <div class="dev-card-name">${{dev.effective_name}}</div>
                                    <div class="dev-card-meta">${{dev.ip || 'No IP'}} &bull; ${{dev.category}}</div>
                                </div>
                            </div>
                            <div>${{statusDot}}</div>
                        </div>
                        <div style="font-size:11px; color:var(--muted); margin-bottom:4px;">
                            ${{dev.room ? `<span>🚪 <strong>${{dev.room}}</strong></span>` : '<span style="color:#f59e0b;">⚠️ Kein Raum</span>'}}
                            ${{dev.mac ? `&bull; <code>${{dev.mac}}</code>` : ''}}
                        </div>
                    </div>
                    ${{controlsHtml}}
                </div>
            `;
        }}

        // Render Compact Secondary Device (Sensors / Gateways / Infrastructure)
        function renderSecondaryDeviceCard(dev) {{
            const m = dev.metadata || {{}};
            let telemetryInfo = [];
            if (m.temperature) telemetryInfo.push(`🌡️ ${{m.temperature}}°C`);
            if (m.humidity) telemetryInfo.push(`💧 ${{m.humidity}}%`);
            if (m.battery) telemetryInfo.push(`🔋 ${{m.battery}}%`);
            if (m.power) telemetryInfo.push(`⚡ ${{m.power}}W`);

            const teleText = telemetryInfo.length > 0 ? telemetryInfo.join(' &bull; ') : (dev.ip || 'Drahtlos / ZigBee');

            return `
                <div class="secondary-card">
                    <div style="display:flex; justify-content:space-between; align-items:center;">
                        <div style="display:flex; align-items:center; gap:8px;">
                            <span>${{dev.category_icon || '📡'}}</span>
                            <div>
                                <div style="font-weight:600; line-height:1.2;">${{dev.effective_name}}</div>
                                <div style="font-size:10px; color:var(--muted);">${{dev.category}} &bull; ${{dev.kind}}</div>
                            </div>
                        </div>
                        <span style="font-size:10px; font-weight:600; color:var(--primary);">${{teleText}}</span>
                    </div>
                </div>
            `;
        }}

        function filterRoomsAndDevices() {{
            renderDeviceCards();
        }}

        // 2. Light Scenes Handler (POST /api/rooms/:name/scene)
        async function activateRoomScene(scene, btn) {{
            let roomName = '';
            if (currentRoomId !== 'ALL' && currentRoomId !== 'UNASSIGNED') {{
                const r = allRoomsData.find(rm => rm.id === currentRoomId);
                if (r) roomName = r.name;
            }}
            if (!roomName) {{
                alert('Bitte wähle zuerst einen Raum aus, um eine Lichtszene zu aktivieren.');
                return;
            }}

            if (btn) btn.disabled = true;
            try {{
                const res = await fetch('/api/rooms/' + encodeURIComponent(roomName) + '/scene', {{
                    method: 'POST',
                    headers: {{ 'Content-Type': 'application/json' }},
                    body: JSON.stringify({{ scene: scene }})
                }});
                const data = await res.json();
                if (res.ok) {{
                    // Update local dev states for this room to reflect scene
                    allDevicesData.forEach(d => {{
                        if (d.room && d.room.toLowerCase() === roomName.toLowerCase()) {{
                            if (scene === 'all_off') {{
                                d.govee_on = false;
                                d.hue_on = false;
                            }} else {{
                                d.govee_on = true;
                                d.hue_on = true;
                                if (scene === 'all_on') d.govee_brightness = 100;
                                if (scene === 'relax') {{ d.govee_brightness = 50; d.govee_kelvin = 2700; }}
                                if (scene === 'focus') {{ d.govee_brightness = 100; d.govee_kelvin = 4500; }}
                                if (scene === 'night') {{ d.govee_brightness = 10; d.govee_kelvin = 2200; }}
                                if (scene === 'movie') {{ d.govee_brightness = 20; d.govee_color = [160, 32, 240]; }}
                            }}
                        }}
                    }});
                    renderDeviceCards();
                }} else {{
                    alert('Fehler beim Aktivieren der Szene: ' + (data.message || 'Unbekannter Fehler'));
                }}
            }} catch(e) {{
                alert('Netzwerkfehler beim Szenenaufruf: ' + e);
            }} finally {{
                if (btn) btn.disabled = false;
            }}
        }}

        // 3. Apple Home Style Merged Light Group Controls
        async function toggleLightGroupPower(groupId, turnOn, btn) {{
            if (btn) btn.disabled = true;
            try {{
                const res = await fetch('/api/light-groups/' + encodeURIComponent(groupId) + '/power', {{
                    method: 'POST',
                    headers: {{ 'Content-Type': 'application/json' }},
                    body: JSON.stringify({{ on: turnOn }})
                }});
                if (res.ok) {{
                    const grp = allLightGroupsData.find(g => g.id === groupId);
                    if (grp) {{
                        grp.device_ids.forEach(id => {{
                            const d = allDevicesData.find(item => item.device_id === id || item.ip === id);
                            if (d) {{ d.govee_on = turnOn; d.hue_on = turnOn; }}
                        }});
                    }}
                    renderDeviceCards();
                }}
            }} catch(e) {{
                alert('Fehler beim Schalten der Gruppe: ' + e);
            }} finally {{
                if (btn) btn.disabled = false;
            }}
        }}

        async function setLightGroupBrightness(groupId, val) {{
            try {{
                const bri = parseInt(val, 10);
                await fetch('/api/light-groups/' + encodeURIComponent(groupId) + '/brightness', {{
                    method: 'POST',
                    headers: {{ 'Content-Type': 'application/json' }},
                    body: JSON.stringify({{ brightness: bri }})
                }});
                const grp = allLightGroupsData.find(g => g.id === groupId);
                if (grp) {{
                    grp.device_ids.forEach(id => {{
                        const d = allDevicesData.find(item => item.device_id === id || item.ip === id);
                        if (d) {{ d.govee_brightness = bri; d.govee_on = true; }}
                    }});
                }}
            }} catch(e) {{
                console.error('Group Brightness error:', e);
            }}
        }}

        async function setLightGroupColor(groupId, r, g, b) {{
            try {{
                await fetch('/api/light-groups/' + encodeURIComponent(groupId) + '/color', {{
                    method: 'POST',
                    headers: {{ 'Content-Type': 'application/json' }},
                    body: JSON.stringify({{ r: r, g: g, b: b }})
                }});
                const grp = allLightGroupsData.find(g => g.id === groupId);
                if (grp) {{
                    grp.device_ids.forEach(id => {{
                        const d = allDevicesData.find(item => item.device_id === id || item.ip === id);
                        if (d) {{ d.govee_color = [r, g, b]; d.govee_on = true; }}
                    }});
                }}
                renderDeviceCards();
            }} catch(e) {{
                console.error('Group Color error:', e);
            }}
        }}

        async function setLightGroupTemp(groupId, kelvin) {{
            try {{
                await fetch('/api/light-groups/' + encodeURIComponent(groupId) + '/temperature', {{
                    method: 'POST',
                    headers: {{ 'Content-Type': 'application/json' }},
                    body: JSON.stringify({{ kelvin: kelvin }})
                }});
                const grp = allLightGroupsData.find(g => g.id === groupId);
                if (grp) {{
                    grp.device_ids.forEach(id => {{
                        const d = allDevicesData.find(item => item.device_id === id || item.ip === id);
                        if (d) {{ d.govee_kelvin = kelvin; d.govee_on = true; }}
                    }});
                }}
                renderDeviceCards();
            }} catch(e) {{
                console.error('Group Temp error:', e);
            }}
        }}

        function handleLightGroupCustomColor(groupId, hex) {{
            if (!hex || hex.length !== 7) return;
            const r = parseInt(hex.slice(1, 3), 16);
            const g = parseInt(hex.slice(3, 5), 16);
            const b = parseInt(hex.slice(5, 7), 16);
            setLightGroupColor(groupId, r, g, b);
        }}

        async function deleteLightGroup(groupId) {{
            if (!confirm('Möchtest Du diese Lampengruppe wirklich auflösen? Die Lampen können danach wieder einzeln gesteuert werden.')) return;
            try {{
                const res = await fetch('/api/light-groups/' + encodeURIComponent(groupId), {{ method: 'DELETE' }});
                if (res.ok) {{
                    allLightGroupsData = allLightGroupsData.filter(g => g.id !== groupId);
                    renderDeviceCards();
                }} else {{
                    alert('Löschen der Gruppe fehlgeschlagen.');
                }}
            }} catch(e) {{
                alert('Fehler: ' + e);
            }}
        }}

        // Grouping Modal
        function openLightGroupModal() {{
            const select = document.getElementById('group-room-select');
            if (select) {{
                select.innerHTML = allRoomsData.map(r => `
                    <option value="${{r.name}}" ${{currentRoomId === r.id ? 'selected' : ''}}>${{r.name}} (${{r.floor}})</option>
                `).join('');
            }}
            updateGroupModalDeviceList();
            const modal = document.getElementById('light-group-modal');
            if (modal) modal.style.display = 'flex';
        }}

        function closeLightGroupModal() {{
            const modal = document.getElementById('light-group-modal');
            if (modal) modal.style.display = 'none';
        }}

        function updateGroupModalDeviceList() {{
            const roomName = document.getElementById('group-room-select')?.value || '';
            const listEl = document.getElementById('group-lights-checklist');
            if (!listEl) return;

            const roomLights = allDevicesData.filter(d => 
                d.room && d.room.toLowerCase() === roomName.toLowerCase() && isControllableActuator(d)
            );

            if (roomLights.length === 0) {{
                listEl.innerHTML = '<div style="color:var(--muted); font-size:11px; padding:6px;">Keine Lampen in diesem Raum gefunden.</div>';
                return;
            }}

            listEl.innerHTML = roomLights.map(l => `
                <label style="display:flex; align-items:center; gap:8px; font-size:12px; margin-bottom:6px; cursor:pointer;">
                    <input type="checkbox" name="group-member" value="${{l.device_id}}" />
                    <span>💡 ${{l.effective_name}} (${{l.ip || 'Hue/Zigbee'}})</span>
                </label>
            `).join('');
        }}

        async function submitCreateLightGroup(e) {{
            e.preventDefault();
            const name = document.getElementById('group-name-input').value.trim();
            const room = document.getElementById('group-room-select').value;
            const checked = Array.from(document.querySelectorAll('input[name="group-member"]:checked')).map(cb => cb.value);

            if (checked.length < 2) {{
                alert('Bitte wähle mindestens 2 Lampen aus, um sie zu einer Gruppe zusammenzufassen.');
                return;
            }}

            try {{
                const res = await fetch('/api/light-groups', {{
                    method: 'POST',
                    headers: {{ 'Content-Type': 'application/json' }},
                    body: JSON.stringify({{
                        name: name,
                        room: room,
                        device_ids: checked,
                        icon: '💡'
                    }})
                }});
                const data = await res.json();
                if (res.ok && data.status === 'ok') {{
                    allLightGroupsData.push(data.group);
                    closeLightGroupModal();
                    document.getElementById('group-name-input').value = '';
                    renderDeviceCards();
                }} else {{
                    alert('Fehler beim Erstellen der Gruppe: ' + (data.message || 'Unbekannter Fehler'));
                }}
            }} catch(err) {{
                alert('Netzwerkfehler: ' + err);
            }}
        }}

        // 4. In-Place Single Device Controls
        async function toggleGoveePower(ip, btn) {{
            if (btn) btn.disabled = true;
            try {{
                const res = await fetch('/api/govee/lights/' + encodeURIComponent(ip) + '/toggle', {{ method: 'POST' }});
                const data = await res.json();
                if (res.ok && data.status === 'ok') {{
                    const d = allDevicesData.find(item => item.ip === ip);
                    if (d) d.govee_on = data.on;
                    renderDeviceCards();
                }}
            }} catch(e) {{
                alert('Govee UDP Fehler: ' + e);
            }} finally {{
                if (btn) btn.disabled = false;
            }}
        }}

        async function setGoveeBrightness(ip, val) {{
            try {{
                const b = parseInt(val, 10);
                await fetch('/api/govee/lights/' + encodeURIComponent(ip) + '/brightness', {{
                    method: 'POST',
                    headers: {{ 'Content-Type': 'application/json' }},
                    body: JSON.stringify({{ brightness: b }})
                }});
                const d = allDevicesData.find(item => item.ip === ip);
                if (d) {{ d.govee_brightness = b; d.govee_on = true; }}
            }} catch(e) {{
                console.error('Govee Brightness Error:', e);
            }}
        }}

        async function setGoveeColor(ip, r, g, b) {{
            try {{
                await fetch('/api/govee/lights/' + encodeURIComponent(ip) + '/color', {{
                    method: 'POST',
                    headers: {{ 'Content-Type': 'application/json' }},
                    body: JSON.stringify({{ r: r, g: g, b: b }})
                }});
                const d = allDevicesData.find(item => item.ip === ip);
                if (d) {{ d.govee_color = [r, g, b]; d.govee_on = true; }}
                renderDeviceCards();
            }} catch(e) {{
                console.error('Govee Color Error:', e);
            }}
        }}

        async function setGoveeTemp(ip, kelvin) {{
            try {{
                await fetch('/api/govee/lights/' + encodeURIComponent(ip) + '/temperature', {{
                    method: 'POST',
                    headers: {{ 'Content-Type': 'application/json' }},
                    body: JSON.stringify({{ kelvin: kelvin }})
                }});
                const d = allDevicesData.find(item => item.ip === ip);
                if (d) {{ d.govee_kelvin = kelvin; d.govee_on = true; }}
                renderDeviceCards();
            }} catch(e) {{
                console.error('Govee Temp Error:', e);
            }}
        }}

        function handleGoveeCustomColor(ip, hex) {{
            if (!hex || hex.length !== 7) return;
            const r = parseInt(hex.slice(1, 3), 16);
            const g = parseInt(hex.slice(3, 5), 16);
            const b = parseInt(hex.slice(5, 7), 16);
            setGoveeColor(ip, r, g, b);
        }}

        // In-place Hue Toggle (NO FULL PAGE RELOAD)
        async function toggleHueLight(lightId, btn) {{
            if (btn) btn.disabled = true;
            try {{
                const res = await fetch('/api/hue/lights/' + encodeURIComponent(lightId) + '/toggle', {{ method: 'POST' }});
                const json = await res.json();
                if (res.ok && json.status === 'ok') {{
                    const d = allDevicesData.find(item => item.hue_id === lightId);
                    if (d) {{
                        d.hue_on = !d.hue_on;
                    }}
                    renderDeviceCards();
                }} else {{
                    alert('Hue Fehler: ' + (json.message || 'Schalten fehlgeschlagen'));
                }}
            }} catch(e) {{
                alert('Netzwerkfehler: ' + e);
            }} finally {{
                if (btn) btn.disabled = false;
            }}
        }}

        // Room Management Modal & Room Editor Functions
        function openRoomModal() {{
            renderModalRooms();
            const m = document.getElementById('room-manager-modal');
            if (m) m.style.display = 'flex';
        }}

        function closeRoomModal() {{
            const m = document.getElementById('room-manager-modal');
            if (m) m.style.display = 'none';
        }}

        function renderModalRooms() {{
            const tbody = document.getElementById('modal-rooms-tbody');
            const countEl = document.getElementById('modal-room-count');
            if (!tbody) return;

            countEl.innerText = allRoomsData.length;
            if (allRoomsData.length === 0) {{
                tbody.innerHTML = '<tr><td colspan="6" style="text-align:center; padding:20px; color:var(--muted);">Noch keine Räume definiert. Lege oben einen Raum an oder importiere aus Hue.</td></tr>';
                return;
            }}

            tbody.innerHTML = allRoomsData.map(r => {{
                const devCount = allDevicesData.filter(d => (d.room || '').toLowerCase() === r.name.toLowerCase()).length;
                const matterTag = r.matter_tag || 'CommonSpace';
                const hueBadge = r.hue_group_id 
                    ? `<span class="badge" style="background:#fef3c7; color:#92400e; font-size:10px;" title="Hue Gruppe #${{r.hue_group_id}}">💡 #${{r.hue_group_id}}</span>`
                    : '<span style="color:var(--muted); font-size:10px;">—</span>';

                return `
                    <tr style="border-bottom:1px solid var(--border);">
                        <td style="padding:8px 10px; font-weight:600;">
                            <span style="font-size:16px; margin-right:6px;">${{r.icon || '📍'}}</span>
                            <span>${{r.name}}</span>
                        </td>
                        <td style="padding:8px 10px; color:var(--muted);">${{r.floor || 'Ohne Etage'}}</td>
                        <td style="padding:8px 10px;">
                            <span class="badge" style="background:#e0e7ff; color:#3730a3; font-size:10px;">✨ ${{matterTag}}</span>
                        </td>
                        <td style="padding:8px 10px;">${{hueBadge}}</td>
                        <td style="padding:8px 10px; text-align:center;">
                            <span class="badge" style="background:var(--badge-bg); font-size:11px; font-weight:600;">${{devCount}}</span>
                        </td>
                        <td style="padding:8px 10px; text-align:right; white-space:nowrap;">
                            <button type="button" class="btn btn-sm" style="font-size:11px; padding:3px 8px; margin-right:4px;" onclick="editRoom('${{r.id}}')">✏️ Edit</button>
                            <button type="button" class="btn btn-sm" style="color:#ef4444; border-color:#fca5a5; font-size:11px; padding:3px 6px;" onclick="deleteRoom('${{r.id}}', '${{r.name}}')">🗑️</button>
                        </td>
                    </tr>
                `;
            }}).join('');
        }}

        function editRoom(roomId) {{
            const r = allRoomsData.find(item => item.id === roomId);
            if (!r) return;

            document.getElementById('rm-id').value = r.id;
            document.getElementById('rm-name').value = r.name;
            document.getElementById('rm-floor').value = r.floor || 'Erdgeschoss';
            document.getElementById('rm-archetype').value = r.archetype || 'other';
            document.getElementById('rm-icon').value = r.icon || '📍';
            document.getElementById('rm-matter').value = r.matter_tag || 'CommonSpace';
            document.getElementById('rm-hue-group-id').value = r.hue_group_id || '';
            document.getElementById('rm-hue-class').value = r.hue_class || '';

            const hueSyncWrap = document.getElementById('rm-hue-sync-wrapper');
            if (hueSyncWrap) {{
                hueSyncWrap.style.display = r.hue_group_id ? 'block' : 'none';
            }}

            document.getElementById('room-editor-title').innerText = '✏️ Raum bearbeiten: ' + r.name;
            document.getElementById('btn-save-room').innerText = '💾 Änderungen speichern';
            document.getElementById('btn-cancel-room-edit').style.display = 'inline-block';
            document.getElementById('rm-name').focus();
        }}

        function cancelEditRoom() {{
            document.getElementById('rm-id').value = '';
            document.getElementById('rm-name').value = '';
            document.getElementById('rm-floor').value = 'Erdgeschoss';
            document.getElementById('rm-archetype').value = 'living_room';
            document.getElementById('rm-icon').value = '🛋️';
            document.getElementById('rm-matter').value = 'LivingRoom';
            document.getElementById('rm-hue-group-id').value = '';
            document.getElementById('rm-hue-class').value = '';

            const hueSyncWrap = document.getElementById('rm-hue-sync-wrapper');
            if (hueSyncWrap) hueSyncWrap.style.display = 'none';

            document.getElementById('room-editor-title').innerText = '➕ Neuen Raum anlegen';
            document.getElementById('btn-save-room').innerText = '💾 Raum speichern';
            document.getElementById('btn-cancel-room-edit').style.display = 'none';
        }}

        function setRoomIcon(emoji) {{
            const iconInput = document.getElementById('rm-icon');
            if (iconInput) iconInput.value = emoji;
        }}

        const archetypeMap = {{
            'living_room': {{ icon: '🛋️', tag: 'LivingRoom', floor: 'Erdgeschoss' }},
            'kitchen': {{ icon: '🍳', tag: 'Kitchen', floor: 'Erdgeschoss' }},
            'dining_room': {{ icon: '🍽️', tag: 'DiningRoom', floor: 'Erdgeschoss' }},
            'bedroom': {{ icon: '🛏️', tag: 'Bedroom', floor: 'Obergeschoss' }},
            'kids_room': {{ icon: '🧸', tag: 'KidsRoom', floor: 'Obergeschoss' }},
            'bathroom': {{ icon: '🛁', tag: 'Bathroom', floor: 'Obergeschoss' }},
            'toilet': {{ icon: '🚽', tag: 'Bathroom', floor: 'Erdgeschoss' }},
            'office': {{ icon: '💼', tag: 'Office', floor: 'Obergeschoss' }},
            'hallway': {{ icon: '🚪', tag: 'Hallway', floor: 'Erdgeschoss' }},
            'stairs': {{ icon: '🪜', tag: 'Hallway', floor: 'Erdgeschoss' }},
            'basement': {{ icon: '📦', tag: 'Basement', floor: 'Keller' }},
            'outdoor': {{ icon: '🌳', tag: 'Outdoor', floor: 'Außenbereich' }},
            'garage': {{ icon: '🚗', tag: 'Garage', floor: 'Außenbereich' }},
            'spa': {{ icon: '🧖', tag: 'Bathroom', floor: 'Keller' }},
            'other': {{ icon: '📍', tag: 'CommonSpace', floor: 'Erdgeschoss' }}
        }};

        function onArchetypeSelect(arch) {{
            const info = archetypeMap[arch];
            if (info) {{
                const iconEl = document.getElementById('rm-icon');
                const matterEl = document.getElementById('rm-matter');
                const floorEl = document.getElementById('rm-floor');
                if (iconEl) iconEl.value = info.icon;
                if (matterEl) matterEl.value = info.tag;
                if (floorEl && !document.getElementById('rm-id').value) floorEl.value = info.floor;
            }}
        }}

        function onRoomNameInput(val) {{
            if (document.getElementById('rm-id').value) return;
            val = (val || '').toLowerCase();
            let detected = null;
            if (val.includes('wohn')) detected = 'living_room';
            else if (val.includes('küh') || val.includes('kueh')) detected = 'kitchen';
            else if (val.includes('ess')) detected = 'dining_room';
            else if (val.includes('schlaf')) detected = 'bedroom';
            else if (val.includes('kinder') || val.includes('tom') || val.includes('sophie')) detected = 'kids_room';
            else if (val.includes('bad') || val.includes('bath')) detected = 'bathroom';
            else if (val.includes('wc') || val.includes('toilet')) detected = 'toilet';
            else if (val.includes('büro') || val.includes('buero') || val.includes('arbeit')) detected = 'office';
            else if (val.includes('flur') || val.includes('windfang') || val.includes('diele')) detected = 'hallway';
            else if (val.includes('keller') || val.includes('ug')) detected = 'basement';
            else if (val.includes('garten') || val.includes('terrasse') || val.includes('balkon') || val.includes('vordach')) detected = 'outdoor';
            else if (val.includes('garage') || val.includes('carport')) detected = 'garage';
            else if (val.includes('sauna') || val.includes('wellness')) detected = 'spa';

            if (detected) {{
                const archEl = document.getElementById('rm-archetype');
                if (archEl) {{
                    archEl.value = detected;
                    onArchetypeSelect(detected);
                }}
            }}
        }}

        async function submitRoomForm(e) {{
            e.preventDefault();
            const id = document.getElementById('rm-id').value.trim();
            const name = document.getElementById('rm-name').value.trim();
            if (!name) return;

            const floor = document.getElementById('rm-floor').value;
            const archetype = document.getElementById('rm-archetype').value;
            const icon = document.getElementById('rm-icon').value.trim();
            const matter = document.getElementById('rm-matter').value.trim();
            const hueGroupId = document.getElementById('rm-hue-group-id').value.trim();
            const hueClass = document.getElementById('rm-hue-class').value.trim();
            const statusEl = document.getElementById('room-op-status');

            try {{
                const res = await fetch('/api/rooms', {{
                    method: 'POST',
                    headers: {{ 'Content-Type': 'application/json' }},
                    body: JSON.stringify({{
                        id: id,
                        name: name,
                        floor: floor,
                        archetype: archetype,
                        icon: icon,
                        matter_tag: matter,
                        hue_group_id: hueGroupId || null,
                        hue_class: hueClass || null,
                    }})
                }});
                const data = await res.json();
                if (res.ok && data.status === 'ok') {{
                    const saved = data.room;
                    const existingIdx = allRoomsData.findIndex(r => r.id === saved.id);
                    let oldName = '';
                    if (existingIdx >= 0) {{
                        oldName = allRoomsData[existingIdx].name;
                        allRoomsData[existingIdx] = saved;
                    }} else {{
                        allRoomsData.push(saved);
                    }}

                    // Propagate rename to allDevicesData and allLightGroupsData in client memory
                    if (oldName && oldName.toLowerCase() !== saved.name.toLowerCase()) {{
                        allDevicesData.forEach(d => {{
                            if (d.room && d.room.toLowerCase() === oldName.toLowerCase()) {{
                                d.room = saved.name;
                            }}
                        }});
                        allLightGroupsData.forEach(g => {{
                            if (g.room && g.room.toLowerCase() === oldName.toLowerCase()) {{
                                g.room = saved.name;
                            }}
                        }});
                    }}

                    if (statusEl) {{
                        statusEl.innerText = `Raum "${{saved.name}}" gespeichert!`;
                        statusEl.style.color = '#10b981';
                        setTimeout(() => {{ if (statusEl) statusEl.innerText = ''; }}, 3000);
                    }}

                    cancelEditRoom();
                    renderModalRooms();
                    renderRoomPills();
                    renderDeviceCards();
                }} else {{
                    alert('Fehler beim Speichern des Raums: ' + (data.message || 'Unbekannter Fehler'));
                }}
            }} catch(err) {{
                alert('Netzwerkfehler: ' + err);
            }}
        }}

        async function deleteRoom(id, name) {{
            if (!confirm(`Möchtest Du den Raum "${{name}}" wirklich löschen?`)) return;
            try {{
                const res = await fetch('/api/rooms/' + encodeURIComponent(id), {{ method: 'DELETE' }});
                if (res.ok) {{
                    allRoomsData = allRoomsData.filter(r => r.id !== id);
                    if (currentRoomId === id) currentRoomId = 'ALL';
                    renderModalRooms();
                    renderRoomPills();
                    renderDeviceCards();
                }} else {{
                    alert('Fehler beim Löschen des Raums');
                }}
            }} catch(err) {{
                alert('Netzwerkfehler: ' + err);
            }}
        }}

        async function importHueRooms() {{
            const btn = document.getElementById('btn-import-hue');
            const statusEl = document.getElementById('room-op-status');
            if (btn) btn.disabled = true;
            if (statusEl) {{
                statusEl.innerText = 'Importiere Räume aus Hue...';
                statusEl.style.color = 'var(--muted)';
            }}
            try {{
                const res = await fetch('/api/rooms/import-hue', {{ method: 'POST' }});
                const json = await res.json();
                if (res.ok && json.status === 'ok') {{
                    if (statusEl) {{
                        statusEl.innerText = json.message || 'Import erfolgreich!';
                        statusEl.style.color = '#10b981';
                    }}
                    setTimeout(() => window.location.reload(), 1000);
                }} else {{
                    if (statusEl) {{
                        statusEl.innerText = 'Fehler: ' + (json.message || 'Import fehlgeschlagen');
                        statusEl.style.color = '#ef4444';
                    }}
                    if (btn) btn.disabled = false;
                }}
            }} catch(e) {{
                if (statusEl) {{
                    statusEl.innerText = 'Netzwerkfehler: ' + e;
                    statusEl.style.color = '#ef4444';
                }}
                if (btn) btn.disabled = false;
            }}
        }}

        document.addEventListener('DOMContentLoaded', initRoomView);
    </script>
    "##,
    rooms_json_str = serde_json::to_string(&rooms_json).unwrap_or_else(|_| "[]".to_string()),
    devices_json_str = serde_json::to_string(&devices_json).unwrap_or_else(|_| "[]".to_string()),
    light_groups_json_str = serde_json::to_string(&light_groups_json).unwrap_or_else(|_| "[]".to_string()),
    );

    page_layout(title, "rooms", &content)
}
