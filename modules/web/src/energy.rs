use std::net::SocketAddr;
use std::time::Duration;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PhaseMetric {
    pub voltage: f64,
    pub current: f64,
    pub act_power: f64,
    pub aprt_power: f64,
    pub pf: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Shelly3EmData {
    pub total_act_power: f64,
    pub total_aprt_power: f64,
    pub total_current: f64,
    pub total_import_kwh: f64,
    pub total_export_kwh: f64,
    pub phase_a: PhaseMetric,
    pub phase_b: PhaseMetric,
    pub phase_c: PhaseMetric,
    pub online: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FroniusSolarData {
    pub pv_power_w: f64,
    pub energy_day_kwh: f64,
    pub energy_year_kwh: f64,
    pub energy_total_kwh: f64,
    pub grid_power_w: Option<f64>,
    pub load_power_w: Option<f64>,
    pub rel_autonomy: Option<f64>,
    pub rel_self_consumption: Option<f64>,
    pub online: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BatteryInfo {
    pub name: String,
    pub hostname: String,
    pub ip: String,
    pub soc_pct: Option<f64>,
    pub power_w: Option<f64>,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EnergyLiveSnapshot {
    pub solar_power_w: f64,
    pub solar_day_kwh: f64,
    pub solar_year_kwh: f64,
    pub solar_total_kwh: f64,
    pub grid_power_w: f64,
    pub is_exporting: bool,
    pub grid_import_kwh: f64,
    pub grid_export_kwh: f64,
    pub house_consumption_w: f64,
    pub autarky_pct: f64,
    pub self_consumption_pct: f64,
    pub fronius: FroniusSolarData,
    pub shelly: Shelly3EmData,
    pub batteries: Vec<BatteryInfo>,
    pub timestamp: String,
    pub fronius_host: String,
    pub shelly_host: String,
}

/// Generic lightweight HTTP/1.1 GET helper over async TcpStream
pub async fn http_get_json(host_or_ip: &str, port: u16, path: &str, timeout_ms: u64) -> Result<String, String> {
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host(format!("{}:{}", host_or_ip, port))
        .await
        .map_err(|e| format!("DNS lookup failed for {host_or_ip}: {e}"))?
        .collect();

    if addrs.is_empty() {
        return Err(format!("No address found for {host_or_ip}"));
    }

    let addr = addrs[0];
    let mut stream = tokio::time::timeout(Duration::from_millis(timeout_ms), TcpStream::connect(addr))
        .await
        .map_err(|_| format!("Connection timeout to {host_or_ip}:{port}"))?
        .map_err(|e| format!("Connect error to {host_or_ip}:{port}: {e}"))?;

    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host_or_ip}\r\nUser-Agent: HomeNodeEnergy/1.0\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
    );

    stream
        .write_all(req.as_bytes())
        .await
        .map_err(|e| format!("Failed to write request to {host_or_ip}: {e}"))?;

    let mut response_bytes = Vec::with_capacity(16384);
    let mut buf = [0u8; 4096];

    loop {
        let read_result = tokio::time::timeout(Duration::from_millis(timeout_ms), stream.read(&mut buf)).await;
        match read_result {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => response_bytes.extend_from_slice(&buf[..n]),
            Ok(Err(e)) => return Err(format!("Read error from {host_or_ip}: {e}")),
            Err(_) => return Err(format!("Read timeout from {host_or_ip}")),
        }
    }

    let text = String::from_utf8_lossy(&response_bytes).to_string();
    if let Some(pos) = text.find("\r\n\r\n") {
        let body = &text[pos + 4..];
        Ok(body.to_string())
    } else if let Some(pos) = text.find("\n\n") {
        let body = &text[pos + 2..];
        Ok(body.to_string())
    } else {
        Ok(text)
    }
}

/// Fetch real-time solar generation from Fronius Inverter
pub async fn fetch_fronius_live(host: &str) -> FroniusSolarData {
    let path = "/solar_api/v1/GetPowerFlowRealtimeData.fcgi";
    match http_get_json(host, 80, path, 1500).await {
        Ok(body) => parse_fronius_powerflow(&body),
        Err(err) => FroniusSolarData {
            online: false,
            error: Some(err),
            ..Default::default()
        },
    }
}

/// Parse Fronius Solar API GetPowerFlowRealtimeData JSON
pub fn parse_fronius_powerflow(raw_json: &str) -> FroniusSolarData {
    let v: serde_json::Value = match serde_json::from_str(raw_json) {
        Ok(val) => val,
        Err(e) => {
            return FroniusSolarData {
                online: false,
                error: Some(format!("Invalid Fronius JSON: {e}")),
                ..Default::default()
            };
        }
    };

    let site = &v["Body"]["Data"]["Site"];
    let inverters = &v["Body"]["Data"]["Inverters"];

    // P_PV is live solar in Watts
    let pv_power = site["P_PV"]
        .as_f64()
        .or_else(|| inverters["1"]["P"].as_f64())
        .unwrap_or(0.0);

    let e_day = site["E_Day"]
        .as_f64()
        .or_else(|| inverters["1"]["E_Day"].as_f64())
        .unwrap_or(0.0)
        / 1000.0;

    let e_year = site["E_Year"]
        .as_f64()
        .or_else(|| inverters["1"]["E_Year"].as_f64())
        .unwrap_or(0.0)
        / 1000.0;

    let e_total = site["E_Total"]
        .as_f64()
        .or_else(|| inverters["1"]["E_Total"].as_f64())
        .unwrap_or(0.0)
        / 1000.0;

    let p_grid = site["P_Grid"].as_f64();
    let p_load = site["P_Load"].as_f64();
    let rel_autonomy = site["rel_Autonomy"].as_f64();
    let rel_self_consumption = site["rel_SelfConsumption"].as_f64();

    FroniusSolarData {
        pv_power_w: if pv_power < 0.0 { 0.0 } else { pv_power },
        energy_day_kwh: (e_day * 100.0).round() / 100.0,
        energy_year_kwh: (e_year * 10.0).round() / 10.0,
        energy_total_kwh: (e_total * 10.0).round() / 10.0,
        grid_power_w: p_grid,
        load_power_w: p_load,
        rel_autonomy,
        rel_self_consumption,
        online: true,
        error: None,
    }
}

/// Fetch real-time 3-phase grid power from Shelly Pro 3EM
pub async fn fetch_shelly_3em_live(host: &str) -> Shelly3EmData {
    let status_res = http_get_json(host, 80, "/rpc/EM.GetStatus?id=0", 1500).await;
    let data_res = http_get_json(host, 80, "/rpc/EMData.GetStatus?id=0", 1500).await;

    match (status_res, data_res) {
        (Ok(status_body), Ok(data_body)) => parse_shelly_3em(&status_body, Some(&data_body)),
        (Ok(status_body), Err(_)) => parse_shelly_3em(&status_body, None),
        (Err(err), _) => Shelly3EmData {
            online: false,
            error: Some(err),
            ..Default::default()
        },
    }
}

/// Parse Shelly Pro 3EM EM.GetStatus and EMData.GetStatus JSON
pub fn parse_shelly_3em(status_json: &str, data_json: Option<&str>) -> Shelly3EmData {
    let s: serde_json::Value = match serde_json::from_str(status_json) {
        Ok(val) => val,
        Err(e) => {
            return Shelly3EmData {
                online: false,
                error: Some(format!("Invalid Shelly Status JSON: {e}")),
                ..Default::default()
            };
        }
    };

    let total_act = s["total_act_power"].as_f64().unwrap_or(0.0);
    let total_aprt = s["total_aprt_power"].as_f64().unwrap_or(0.0);
    let total_curr = s["total_current"].as_f64().unwrap_or(0.0);

    let phase_a = PhaseMetric {
        voltage: (s["a_voltage"].as_f64().unwrap_or(0.0) * 10.0).round() / 10.0,
        current: (s["a_current"].as_f64().unwrap_or(0.0) * 100.0).round() / 100.0,
        act_power: (s["a_act_power"].as_f64().unwrap_or(0.0) * 10.0).round() / 10.0,
        aprt_power: (s["a_aprt_power"].as_f64().unwrap_or(0.0) * 10.0).round() / 10.0,
        pf: s["a_pf"].as_f64().unwrap_or(0.0),
    };

    let phase_b = PhaseMetric {
        voltage: (s["b_voltage"].as_f64().unwrap_or(0.0) * 10.0).round() / 10.0,
        current: (s["b_current"].as_f64().unwrap_or(0.0) * 100.0).round() / 100.0,
        act_power: (s["b_act_power"].as_f64().unwrap_or(0.0) * 10.0).round() / 10.0,
        aprt_power: (s["b_aprt_power"].as_f64().unwrap_or(0.0) * 10.0).round() / 10.0,
        pf: s["b_pf"].as_f64().unwrap_or(0.0),
    };

    let phase_c = PhaseMetric {
        voltage: (s["c_voltage"].as_f64().unwrap_or(0.0) * 10.0).round() / 10.0,
        current: (s["c_current"].as_f64().unwrap_or(0.0) * 100.0).round() / 100.0,
        act_power: (s["c_act_power"].as_f64().unwrap_or(0.0) * 10.0).round() / 10.0,
        aprt_power: (s["c_aprt_power"].as_f64().unwrap_or(0.0) * 10.0).round() / 10.0,
        pf: s["c_pf"].as_f64().unwrap_or(0.0),
    };

    let mut import_kwh = 0.0;
    let mut export_kwh = 0.0;

    if let Some(dj) = data_json {
        if let Ok(d) = serde_json::from_str::<serde_json::Value>(dj) {
            import_kwh = (d["total_act"].as_f64().unwrap_or(0.0) / 1000.0 * 10.0).round() / 10.0;
            export_kwh = (d["total_act_ret"].as_f64().unwrap_or(0.0) / 1000.0 * 10.0).round() / 10.0;
        }
    }

    Shelly3EmData {
        total_act_power: (total_act * 10.0).round() / 10.0,
        total_aprt_power: (total_aprt * 10.0).round() / 10.0,
        total_current: (total_curr * 100.0).round() / 100.0,
        total_import_kwh: import_kwh,
        total_export_kwh: export_kwh,
        phase_a,
        phase_b,
        phase_c,
        online: true,
        error: None,
    }
}

/// Collect full energy snapshot by querying live devices concurrently
pub async fn collect_energy_snapshot(
    fronius_host: &str,
    shelly_host: &str,
    batteries: Vec<BatteryInfo>,
) -> EnergyLiveSnapshot {
    let (fronius, shelly) = tokio::join!(
        fetch_fronius_live(fronius_host),
        fetch_shelly_3em_live(shelly_host)
    );

    let solar_w = if fronius.online { fronius.pv_power_w } else { 0.0 };
    let grid_w = if shelly.online { shelly.total_act_power } else { 0.0 };
    let is_exporting = grid_w < -1.0;

    // Power balance: House Load = Solar Power + Grid Power
    // Note: When exporting, grid_w is negative, so house load is solar - abs(grid_w)
    let house_consumption_w = (solar_w + grid_w).max(0.0);

    // Autarky: How much of current home consumption is covered by solar
    let autarky_pct = if house_consumption_w > 1.0 {
        let solar_used = solar_w.min(house_consumption_w);
        ((solar_used / house_consumption_w) * 100.0).clamp(0.0, 100.0)
    } else {
        if solar_w > 0.0 { 100.0 } else { 0.0 }
    };

    // Self-consumption: How much of current solar production is consumed on-site
    let self_consumption_pct = if solar_w > 1.0 {
        let consumed = solar_w.min(house_consumption_w);
        ((consumed / solar_w) * 100.0).clamp(0.0, 100.0)
    } else {
        0.0
    };

    let now_str = chrono::Local::now().format("%H:%M:%S").to_string();

    EnergyLiveSnapshot {
        solar_power_w: (solar_w * 10.0).round() / 10.0,
        solar_day_kwh: fronius.energy_day_kwh,
        solar_year_kwh: fronius.energy_year_kwh,
        solar_total_kwh: fronius.energy_total_kwh,
        grid_power_w: (grid_w * 10.0).round() / 10.0,
        is_exporting,
        grid_import_kwh: shelly.total_import_kwh,
        grid_export_kwh: shelly.total_export_kwh,
        house_consumption_w: (house_consumption_w * 10.0).round() / 10.0,
        autarky_pct: (autarky_pct * 10.0).round() / 10.0,
        self_consumption_pct: (self_consumption_pct * 10.0).round() / 10.0,
        fronius,
        shelly,
        batteries,
        timestamp: now_str,
        fronius_host: fronius_host.to_string(),
        shelly_host: shelly_host.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fronius_payload() {
        let raw = r#"{
            "Body": {
                "Data": {
                    "Inverters": {
                        "1": {
                            "DT": 105,
                            "E_Day": 22029,
                            "E_Total": 67127400,
                            "E_Year": 5354226.5,
                            "P": 1450.0
                        }
                    },
                    "Site": {
                        "E_Day": 22029,
                        "E_Total": 67127400,
                        "E_Year": 5354226.5,
                        "P_PV": 1450.0,
                        "P_Grid": -850.0,
                        "P_Load": -600.0,
                        "rel_Autonomy": 100.0
                    }
                }
            }
        }"#;

        let parsed = parse_fronius_powerflow(raw);
        assert!(parsed.online);
        assert_eq!(parsed.pv_power_w, 1450.0);
        assert_eq!(parsed.energy_day_kwh, 22.03);
        assert_eq!(parsed.energy_total_kwh, 67127.4);
    }

    #[test]
    fn parses_shelly_3em_payloads() {
        let status = r#"{
            "id": 0,
            "a_current": 1.988,
            "a_voltage": 234.1,
            "a_act_power": -77.8,
            "a_aprt_power": 465.5,
            "a_pf": 0.17,
            "b_current": 0.588,
            "b_voltage": 234.5,
            "b_act_power": 30.6,
            "b_aprt_power": 138.0,
            "b_pf": 0.23,
            "c_current": 0.608,
            "c_voltage": 234.9,
            "c_act_power": 50.8,
            "c_aprt_power": 142.9,
            "c_pf": 0.36,
            "total_current": 3.185,
            "total_act_power": 3.566,
            "total_aprt_power": 746.368
        }"#;

        let data = r#"{
            "id": 0,
            "total_act": 7423133.34,
            "total_act_ret": 4692575.75
        }"#;

        let parsed = parse_shelly_3em(status, Some(data));
        assert!(parsed.online);
        assert_eq!(parsed.total_act_power, 3.6);
        assert_eq!(parsed.phase_a.act_power, -77.8);
        assert_eq!(parsed.phase_a.voltage, 234.1);
        assert_eq!(parsed.total_import_kwh, 7423.1);
        assert_eq!(parsed.total_export_kwh, 4692.6);
    }
}
