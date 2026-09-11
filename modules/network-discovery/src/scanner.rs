use std::collections::{HashMap, HashSet};
use std::fs;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use dns_lookup::lookup_addr;
use if_addrs::get_if_addrs;
use ipnet::Ipv4Net;
use serde::{Deserialize, Serialize};
use surge_ping::{Client, Config, PingIdentifier, PingSequence, ICMP};
use tokio::net::TcpStream;
use tokio::process::Command;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tracing::{debug, info};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredDevice {
    pub device_id: String,
    pub display_name: String,
    pub kind: String,
    pub ip: String,
    pub mac: Option<String>,
    pub hostname: Option<String>,
    pub interface: String,
    pub vendor: Option<String>,
    pub capabilities: Vec<String>,
    pub source: String,
}

#[derive(Debug, Clone)]
pub struct ScannerConfig {
    pub max_active_targets: usize,
    pub enable_active_icmp: bool,
    pub interface_allowlist: Vec<String>,
    pub interface_denylist: Vec<String>,
}

impl Default for ScannerConfig {
    fn default() -> Self {
        Self {
            max_active_targets: 256,
            enable_active_icmp: true,
            interface_allowlist: Vec::new(),
            interface_denylist: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
struct NetworkInterface {
    #[allow(dead_code)]
    name: String,
    ipv4_addrs: Vec<Ipv4Addr>,
    ipv4_subnets: Vec<Ipv4Net>,
}

#[derive(Debug, Clone)]
pub struct RawObservation {
    pub ip: String,
    pub mac: Option<String>,
    pub interface: String,
    pub hostname: Option<String>,
    pub source: String,
}

pub struct NetworkScanner {
    config: ScannerConfig,
}

impl NetworkScanner {
    pub fn new(config: ScannerConfig) -> Self {
        Self { config }
    }

    pub async fn scan(&self) -> Result<Vec<DiscoveredDevice>> {
        let interfaces = self.discover_interfaces()?;
        let mut observations = Vec::new();

        // 1. Passive ARP collection (Linux /proc/net/arp and macOS arp -an)
        observations.extend(collect_from_arp());

        // 2. Passive neighbor discovery (ip neigh on Linux)
        observations.extend(collect_from_ip_neigh().await);

        // 3. Active subnet ICMP probes
        if self.config.enable_active_icmp {
            let active = self.run_active_sweep(&interfaces, &observations).await;
            observations.extend(active);
        }

        // 4. mDNS hints (avahi-browse or dns-sd)
        observations.extend(collect_from_mdns().await);

        // 5. Reverse DNS hostname enrichment
        enrich_hostnames(&mut observations).await;

        // 6. Aggregate by IP/MAC and classify devices
        let devices = aggregate_and_classify(observations);
        info!("Network scan completed: found {} unique devices", devices.len());

        Ok(devices)
    }

    fn discover_interfaces(&self) -> Result<Vec<NetworkInterface>> {
        let mut grouped: HashMap<String, NetworkInterface> = HashMap::new();

        for iface in get_if_addrs().context("failed to enumerate network interfaces")? {
            if iface.is_loopback() {
                continue;
            }

            let name = iface.name;
            if !self.config.interface_allowlist.is_empty()
                && !self.config.interface_allowlist.iter().any(|allowed| allowed == &name)
            {
                continue;
            }
            if self.config.interface_denylist.iter().any(|denied| denied == &name) {
                continue;
            }

            let entry = grouped.entry(name.clone()).or_insert_with(|| NetworkInterface {
                name: name.clone(),
                ipv4_addrs: Vec::new(),
                ipv4_subnets: Vec::new(),
            });

            if let if_addrs::IfAddr::V4(v4) = iface.addr {
                entry.ipv4_addrs.push(v4.ip);
                if let Ok(net) = Ipv4Net::with_netmask(v4.ip, v4.netmask) {
                    if net.prefix_len() >= 16 && net.prefix_len() <= 30 {
                        entry.ipv4_subnets.push(net);
                    }
                }
            }
        }

        Ok(grouped.into_values().collect())
    }

    async fn run_active_sweep(
        &self,
        interfaces: &[NetworkInterface],
        known_observations: &[RawObservation],
    ) -> Vec<RawObservation> {
        let mut target_ips = HashSet::new();
        let known_ips: HashSet<&str> = known_observations.iter().map(|o| o.ip.as_str()).collect();

        for iface in interfaces {
            for subnet in &iface.ipv4_subnets {
                for host in subnet.hosts() {
                    target_ips.insert(host);
                    if target_ips.len() >= self.config.max_active_targets {
                        break;
                    }
                }
                if target_ips.len() >= self.config.max_active_targets {
                    break;
                }
            }
        }

        let ping_client = match build_ping_client() {
            Ok(client) => Some(Arc::new(client)),
            Err(err) => {
                debug!("Raw ICMP ping client unavailable (will use TCP probe fallback): {err}");
                None
            }
        };

        let semaphore = Arc::new(Semaphore::new(64));
        let mut join_set = JoinSet::new();

        for (idx, target) in target_ips.into_iter().enumerate() {
            let target_str = target.to_string();
            let was_known = known_ips.contains(target_str.as_str());
            let client = ping_client.clone();
            let sem = semaphore.clone();

            join_set.spawn(async move {
                let _permit = sem.acquire().await.ok()?;

                // Try ICMP first if client available
                if let Some(client) = client {
                    if probe_icmp(&client, target, idx).await {
                        return Some(target_str);
                    }
                }

                // If not already known, fallback to fast TCP port probes
                if !was_known && probe_tcp_ports(target).await {
                    return Some(target_str);
                }

                None
            });
        }

        let mut discovered = Vec::new();
        while let Some(res) = join_set.join_next().await {
            if let Ok(Some(ip)) = res {
                discovered.push(RawObservation {
                    ip,
                    mac: None,
                    interface: "lan".to_string(),
                    hostname: None,
                    source: "active-probe".to_string(),
                });
            }
        }

        discovered
    }
}

fn collect_from_arp() -> Vec<RawObservation> {
    let mut out = Vec::new();

    // Linux: /proc/net/arp
    let arp_path = Path::new("/proc/net/arp");
    if arp_path.exists() {
        if let Ok(content) = fs::read_to_string(arp_path) {
            for line in content.lines().skip(1) {
                let fields = line.split_whitespace().collect::<Vec<_>>();
                if fields.len() >= 6 {
                    let ip = fields[0];
                    let mac = fields[3];
                    let iface = fields[5];
                    if mac != "00:00:00:00:00:00" && !mac.is_empty() {
                        out.push(RawObservation {
                            ip: ip.to_string(),
                            mac: Some(mac.to_lowercase()),
                            interface: iface.to_string(),
                            hostname: None,
                            source: "arp".to_string(),
                        });
                    }
                }
            }
        }
    }

    // macOS fallback: arp -an
    if out.is_empty() {
        if let Ok(output) = std::process::Command::new("arp").arg("-an").output() {
            if output.status.success() {
                let text = String::from_utf8_lossy(&output.stdout);
                out.extend(parse_macos_arp(&text));
            }
        }
    }

    out
}

pub fn parse_macos_arp(output: &str) -> Vec<RawObservation> {
    let mut out = Vec::new();
    // format: ? (192.168.178.1) at b4:fc:7d:53:b2:89 on en0 ifscope [ethernet]
    for line in output.lines() {
        let line = line.trim();
        if !line.starts_with('?') {
            continue;
        }

        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 6 {
            continue;
        }

        let ip = parts[1].trim_matches(|c| c == '(' || c == ')');
        let mac = parts[3];
        let on_idx = parts.iter().position(|&p| p == "on");
        let iface = on_idx
            .and_then(|idx| parts.get(idx + 1))
            .unwrap_or(&"lan");

        if mac.contains(':') && mac != "ff:ff:ff:ff:ff:ff" && !mac.contains("incomplete") {
            out.push(RawObservation {
                ip: ip.to_string(),
                mac: Some(normalize_mac(mac)),
                interface: iface.to_string(),
                hostname: None,
                source: "arp".to_string(),
            });
        }
    }
    out
}

async fn collect_from_ip_neigh() -> Vec<RawObservation> {
    let mut out = Vec::new();
    let output = match Command::new("ip").args(["neigh", "show"]).output().await {
        Ok(output) if output.status.success() => output,
        _ => return out,
    };

    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        let parts = line.split_whitespace().collect::<Vec<_>>();
        if parts.len() < 3 {
            continue;
        }

        let ip = parts[0];
        let mut iface = "lan".to_string();
        let mut mac = None;

        for idx in 0..parts.len() {
            if parts[idx] == "dev" {
                if let Some(val) = parts.get(idx + 1) {
                    iface = (*val).to_string();
                }
            }
            if parts[idx] == "lladdr" {
                if let Some(val) = parts.get(idx + 1) {
                    mac = Some(normalize_mac(val));
                }
            }
        }

        if let Some(mac) = mac {
            out.push(RawObservation {
                ip: ip.to_string(),
                mac: Some(mac),
                interface: iface,
                hostname: None,
                source: "neigh".to_string(),
            });
        }
    }

    out
}

async fn collect_from_mdns() -> Vec<RawObservation> {
    let mut out = Vec::new();

    // Linux: avahi-browse -atrp
    let avahi_output = Command::new("avahi-browse")
        .args(["-atrp", "-t"])
        .output()
        .await;

    if let Ok(output) = avahi_output {
        if output.status.success() {
            let text = String::from_utf8_lossy(&output.stdout);
            for line in text.lines() {
                if !line.starts_with('=') {
                    continue;
                }
                let fields: Vec<&str> = line.split(';').collect();
                if fields.len() >= 8 {
                    let iface = fields[1].to_string();
                    let hostname = fields.get(6).or_else(|| fields.get(3)).map(|s| s.to_string());
                    let addr = fields[7];
                    if addr.parse::<Ipv4Addr>().is_ok() {
                        out.push(RawObservation {
                            ip: addr.to_string(),
                            mac: None,
                            interface: iface,
                            hostname,
                            source: "mdns".to_string(),
                        });
                    }
                }
            }
        }
    }

    out
}

async fn enrich_hostnames(observations: &mut [RawObservation]) {
    let mut targets = HashMap::new();
    for obs in observations.iter() {
        if obs.hostname.is_none() {
            if let Ok(ip) = obs.ip.parse::<IpAddr>() {
                targets.entry(obs.ip.clone()).or_insert(ip);
            }
        }
    }

    let mut join_set = JoinSet::new();
    for (ip_str, ip_addr) in targets {
        join_set.spawn(async move {
            let resolved = tokio::time::timeout(Duration::from_millis(500), async move {
                tokio::task::spawn_blocking(move || lookup_addr(&ip_addr))
                    .await
                    .ok()
                    .and_then(|r| r.ok())
            })
            .await
            .ok()
            .flatten();

            (ip_str, resolved)
        });
    }

    let mut hostnames = HashMap::new();
    while let Some(res) = join_set.join_next().await {
        if let Ok((ip, Some(hostname))) = res {
            hostnames.insert(ip, hostname);
        }
    }

    for obs in observations.iter_mut() {
        if obs.hostname.is_none() {
            if let Some(host) = hostnames.get(&obs.ip) {
                obs.hostname = Some(host.clone());
            }
        }
    }
}

fn aggregate_and_classify(observations: Vec<RawObservation>) -> Vec<DiscoveredDevice> {
    let mut by_ip: HashMap<String, DiscoveredDevice> = HashMap::new();

    for obs in observations {
        let entry = by_ip.entry(obs.ip.clone()).or_insert_with(|| {
            let device_id = format!("net-{}", obs.ip.replace('.', "-"));
            let display_name = obs
                .hostname
                .clone()
                .unwrap_or_else(|| format!("Host {}", obs.ip));
            let kind = classify_device(&display_name, &obs.ip, obs.mac.as_deref());
            let vendor = obs.mac.as_deref().and_then(guess_vendor).map(String::from);

            DiscoveredDevice {
                device_id,
                display_name,
                kind,
                ip: obs.ip.clone(),
                mac: obs.mac.clone(),
                hostname: obs.hostname.clone(),
                interface: obs.interface.clone(),
                vendor,
                capabilities: vec!["ip".to_string()],
                source: obs.source.clone(),
            }
        });

        if entry.mac.is_none() && obs.mac.is_some() {
            entry.mac = obs.mac.clone();
            entry.vendor = obs.mac.as_deref().and_then(guess_vendor).map(String::from);
            entry.kind = classify_device(&entry.display_name, &entry.ip, obs.mac.as_deref());
        }

        if (entry.hostname.is_none() || entry.display_name.starts_with("Host "))
            && obs.hostname.is_some()
        {
            if let Some(h) = obs.hostname {
                entry.display_name = h.clone();
                entry.hostname = Some(h);
                entry.kind = classify_device(&entry.display_name, &entry.ip, entry.mac.as_deref());
            }
        }

        if !entry.capabilities.contains(&obs.source) {
            entry.capabilities.push(obs.source);
        }
    }

    let mut devices: Vec<_> = by_ip.into_values().collect();
    devices.sort_by(|a, b| {
        let ip_a: Option<Ipv4Addr> = a.ip.parse().ok();
        let ip_b: Option<Ipv4Addr> = b.ip.parse().ok();
        ip_a.cmp(&ip_b)
    });

    devices
}

fn classify_device(name: &str, ip: &str, mac: Option<&str>) -> String {
    let lower = name.to_lowercase();
    let vendor = mac.and_then(guess_vendor).unwrap_or("").to_lowercase();

    if lower.contains("printer") || lower.contains("epson") || lower.contains("canon") || lower.contains("brother") || lower.contains("hp-") {
        return "printer".to_string();
    }
    if lower.contains("camera") || lower.contains("blink") || lower.contains("ring") {
        return "camera".to_string();
    }
    if lower.contains("watch") {
        return "wearable".to_string();
    }
    if lower.contains("iphone") || lower.contains("ipad") || lower.contains("galaxy") || lower.contains("pixel") || lower.contains("android") {
        return "mobile".to_string();
    }
    if lower.contains("macbook") || lower.contains("imac") || lower.contains("macmini") || lower.contains("pc") || lower.contains("desktop") || lower.contains("laptop") {
        return "computer".to_string();
    }
    if lower.contains("apple-tv") || lower.contains("chromecast") || lower.contains("firetv") || lower.contains("shield") || lower.contains("tv") {
        return "streaming".to_string();
    }
    if lower.contains("homepod") || lower.contains("sonos") || lower.contains("speaker") {
        return "audio".to_string();
    }
    if lower.contains("snom") || lower.contains("voip") || lower.contains("sip") {
        return "phone".to_string();
    }
    if vendor.contains("espressif") || vendor.contains("raspberry") || lower.contains("shelly") || lower.contains("sonoff") || lower.contains("esp32") || lower.contains("esp8266") || lower.contains("netatmo") || lower.contains("ecoflow") {
        return "iot".to_string();
    }
    if lower == "fritz.box" || lower.starts_with("fritz.box") || lower.contains("fritz!box") || lower.contains("router") || lower.contains("gateway") || ip.ends_with(".1") {
        return "router".to_string();
    }

    "network-device".to_string()
}

fn guess_vendor(mac: &str) -> Option<&'static str> {
    let norm = normalize_mac(mac);
    let prefix: String = norm.split(':').take(3).collect::<Vec<_>>().join(":");

    match prefix.as_str() {
        "b4:fc:7d" | "3c:37:12" | "dc:39:6f" => Some("AVM Fritz!Box"),
        "b8:27:eb" | "dc:a6:32" | "e4:5f:01" => Some("Raspberry Pi Foundation"),
        "24:6f:28" | "24:0a:c4" | "30:ae:a4" => Some("Espressif Inc."),
        "00:17:88" => Some("Philips Lighting / Hue"),
        "00:11:32" => Some("Synology"),
        "cc:40:85" | "a0:85:e3" | "be:39:d4" | "90:dd:5d" => Some("Apple Inc."),
        _ => None,
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

fn build_ping_client() -> Result<Client> {
    let config = Config::builder().kind(ICMP::V4).build();
    Client::new(&config).context("failed to create ICMP client")
}

async fn probe_icmp(client: &Client, ip: Ipv4Addr, seed: usize) -> bool {
    let mut pinger = client.pinger(IpAddr::V4(ip), PingIdentifier(seed as u16)).await;
    let payload = [0; 16];
    tokio::time::timeout(Duration::from_millis(250), pinger.ping(PingSequence(1), &payload))
        .await
        .ok()
        .and_then(|r| r.ok())
        .is_some()
}

async fn probe_tcp_ports(ip: Ipv4Addr) -> bool {
    for port in [80, 443, 22, 8080] {
        let addr = SocketAddr::new(IpAddr::V4(ip), port);
        if tokio::time::timeout(Duration::from_millis(80), TcpStream::connect(addr))
            .await
            .is_ok_and(|r| r.is_ok())
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_macos_arp_correctly() {
        let output = r#"
? (192.168.178.1) at b4:fc:7d:53:b2:89 on en0 ifscope [ethernet]
? (192.168.178.44) at cc:40:85:5f:78:78 on en0 ifscope [ethernet]
? (192.168.178.181) at (incomplete) on en0 ifscope [ethernet]
? (224.0.0.251) at 1:0:5e:0:0:fb on en0 ifscope permanent [ethernet]
"#;
        let obs = parse_macos_arp(output);
        assert_eq!(obs.len(), 3);
        assert_eq!(obs[0].ip, "192.168.178.1");
        assert_eq!(obs[0].mac.as_deref(), Some("b4:fc:7d:53:b2:89"));
        assert_eq!(obs[0].interface, "en0");
    }

    #[test]
    fn classifies_gateway_as_router() {
        let kind = classify_device("fritz.box", "192.168.178.1", Some("b4:fc:7d:53:b2:89"));
        assert_eq!(kind, "router");
    }

    #[test]
    fn normalizes_short_mac_octets() {
        assert_eq!(normalize_mac("26:d:d7:f5:c4:77"), "26:0d:d7:f5:c4:77");
    }

    #[test]
    fn classifies_various_device_types() {
        assert_eq!(classify_device("macbookprom2.fritz.box", "192.168.178.46", None), "computer");
        assert_eq!(classify_device("iphone13pro.fritz.box", "192.168.178.51", None), "mobile");
        assert_eq!(classify_device("camera-eg.fritz.box", "192.168.178.61", None), "camera");
        assert_eq!(classify_device("snom370-01.fritz.box", "192.168.178.90", None), "phone");
        assert_eq!(classify_device("homepod-r-2.fritz.box", "192.168.178.82", None), "audio");
        assert_eq!(classify_device("applewatch10stephan.fritz.box", "192.168.178.49", None), "wearable");
        assert_eq!(classify_device("fritz.box", "192.168.178.1", None), "router");
    }
}
