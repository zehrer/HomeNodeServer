use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::RwLock;
use tracing::{info, warn};

pub const GOVEE_DISCOVERY_PORT: u16 = 4001;
pub const GOVEE_RECEIVE_PORT: u16 = 4002;
pub const GOVEE_CONTROL_PORT: u16 = 4003;
pub const GOVEE_MULTICAST_ADDR: &str = "239.255.255.250";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoveeDeviceState {
    pub ip: String,
    pub device: String, // MAC or Govee Hardware ID
    pub sku: String,    // e.g. "H70B3", "H70B5"
    pub on_off: bool,
    pub brightness: u8,
    pub color: (u8, u8, u8),
    pub color_tem_kelvin: u32,
    pub last_seen: String,
}

#[derive(Clone)]
pub struct GoveeManager {
    devices: Arc<RwLock<HashMap<String, GoveeDeviceState>>>,
    sender: Arc<UdpSocket>,
}

impl GoveeManager {
    pub async fn new() -> Result<Self> {
        let sender = UdpSocket::bind("0.0.0.0:0").await?;
        sender.set_broadcast(true)?;

        let devices = Arc::new(RwLock::new(HashMap::new()));
        let manager = Self {
            devices: devices.clone(),
            sender: Arc::new(sender),
        };

        // Start UDP listener on port 4002 to receive scan responses and devStatus packets
        let listener_devices = devices.clone();
        tokio::spawn(async move {
            match UdpSocket::bind(format!("0.0.0.0:{GOVEE_RECEIVE_PORT}")).await {
                Ok(listener) => {
                    info!("Govee UDP listener active on port {GOVEE_RECEIVE_PORT}");
                    let mut buf = [0u8; 4096];
                    loop {
                        match listener.recv_from(&mut buf).await {
                            Ok((len, remote_addr)) => {
                                if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&buf[..len]) {
                                    handle_incoming_packet(&json, remote_addr.ip().to_string(), &listener_devices).await;
                                }
                            }
                            Err(err) => {
                                warn!("Error receiving Govee UDP packet: {err}");
                                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                            }
                        }
                    }
                }
                Err(err) => {
                    warn!("Could not bind Govee UDP listener on port {GOVEE_RECEIVE_PORT}: {err}");
                }
            }
        });

        Ok(manager)
    }

    pub async fn scan(&self, candidate_ips: &[String]) {
        let scan_msg = serde_json::json!({
            "msg": {
                "cmd": "scan",
                "data": {
                    "account_topic": "reserve"
                }
            }
        });
        let bytes = scan_msg.to_string().into_bytes();

        // Broadcast to multicast group
        if let Ok(mcast_target) = format!("{GOVEE_MULTICAST_ADDR}:{GOVEE_DISCOVERY_PORT}").parse::<SocketAddr>() {
            let _ = self.sender.send_to(&bytes, mcast_target).await;
        }

        // Also unicast to known Govee candidate IPs
        for ip in candidate_ips {
            if let Ok(addr) = format!("{ip}:{GOVEE_DISCOVERY_PORT}").parse::<SocketAddr>() {
                let _ = self.sender.send_to(&bytes, addr).await;
            }
        }
    }

    pub async fn query_status(&self, ip: &str) -> Result<()> {
        let msg = serde_json::json!({
            "msg": {
                "cmd": "devStatus",
                "data": {}
            }
        });
        let target: SocketAddr = format!("{ip}:{GOVEE_CONTROL_PORT}").parse()?;
        self.sender.send_to(msg.to_string().as_bytes(), target).await?;
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        Ok(())
    }

    pub async fn set_power(&self, ip: &str, on: bool) -> Result<()> {
        let val = if on { 1 } else { 0 };
        let msg = serde_json::json!({
            "msg": {
                "cmd": "turn",
                "data": {
                    "value": val
                }
            }
        });
        let target: SocketAddr = format!("{ip}:{GOVEE_CONTROL_PORT}").parse()?;
        self.sender.send_to(msg.to_string().as_bytes(), target).await?;

        // Optimistically update cache
        {
            let mut devs = self.devices.write().await;
            if let Some(dev) = devs.get_mut(ip) {
                dev.on_off = on;
                dev.last_seen = chrono::Utc::now().to_rfc3339();
            } else {
                devs.insert(
                    ip.to_string(),
                    GoveeDeviceState {
                        ip: ip.to_string(),
                        device: String::new(),
                        sku: "Govee Light".to_string(),
                        on_off: on,
                        brightness: 100,
                        color: (255, 255, 255),
                        color_tem_kelvin: 0,
                        last_seen: chrono::Utc::now().to_rfc3339(),
                    },
                );
            }
        }

        // Trigger fresh status confirmation
        let _ = self.query_status(ip).await;
        Ok(())
    }

    pub async fn set_brightness(&self, ip: &str, brightness: u8) -> Result<()> {
        let b = brightness.clamp(1, 100);
        let msg = serde_json::json!({
            "msg": {
                "cmd": "brightness",
                "data": {
                    "value": b
                }
            }
        });
        let target: SocketAddr = format!("{ip}:{GOVEE_CONTROL_PORT}").parse()?;
        self.sender.send_to(msg.to_string().as_bytes(), target).await?;

        // Optimistically update cache
        {
            let mut devs = self.devices.write().await;
            if let Some(dev) = devs.get_mut(ip) {
                dev.brightness = b;
                dev.on_off = true;
                dev.last_seen = chrono::Utc::now().to_rfc3339();
            }
        }

        let _ = self.query_status(ip).await;
        Ok(())
    }

    pub async fn set_color(&self, ip: &str, r: u8, g: u8, b: u8) -> Result<()> {
        let msg = serde_json::json!({
            "msg": {
                "cmd": "colorwc",
                "data": {
                    "color": {
                        "r": r,
                        "g": g,
                        "b": b
                    },
                    "colorTemInKelvin": 0
                }
            }
        });
        let target: SocketAddr = format!("{ip}:{GOVEE_CONTROL_PORT}").parse()?;
        self.sender.send_to(msg.to_string().as_bytes(), target).await?;

        // Optimistically update cache
        {
            let mut devs = self.devices.write().await;
            if let Some(dev) = devs.get_mut(ip) {
                dev.color = (r, g, b);
                dev.color_tem_kelvin = 0;
                dev.on_off = true;
                dev.last_seen = chrono::Utc::now().to_rfc3339();
            }
        }

        let _ = self.query_status(ip).await;
        Ok(())
    }

    pub async fn set_color_temp(&self, ip: &str, kelvin: u32) -> Result<()> {
        let k = kelvin.clamp(2000, 9000);
        let msg = serde_json::json!({
            "msg": {
                "cmd": "colorwc",
                "data": {
                    "color": {
                        "r": 0,
                        "g": 0,
                        "b": 0
                    },
                    "colorTemInKelvin": k
                }
            }
        });
        let target: SocketAddr = format!("{ip}:{GOVEE_CONTROL_PORT}").parse()?;
        self.sender.send_to(msg.to_string().as_bytes(), target).await?;

        // Optimistically update cache
        {
            let mut devs = self.devices.write().await;
            if let Some(dev) = devs.get_mut(ip) {
                dev.color_tem_kelvin = k;
                dev.on_off = true;
                dev.last_seen = chrono::Utc::now().to_rfc3339();
            }
        }

        let _ = self.query_status(ip).await;
        Ok(())
    }

    pub async fn get_device(&self, ip: &str) -> Option<GoveeDeviceState> {
        let devs = self.devices.read().await;
        devs.get(ip).cloned()
    }

    pub async fn list_devices(&self) -> Vec<GoveeDeviceState> {
        let devs = self.devices.read().await;
        devs.values().cloned().collect()
    }
}

async fn handle_incoming_packet(
    json: &serde_json::Value,
    remote_ip: String,
    devices: &Arc<RwLock<HashMap<String, GoveeDeviceState>>>,
) {
    let msg = match json.get("msg") {
        Some(m) => m,
        None => return,
    };
    let cmd = msg.get("cmd").and_then(|c| c.as_str()).unwrap_or("");
    let data = match msg.get("data") {
        Some(d) => d,
        None => return,
    };

    let now_str = chrono::Utc::now().to_rfc3339();

    if cmd == "scan" {
        let ip = data.get("ip").and_then(|i| i.as_str()).unwrap_or(&remote_ip).to_string();
        let device = data.get("device").and_then(|d| d.as_str()).unwrap_or("").to_string();
        let sku = data.get("sku").and_then(|s| s.as_str()).unwrap_or("Govee Light").to_string();

        let mut devs = devices.write().await;
        let entry = devs.entry(ip.clone()).or_insert_with(|| GoveeDeviceState {
            ip: ip.clone(),
            device: device.clone(),
            sku: sku.clone(),
            on_off: false,
            brightness: 100,
            color: (255, 255, 255),
            color_tem_kelvin: 0,
            last_seen: now_str.clone(),
        });
        entry.device = device;
        entry.sku = sku;
        entry.last_seen = now_str;
    } else if cmd == "devStatus" {
        let ip = remote_ip;
        let on_off = data.get("onOff").and_then(|o| o.as_i64()).unwrap_or(0) == 1;
        let brightness = data.get("brightness").and_then(|b| b.as_u64()).unwrap_or(100) as u8;
        let r = data.get("color").and_then(|c| c.get("r")).and_then(|v| v.as_u64()).unwrap_or(255) as u8;
        let g = data.get("color").and_then(|c| c.get("g")).and_then(|v| v.as_u64()).unwrap_or(255) as u8;
        let b = data.get("color").and_then(|c| c.get("b")).and_then(|v| v.as_u64()).unwrap_or(255) as u8;
        let kelvin = data.get("colorTemInKelvin").and_then(|k| k.as_u64()).unwrap_or(0) as u32;

        let mut devs = devices.write().await;
        let entry = devs.entry(ip.clone()).or_insert_with(|| GoveeDeviceState {
            ip: ip.clone(),
            device: String::new(),
            sku: "Govee Light".to_string(),
            on_off,
            brightness,
            color: (r, g, b),
            color_tem_kelvin: kelvin,
            last_seen: now_str.clone(),
        });
        entry.on_off = on_off;
        entry.brightness = brightness;
        entry.color = (r, g, b);
        entry.color_tem_kelvin = kelvin;
        entry.last_seen = now_str;
    }
}
