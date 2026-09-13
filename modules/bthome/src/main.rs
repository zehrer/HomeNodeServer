mod dedup;
mod parser;

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use axum::routing::{get, post};
use axum::Router;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tonic::transport::Channel;
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

use dedup::BTHomeDedup;
use homenode_sdk::proto::home_node_control_client::HomeNodeControlClient;
use homenode_sdk::proto::{Empty, HealthState, ModuleRegistration, UpsertDevicesRequest};
use homenode_sdk::{
    connect_control_client, device_record, module_health, module_manifest, ModuleEnvironment,
};
use parser::{parse_bthome_v2, ButtonEventType};

#[derive(Debug, Deserialize)]
struct BTHomeConfig {
    #[serde(default = "default_health_message")]
    health_message: String,
    #[serde(default = "default_listen_port")]
    listen_port: u16,
    #[serde(default = "default_dedup_window_ms")]
    dedup_window_ms: u64,
    #[serde(default = "default_scan_interval_secs")]
    scan_interval_secs: u64,
    #[serde(default = "default_min_rssi")]
    min_rssi: i16,
    #[serde(default)]
    shelly_gateways: Vec<String>,
    #[serde(default = "default_auto_connect_shelly_ws")]
    auto_connect_shelly_ws: bool,
    #[serde(default)]
    homenode_host: Option<String>,
    #[serde(default)]
    devices: Vec<ConfiguredDevice>,
}

#[derive(Debug, Clone, Deserialize)]
struct ConfiguredDevice {
    mac: String,
    name: String,
    #[serde(default)]
    #[allow(dead_code)]
    bind_key: Option<String>,
}

fn default_health_message() -> String {
    "BTHome BLE gateway receiver active".to_string()
}

fn default_listen_port() -> u16 {
    8124
}

fn default_dedup_window_ms() -> u64 {
    2500
}

fn default_scan_interval_secs() -> u64 {
    10
}

fn default_min_rssi() -> i16 {
    -90
}

fn default_auto_connect_shelly_ws() -> bool {
    true
}

impl Default for BTHomeConfig {
    fn default() -> Self {
        Self {
            health_message: default_health_message(),
            listen_port: default_listen_port(),
            dedup_window_ms: default_dedup_window_ms(),
            scan_interval_secs: default_scan_interval_secs(),
            min_rssi: default_min_rssi(),
            shelly_gateways: Vec::new(),
            auto_connect_shelly_ws: true,
            homenode_host: None,
            devices: Vec::new(),
        }
    }
}

fn load_config(path: &Path) -> Result<BTHomeConfig> {
    if !path.exists() {
        warn!("Config file {} not found; using defaults", path.display());
        return Ok(BTHomeConfig::default());
    }

    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config from {}", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(BTHomeConfig::default());
    }

    toml::from_str(&raw).context("failed to parse BTHome TOML config")
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,homenode_module_bthome=debug"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

#[derive(Default, Debug, Serialize)]
struct BTHomeMetrics {
    packets_received: u64,
    packets_deduplicated: u64,
    packets_processed: u64,
    devices_seen: usize,
    ws_connections_active: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BthomeLiveEvent {
    pub id: String,
    pub timestamp: String,
    pub mac: String,
    pub device_name: String,
    pub event_type: String,
    pub description: String,
    pub icon: String,
    pub gateway: String,
    #[serde(default)]
    pub gateway_id: Option<String>,
    #[serde(default)]
    pub gateway_ip: Option<String>,
    #[serde(default)]
    pub gateway_name: Option<String>,
    #[serde(default)]
    pub room: Option<String>,
    pub rssi: i16,
    pub battery: Option<u8>,
    pub details: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifiedShellyGateway {
    pub ip: String,
    pub id: String,
    pub mac: String,
    pub model: String,
    pub gen: i64,
    pub name: String,
    pub ble_supported: bool,
    pub ble_enabled: bool,
    pub ws_connected: bool,
    pub last_seen: String,
}

#[derive(Clone)]
struct AppState {
    dedup: Arc<BTHomeDedup>,
    configured_devices: Arc<HashMap<String, ConfiguredDevice>>,
    client: Arc<Mutex<HomeNodeControlClient<Channel>>>,
    module_id: String,
    min_rssi: i16,
    metrics: Arc<Mutex<BTHomeMetrics>>,
    known_devices: Arc<Mutex<HashMap<String, String>>>,
    device_telemetry: Arc<Mutex<HashMap<String, HashMap<String, String>>>>,
    recent_events: Arc<Mutex<VecDeque<BthomeLiveEvent>>>,
    verified_gateways: Arc<Mutex<HashMap<String, VerifiedShellyGateway>>>,
}

fn decode_hex_payload(s: &str) -> Option<Vec<u8>> {
    let cleaned = s.trim().trim_start_matches("0x").replace([':', ' ', '-'], "");
    if cleaned.is_empty() || cleaned.len() % 2 != 0 {
        return None;
    }
    (0..cleaned.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&cleaned[i..i + 2], 16).ok())
        .collect()
}

fn decode_raw_payload(s: &str) -> Option<Vec<u8>> {
    // Try hex first
    if let Some(hex) = decode_hex_payload(s) {
        return Some(hex);
    }
    // Fallback to base64 (used by Shelly CloudRelay / Firmware 2.0)
    BASE64_STANDARD.decode(s.trim()).ok()
}

#[derive(Debug, Clone)]
struct IngestItem {
    mac: String,
    gateway: String,
    gateway_id: Option<String>,
    gateway_ip: Option<String>,
    rssi: i16,
    payload_bytes: Vec<u8>,
    synthetic_button: Option<ButtonEventType>,
}

fn extract_ingest_items(val: &Value) -> Vec<IngestItem> {
    extract_ingest_items_with_fallback(val, None)
}

fn extract_ingest_items_with_fallback(val: &Value, fallback_gw: Option<&str>) -> Vec<IngestItem> {
    let mut items = Vec::new();

    // Case 1: Array of items
    if let Some(arr) = val.as_array() {
        for item in arr {
            items.extend(extract_ingest_items_with_fallback(item, fallback_gw));
        }
        return items;
    }

    // Case 2: Shelly RPC notification format: { "params": { "events": [...] } }
    if let Some(events) = val.pointer("/params/events").and_then(|v| v.as_array()) {
        let src = val.get("src").and_then(|v| v.as_str());
        let gateway_id = src.map(|s| s.to_string());
        let gateway_ip = fallback_gw.map(|s| s.to_string());
        let gateway = match (&gateway_ip, &gateway_id) {
            (Some(ip), Some(id)) => format!("{ip} ({id})"),
            (Some(ip), None) => ip.clone(),
            (None, Some(id)) => id.clone(),
            (None, None) => "shelly-gateway".to_string(),
        };

        for ev in events {
            let _comp = ev.get("component").and_then(|v| v.as_str()).unwrap_or("");
            let event_type = ev.get("event").and_then(|v| v.as_str()).unwrap_or("");

            let btn_ev = match event_type {
                "single_push" => Some(ButtonEventType::Press),
                "double_push" => Some(ButtonEventType::DoublePress),
                "triple_push" => Some(ButtonEventType::TriplePress),
                "long_push" => Some(ButtonEventType::LongPress),
                "long_double_push" => Some(ButtonEventType::LongDoublePress),
                "long_triple_push" => Some(ButtonEventType::LongTriplePress),
                "hold" => Some(ButtonEventType::Hold),
                _ => None,
            };

            let addr = ev
                .pointer("/data/addr")
                .or_else(|| ev.pointer("/data/mac"))
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_lowercase());

            let rssi = ev
                .pointer("/data/rssi")
                .and_then(|v| v.as_i64())
                .map(|n| n as i16)
                .unwrap_or(-60);

            if let Some(btn) = btn_ev {
                if let Some(mac) = addr {
                    items.push(IngestItem {
                        mac,
                        gateway: gateway.clone(),
                        gateway_id: gateway_id.clone(),
                        gateway_ip: gateway_ip.clone(),
                        rssi,
                        payload_bytes: Vec::new(),
                        synthetic_button: Some(btn),
                    });
                    continue;
                }
            }

            if let Some(data) = ev.get("data") {
                let mut sub_items = extract_ingest_items_with_fallback(data, fallback_gw);
                for sub in &mut sub_items {
                    if sub.gateway == "shelly-gateway" {
                        sub.gateway = gateway.clone();
                        sub.gateway_id = gateway_id.clone();
                        sub.gateway_ip = gateway_ip.clone();
                    }
                }
                items.extend(sub_items);
            }
        }
        if !items.is_empty() {
            return items;
        }
    }

    let obj = match val.as_object() {
        Some(o) => o,
        None => return items,
    };

    let src = obj
        .get("src")
        .or_else(|| obj.get("gateway_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let raw_ip = obj
        .get("gateway_ip")
        .or_else(|| obj.get("gateway"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| fallback_gw.map(|s| s.to_string()));

    let gateway_id = src;
    let gateway_ip = raw_ip;
    let gateway = match (&gateway_ip, &gateway_id) {
        (Some(ip), Some(id)) => format!("{ip} ({id})"),
        (Some(ip), None) => ip.clone(),
        (None, Some(id)) => id.clone(),
        (None, None) => "shelly-gateway".to_string(),
    };

    // Case 3: Shelly CloudRelay format: { "devices": [ { "<mac>": { "sdata": { "fcd2": "<base64>" } } } ] }
    if let Some(devs) = obj.get("devices").or_else(|| val.pointer("/result/devices")).and_then(|v| v.as_array()) {
        for dev_entry in devs {
            if let Some(dev_map) = dev_entry.as_object() {
                for (mac, info) in dev_map {
                    let sdata_str = info
                        .pointer("/sdata/fcd2")
                        .or_else(|| info.pointer("/sdata/FCD2"))
                        .and_then(|v| v.as_str());

                    if let Some(s) = sdata_str {
                        if let Some(bytes) = decode_raw_payload(s) {
                            let rssi = info
                                .get("rssi")
                                .and_then(|v| v.as_i64())
                                .map(|n| n as i16)
                                .unwrap_or(-65);

                            items.push(IngestItem {
                                mac: mac.trim().to_lowercase(),
                                gateway: gateway.clone(),
                                gateway_id: gateway_id.clone(),
                                gateway_ip: gateway_ip.clone(),
                                rssi,
                                payload_bytes: bytes,
                                synthetic_button: None,
                            });
                        }
                    }
                }
            }
        }
        if !items.is_empty() {
            return items;
        }
    }

    // Extract MAC / Addr
    let mac = obj
        .get("mac")
        .or_else(|| obj.get("addr"))
        .or_else(|| obj.get("address"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_lowercase());

    let mac = match mac {
        Some(m) if !m.is_empty() => m,
        _ => return items,
    };

    let rssi = obj
        .get("rssi")
        .and_then(|v| v.as_i64())
        .map(|n| n as i16)
        .unwrap_or(-70);

    let mut payload_bytes = None;

    if let Some(raw_str) = obj
        .get("data")
        .or_else(|| obj.get("payload"))
        .and_then(|v| v.as_str())
    {
        payload_bytes = decode_raw_payload(raw_str);
    } else if let Some(arr) = obj
        .get("data")
        .or_else(|| obj.get("payload"))
        .and_then(|v| v.as_array())
    {
        let bytes: Option<Vec<u8>> = arr.iter().map(|n| n.as_u64().map(|b| b as u8)).collect();
        payload_bytes = bytes;
    } else if let Some(svc) = obj.get("service_data") {
        if let Some(s) = svc.as_str() {
            payload_bytes = decode_raw_payload(s);
        } else if let Some(svc_obj) = svc.as_object() {
            if let Some(fcd2) = svc_obj.get("fcd2").or_else(|| svc_obj.get("FCD2")) {
                if let Some(s) = fcd2.as_str() {
                    payload_bytes = decode_raw_payload(s);
                }
            }
        }
    }

    if let Some(bytes) = payload_bytes {
        if !bytes.is_empty() {
            items.push(IngestItem {
                mac,
                gateway,
                gateway_id,
                gateway_ip,
                rssi,
                payload_bytes: bytes,
                synthetic_button: None,
            });
        }
    }

    items
}

async fn process_ingest_items(state: &AppState, items: Vec<IngestItem>) -> (usize, usize) {
    let mut deduplicated_count = 0;
    let mut processed_count = 0;

    for item in items {
        {
            let mut m = state.metrics.lock().await;
            m.packets_received += 1;
        }

        if item.rssi < state.min_rssi {
            debug!(
                "Ignoring advertisement from {} (RSSI {} < min {})",
                item.mac, item.rssi, state.min_rssi
            );
            continue;
        }

        let norm_mac = item.mac.trim().to_lowercase();
        let mac_clean = norm_mac.replace([':', '-'], "");
        let mac_suffix = if mac_clean.len() >= 4 {
            &mac_clean[mac_clean.len() - 4..]
        } else {
            &mac_clean
        };
        let dev_id = format!("bthome-{}", mac_clean);

        // Handle synthetic button event (e.g. from native Shelly RPC bthomedevice.single_push)
        if let Some(btn) = item.synthetic_button {
            let btn_byte = match btn {
                ButtonEventType::None => 0,
                ButtonEventType::Press => 1,
                ButtonEventType::DoublePress => 2,
                ButtonEventType::TriplePress => 3,
                ButtonEventType::LongPress => 4,
                ButtonEventType::LongDoublePress => 5,
                ButtonEventType::LongTriplePress => 6,
                ButtonEventType::Hold => 128,
                ButtonEventType::Unknown(code) => code,
            };
            let is_dup = state.dedup.is_duplicate(
                &norm_mac,
                None,
                &[btn_byte],
                item.rssi,
                &item.gateway,
            );
            if is_dup {
                deduplicated_count += 1;
                let mut m = state.metrics.lock().await;
                m.packets_deduplicated += 1;
                continue;
            }

            processed_count += 1;
            {
                let mut m = state.metrics.lock().await;
                m.packets_processed += 1;
            }

            let (btn_str, btn_desc) = match btn {
                ButtonEventType::Press => ("press", "Single Press"),
                ButtonEventType::DoublePress => ("double_press", "Double Press"),
                ButtonEventType::TriplePress => ("triple_press", "Triple Press"),
                ButtonEventType::LongPress => ("long_press", "Long Press"),
                ButtonEventType::LongDoublePress => ("long_double_press", "Long Double Press"),
                ButtonEventType::LongTriplePress => ("long_triple_press", "Long Triple Press"),
                ButtonEventType::Hold => ("hold", "Hold"),
                _ => ("unknown", "Button Action"),
            };

            info!(
                "🔘 BTHome Push Event: {} -> {:?} (via {})",
                norm_mac, btn, item.gateway
            );

            // Fetch previous telemetry if known to preserve battery etc.
            let mut meta = {
                let telem = state.device_telemetry.lock().await;
                telem.get(&norm_mac).cloned().unwrap_or_default()
            };

            meta.insert("mac".to_string(), norm_mac.clone());
            meta.insert("protocol".to_string(), "bthome-v2".to_string());
            meta.insert("source".to_string(), "bthome".to_string());
            meta.insert("sources".to_string(), "bthome,shelly-gateway".to_string());
            meta.insert("gateway".to_string(), item.gateway.clone());
            meta.insert("shelly_gateway".to_string(), item.gateway.clone());
            meta.insert("rssi".to_string(), item.rssi.to_string());
            meta.insert("last_seen".to_string(), Utc::now().to_rfc3339());
            meta.insert("status".to_string(), "active".to_string());
            meta.insert("button_event".to_string(), btn_str.to_string());
            meta.insert("category".to_string(), "button".to_string());
            meta.insert("category_title".to_string(), "Buttons & Remote Controls".to_string());
            meta.insert("category_icon".to_string(), "🔘".to_string());

            let display_name = state
                .configured_devices
                .get(&norm_mac)
                .map(|d| d.name.clone())
                .unwrap_or_else(|| format!("Shelly BLU Button {}", mac_suffix));

            {
                let mut telem = state.device_telemetry.lock().await;
                telem.insert(norm_mac.clone(), meta.clone());
            }

            {
                let battery = meta.get("battery").and_then(|s| s.parse::<u8>().ok());
                let mut evs = state.recent_events.lock().await;
                evs.push_front(BthomeLiveEvent {
                    id: format!("{}-{}", norm_mac, Utc::now().timestamp_millis()),
                    timestamp: Utc::now().to_rfc3339(),
                    mac: norm_mac.clone(),
                    device_name: display_name.clone(),
                    event_type: btn_str.to_string(),
                    description: format!("🔘 {}", btn_desc),
                    icon: "🔘".to_string(),
                    gateway: item.gateway.clone(),
                    gateway_id: item.gateway_id.clone(),
                    gateway_ip: item.gateway_ip.clone(),
                    gateway_name: None,
                    room: None,
                    rssi: item.rssi,
                    battery,
                    details: meta.clone(),
                });
                while evs.len() > 100 {
                    evs.pop_back();
                }
            }

            let dev = device_record(
                state.module_id.clone(),
                dev_id,
                display_name.clone(),
                "button",
                ["ble", "bthome", "button", "battery"],
                meta,
            );

            {
                let mut known = state.known_devices.lock().await;
                known.insert(norm_mac.clone(), display_name);
                let mut m = state.metrics.lock().await;
                m.devices_seen = known.len();
            }

            let mut client_guard = state.client.lock().await;
            let _ = client_guard
                .upsert_devices(UpsertDevicesRequest {
                    module_id: state.module_id.clone(),
                    devices: vec![dev],
                })
                .await;
            continue;
        }

        let packet = match parse_bthome_v2(&item.payload_bytes) {
            Ok(p) => p,
            Err(e) => {
                debug!("Failed to parse BTHome V2 payload from {}: {}", item.mac, e);
                continue;
            }
        };

        let pid = packet.packet_id();
        let is_dup = state.dedup.is_duplicate(
            &item.mac,
            pid,
            &item.payload_bytes,
            item.rssi,
            &item.gateway,
        );

        if is_dup {
            deduplicated_count += 1;
            let mut m = state.metrics.lock().await;
            m.packets_deduplicated += 1;
            debug!(
                "Deduplicated packet for {} via gateway {} (packet_id={:?})",
                item.mac, item.gateway, pid
            );
            continue;
        }

        processed_count += 1;
        {
            let mut m = state.metrics.lock().await;
            m.packets_processed += 1;
        }

        // Build device metadata and capabilities, preserving prior telemetry (battery etc.)
        let mut meta = {
            let telem = state.device_telemetry.lock().await;
            telem.get(&norm_mac).cloned().unwrap_or_default()
        };
        meta.insert("mac".to_string(), norm_mac.clone());
        meta.insert("protocol".to_string(), "bthome-v2".to_string());
        meta.insert("source".to_string(), "bthome".to_string());
        meta.insert("sources".to_string(), "bthome,shelly-gateway".to_string());
        meta.insert("gateway".to_string(), item.gateway.clone());
        meta.insert("shelly_gateway".to_string(), item.gateway.clone());
        meta.insert("rssi".to_string(), item.rssi.to_string());
        meta.insert("last_seen".to_string(), Utc::now().to_rfc3339());
        meta.insert("status".to_string(), "active".to_string());

        let mut capabilities = vec!["ble".to_string(), "bthome".to_string()];
        let mut kind = "sensor";
        let mut category_title = "Sensors & Detectors";
        let mut category_icon = "👁️";
        let mut default_name = format!("BTHome Sensor {}", mac_suffix);

        let mut live_event_opt: Option<(String, String, String)> = None;

        if let Some(bat) = packet.battery() {
            meta.insert("battery".to_string(), bat.to_string());
            capabilities.push("battery".to_string());
        }

        if let Some(btn) = packet.button_event() {
            let (btn_str, btn_desc) = match btn {
                ButtonEventType::Press => ("press", "Single Press"),
                ButtonEventType::DoublePress => ("double_press", "Double Press"),
                ButtonEventType::TriplePress => ("triple_press", "Triple Press"),
                ButtonEventType::LongPress => ("long_press", "Long Press"),
                ButtonEventType::LongDoublePress => ("long_double_press", "Long Double Press"),
                ButtonEventType::LongTriplePress => ("long_triple_press", "Long Triple Press"),
                ButtonEventType::Hold => ("hold", "Hold"),
                _ => ("unknown", "Button Action"),
            };
            meta.insert("button_event".to_string(), btn_str.to_string());
            meta.insert("button_event_raw".to_string(), format!("{:?}", btn));
            capabilities.push("button".to_string());
            kind = "button";
            category_title = "Buttons & Remote Controls";
            category_icon = "🔘";
            default_name = format!("Shelly BLU Button {}", mac_suffix);
            info!(
                "🔘 BTHome Button Event: {} -> {:?} (via {})",
                norm_mac, btn, item.gateway
            );
            live_event_opt = Some((btn_str.to_string(), format!("🔘 {}", btn_desc), "🔘".to_string()));
        }

        if let Some(open) = packet.door_window() {
            meta.insert("contact_open".to_string(), open.to_string());
            meta.insert(
                "contact_state".to_string(),
                if open { "open" } else { "closed" }.to_string(),
            );
            capabilities.push("contact-sensor".to_string());
            kind = "contact-sensor";
            category_title = "Doors & Windows";
            category_icon = "🚪";
            default_name = format!("Shelly BLU Door/Window {}", mac_suffix);
            info!(
                "🚪 BTHome Door/Window: {} -> {} (via {})",
                norm_mac,
                if open { "OPEN" } else { "CLOSED" },
                item.gateway
            );
            live_event_opt = Some((
                if open { "door_opened".to_string() } else { "door_closed".to_string() },
                if open { "🚪 Door/Window Opened".to_string() } else { "🚪 Door/Window Closed".to_string() },
                "🚪".to_string(),
            ));
        }

        if let Some(motion) = packet.motion() {
            meta.insert("motion_detected".to_string(), motion.to_string());
            capabilities.push("motion-sensor".to_string());
            kind = "motion-sensor";
            category_title = "Motion Detectors";
            category_icon = "🚶";
            default_name = format!("Shelly BLU Motion {}", mac_suffix);
            if motion {
                live_event_opt = Some(("motion_detected".to_string(), "🚶 Motion Detected".to_string(), "🚶".to_string()));
            }
        }

        if let Some(temp) = packet.temperature() {
            meta.insert("temperature_c".to_string(), format!("{:.2}", temp));
            capabilities.push("temperature".to_string());
        }

        if let Some(hum) = packet.humidity() {
            meta.insert("humidity_pct".to_string(), format!("{:.2}", hum));
            capabilities.push("humidity".to_string());
        }

        if let Some(lux) = packet.illuminance() {
            meta.insert("illuminance_lux".to_string(), format!("{:.1}", lux));
            capabilities.push("illuminance".to_string());
        }

        let display_name = state
            .configured_devices
            .get(&norm_mac)
            .map(|d| d.name.clone())
            .unwrap_or(default_name);

        meta.insert("category".to_string(), kind.to_string());
        meta.insert("category_title".to_string(), category_title.to_string());
        meta.insert("category_icon".to_string(), category_icon.to_string());

        {
            let mut telem = state.device_telemetry.lock().await;
            telem.insert(norm_mac.clone(), meta.clone());
        }

        // Push to recent events log
        let battery = meta.get("battery").and_then(|s| s.parse::<u8>().ok());
        if let Some((ev_type, ev_desc, ev_icon)) = live_event_opt {
            let mut evs = state.recent_events.lock().await;
            evs.push_front(BthomeLiveEvent {
                id: format!("{}-{}", norm_mac, Utc::now().timestamp_millis()),
                timestamp: Utc::now().to_rfc3339(),
                mac: norm_mac.clone(),
                device_name: display_name.clone(),
                event_type: ev_type,
                description: ev_desc,
                icon: ev_icon,
                gateway: item.gateway.clone(),
                gateway_id: item.gateway_id.clone(),
                gateway_ip: item.gateway_ip.clone(),
                gateway_name: None,
                room: None,
                rssi: item.rssi,
                battery,
                details: meta.clone(),
            });
            while evs.len() > 100 {
                evs.pop_back();
            }
        } else if let Some(bat) = battery {
            let mut evs = state.recent_events.lock().await;
            let should_add = !evs.iter().take(5).any(|e| e.mac == norm_mac && e.event_type == "battery_update");
            if should_add {
                evs.push_front(BthomeLiveEvent {
                    id: format!("{}-{}", norm_mac, Utc::now().timestamp_millis()),
                    timestamp: Utc::now().to_rfc3339(),
                    mac: norm_mac.clone(),
                    device_name: display_name.clone(),
                    event_type: "battery_update".to_string(),
                    description: format!("🔋 Battery {}%", bat),
                    icon: "🔋".to_string(),
                    gateway: item.gateway.clone(),
                    gateway_id: item.gateway_id.clone(),
                    gateway_ip: item.gateway_ip.clone(),
                    gateway_name: None,
                    room: None,
                    rssi: item.rssi,
                    battery: Some(bat),
                    details: meta.clone(),
                });
                while evs.len() > 100 {
                    evs.pop_back();
                }
            }
        }

        let dev = device_record(
            state.module_id.clone(),
            dev_id,
            display_name.clone(),
            kind,
            capabilities,
            meta,
        );

        {
            let mut known = state.known_devices.lock().await;
            known.insert(norm_mac.clone(), display_name);
            let mut m = state.metrics.lock().await;
            m.devices_seen = known.len();
        }

        let mut client_guard = state.client.lock().await;
        if let Err(e) = client_guard
            .upsert_devices(UpsertDevicesRequest {
                module_id: state.module_id.clone(),
                devices: vec![dev],
            })
            .await
        {
            error!("Failed to upsert device {} to supervisor: {}", norm_mac, e);
        }
    }

    (processed_count, deduplicated_count)
}

async fn bthome_ingest_handler(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let items = extract_ingest_items(&body);
    if items.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "status": "error",
                "message": "No valid BLE advertisement items found in payload"
            })),
        );
    }

    let (processed, deduplicated) = process_ingest_items(&state, items).await;

    (
        StatusCode::OK,
        Json(json!({
            "status": "ok",
            "processed": processed,
            "deduplicated": deduplicated
        })),
    )
}

async fn ws_handler(
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let peer_ip = peer_addr.ip().to_string();
    ws.on_upgrade(move |socket| handle_ws_socket(socket, peer_ip, state))
}

async fn handle_ws_socket(mut socket: WebSocket, peer_ip: String, state: AppState) {
    info!("📡 Shelly [{}] connected to Outbound WebSocket", peer_ip);
    {
        let mut m = state.metrics.lock().await;
        m.ws_connections_active += 1;
    }
    {
        let mut gws = state.verified_gateways.lock().await;
        if let Some(gw) = gws.get_mut(&peer_ip) {
            gw.ws_connected = true;
            gw.last_seen = Utc::now().to_rfc3339();
        }
    }

    while let Some(msg) = socket.recv().await {
        match msg {
            Ok(WsMessage::Text(text)) => {
                debug!("Shelly WS [{}] incoming message: {}", peer_ip, text);
                if let Ok(val) = serde_json::from_str::<Value>(&text) {
                    if let Some(src) = val.get("src").and_then(|v| v.as_str()) {
                        let mut gws = state.verified_gateways.lock().await;
                        if let Some(gw) = gws.get_mut(&peer_ip) {
                            if gw.id.is_empty() {
                                gw.id = src.to_string();
                            }
                            gw.ws_connected = true;
                            gw.last_seen = Utc::now().to_rfc3339();
                        }
                    }

                    let items = extract_ingest_items_with_fallback(&val, Some(&peer_ip));
                    if !items.is_empty() {
                        process_ingest_items(&state, items).await;
                    }
                }
            }
            Ok(WsMessage::Ping(payload)) => {
                let _ = socket.send(WsMessage::Pong(payload)).await;
            }
            Ok(WsMessage::Close(_)) => {
                info!("Shelly WebSocket [{}] disconnected", peer_ip);
                break;
            }
            Err(e) => {
                warn!("Shelly WebSocket [{}] connection error: {}", peer_ip, e);
                break;
            }
            _ => {}
        }
    }

    {
        let mut m = state.metrics.lock().await;
        if m.ws_connections_active > 0 {
            m.ws_connections_active -= 1;
        }
    }
    {
        let mut gws = state.verified_gateways.lock().await;
        if let Some(gw) = gws.get_mut(&peer_ip) {
            gw.ws_connected = false;
        }
    }
}

async fn health_handler(State(state): State<AppState>) -> impl IntoResponse {
    let metrics = state.metrics.lock().await;
    let gws = state.verified_gateways.lock().await;
    let verified_count = gws.values().filter(|g| g.ble_supported).count();
    let active_gw_ips: Vec<String> = gws.values().filter(|g| g.ws_connected).map(|g| g.ip.clone()).collect();
    (
        StatusCode::OK,
        Json(json!({
            "status": "ready",
            "module_id": state.module_id,
            "verified_gateways_count": verified_count,
            "active_gateways": active_gw_ips,
            "metrics": *metrics
        })),
    )
}

async fn events_handler(State(state): State<AppState>) -> impl IntoResponse {
    let events = state.recent_events.lock().await;
    let list: Vec<BthomeLiveEvent> = events.iter().cloned().collect();
    (StatusCode::OK, Json(list))
}

async fn gateways_handler(State(state): State<AppState>) -> impl IntoResponse {
    let gws = state.verified_gateways.lock().await;
    let list: Vec<VerifiedShellyGateway> = gws.values().cloned().collect();
    (StatusCode::OK, Json(list))
}

async fn http_post_json(ip: &str, port: u16, path: &str, body: &Value) -> Option<Value> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    let addr = format!("{}:{}", ip, port);
    let body_str = serde_json::to_string(body).ok()?;
    let req = format!(
        "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        path, ip, body_str.len(), body_str
    );

    let mut stream = tokio::time::timeout(Duration::from_secs(2), TcpStream::connect(&addr))
        .await
        .ok()?
        .ok()?;

    tokio::time::timeout(Duration::from_secs(2), stream.write_all(req.as_bytes()))
        .await
        .ok()?
        .ok()?;

    let mut resp = Vec::new();
    let mut buf = [0u8; 4096];
    let read_fut = async {
        loop {
            let n = stream.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            resp.extend_from_slice(&buf[..n]);
        }
        Ok::<(), std::io::Error>(())
    };

    tokio::time::timeout(Duration::from_secs(3), read_fut)
        .await
        .ok()?
        .ok()?;

    let resp_str = String::from_utf8(resp).ok()?;
    let body_part = resp_str.split("\r\n\r\n").nth(1)?;
    serde_json::from_str(body_part).ok()
}

// Background task to configure and sync connected Shellys automatically
async fn shelly_gateway_worker(
    configured_gateways: Vec<String>,
    host_ip: String,
    port: u16,
    auto_connect_ws: bool,
    state: AppState,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(45));
    loop {
        interval.tick().await;

        let mut all_gateways = configured_gateways.clone();

        // Dynamically discover all Shelly devices from HomeNode runtime snapshot
        {
            let mut client_guard = state.client.lock().await;
            if let Ok(resp) = client_guard.get_runtime_snapshot(Empty {}).await {
                let snapshot = resp.into_inner();
                for dev in snapshot.devices {
                    let is_shelly = dev.metadata.get("vendor").map(|v| v.to_lowercase().contains("shelly")).unwrap_or(false)
                        || dev.metadata.get("sources").map(|s| s.contains("shelly")).unwrap_or(false)
                        || dev.display_name.to_lowercase().contains("shelly")
                        || dev.metadata.get("hostname").map(|h| h.to_lowercase().contains("shelly")).unwrap_or(false);

                    if is_shelly {
                        if let Some(ip) = dev.metadata.get("ip") {
                            if !ip.is_empty() && ip != "Layer 2" && ip != "-" && !all_gateways.contains(ip) {
                                all_gateways.push(ip.clone());
                            }
                        }
                    }
                }
            }
        }

        for ip in &all_gateways {
            // First check if it's a reachable Shelly Gen2/Gen3
            let dev_info = http_post_json(ip, 80, "/rpc/Shelly.GetDeviceInfo", &json!({})).await;
            let Some(info_obj) = dev_info else {
                state.verified_gateways.lock().await.remove(ip);
                continue;
            };

            // If it returned an error (e.g. {"code": 404, "message": "No handler for Shelly.GetDeviceInfo"})
            if info_obj.get("code").is_some() {
                state.verified_gateways.lock().await.remove(ip);
                continue;
            }

            let gen = info_obj.get("gen").and_then(|v| v.as_i64()).unwrap_or(0);
            if gen < 2 {
                // Gen1 Shellys do not support BLE RPC or Outbound WS
                state.verified_gateways.lock().await.remove(ip);
                continue;
            }

            // Explicitly test if BLE is supported on this hardware/firmware by querying BLE.GetConfig
            let ble_cfg = http_post_json(ip, 80, "/rpc/BLE.GetConfig", &json!({})).await;
            let ble_supported = match ble_cfg {
                Some(ref cfg) => cfg.get("code").is_none(),
                None => false,
            };

            if !ble_supported {
                debug!("Shelly at {} (Gen{}) does not support BLE service/RPC", ip, gen);
                state.verified_gateways.lock().await.remove(ip);
                continue;
            }

            let id = info_obj.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let mac = info_obj.get("mac").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let model = info_obj.get("model").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let name = info_obj.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();

            debug!("Shelly Gen{} BLE Gateway verified at http://{} ({}, model={})", gen, ip, id, model);

            // 1. Initial CloudRelay / BTHome sync (Firmware 2.0 native cache)
            if let Some(val) = http_post_json(ip, 80, "/rpc/BLE.CloudRelay.ListInfos", &json!({})).await {
                let items = extract_ingest_items_with_fallback(&val, Some(ip));
                if !items.is_empty() {
                    info!(
                        "Discovered {} BTHome devices from Shelly Gateway {}",
                        items.len(),
                        ip
                    );
                    process_ingest_items(&state, items).await;
                }
            }

            // 2. Enable BLE RPC
            let _ = http_post_json(
                ip,
                80,
                "/rpc/BLE.SetConfig",
                &json!({"config": {"enable": true, "rpc": {"enable": true}}}),
            )
            .await;

            // 3. Auto-configure Outbound WebSocket if requested
            if auto_connect_ws {
                let ws_target = format!("ws://{}:{}/ws", host_ip, port);
                let needs_update = match http_post_json(ip, 80, "/rpc/WS.GetConfig", &json!({})).await {
                    Some(cfg) => {
                        let current_server = cfg.get("server").and_then(|v| v.as_str()).unwrap_or("");
                        let enabled = cfg.get("enable").and_then(|v| v.as_bool()).unwrap_or(false);
                        !enabled || current_server != ws_target
                    }
                    None => false,
                };

                if needs_update {
                    info!(
                        "Configuring Shelly {} Outbound WebSocket to {}",
                        ip, ws_target
                    );
                    if let Some(res) = http_post_json(
                        ip,
                        80,
                        "/rpc/WS.SetConfig",
                        &json!({
                            "config": {
                                "enable": true,
                                "server": ws_target,
                                "ssl_ca": "ca.pem"
                            }
                        }),
                    )
                    .await {
                        if res.get("restart_required").and_then(|v| v.as_bool()).unwrap_or(false) {
                            info!("Rebooting Shelly {} to apply Outbound WebSocket config", ip);
                            let _ = http_post_json(ip, 80, "/rpc/Shelly.Reboot", &json!({})).await;
                        }
                    }
                }
            }

            // Record as verified gateway
            {
                let mut gw_guard = state.verified_gateways.lock().await;
                let existing_ws = gw_guard.get(ip).map(|g| g.ws_connected).unwrap_or(false);
                gw_guard.insert(ip.clone(), VerifiedShellyGateway {
                    ip: ip.clone(),
                    id,
                    mac,
                    model,
                    gen,
                    name,
                    ble_supported: true,
                    ble_enabled: true,
                    ws_connected: existing_ws,
                    last_seen: Utc::now().to_rfc3339(),
                });
            }
        }
    }
}


#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let env = ModuleEnvironment::from_env()?;
    let config = load_config(&env.config_path)?;

    info!(
        "Starting BTHome BLE receiver module id={}, listen_port={}, dedup_window={}ms, min_rssi={}dBm",
        env.module_id, config.listen_port, config.dedup_window_ms, config.min_rssi
    );

    let client = connect_control_client(&env.socket_path)
        .await
        .context("failed to connect to supervisor control plane")?;

    let client = Arc::new(Mutex::new(client));

    {
        let mut client_guard = client.lock().await;
        client_guard
            .register_module(ModuleRegistration {
                manifest: Some(module_manifest(
                    env.module_id.clone(),
                    "BTHome BLE Receiver",
                    env!("CARGO_PKG_VERSION"),
                    [
                        "bluetooth",
                        "ble",
                        "bthome",
                        "sensor",
                        "battery",
                        "shelly-gateway",
                    ],
                )),
                initial_health: Some(module_health(
                    env.module_id.clone(),
                    HealthState::Starting,
                    "Starting BTHome BLE gateway receiver",
                )),
            })
            .await
            .context("failed to register bthome module")?;
    }

    let mut configured_map = HashMap::new();
    let mut initial_devices = Vec::new();
    let mut known_map = HashMap::new();

    for dev in &config.devices {
        let norm_mac = dev.mac.trim().to_lowercase();
        configured_map.insert(norm_mac.clone(), dev.clone());
        known_map.insert(norm_mac.clone(), dev.name.clone());

        let dev_id = format!("bthome-{}", norm_mac.replace([':', '-'], ""));
        let mut meta = HashMap::new();
        meta.insert("mac".to_string(), norm_mac.clone());
        meta.insert("protocol".to_string(), "bthome-v2".to_string());
        meta.insert("source".to_string(), "bthome".to_string());
        meta.insert("sources".to_string(), "bthome,shelly-gateway".to_string());
        meta.insert("status".to_string(), "active".to_string());
        meta.insert("category".to_string(), "sensor".to_string());
        meta.insert("category_title".to_string(), "Sensors & Detectors".to_string());
        meta.insert("category_icon".to_string(), "👁️".to_string());

        initial_devices.push(device_record(
            env.module_id.clone(),
            dev_id,
            dev.name.clone(),
            "sensor",
            ["ble", "bthome", "battery"],
            meta,
        ));
    }

    if !initial_devices.is_empty() {
        let mut client_guard = client.lock().await;
        client_guard
            .upsert_devices(UpsertDevicesRequest {
                module_id: env.module_id.clone(),
                devices: initial_devices,
            })
            .await?;
    }

    {
        let mut client_guard = client.lock().await;
        client_guard
            .report_health(module_health(
                env.module_id.clone(),
                HealthState::Ready,
                format!(
                    "{} (port :{}, {} pre-configured)",
                    config.health_message,
                    config.listen_port,
                    config.devices.len()
                ),
            ))
            .await?;
    }

    let app_state = AppState {
        dedup: Arc::new(BTHomeDedup::new(Duration::from_millis(
            config.dedup_window_ms,
        ))),
        configured_devices: Arc::new(configured_map),
        client: Arc::clone(&client),
        module_id: env.module_id.clone(),
        min_rssi: config.min_rssi,
        metrics: Arc::new(Mutex::new(BTHomeMetrics {
            devices_seen: known_map.len(),
            ..Default::default()
        })),
        known_devices: Arc::new(Mutex::new(known_map)),
        device_telemetry: Arc::new(Mutex::new(HashMap::new())),
        recent_events: Arc::new(Mutex::new(VecDeque::new())),
        verified_gateways: Arc::new(Mutex::new(HashMap::new())),
    };

    // Build Axum HTTP & WebSocket Router
    let router = Router::new()
        .route("/bthome", post(bthome_ingest_handler))
        .route("/api/bthome", post(bthome_ingest_handler))
        .route("/ws", get(ws_handler))
        .route("/rpc", get(ws_handler))
        .route("/events", get(events_handler))
        .route("/api/events", get(events_handler))
        .route("/gateways", get(gateways_handler))
        .route("/api/gateways", get(gateways_handler))
        .route("/health", get(health_handler))
        .with_state(app_state.clone());

    let bind_addr = SocketAddr::from(([0, 0, 0, 0], config.listen_port));
    info!(
        "Binding BTHome Shelly Gateway Server on http://{}",
        bind_addr
    );
    let listener = tokio::net::TcpListener::bind(bind_addr)
        .await
        .with_context(|| format!("failed to bind TCP listener on {}", bind_addr))?;

    // Spawn HTTP/WS server task
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>()).await {
            error!("BTHome HTTP/WS server error: {}", e);
        }
    });

    info!(
        "BTHome module registered and listening on port {}. Ready to receive Shelly BLE WebSocket and Webhooks.",
        config.listen_port
    );

    // Auto-detect local host IP for Shelly Outbound WebSocket target if not provided
    let host_ip = config.homenode_host.unwrap_or_else(|| {
        "192.168.178.46".to_string()
    });

    let gateways = config.shelly_gateways.clone();
    let state_clone = app_state.clone();
    let auto_ws = config.auto_connect_shelly_ws;
    let port = config.listen_port;

    tokio::spawn(async move {
        shelly_gateway_worker(gateways, host_ip, port, auto_ws, state_clone).await;
    });

    // Heartbeat reporting loop
    let mut ticker = tokio::time::interval(Duration::from_secs(config.scan_interval_secs));
    loop {
        ticker.tick().await;
        let metrics = app_state.metrics.lock().await;
        let msg = format!(
            "BTHome receiver active (port :{}, {} pkts, {} deduped, {} processed, {} ws conns, {} sensors)",
            config.listen_port,
            metrics.packets_received,
            metrics.packets_deduplicated,
            metrics.packets_processed,
            metrics.ws_connections_active,
            metrics.devices_seen
        );
        let mut client_guard = client.lock().await;
        let _ = client_guard
            .report_health(module_health(
                env.module_id.clone(),
                HealthState::Ready,
                msg,
            ))
            .await;
    }
}

#[cfg(test)]
mod ingest_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_decode_hex_and_base64_payloads() {
        let expected = vec![0x40, 0x01, 0x5F, 0x3A, 0x01];
        assert_eq!(decode_raw_payload("40015f3a01"), Some(expected.clone()));
        assert_eq!(decode_raw_payload("0x40015f3a01"), Some(expected.clone()));
        assert_eq!(decode_raw_payload("40:01:5F:3A:01"), Some(expected.clone()));
        
        // Base64 from real Shelly Firmware 2.0 CloudRelay: "RAAKAV46ADoBYAA="
        let decoded = decode_raw_payload("RAAKAV46ADoBYAA=").expect("valid base64");
        assert_eq!(decoded, vec![68, 0, 10, 1, 94, 58, 0, 58, 1, 96, 0]);
    }

    #[test]
    fn test_extract_shelly_cloudrelay_format() {
        let val = json!({
            "src": "shellyplugsg3-543204689994",
            "devices": [
                {
                    "f8:44:77:06:50:84": {
                        "name": null,
                        "model": 0,
                        "sdata": {
                            "fcd2": "RAAKAV46ADoBYAA="
                        },
                        "last_seen": 1789314797
                    }
                }
            ]
        });
        let items = extract_ingest_items(&val);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].mac, "f8:44:77:06:50:84");
        assert_eq!(items[0].gateway, "shellyplugsg3-543204689994");
        assert_eq!(items[0].payload_bytes, vec![68, 0, 10, 1, 94, 58, 0, 58, 1, 96, 0]);
    }

    #[test]
    fn test_extract_shelly_native_rpc_push_event() {
        let val = json!({
            "src": "shellyplugsg3-543204689994",
            "method": "NotifyEvent",
            "params": {
                "ts": 1789315050.12,
                "events": [
                    {
                        "component": "bthomedevice:200",
                        "event": "single_push",
                        "data": {
                            "addr": "f8:44:77:06:50:84"
                        }
                    }
                ]
            }
        });
        let items = extract_ingest_items(&val);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].mac, "f8:44:77:06:50:84");
        assert_eq!(items[0].synthetic_button, Some(ButtonEventType::Press));
    }
}
