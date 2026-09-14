use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use homenode_sdk::proto::home_node_control_client::HomeNodeControlClient;
use homenode_sdk::proto::{HealthState, ModuleRegistration, SubscribeCommandsRequest, UpsertDevicesRequest};
use homenode_sdk::{
    connect_control_client, device_record, module_health, module_manifest, ModuleEnvironment,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tonic::transport::Channel;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

fn default_health_message() -> String {
    "Philips Hue Interface Ready".to_string()
}

fn default_scan_interval_secs() -> u64 {
    5
}

fn default_api_port() -> u16 {
    80
}

fn default_listen_port() -> u16 {
    8125
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct HueConfig {
    #[serde(default = "default_health_message")]
    health_message: String,
    #[serde(default = "default_scan_interval_secs")]
    scan_interval_secs: u64,
    #[serde(default = "default_api_port")]
    api_port: u16,
    #[serde(default = "default_listen_port")]
    listen_port: u16,
    bridge_ip: Option<String>,
    username: Option<String>,
}

impl Default for HueConfig {
    fn default() -> Self {
        Self {
            health_message: default_health_message(),
            scan_interval_secs: default_scan_interval_secs(),
            api_port: default_api_port(),
            listen_port: default_listen_port(),
            bridge_ip: Some("192.168.178.12".to_string()),
            username: None,
        }
    }
}

fn load_config(path: &Path) -> Result<HueConfig> {
    if !path.exists() {
        warn!("Config file {} not found; using defaults", path.display());
        return Ok(HueConfig::default());
    }

    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config from {}", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(HueConfig::default());
    }

    toml::from_str(&raw).context("failed to parse Philips Hue TOML config")
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct HueCredentials {
    username: String,
    #[serde(default)]
    clientkey: Option<String>,
    #[serde(default)]
    bridge_ip: Option<String>,
    #[serde(default)]
    paired_at: Option<String>,
}

impl HueCredentials {
    fn load_from_file(path: &Path) -> Option<Self> {
        let content = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&content).ok()
    }

    fn save_to_file(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let serialized = serde_json::to_string_pretty(self)?;
        std::fs::write(path, serialized)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HueLightState {
    pub id: String,
    pub name: String,
    pub on: bool,
    pub bri: u8,
    pub reachable: bool,
    pub modelid: String,
    pub swversion: String,
    pub colormode: Option<String>,
    pub ct: Option<u16>,
    pub light_type: String,
}

#[derive(Clone)]
struct AppState {
    config: Arc<Mutex<HueConfig>>,
    credentials: Arc<Mutex<Option<HueCredentials>>>,
    client: Arc<Mutex<HomeNodeControlClient<Channel>>>,
    http_client: reqwest::Client,
    creds_path: PathBuf,
    active_lights: Arc<Mutex<HashMap<String, HueLightState>>>,
    module_id: String,
    pairing_in_progress: Arc<Mutex<bool>>,
    last_status: Arc<Mutex<String>>,
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,homenode_module_philips_hue=debug"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

async fn pair_hue_bridge(
    http: &reqwest::Client,
    ip: &str,
    port: u16,
) -> Result<HueCredentials, String> {
    let url = format!("http://{}:{}/api", ip, port);
    let body = json!({
        "devicetype": "homenode#server",
        "generateclientkey": true
    });

    let resp = match http
        .post(&url)
        .timeout(Duration::from_secs(3))
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return Err(format!("Verbindungsfehler zur Hue Bridge: {}", e)),
    };

    let text = match resp.text().await {
        Ok(t) => t,
        Err(e) => return Err(format!("Fehler beim Lesen der Antwort: {}", e)),
    };

    let val: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => return Err(format!("Ungültiges JSON von Hue Bridge: {}", e)),
    };

    if let Some(arr) = val.as_array() {
        if let Some(first) = arr.first() {
            if let Some(err) = first.get("error") {
                let err_type = err.get("type").and_then(|v| v.as_i64()).unwrap_or(0);
                if err_type == 101 {
                    return Err("link button not pressed".to_string());
                }
                let desc = err.get("description").and_then(|v| v.as_str()).unwrap_or("Unbekannter Fehler");
                return Err(format!("Hue Bridge Fehler: {}", desc));
            }
            if let Some(succ) = first.get("success") {
                let username = succ.get("username").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let clientkey = succ.get("clientkey").and_then(|v| v.as_str()).map(|s| s.to_string());
                if !username.is_empty() {
                    return Ok(HueCredentials {
                        username,
                        clientkey,
                        bridge_ip: Some(ip.to_string()),
                        paired_at: Some(Utc::now().to_rfc3339()),
                    });
                }
            }
        }
    }

    Err(format!("Unerwartetes Antwortformat: {}", text))
}

async fn fetch_and_sync_devices(state: &AppState) -> Result<()> {
    let (ip, port, username) = {
        let cfg = state.config.lock().await;
        let creds = state.credentials.lock().await;
        let bridge_ip = creds.as_ref().and_then(|c| c.bridge_ip.clone()).or_else(|| cfg.bridge_ip.clone());
        let user = creds.as_ref().map(|c| c.username.clone()).or_else(|| cfg.username.clone());
        (bridge_ip, cfg.api_port, user)
    };

    let (Some(ip), Some(username)) = (ip, username) else {
        return Ok(());
    };

    let lights_url = format!("http://{}:{}/api/{}/lights", ip, port, username);
    let resp = match state.http_client.get(&lights_url).timeout(Duration::from_secs(3)).send().await {
        Ok(r) => r,
        Err(e) => {
            warn!("Hue Bridge {} unreachable: {}", ip, e);
            return Ok(());
        }
    };

    let val: Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            warn!("Failed to parse Hue lights JSON: {}", e);
            return Ok(());
        }
    };

    let Some(lights_obj) = val.as_object() else {
        return Ok(());
    };

    let mut device_records = Vec::new();
    let mut lights_cache = HashMap::new();

    for (id_str, light_val) in lights_obj {
        let name = light_val.get("name").and_then(|v| v.as_str()).unwrap_or("Hue Light").to_string();
        let modelid = light_val.get("modelid").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let swversion = light_val.get("swversion").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let light_type = light_val.get("type").and_then(|v| v.as_str()).unwrap_or("Light").to_string();
        let uniqueid = light_val.get("uniqueid").and_then(|v| v.as_str()).unwrap_or("").to_string();

        let state_obj = light_val.get("state");
        let on = state_obj.and_then(|s| s.get("on")).and_then(|v| v.as_bool()).unwrap_or(false);
        let bri = state_obj.and_then(|s| s.get("bri")).and_then(|v| v.as_u64()).unwrap_or(0) as u8;
        let reachable = state_obj.and_then(|s| s.get("reachable")).and_then(|v| v.as_bool()).unwrap_or(true);
        let colormode = state_obj.and_then(|s| s.get("colormode")).and_then(|v| v.as_str()).map(|s| s.to_string());
        let ct = state_obj.and_then(|s| s.get("ct")).and_then(|v| v.as_u64()).map(|v| v as u16);

        lights_cache.insert(id_str.clone(), HueLightState {
            id: id_str.clone(),
            name: name.clone(),
            on,
            bri,
            reachable,
            modelid: modelid.clone(),
            swversion: swversion.clone(),
            colormode: colormode.clone(),
            ct,
            light_type: light_type.clone(),
        });

        let mut meta = HashMap::new();
        meta.insert("ip".to_string(), ip.clone());
        meta.insert("bridge_ip".to_string(), ip.clone());
        meta.insert("light_id".to_string(), id_str.clone());
        meta.insert("uniqueid".to_string(), uniqueid.clone());
        meta.insert("model".to_string(), modelid.clone());
        meta.insert("vendor".to_string(), "Philips Hue (Signify)".to_string());
        meta.insert("source".to_string(), "philips-hue".to_string());
        meta.insert("sources".to_string(), "philips-hue".to_string());
        meta.insert("status".to_string(), if reachable { "active".to_string() } else { "inactive".to_string() });
        meta.insert("category".to_string(), "lighting".to_string());
        meta.insert("category_title".to_string(), "Lighting & Lamps".to_string());
        meta.insert("category_icon".to_string(), "💡".to_string());
        meta.insert("power_state".to_string(), if on { "on".to_string() } else { "off".to_string() });
        meta.insert("brightness".to_string(), bri.to_string());
        if let Some(cm) = &colormode {
            meta.insert("colormode".to_string(), cm.clone());
        }
        meta.insert("last_seen".to_string(), Utc::now().to_rfc3339());

        let dev_id = if !uniqueid.is_empty() {
            let clean_uid = uniqueid.split('-').next().unwrap_or(&uniqueid).replace(':', "").to_lowercase();
            format!("hue-light-{}", clean_uid)
        } else {
            format!("hue-light-{}", id_str)
        };

        device_records.push(device_record(
            state.module_id.clone(),
            dev_id,
            name,
            "lighting",
            ["lighting", "switch", "brightness", "philips-hue"],
            meta,
        ));
    }

    // Also fetch sensors if possible
    let sensors_url = format!("http://{}:{}/api/{}/sensors", ip, port, username);
    if let Ok(sensor_resp) = state.http_client.get(&sensors_url).timeout(Duration::from_secs(3)).send().await {
        if let Ok(sensor_val) = sensor_resp.json::<Value>().await {
            if let Some(sensors_obj) = sensor_val.as_object() {
                for (id_str, s_val) in sensors_obj {
                    let s_type = s_val.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    // Only process tangible physical sensors: Motion (ZLLPresence), Dimmer/Tap Switches (ZLLSwitch, ZGPSwitch)
                    if s_type == "ZLLPresence" || s_type == "ZLLSwitch" || s_type == "ZGPSwitch" {
                        let s_name = s_val.get("name").and_then(|v| v.as_str()).unwrap_or("Hue Sensor").to_string();
                        let _s_model = s_val.get("modelid").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let uniqueid = s_val.get("uniqueid").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let state_obj = s_val.get("state");
                        let config_obj = s_val.get("config");

                        let is_presence = s_type == "ZLLPresence";
                        let presence = state_obj.and_then(|s| s.get("presence")).and_then(|v| v.as_bool()).unwrap_or(false);
                        let battery = config_obj.and_then(|c| c.get("battery")).and_then(|v| v.as_u64());

                        let mut meta = HashMap::new();
                        meta.insert("ip".to_string(), ip.clone());
                        meta.insert("sensor_id".to_string(), id_str.clone());
                        meta.insert("uniqueid".to_string(), uniqueid.clone());
                        meta.insert("vendor".to_string(), "Philips Hue (Signify)".to_string());
                        meta.insert("source".to_string(), "philips-hue".to_string());
                        meta.insert("sources".to_string(), "philips-hue".to_string());
                        meta.insert("status".to_string(), "active".to_string());
                        if let Some(bat) = battery {
                            meta.insert("battery".to_string(), bat.to_string());
                        }

                        let (kind, title, icon) = if is_presence {
                            meta.insert("motion_detected".to_string(), presence.to_string());
                            ("sensor", "Motion Detectors", "🚶")
                        } else {
                            ("button", "Remote Controls & Switches", "🔘")
                        };

                        meta.insert("category".to_string(), kind.to_string());
                        meta.insert("category_title".to_string(), title.to_string());
                        meta.insert("category_icon".to_string(), icon.to_string());
                        meta.insert("last_seen".to_string(), Utc::now().to_rfc3339());

                        let dev_id = if !uniqueid.is_empty() {
                            let clean_uid = uniqueid.split('-').next().unwrap_or(&uniqueid).replace(':', "").to_lowercase();
                            format!("hue-sensor-{}", clean_uid)
                        } else {
                            format!("hue-sensor-{}", id_str)
                        };

                        device_records.push(device_record(
                            state.module_id.clone(),
                            dev_id,
                            s_name,
                            kind,
                            ["sensor", "battery", "philips-hue"],
                            meta,
                        ));
                    }
                }
            }
        }
    }

    let light_count = lights_cache.len();
    let total_count = device_records.len();

    {
        let mut cache = state.active_lights.lock().await;
        *cache = lights_cache;
    }

    if !device_records.is_empty() {
        let mut client_guard = state.client.lock().await;
        let _ = client_guard
            .upsert_devices(UpsertDevicesRequest {
                module_id: state.module_id.clone(),
                devices: device_records,
            })
            .await;
    }

    let status_msg = format!("Verbunden mit Hue Bridge ({}: {}) - {} Lampen, {} Geräte aktiv", ip, username, light_count, total_count);
    {
        let mut s = state.last_status.lock().await;
        *s = status_msg.clone();
    }

    let mut client_guard = state.client.lock().await;
    let _ = client_guard
        .report_health(module_health(
            state.module_id.clone(),
            HealthState::Ready,
            status_msg,
        ))
        .await;

    Ok(())
}

async fn set_light_state(state: &AppState, light_id: &str, body: &Value) -> Result<String> {
    let (ip, port, username) = {
        let cfg = state.config.lock().await;
        let creds = state.credentials.lock().await;
        let bridge_ip = creds.as_ref().and_then(|c| c.bridge_ip.clone()).or_else(|| cfg.bridge_ip.clone());
        let user = creds.as_ref().map(|c| c.username.clone()).or_else(|| cfg.username.clone());
        (bridge_ip, cfg.api_port, user)
    };

    let (Some(ip), Some(username)) = (ip, username) else {
        return Err(anyhow::anyhow!("Keine Hue Bridge Verbindung konfiguriert"));
    };

    let url = format!("http://{}:{}/api/{}/lights/{}/state", ip, port, username, light_id);
    let resp = state
        .http_client
        .put(&url)
        .timeout(Duration::from_secs(3))
        .json(body)
        .send()
        .await?;

    let text = resp.text().await?;
    info!("💡 Hue Light {} state updated: {}", light_id, text);
    Ok(text)
}

// REST Handlers for Web module interaction
async fn health_handler(State(state): State<AppState>) -> impl IntoResponse {
    let status = state.last_status.lock().await.clone();
    let creds = state.credentials.lock().await;
    let is_paired = creds.is_some() && !creds.as_ref().unwrap().username.is_empty();
    let lights = state.active_lights.lock().await;

    (
        StatusCode::OK,
        Json(json!({
            "status": "ready",
            "module_id": state.module_id,
            "is_paired": is_paired,
            "message": status,
            "lights_count": lights.len(),
            "paired_bridge_ip": creds.as_ref().and_then(|c| c.bridge_ip.clone())
        })),
    )
}

async fn pair_handler(State(state): State<AppState>) -> impl IntoResponse {
    let (ip, port) = {
        let cfg = state.config.lock().await;
        (cfg.bridge_ip.clone().unwrap_or_else(|| "192.168.178.12".to_string()), cfg.api_port)
    };

    info!("Initiating Hue Bridge pairing request to {}:{}", ip, port);
    {
        let mut p = state.pairing_in_progress.lock().await;
        *p = true;
    }
    let res = pair_hue_bridge(&state.http_client, &ip, port).await;
    {
        let mut p = state.pairing_in_progress.lock().await;
        *p = false;
    }
    match res {
        Ok(creds) => {
            info!("🎉 Hue Bridge paired successfully! Username: {}", creds.username);
            let _ = creds.save_to_file(&state.creds_path);
            {
                let mut c = state.credentials.lock().await;
                *c = Some(creds);
            }
            let _ = fetch_and_sync_devices(&state).await;
            (
                StatusCode::OK,
                Json(json!({
                    "status": "ok",
                    "paired": true,
                    "message": "Erfolgreich mit der Philips Hue Bridge gekoppelt!"
                })),
            )
        }
        Err(e) => {
            warn!("Hue pairing attempt: {}", e);
            let is_waiting = e.contains("link button not pressed");
            (
                if is_waiting { StatusCode::ACCEPTED } else { StatusCode::BAD_REQUEST },
                Json(json!({
                    "status": if is_waiting { "waiting_for_button" } else { "error" },
                    "paired": false,
                    "message": if is_waiting {
                        "Bitte drücke jetzt den großen runden Button auf deiner Philips Hue Bridge!"
                    } else {
                        &e
                    }
                })),
            )
        }
    }
}

async fn lights_handler(State(state): State<AppState>) -> impl IntoResponse {
    let lights = state.active_lights.lock().await;
    let list: Vec<HueLightState> = lights.values().cloned().collect();
    (StatusCode::OK, Json(list))
}

async fn light_toggle_handler(
    AxumPath(id): AxumPath<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let current_on = {
        let lights = state.active_lights.lock().await;
        lights.get(&id).map(|l| l.on).unwrap_or(false)
    };

    let target_on = !current_on;
    match set_light_state(&state, &id, &json!({"on": target_on})).await {
        Ok(_) => {
            {
                let mut lights = state.active_lights.lock().await;
                if let Some(l) = lights.get_mut(&id) {
                    l.on = target_on;
                }
            }
            (StatusCode::OK, Json(json!({"status": "ok", "on": target_on})))
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"status": "error", "message": e.to_string()})),
        ),
    }
}

async fn status_handler(State(state): State<AppState>) -> impl IntoResponse {
    let creds = state.credentials.lock().await;
    let cfg = state.config.lock().await;
    let lights = state.active_lights.lock().await;
    let pairing = *state.pairing_in_progress.lock().await;
    (
        StatusCode::OK,
        Json(json!({
            "paired": creds.is_some(),
            "bridge_ip": cfg.bridge_ip,
            "lights_count": lights.len(),
            "pairing_in_progress": pairing,
        })),
    )
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let env = ModuleEnvironment::from_env()?;
    let config = load_config(&env.config_path)?;

    info!(
        "Starting Philips Hue module id={}, bridge_ip={:?}, scan_interval={}s",
        env.module_id, config.bridge_ip, config.scan_interval_secs
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
                    "Philips Hue Interface",
                    env!("CARGO_PKG_VERSION"),
                    ["lighting", "zigbee", "hue", "lights", "smart-home"],
                )),
                initial_health: Some(module_health(
                    env.module_id.clone(),
                    HealthState::Starting,
                    "Starting Philips Hue interface",
                )),
            })
            .await?;
    }

    // Storage for credentials
    let workspace_root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string()));
    let creds_path = workspace_root.join("data").join("hue_credentials.json");
    let initial_creds = HueCredentials::load_from_file(&creds_path).or_else(|| {
        config.username.as_ref().filter(|u| !u.trim().is_empty()).map(|u| HueCredentials {
            username: u.clone(),
            clientkey: None,
            bridge_ip: config.bridge_ip.clone(),
            paired_at: Some(Utc::now().to_rfc3339()),
        })
    });

    let is_paired = initial_creds.is_some();
    let initial_status = if is_paired {
        "Philips Hue verbunden (Schlüssel vorhanden)".to_string()
    } else {
        "Philips Hue bereit (Kopplung erforderlich: Link-Button drücken)".to_string()
    };

    {
        let mut client_guard = client.lock().await;
        client_guard
            .report_health(module_health(
                env.module_id.clone(),
                HealthState::Ready,
                initial_status.clone(),
            ))
            .await?;
    }

    let http_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(4))
        .build()?;

    let app_state = AppState {
        config: Arc::new(Mutex::new(config.clone())),
        credentials: Arc::new(Mutex::new(initial_creds)),
        client: Arc::clone(&client),
        http_client,
        creds_path: creds_path.clone(),
        active_lights: Arc::new(Mutex::new(HashMap::new())),
        module_id: env.module_id.clone(),
        pairing_in_progress: Arc::new(Mutex::new(false)),
        last_status: Arc::new(Mutex::new(initial_status)),
    };

    // Command subscription listener task
    let cmd_socket_path = env.socket_path.clone();
    let cmd_module_id = env.module_id.clone();
    let cmd_state = app_state.clone();

    tokio::spawn(async move {
        loop {
            match connect_control_client(&cmd_socket_path).await {
                Ok(mut rpc) => {
                    match rpc
                        .subscribe_commands(SubscribeCommandsRequest {
                            module_id: cmd_module_id.clone(),
                        })
                        .await
                    {
                        Ok(response) => {
                            let mut stream = response.into_inner();
                            while let Ok(Some(cmd)) = stream.message().await {
                                info!("Philips Hue received command: action={}", cmd.action);
                                match cmd.action.as_str() {
                                    "pair" => {
                                        let (ip, port) = {
                                            let cfg = cmd_state.config.lock().await;
                                            (cfg.bridge_ip.clone().unwrap_or_else(|| "192.168.178.12".to_string()), cfg.api_port)
                                        };
                                        if let Ok(creds) = pair_hue_bridge(&cmd_state.http_client, &ip, port).await {
                                            info!("🎉 Hue Bridge paired via command! Username: {}", creds.username);
                                            let _ = creds.save_to_file(&cmd_state.creds_path);
                                            {
                                                let mut c = cmd_state.credentials.lock().await;
                                                *c = Some(creds);
                                            }
                                            let _ = fetch_and_sync_devices(&cmd_state).await;
                                        }
                                    }
                                    "turn_on" | "turn_off" | "toggle" => {
                                        if let Some(light_id) = cmd.params.get("light_id") {
                                            let target_on = if cmd.action == "turn_on" {
                                                true
                                            } else if cmd.action == "turn_off" {
                                                false
                                            } else {
                                                let lights = cmd_state.active_lights.lock().await;
                                                !lights.get(light_id).map(|l| l.on).unwrap_or(false)
                                            };
                                            let _ = set_light_state(&cmd_state, light_id, &json!({"on": target_on})).await;
                                        }
                                    }
                                    "set_brightness" => {
                                        if let (Some(light_id), Some(bri_str)) = (cmd.params.get("light_id"), cmd.params.get("brightness")) {
                                            if let Ok(bri_val) = bri_str.parse::<u8>() {
                                                let _ = set_light_state(&cmd_state, light_id, &json!({"on": true, "bri": bri_val})).await;
                                            }
                                        }
                                    }
                                    "sync" => {
                                        let _ = fetch_and_sync_devices(&cmd_state).await;
                                    }
                                    _ => {}
                                }
                            }
                        }
                        Err(e) => {
                            error!("Failed to subscribe to supervisor commands: {e}");
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to connect for command subscription: {e}");
                }
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    });

    // Build Axum HTTP Router
    let router = Router::new()
        .route("/health", get(health_handler))
        .route("/api/status", get(status_handler))
        .route("/api/pair", post(pair_handler))
        .route("/api/lights", get(lights_handler))
        .route("/api/lights/:id/toggle", post(light_toggle_handler))
        .with_state(app_state.clone());

    let bind_addr = SocketAddr::from(([0, 0, 0, 0], config.listen_port));
    info!("Binding Philips Hue Interface HTTP Server on http://{}", bind_addr);
    let listener = tokio::net::TcpListener::bind(bind_addr)
        .await
        .with_context(|| format!("failed to bind TCP listener on {}", bind_addr))?;

    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, router).await {
            error!("Philips Hue HTTP server error: {}", e);
        }
    });

    // Initial sync
    let state_clone = app_state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let _ = fetch_and_sync_devices(&state_clone).await;
    });

    // Periodic polling loop
    let mut interval = tokio::time::interval(Duration::from_secs(config.scan_interval_secs));
    loop {
        interval.tick().await;

        let has_creds = {
            let creds = app_state.credentials.lock().await;
            creds.is_some()
        };

        if has_creds {
            let _ = fetch_and_sync_devices(&app_state).await;
        } else {
            // Attempt automatic background pairing if bridge is reachable
            let (bridge_ip, port) = {
                let cfg = app_state.config.lock().await;
                (cfg.bridge_ip.clone().unwrap_or_else(|| "192.168.178.12".to_string()), cfg.api_port)
            };

            match pair_hue_bridge(&app_state.http_client, &bridge_ip, port).await {
                Ok(creds) => {
                    info!("🎉 Hue Bridge paired successfully! Username: {}", creds.username);
                    let _ = creds.save_to_file(&app_state.creds_path);
                    {
                        let mut c = app_state.credentials.lock().await;
                        *c = Some(creds);
                    }
                    let _ = fetch_and_sync_devices(&app_state).await;
                }
                Err(e) => {
                    let msg = if e.contains("link button not pressed") {
                        "Warte auf Hue Bridge Link-Button (Großen runden Knopf auf der Bridge drücken)".to_string()
                    } else {
                        format!("Hue Bridge Verbindungsversuch: {}", e)
                    };
                    let mut s = app_state.last_status.lock().await;
                    *s = msg.clone();
                    let mut client_guard = app_state.client.lock().await;
                    let _ = client_guard.report_health(module_health(
                        app_state.module_id.clone(),
                        HealthState::Ready,
                        msg,
                    )).await;
                }
            }
        }
    }
}
