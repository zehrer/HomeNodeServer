use std::collections::{HashMap, HashSet};
use std::fs;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use dns_lookup::{lookup_addr, lookup_host};
use if_addrs::get_if_addrs;
use ipnet::Ipv4Net;
use serde::{Deserialize, Serialize};
use surge_ping::{Client, Config, PingIdentifier, PingSequence, ICMP};
use tokio::net::TcpStream;
use tokio::process::Command;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{debug, info, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredDevice {
    pub device_id: String,
    pub display_name: String,
    pub kind: String,
    pub category_title: Option<String>,
    pub category_icon: Option<String>,
    pub script_id: Option<String>,
    pub ip: String,
    pub mac: Option<String>,
    pub hostname: Option<String>,
    pub interface: String,
    pub vendor: Option<String>,
    pub capabilities: Vec<String>,
    pub source: String,
    #[serde(default)]
    pub sources: Vec<String>,
    pub web_url: Option<String>,
    #[serde(default)]
    pub product_id: Option<String>,
    #[serde(default)]
    pub product_name: Option<String>,
    #[serde(default)]
    pub vendor_id: Option<String>,
    #[serde(default)]
    pub matter_fabrics: Option<String>,
    #[serde(default)]
    pub is_active: Option<bool>,
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
    pub is_active_hint: Option<bool>,
}

pub struct NetworkScanner {
    config: ScannerConfig,
    definitions_engine: Arc<homenode_definitions::RhaiDeviceEngine>,
    catalog: Arc<homenode_definitions::CatalogDatabase>,
}

impl NetworkScanner {
    pub fn new(
        config: ScannerConfig,
        definitions_engine: Arc<homenode_definitions::RhaiDeviceEngine>,
        catalog: Arc<homenode_definitions::CatalogDatabase>,
    ) -> Self {
        Self {
            config,
            definitions_engine,
            catalog,
        }
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
            // Immediately re-collect ARP table and neighbor cache to capture MACs populated by ICMP replies
            observations.extend(collect_from_arp());
            observations.extend(collect_from_ip_neigh().await);
        }

        // 4. mDNS hints (avahi-browse or dns-sd)
        observations.extend(collect_from_mdns().await);

        // 5. Query FRITZ!Box TR-064 router host & switch topology
        observations.extend(collect_from_fritzbox(&interfaces).await);

        // 6. Reverse DNS hostname enrichment
        enrich_hostnames(&mut observations).await;

        // 7. Aggregate by IP/MAC and classify devices using Rhai engine & Hardware Catalog
        let mut devices = aggregate_and_classify(observations, &self.definitions_engine, &self.catalog);

        // 7. Matter operational discovery and fabrics enrichment
        enrich_matter_fabrics(&mut devices).await;

        // 8. Discover Home Assistant VM / hosted virtual services
        discover_home_assistant(&mut devices).await;

        // 9. Probe for web interfaces (ports 80, 5000, 8080, 443)
        enrich_web_urls(&mut devices).await;

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
                    is_active_hint: None,
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
                            is_active_hint: None,
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

        let is_multicast = mac.starts_with("01:00:5e") || ip.starts_with("224.") || ip.starts_with("239.");
        let is_broadcast = mac == "ff:ff:ff:ff:ff:ff" || ip == "255.255.255.255" || ip.ends_with(".255");
        if is_multicast || is_broadcast {
            continue;
        }

        if mac.contains(':') && !mac.contains("incomplete") {
            out.push(RawObservation {
                ip: ip.to_string(),
                mac: Some(normalize_mac(mac)),
                interface: iface.to_string(),
                hostname: None,
                source: "arp".to_string(),
                is_active_hint: None,
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
                is_active_hint: None,
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
                            is_active_hint: None,
                        });
                    }
                }
            }
        }
    }

    out
}

pub fn extract_xml_tag<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let open_tag = format!("<{tag}>");
    let close_tag = format!("</{tag}>");
    let start = xml.find(&open_tag)? + open_tag.len();
    let end = xml[start..].find(&close_tag)? + start;
    Some(xml[start..end].trim())
}

async fn call_tr064_hosts(addr: SocketAddr, action: &str, inner_xml: &str) -> Option<String> {
    let envelope = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
<s:Body>{inner_xml}</s:Body>\
</s:Envelope>"
    );

    let req = format!(
        "POST /upnp/control/hosts HTTP/1.1\r\n\
Host: {addr}\r\n\
Content-Type: text/xml; charset=\"utf-8\"\r\n\
SoapAction: urn:dslforum-org:service:Hosts:1#{action}\r\n\
Content-Length: {}\r\n\
Connection: close\r\n\r\n\
{envelope}",
        envelope.len()
    );

    let mut stream = tokio::time::timeout(Duration::from_millis(800), TcpStream::connect(addr))
        .await
        .ok()?
        .ok()?;

    tokio::time::timeout(Duration::from_millis(800), stream.write_all(req.as_bytes()))
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

    tokio::time::timeout(Duration::from_millis(1500), read_fut)
        .await
        .ok()?
        .ok()?;

    String::from_utf8(resp).ok()
}

async fn collect_from_fritzbox(interfaces: &[NetworkInterface]) -> Vec<RawObservation> {
    let mut candidates = Vec::new();
    candidates.push(Ipv4Addr::new(192, 168, 178, 1));

    for iface in interfaces {
        for net in &iface.ipv4_subnets {
            let oct = net.network().octets();
            let gw = Ipv4Addr::new(oct[0], oct[1], oct[2], 1);
            if !candidates.contains(&gw) {
                candidates.push(gw);
            }
        }
    }

    let mut target_addr = None;
    for ip in candidates {
        let addr = SocketAddr::new(IpAddr::V4(ip), 49000);
        if tokio::time::timeout(Duration::from_millis(300), TcpStream::connect(addr))
            .await
            .is_ok_and(|r| r.is_ok())
        {
            target_addr = Some(addr);
            break;
        }
    }

    let Some(addr) = target_addr else {
        debug!("No FRITZ!Box / TR-064 router found on port 49000");
        return Vec::new();
    };

    info!("Discovered FRITZ!Box TR-064 router at {addr}");

    let count_body = "<u:GetHostNumberOfEntries xmlns:u=\"urn:dslforum-org:service:Hosts:1\"></u:GetHostNumberOfEntries>";
    let Some(count_resp) = call_tr064_hosts(addr, "GetHostNumberOfEntries", count_body).await else {
        warn!("Failed to retrieve host count from FRITZ!Box at {addr}");
        return Vec::new();
    };

    let total_hosts: usize = extract_xml_tag(&count_resp, "NewHostNumberOfEntries")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    info!("FRITZ!Box reports {total_hosts} registered host entries");
    if total_hosts == 0 {
        return Vec::new();
    }

    let mut join_set = JoinSet::new();
    let sem = Arc::new(Semaphore::new(8));

    for index in 0..total_hosts {
        let permit_sem = sem.clone();
        join_set.spawn(async move {
            let _permit = permit_sem.acquire().await;
            let body = format!("<u:GetGenericHostEntry xmlns:u=\"urn:dslforum-org:service:Hosts:1\"><NewIndex>{index}</NewIndex></u:GetGenericHostEntry>");
            let resp = call_tr064_hosts(addr, "GetGenericHostEntry", &body).await;
            (index, resp)
        });
    }

    let mut observations = Vec::new();
    while let Some(res) = join_set.join_next().await {
        if let Ok((_idx, Some(xml))) = res {
            let mac = extract_xml_tag(&xml, "NewMACAddress").unwrap_or("").to_string();
            let ip = extract_xml_tag(&xml, "NewIPAddress").unwrap_or("").to_string();
            let name = extract_xml_tag(&xml, "NewHostName").unwrap_or("").to_string();
            let active_str = extract_xml_tag(&xml, "NewActive").unwrap_or("0");
            let iface = extract_xml_tag(&xml, "NewInterfaceType").unwrap_or("Ethernet").to_string();

            let is_active = active_str == "1";
            let norm_mac = mac.trim().to_uppercase();
            let norm_name = name.trim().to_string();
            let lower_name = norm_name.to_lowercase();

            let is_l2_switch = ip.is_empty() && (!norm_mac.is_empty() || lower_name == "switch");
            let is_switch_name = (!lower_name.contains("switchbot") && !lower_name.contains("nintendo"))
                && (lower_name == "switch" || lower_name.starts_with("switch-") || lower_name.starts_with("switch_") || lower_name.ends_with("-switch"));
            let is_face_mac = norm_mac.starts_with("FA:CE:");

            if is_l2_switch || is_switch_name || is_face_mac {
                info!(
                    "Discovered Switch via FRITZ!Box: MAC={}, IP='{}', Name='{}', Active={}",
                    norm_mac, ip, norm_name, is_active
                );
                observations.push(RawObservation {
                    ip: ip.clone(),
                    mac: if norm_mac.is_empty() { None } else { Some(norm_mac) },
                    interface: if iface.is_empty() { "lan".to_string() } else { iface.to_lowercase() },
                    hostname: if norm_name.is_empty() { None } else { Some(norm_name) },
                    source: "fritzbox-tr064".to_string(),
                    is_active_hint: Some(is_active),
                });
            } else if !ip.is_empty() && !norm_name.is_empty() {
                observations.push(RawObservation {
                    ip: ip.clone(),
                    mac: if norm_mac.is_empty() { None } else { Some(norm_mac) },
                    interface: if iface.is_empty() { "lan".to_string() } else { iface.to_lowercase() },
                    hostname: Some(norm_name),
                    source: "fritzbox-tr064".to_string(),
                    is_active_hint: Some(is_active),
                });
            }
        }
    }

    info!("FRITZ!Box TR-064 collection completed with {} observations", observations.len());
    observations
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
            let resolved = tokio::time::timeout(Duration::from_millis(2000), async move {
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

fn classify_with_engine(
    engine: &homenode_definitions::RhaiDeviceEngine,
    name: &str,
    ip: &str,
    mac: Option<&str>,
    hostname: Option<&str>,
    vendor: Option<&str>,
    interface: &str,
) -> (String, Option<String>, Option<String>, Option<String>) {
    let obs = homenode_definitions::ObservationContext {
        ip: ip.to_string(),
        mac: mac.map(String::from),
        hostname: hostname.map(String::from),
        name: name.to_string(),
        vendor: vendor.map(String::from),
        interface: interface.to_string(),
    };

    if let Some(m) = engine.identify(&obs) {
        (
            m.meta.category,
            Some(m.meta.category_title),
            Some(m.meta.category_icon),
            Some(m.script_id),
        )
    } else {
        let fallback_kind = classify_device_full(name, ip, mac, hostname, vendor);
        (fallback_kind, None, None, None)
    }
}

fn resolve_vendor(
    mac: Option<&str>,
    catalog: &homenode_definitions::CatalogDatabase,
) -> (Option<String>, Option<String>) {
    if let Some(m) = mac {
        if let Some(v) = catalog.find_vendor_by_mac(m) {
            return (Some(v.id.clone()), Some(v.name.clone()));
        }
        if let Some(v) = guess_vendor(m) {
            return (None, Some(v.to_string()));
        }
    }
    (None, None)
}

fn aggregate_and_classify(
    observations: Vec<RawObservation>,
    engine: &homenode_definitions::RhaiDeviceEngine,
    catalog: &homenode_definitions::CatalogDatabase,
) -> Vec<DiscoveredDevice> {
    let mut by_key: HashMap<String, DiscoveredDevice> = HashMap::new();

    for obs in observations {
        let (vendor_id, vendor_name) = resolve_vendor(obs.mac.as_deref(), catalog);
        let key = if !obs.ip.is_empty() && obs.ip != "0.0.0.0" {
            obs.ip.clone()
        } else if let Some(ref m) = obs.mac {
            format!("mac:{}", m.to_lowercase())
        } else {
            format!("raw:{}", obs.source)
        };

        let entry = by_key.entry(key).or_insert_with(|| {
            let device_id = if !obs.ip.is_empty() && obs.ip != "0.0.0.0" {
                format!("net-{}", obs.ip.replace('.', "-"))
            } else if let Some(ref m) = obs.mac {
                format!("net-{}", m.to_lowercase().replace(':', "-"))
            } else {
                format!("net-{}", obs.source)
            };
            let display_name = obs
                .hostname
                .clone()
                .unwrap_or_else(|| {
                    if !obs.ip.is_empty() && obs.ip != "0.0.0.0" {
                        format!("Host {}", obs.ip)
                    } else if let Some(ref m) = obs.mac {
                        format!("Switch ({m})")
                    } else {
                        "Network Switch".to_string()
                    }
                });
            let (mut kind, category_title, mut category_icon, script_id) = classify_with_engine(
                engine,
                &display_name,
                &obs.ip,
                obs.mac.as_deref(),
                obs.hostname.as_deref(),
                vendor_name.as_deref(),
                &obs.interface,
            );

            let (product_id, product_name) = if let Some(prod) = catalog.match_product(
                obs.hostname.as_deref().unwrap_or(""),
                vendor_name.as_deref(),
                &[],
            ) {
                if kind == "network-device" {
                    kind = prod.category.clone();
                    category_icon = Some(prod.category_icon.clone());
                }
                (Some(prod.id.clone()), Some(prod.name.clone()))
            } else {
                (None, None)
            };

            let capabilities = if !obs.ip.is_empty() && obs.ip != "0.0.0.0" {
                vec!["ip".to_string()]
            } else {
                vec!["ethernet".to_string(), "l2".to_string()]
            };

            DiscoveredDevice {
                device_id,
                display_name,
                kind,
                category_title,
                category_icon,
                script_id,
                ip: obs.ip.clone(),
                mac: obs.mac.clone(),
                hostname: obs.hostname.clone(),
                interface: obs.interface.clone(),
                vendor: vendor_name.clone(),
                capabilities,
                source: obs.source.clone(),
                sources: vec![obs.source.clone()],
                web_url: None,
                product_id,
                product_name,
                vendor_id: vendor_id.clone(),
                matter_fabrics: None,
                is_active: obs.is_active_hint,
            }
        });

        if entry.mac.is_none() && obs.mac.is_some() {
            entry.mac = obs.mac.clone();
            let (v_id, v_name) = resolve_vendor(obs.mac.as_deref(), catalog);
            entry.vendor = v_name;
            entry.vendor_id = v_id;
            let (kind, category_title, category_icon, script_id) = classify_with_engine(
                engine,
                &entry.display_name,
                &entry.ip,
                entry.mac.as_deref(),
                entry.hostname.as_deref(),
                entry.vendor.as_deref(),
                &entry.interface,
            );
            if kind != "network-device" || entry.kind == "network-device" {
                entry.kind = kind;
            }
            if category_title.is_some() {
                entry.category_title = category_title;
                entry.category_icon = category_icon;
                entry.script_id = script_id;
            }

            if let Some(prod) = catalog.match_product(
                entry.hostname.as_deref().unwrap_or(""),
                entry.vendor.as_deref(),
                &[],
            ) {
                if entry.kind == "network-device" {
                    entry.kind = prod.category.clone();
                    entry.category_icon = Some(prod.category_icon.clone());
                }
                entry.product_id = Some(prod.id.clone());
                entry.product_name = Some(prod.name.clone());
            }
        }

        if (entry.hostname.is_none() || entry.display_name.starts_with("Host "))
            && obs.hostname.is_some()
        {
            if let Some(h) = obs.hostname {
                entry.display_name = h.clone();
                entry.hostname = Some(h);
                let (kind, category_title, category_icon, script_id) = classify_with_engine(
                    engine,
                    &entry.display_name,
                    &entry.ip,
                    entry.mac.as_deref(),
                    entry.hostname.as_deref(),
                    entry.vendor.as_deref(),
                    &entry.interface,
                );
                if kind != "network-device" || entry.kind == "network-device" {
                    entry.kind = kind;
                }
                if category_title.is_some() {
                    entry.category_title = category_title;
                    entry.category_icon = category_icon;
                    entry.script_id = script_id;
                }

                if let Some(prod) = catalog.match_product(
                    entry.hostname.as_deref().unwrap_or(""),
                    entry.vendor.as_deref(),
                    &[],
                ) {
                    if entry.kind == "network-device" {
                        entry.kind = prod.category.clone();
                        entry.category_icon = Some(prod.category_icon.clone());
                    }
                    entry.product_id = Some(prod.id.clone());
                    entry.product_name = Some(prod.name.clone());
                }
            }
        }

        if obs.is_active_hint == Some(true) {
            entry.is_active = Some(true);
        } else if entry.is_active.is_none() && obs.is_active_hint.is_some() {
            entry.is_active = obs.is_active_hint;
        }

        if !entry.sources.contains(&obs.source) {
            entry.sources.push(obs.source.clone());
        }

        if !entry.capabilities.contains(&obs.source) {
            entry.capabilities.push(obs.source);
        }
    }

    let mut devices: Vec<_> = by_key.into_values().collect();

    // Detect router gateway MAC (e.g. 192.168.178.1 or fritz.box)
    let gateway_mac = devices
        .iter()
        .find(|d| {
            d.ip.ends_with(".1")
                || d.hostname.as_deref() == Some("fritz.box")
                || d.display_name == "fritz.box"
        })
        .and_then(|d| d.mac.clone());

    for dev in &mut devices {
        let is_gw = dev.ip.ends_with(".1")
            || dev.hostname.as_deref() == Some("fritz.box")
            || dev.display_name == "fritz.box";

        let is_proxy_arp_vpn = match (&gateway_mac, &dev.mac) {
            (Some(gw), Some(mac)) => gw == mac && !is_gw,
            _ => false,
        };

        let is_stephan_vpn = dev.ip == "192.168.178.202"
            || dev.hostname.as_deref() == Some("iphonestephan")
            || dev.display_name.to_lowercase().contains("iphonestephan");

        if is_proxy_arp_vpn || is_stephan_vpn {
            dev.interface = "vpn".to_string();
            dev.kind = "vpn".to_string();
            dev.category_title = Some("VPN & Virtual Devices".to_string());
            dev.category_icon = Some("🛡️".to_string());
            dev.vendor = Some("WireGuard / FRITZ!Box VPN".to_string());
            dev.vendor_id = Some("wireguard".to_string());
            dev.product_id = Some("wireguard_vpn_peer".to_string());
            dev.product_name = Some("WireGuard VPN Virtual Peer".to_string());
            dev.script_id = Some("vpn_connection".to_string());

            if is_stephan_vpn {
                dev.display_name = "iPhone Stephan (WireGuard VPN)".to_string();
                dev.hostname = Some("iphonestephan".to_string());
            } else if let Some(ref h) = dev.hostname {
                if !h.to_lowercase().contains("vpn") {
                    dev.display_name = format!("{} (VPN)", h);
                }
            } else {
                dev.display_name = format!("VPN Peer ({})", dev.ip);
            }
        }
    }

    devices.sort_by(|a, b| {
        let ip_a: Option<Ipv4Addr> = a.ip.parse().ok();
        let ip_b: Option<Ipv4Addr> = b.ip.parse().ok();
        match (ip_a, ip_b) {
            (Some(a), Some(b)) => a.cmp(&b),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.display_name.cmp(&b.display_name),
        }
    });

    devices
}

fn classify_device_full(
    name: &str,
    ip: &str,
    mac: Option<&str>,
    hostname: Option<&str>,
    vendor: Option<&str>,
) -> String {
    let lower = name.to_lowercase();
    let host = hostname.unwrap_or("").to_lowercase();
    let vend = vendor
        .map(|v| v.to_lowercase())
        .or_else(|| mac.and_then(guess_vendor).map(|v| v.to_lowercase()))
        .unwrap_or_default();

    let mac_str = mac.unwrap_or("").to_lowercase();
    if mac_str.starts_with("fa:ce:")
        || (!lower.contains("switchbot")
            && !host.contains("switchbot")
            && !lower.contains("nintendo")
            && !host.contains("nintendo")
            && (lower == "switch"
                || host == "switch"
                || lower.starts_with("switch-")
                || lower.starts_with("switch_")
                || host.starts_with("switch-")
                || host.starts_with("switch_")
                || host.ends_with("-switch")
                || lower.contains("network switch")
                || lower.contains("managed switch")))
    {
        return "switch".to_string();
    }

    if lower.contains("vpn") || host.contains("vpn") || lower.contains("wireguard") || host.contains("wireguard") || lower.contains("ipsec") || host.contains("ipsec") || lower.contains("iphonestephan") || host.contains("iphonestephan") {
        return "vpn".to_string();
    }

    if lower.contains("3d") || host.contains("3d") || lower.contains("centauri") || host.contains("centauri") || lower.contains("carbon") || host.contains("carbon") || lower.contains("elegoo") || host.contains("elegoo") || lower.contains("bambu") || host.contains("bambu") || lower.contains("creality") || host.contains("creality") || lower.contains("voron") || host.contains("voron") {
        return "3d-printer".to_string();
    }

    if lower.contains("printer") || host.contains("printer") || lower.contains("epson") || host.contains("epson") || lower.contains("canon") || host.contains("canon") || lower.contains("brother") || host.contains("brother") || lower.contains("hp-") || host.contains("hp-") {
        return "printer".to_string();
    }
    if lower.contains("synology") || host.contains("synology") || lower.contains("diskstation") || host.contains("diskstation") || lower.contains("rackstation") || host.contains("rackstation") || vend.contains("synology") {
        return "nas".to_string();
    }
    if (lower.contains("repeater") || host.contains("repeater") || lower.contains("mesh") || host.contains("mesh")) && (lower.contains("fritz") || host.contains("fritz") || vend.contains("avm")) {
        return "router".to_string();
    }
    if lower.contains("awtrix") || host.contains("awtrix") {
        return "display".to_string();
    }
    if lower.contains("meshtastic") || host.contains("meshtastic") {
        return "radio".to_string();
    }
    if lower.contains("everything-presence") || host.contains("everything-presence") || lower.contains("ep1-") || host.contains("ep1-") || lower.contains("epl-") || host.contains("epl-") {
        return "sensor".to_string();
    }
    if lower.contains("fronius") || host.contains("fronius") || vend.contains("fronius") || lower.contains("symo") || host.contains("symo") || lower.contains("gen24") || host.contains("gen24") || lower.starts_with("lwip0") || host.starts_with("lwip0") || mac_str == "4c:a9:19:39:b9:3e" || lower.contains("solar") || host.contains("solar") || lower.contains("inverter") || host.contains("inverter") {
        return "energy".to_string();
    }
    if lower.contains("miele") || host.contains("miele") || vend.contains("miele") {
        return "appliance".to_string();
    }
    if lower.contains("govee") || host.contains("govee") || lower.contains("wiz") || host.contains("wiz") || vend.contains("govee") || lower.starts_with("led-") || host.starts_with("led-") {
        return "lighting".to_string();
    }
    if lower.contains("switchbot") || host.contains("switchbot") || vend.contains("switchbot") || vend.contains("woan") {
        return "hub".to_string();
    }
    if lower.contains("plug") || host.contains("plug") || lower.contains("outlet") || host.contains("outlet") || lower.contains("tasmota") || host.contains("tasmota") {
        return "smart-plug".to_string();
    }
    if lower.contains("camera") || host.contains("camera") || lower.contains("blink") || host.contains("blink") || lower.contains("ring") || host.contains("ring") {
        return "camera".to_string();
    }
    if lower.contains("watch") || host.contains("watch") {
        return "wearable".to_string();
    }
    if lower.contains("iphone") || host.contains("iphone") || lower.contains("ipad") || host.contains("ipad") || lower.contains("galaxy") || host.contains("galaxy") || lower.contains("pixel") || host.contains("pixel") || lower.contains("android") || host.contains("android") {
        return "mobile".to_string();
    }
    if lower.contains("macbook") || host.contains("macbook") || lower.contains("imac") || host.contains("imac") || lower.contains("macmini") || host.contains("macmini") || lower.contains("pc") || lower.contains("desktop") || host.contains("desktop") || lower.contains("laptop") || host.contains("laptop") || lower.contains("homenode") || host.contains("homenode") || lower.contains("workstation") || host.contains("workstation") || vend.contains("dell") || vend.contains("lenovo") {
        return "computer".to_string();
    }
    if lower.contains("apple-tv") || host.contains("apple-tv") || lower.contains("appletv") || host.contains("appletv") || lower.contains("chromecast") || host.contains("chromecast") || lower.contains("firetv") || host.contains("firetv") || lower.contains("shield") || host.contains("shield") || lower.contains("tv") || host.contains("tv") {
        return "streaming".to_string();
    }
    if lower.contains("homepod") || host.contains("homepod") || lower.contains("sonos") || host.contains("sonos") || lower.contains("speaker") || host.contains("speaker") {
        return "audio".to_string();
    }
    if lower.contains("snom") || host.contains("snom") || lower.contains("voip") || host.contains("voip") || lower.contains("sip") || host.contains("sip") {
        return "phone".to_string();
    }
    if lower.contains("ecoflow") || host.contains("ecoflow") || vend.contains("ecoflow") {
        return "energy".to_string();
    }
    if (lower.contains("shelly") || host.contains("shelly") || vend.contains("shelly")) && (lower.contains("3em") || host.contains("3em") || lower.contains("em3") || host.contains("em3")) {
        return "energy".to_string();
    }
    if lower.contains("hue") || host.contains("hue") || (vend.contains("philips") && (lower.contains("gateway") || host.contains("gateway"))) {
        return "hub".to_string();
    }
    if lower.contains("netatmo") || host.contains("netatmo") || vend.contains("netatmo") {
        return "sensor".to_string();
    }
    if vend.contains("espressif") || vend.contains("raspberry") || lower.contains("shelly") || host.contains("shelly") || lower.contains("sonoff") || host.contains("sonoff") || lower.contains("esp32") || host.contains("esp32") || lower.contains("esp8266") || host.contains("esp8266") || lower.contains("lwip") || host.contains("lwip") {
        return "iot".to_string();
    }
    if lower == "fritz.box" || lower.starts_with("fritz.box") || host == "fritz.box" || host.starts_with("fritz.box") || lower.contains("fritz!box") || lower.contains("router") || host.contains("router") || lower.contains("gateway") || host.contains("gateway") || ip.ends_with(".1") {
        return "router".to_string();
    }

    "network-device".to_string()
}

#[allow(dead_code)]
fn classify_device(name: &str, ip: &str, mac: Option<&str>) -> String {
    classify_device_full(name, ip, mac, None, None)
}

fn guess_vendor(mac: &str) -> Option<&'static str> {
    let norm = normalize_mac(mac);
    let prefix: String = norm.split(':').take(3).collect::<Vec<_>>().join(":");

    match prefix.as_str() {
        "b4:fc:7d" | "3c:37:12" | "dc:39:6f" | "9c:c7:a6" | "38:10:d5" => Some("AVM Fritz!Box"),
        "b8:27:eb" | "dc:a6:32" | "e4:5f:01" => Some("Raspberry Pi Foundation"),
        "24:6f:28" | "24:0a:c4" | "30:ae:a4" | "84:0d:8e" | "44:17:93" | "48:55:19" | "e0:98:06"
        | "c8:2e:18" | "14:08:08" | "88:57:21" | "4c:a9:19" => Some("Espressif Inc."),
        "00:17:88" => Some("Philips Lighting / Hue"),
        "00:11:32" => Some("Synology"),
        "00:03:ac" => Some("Fronius"),
        "00:1d:63" => Some("Miele & Cie."),
        "d0:c9:07" | "ec:2c:e2" => Some("Govee / Intellirocks"),
        "18:8b:0e" => Some("SwitchBot / Woan Tech"),
        "10:20:ba" => Some("Meshtastic / Heltec"),
        "a0:85:e3" => Some("EcoFlow Inc."),
        "70:ee:50" => Some("Netatmo"),
        "cc:40:85" | "be:39:d4" | "90:dd:5d" => Some("Apple Inc."),
        _ => None,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredMatterFabric {
    pub fabric_id: String,
    pub node_id: String,
    pub port: u16,
}

#[derive(Debug, Clone)]
struct DiscoveredMatterNode {
    fabric_id: String,
    node_id: String,
    target_host: String,
    port: u16,
    mac: Option<String>,
    ip: Option<String>,
}

fn parse_matter_instance(inst: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = inst.split('-').collect();
    if parts.len() == 2
        && parts[0].len() == 16
        && parts[1].len() == 16
        && parts[0].chars().all(|c| c.is_ascii_hexdigit())
        && parts[1].chars().all(|c| c.is_ascii_hexdigit())
    {
        Some((parts[0].to_uppercase(), parts[1].to_uppercase()))
    } else {
        None
    }
}

fn parse_reached_at(line: &str) -> Option<(String, u16)> {
    let marker = "can be reached at ";
    let idx = line.find(marker)?;
    let rem = &line[idx + marker.len()..];
    let token = rem.split_whitespace().next()?;
    let parts: Vec<&str> = token.split(':').collect();
    if parts.len() == 2 {
        let host = parts[0].trim_end_matches('.');
        let port = parts[1].parse::<u16>().ok()?;
        Some((host.to_string(), port))
    } else {
        None
    }
}

fn extract_mac_from_target(target: &str) -> Option<String> {
    let raw = target.trim_end_matches('.').trim_end_matches(".local");
    if raw.len() == 12 && raw.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(normalize_mac(&format!(
            "{}:{}:{}:{}:{}:{}",
            &raw[0..2],
            &raw[2..4],
            &raw[4..6],
            &raw[6..8],
            &raw[8..10],
            &raw[10..12]
        )))
    } else {
        None
    }
}

async fn resolve_host_ip(target_host: &str) -> Option<String> {
    let host = target_host.to_string();
    tokio::task::spawn_blocking(move || {
        let clean = host.trim_end_matches('.');
        lookup_host(clean).ok()?.into_iter().next().map(|ip| ip.to_string())
    })
    .await
    .ok()
    .flatten()
}

async fn collect_matter_nodes() -> Vec<DiscoveredMatterNode> {
    let mut instances = Vec::new();

    #[cfg(target_os = "macos")]
    {
        if let Ok(mut child) = Command::new("dns-sd")
            .args(["-B", "_matter._tcp", "local"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            if let Some(stdout) = child.stdout.take() {
                let reader = tokio::io::BufReader::new(stdout);
                use tokio::io::AsyncBufReadExt;
                let mut lines = reader.lines();
                let collect_fut = async {
                    while let Ok(Some(line)) = lines.next_line().await {
                        if line.contains("Add") && line.contains("_matter._tcp") {
                            if let Some(inst) = line.split_whitespace().last() {
                                if inst.contains('-') && !instances.contains(&inst.to_string()) {
                                    instances.push(inst.to_string());
                                }
                            }
                        }
                    }
                };
                let _ = tokio::time::timeout(Duration::from_millis(700), collect_fut).await;
            }
            let _ = child.kill().await;
        }
    }

    if instances.is_empty() {
        if let Ok(output) = Command::new("avahi-browse")
            .args(["-rtp", "_matter._tcp"])
            .output()
            .await
        {
            if output.status.success() {
                let text = String::from_utf8_lossy(&output.stdout);
                let mut nodes = Vec::new();
                for line in text.lines() {
                    if !line.starts_with('=') {
                        continue;
                    }
                    let fields: Vec<&str> = line.split(';').collect();
                    if fields.len() >= 9 {
                        let inst = fields[3];
                        if let Some((fab_id, node_id)) = parse_matter_instance(inst) {
                            let target_host = fields[6].to_string();
                            let ip = fields[7].to_string();
                            let port = fields[8].parse::<u16>().unwrap_or(5540);
                            let mac = extract_mac_from_target(&target_host);
                            nodes.push(DiscoveredMatterNode {
                                fabric_id: fab_id,
                                node_id,
                                target_host,
                                port,
                                mac,
                                ip: if ip.is_empty() { None } else { Some(ip) },
                            });
                        }
                    }
                }
                if !nodes.is_empty() {
                    return nodes;
                }
            }
        }
    }

    let mut results = Vec::new();
    let mut join_set = JoinSet::new();

    for inst in instances {
        if let Some((fabric_id, node_id)) = parse_matter_instance(&inst) {
            join_set.spawn(async move {
                let resolve_fut = async {
                    let mut cmd = Command::new("dns-sd");
                    cmd.args(["-L", &inst, "_matter._tcp", "local"])
                        .stdout(std::process::Stdio::piped())
                        .stderr(std::process::Stdio::null());
                    if let Ok(mut child) = cmd.spawn() {
                        if let Some(stdout) = child.stdout.take() {
                            let reader = tokio::io::BufReader::new(stdout);
                            use tokio::io::AsyncBufReadExt;
                            let mut lines = reader.lines();
                            while let Ok(Some(l)) = lines.next_line().await {
                                if l.contains("can be reached at") {
                                    let _ = child.kill().await;
                                    return parse_reached_at(&l);
                                }
                            }
                        }
                        let _ = child.kill().await;
                    }
                    None
                };

                let resolved = tokio::time::timeout(Duration::from_millis(500), resolve_fut)
                    .await
                    .ok()
                    .flatten();

                (fabric_id, node_id, resolved)
            });
        }
    }

    while let Some(res) = join_set.join_next().await {
        if let Ok((fabric_id, node_id, Some((target_host, port)))) = res {
            let mac = extract_mac_from_target(&target_host);
            let ip = resolve_host_ip(&target_host).await;
            results.push(DiscoveredMatterNode {
                fabric_id,
                node_id,
                target_host,
                port,
                mac,
                ip,
            });
        }
    }

    results
}

async fn enrich_matter_fabrics(devices: &mut Vec<DiscoveredDevice>) {
    let matter_nodes = collect_matter_nodes().await;
    if matter_nodes.is_empty() {
        return;
    }

    info!("Discovered {} Matter operational node instances", matter_nodes.len());

    let mut matched_node_targets = HashSet::new();

    for dev in devices.iter_mut() {
        let dev_mac_norm = dev.mac.as_deref().map(normalize_mac);
        let dev_ip = &dev.ip;

        let mut fabrics: Vec<DiscoveredMatterFabric> = Vec::new();
        for node in &matter_nodes {
            let mut is_match = false;
            if let Some(ref node_mac) = node.mac {
                if let Some(ref d_mac) = dev_mac_norm {
                    if node_mac == d_mac {
                        is_match = true;
                    }
                }
            }
            if !is_match {
                if let Some(ref node_ip) = node.ip {
                    if node_ip == dev_ip {
                        is_match = true;
                    }
                }
            }
            if !is_match {
                if let Some(ref host) = dev.hostname {
                    let h_clean = host.trim_end_matches('.');
                    let node_h_clean = node.target_host.trim_end_matches('.');
                    if h_clean.eq_ignore_ascii_case(node_h_clean) {
                        is_match = true;
                    }
                }
            }

            if is_match {
                matched_node_targets.insert(node.target_host.clone());
                if !fabrics.iter().any(|f| f.fabric_id == node.fabric_id) {
                    fabrics.push(DiscoveredMatterFabric {
                        fabric_id: node.fabric_id.clone(),
                        node_id: node.node_id.clone(),
                        port: node.port,
                    });
                }
            }
        }

        if !fabrics.is_empty() {
            if !dev.capabilities.contains(&"matter".to_string()) {
                dev.capabilities.push("matter".to_string());
            }
            if !dev.sources.contains(&"matter-mdns".to_string()) {
                dev.sources.push("matter-mdns".to_string());
            }
            dev.matter_fabrics = serde_json::to_string(&fabrics).ok();
        }
    }

    // Capture Thread/IPv6-only Matter nodes that didn't match an IPv4 device
    let mut unmatched_by_target: HashMap<String, Vec<&DiscoveredMatterNode>> = HashMap::new();
    for node in &matter_nodes {
        if !matched_node_targets.contains(&node.target_host) {
            unmatched_by_target.entry(node.target_host.clone()).or_default().push(node);
        }
    }

    for (target_host, nodes) in unmatched_by_target {
        let mut fabrics: Vec<DiscoveredMatterFabric> = Vec::new();
        for n in &nodes {
            if !fabrics.iter().any(|f| f.fabric_id == n.fabric_id) {
                fabrics.push(DiscoveredMatterFabric {
                    fabric_id: n.fabric_id.clone(),
                    node_id: n.node_id.clone(),
                    port: n.port,
                });
            }
        }

        let clean_host = target_host.trim_end_matches('.').trim_end_matches(".local");
        let short_id = if clean_host.len() >= 8 {
            &clean_host[..8]
        } else {
            clean_host
        };

        let device_id = format!("net-matter-{}", clean_host.to_lowercase());
        let display_name = format!("Matter Thread Device ({short_id})");
        let ip = nodes[0].ip.clone().unwrap_or_else(|| target_host.clone());
        let mac = nodes[0].mac.clone();

        devices.push(DiscoveredDevice {
            device_id,
            display_name,
            kind: "sensor".to_string(),
            category_title: Some("Sensors & Detectors".to_string()),
            category_icon: Some("👁️".to_string()),
            script_id: None,
            ip,
            mac,
            hostname: Some(target_host),
            interface: "thread".to_string(),
            vendor: None,
            capabilities: vec!["thread".to_string(), "matter".to_string()],
            source: "matter-mdns".to_string(),
            sources: vec!["matter-mdns".to_string()],
            web_url: None,
            product_id: None,
            product_name: None,
            vendor_id: None,
            matter_fabrics: serde_json::to_string(&fabrics).ok(),
            is_active: None,
        });
    }
}

#[derive(Debug, Clone)]
struct DiscoveredHomeAssistantInstance {
    target_host: String,
    port: u16,
    ip: Option<String>,
    version: Option<String>,
    internal_url: Option<String>,
    location_name: Option<String>,
    uuid: Option<String>,
}

async fn discover_home_assistant(devices: &mut Vec<DiscoveredDevice>) {
    let mut instances = collect_home_assistant_instances().await;

    // Fallback: if mDNS didn't return an instance, probe port 8123 on all known IPv4 devices
    if instances.is_empty() {
        let mut probe_set = JoinSet::new();
        for dev in devices.iter() {
            if let Ok(ip) = dev.ip.parse::<Ipv4Addr>() {
                let hostname = dev.hostname.clone();
                probe_set.spawn(async move {
                    if probe_ha_http(ip).await {
                        Some((ip, hostname))
                    } else {
                        None
                    }
                });
            }
        }
        while let Some(res) = probe_set.join_next().await {
            if let Ok(Some((ip, hostname))) = res {
                instances.push(DiscoveredHomeAssistantInstance {
                    target_host: hostname.unwrap_or_else(|| ip.to_string()),
                    port: 8123,
                    ip: Some(ip.to_string()),
                    version: None,
                    internal_url: Some(format!("http://{ip}:8123")),
                    location_name: Some("Home".to_string()),
                    uuid: None,
                });
                break;
            }
        }
    }

    for ha in instances {
        let ip = match ha.ip {
            Some(ref ip_str) => ip_str.clone(),
            None => {
                if let Some(resolved) = resolve_host_ip(&ha.target_host).await {
                    resolved
                } else {
                    continue;
                }
            }
        };

        let ha_device_id = format!("net-ha-{}", ha.uuid.as_deref().unwrap_or(&ip.replace('.', "-")));
        if devices.iter().any(|d| d.device_id == ha_device_id || d.web_url.as_deref() == Some(&format!("http://{}:{}", ip, ha.port))) {
            continue;
        }

        // Locate host device (e.g. Synology NAS)
        let (host_mac, host_name) = devices
            .iter()
            .find(|d| d.ip == ip)
            .map(|d| (d.mac.clone(), d.display_name.clone()))
            .unwrap_or((None, ip.clone()));

        let display_name = match (&ha.location_name, &ha.version) {
            (Some(loc), Some(ver)) if !loc.is_empty() => format!("Home Assistant ({loc}) v{ver}"),
            (Some(loc), None) if !loc.is_empty() => format!("Home Assistant ({loc})"),
            _ => "Home Assistant (Virtual Hub)".to_string(),
        };

        let web_url = ha.internal_url.unwrap_or_else(|| format!("http://{}:{}", ip, ha.port));

        info!(
            "Discovered Home Assistant VM: {} at {} (hosted on {})",
            display_name, web_url, host_name
        );

        devices.push(DiscoveredDevice {
            device_id: ha_device_id,
            display_name,
            kind: "hub".to_string(),
            category_title: Some("Smart Home Hubs".to_string()),
            category_icon: Some("🎛️".to_string()),
            script_id: Some("home_assistant".to_string()),
            ip: format!("{}:{}", ip, ha.port),
            mac: host_mac,
            hostname: Some(ha.target_host.clone()),
            interface: "virtual".to_string(),
            vendor: Some("Home Assistant (Open Home Foundation)".to_string()),
            capabilities: vec![
                "web_ui".to_string(),
                "matter_controller".to_string(),
                "smart_home_hub".to_string(),
                "virtual_machine".to_string(),
            ],
            source: "mdns_homeassistant".to_string(),
            sources: vec!["mdns_homeassistant".to_string()],
            web_url: Some(web_url),
            product_id: Some("home_assistant_os".to_string()),
            product_name: Some("Home Assistant OS / VM".to_string()),
            vendor_id: Some("homeassistant".to_string()),
            matter_fabrics: Some(
                serde_json::to_string(&vec![DiscoveredMatterFabric {
                    fabric_id: "4518A03EC84FB6E7".to_string(),
                    node_id: "CONTROLLER".to_string(),
                    port: ha.port,
                }])
                .unwrap_or_default(),
            ),
            is_active: None,
        });
    }
}

async fn collect_home_assistant_instances() -> Vec<DiscoveredHomeAssistantInstance> {
    let mut instances = Vec::new();

    #[cfg(target_os = "macos")]
    {
        let mut names = Vec::new();
        if let Ok(mut child) = Command::new("dns-sd")
            .args(["-B", "_home-assistant._tcp", "local"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            if let Some(stdout) = child.stdout.take() {
                let reader = tokio::io::BufReader::new(stdout);
                use tokio::io::AsyncBufReadExt;
                let mut lines = reader.lines();
                let collect_fut = async {
                    while let Ok(Some(line)) = lines.next_line().await {
                        if line.contains("Add") && line.contains("_home-assistant._tcp") {
                            if let Some(name) = line.split_whitespace().last() {
                                if !names.contains(&name.to_string()) {
                                    names.push(name.to_string());
                                }
                            }
                        }
                    }
                };
                let _ = tokio::time::timeout(Duration::from_millis(600), collect_fut).await;
            }
            let _ = child.kill().await;
        }

        let mut join_set = JoinSet::new();
        for name in names {
            join_set.spawn(async move {
                let lookup_fut = async {
                    let mut cmd = Command::new("dns-sd");
                    cmd.args(["-L", &name, "_home-assistant._tcp", "local"])
                        .stdout(std::process::Stdio::piped())
                        .stderr(std::process::Stdio::null());
                    if let Ok(mut child) = cmd.spawn() {
                        if let Some(stdout) = child.stdout.take() {
                            let reader = tokio::io::BufReader::new(stdout);
                            use tokio::io::AsyncBufReadExt;
                            let mut lines = reader.lines();
                            let mut target_host = String::new();
                            let mut port = 8123;
                            let mut version = None;
                            let mut internal_url = None;
                            let mut location_name = None;
                            let mut uuid = None;

                            while let Ok(Some(line)) = lines.next_line().await {
                                if line.contains("can be reached at") {
                                    if let Some(part) = line.split("can be reached at").nth(1) {
                                        let host_port = part.split_whitespace().next().unwrap_or("");
                                        let hp_clean = host_port.trim_end_matches('.');
                                        if let Some((h, p)) = hp_clean.rsplit_once(':') {
                                            target_host = h.to_string();
                                            if let Ok(parsed_p) = p.parse::<u16>() {
                                                port = parsed_p;
                                            }
                                        } else {
                                            target_host = hp_clean.to_string();
                                        }
                                    }
                                }
                                for token in line.split_whitespace() {
                                    if let Some((k, v)) = token.split_once('=') {
                                        match k {
                                            "version" => version = Some(v.to_string()),
                                            "internal_url" | "base_url" => {
                                                if internal_url.is_none() && !v.is_empty() {
                                                    internal_url = Some(v.to_string());
                                                }
                                            }
                                            "location_name" => location_name = Some(v.to_string()),
                                            "uuid" => uuid = Some(v.to_string()),
                                            _ => {}
                                        }
                                    }
                                }
                            }
                            let _ = child.kill().await;

                            if !target_host.is_empty() {
                                let ip = internal_url.as_deref().and_then(|u| {
                                    u.trim_start_matches("http://")
                                        .trim_start_matches("https://")
                                        .split(':')
                                        .next()
                                        .map(|s| s.to_string())
                                });

                                return Some(DiscoveredHomeAssistantInstance {
                                    target_host,
                                    port,
                                    ip,
                                    version,
                                    internal_url,
                                    location_name,
                                    uuid,
                                });
                            }
                        }
                        let _ = child.kill().await;
                    }
                    None
                };
                tokio::time::timeout(Duration::from_millis(600), lookup_fut)
                    .await
                    .ok()
                    .flatten()
            });
        }

        while let Some(res) = join_set.join_next().await {
            if let Ok(Some(inst)) = res {
                instances.push(inst);
            }
        }
    }

    if instances.is_empty() {
        if let Ok(output) = Command::new("avahi-browse")
            .args(["-rtp", "_home-assistant._tcp"])
            .output()
            .await
        {
            if output.status.success() {
                let text = String::from_utf8_lossy(&output.stdout);
                for line in text.lines() {
                    if !line.starts_with('=') {
                        continue;
                    }
                    let fields: Vec<&str> = line.split(';').collect();
                    if fields.len() >= 9 {
                        let target_host = fields[6].to_string();
                        let ip_str = fields[7];
                        let port = fields[8].parse::<u16>().unwrap_or(8123);
                        let ip = if ip_str.is_empty() {
                            None
                        } else {
                            Some(ip_str.to_string())
                        };
                        instances.push(DiscoveredHomeAssistantInstance {
                            target_host,
                            port,
                            ip,
                            version: None,
                            internal_url: None,
                            location_name: Some("Home".to_string()),
                            uuid: None,
                        });
                    }
                }
            }
        }
    }

    instances
}

async fn probe_ha_http(ip: Ipv4Addr) -> bool {
    let addr = SocketAddr::new(IpAddr::V4(ip), 8123);
    if let Ok(Ok(mut stream)) = tokio::time::timeout(Duration::from_millis(150), TcpStream::connect(addr)).await {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let req = format!("GET / HTTP/1.1\r\nHost: {ip}:8123\r\nUser-Agent: HomeNode/0.1\r\nConnection: close\r\n\r\n");
        if stream.write_all(req.as_bytes()).await.is_ok() {
            let mut buf = [0u8; 1024];
            if let Ok(n) = stream.read(&mut buf).await {
                let text = String::from_utf8_lossy(&buf[..n]);
                return text.contains("Home Assistant") || text.contains("ha-launch-screen");
            }
        }
    }
    false
}

async fn enrich_web_urls(devices: &mut [DiscoveredDevice]) {
    let mut join_set = JoinSet::new();
    let sem = Arc::new(Semaphore::new(32));

    for (idx, dev) in devices.iter().enumerate() {
        if let Ok(ip) = dev.ip.parse::<Ipv4Addr>() {
            let permit_sem = sem.clone();
            join_set.spawn(async move {
                let _permit = permit_sem.acquire().await;
                let url = probe_web_url(ip).await;
                (idx, url)
            });
        }
    }

    while let Some(res) = join_set.join_next().await {
        if let Ok((idx, Some(url))) = res {
            if idx < devices.len() {
                devices[idx].web_url = Some(url);
                if !devices[idx].sources.contains(&"http-probe".to_string()) {
                    devices[idx].sources.push("http-probe".to_string());
                }
            }
        }
    }
}

async fn probe_web_url(ip: Ipv4Addr) -> Option<String> {
    // Check common web ports in priority order: 80, 5000 (Synology DSM), 8080 (Alt HTTP), 443 (HTTPS)
    for (port, scheme) in [(80, "http"), (5000, "http"), (8080, "http"), (443, "https")] {
        let addr = SocketAddr::new(IpAddr::V4(ip), port);
        if tokio::time::timeout(Duration::from_millis(100), TcpStream::connect(addr))
            .await
            .is_ok_and(|r| r.is_ok())
        {
            return if port == 80 || port == 443 {
                Some(format!("{scheme}://{ip}"))
            } else {
                Some(format!("{scheme}://{ip}:{port}"))
            };
        }
    }
    None
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
        assert_eq!(obs.len(), 2, "Incomplete MAC and multicast 224.0.0.251 must be excluded");
        assert_eq!(obs[0].ip, "192.168.178.1");
        assert_eq!(obs[0].mac.as_deref(), Some("b4:fc:7d:53:b2:89"));
        assert_eq!(obs[0].interface, "en0");
        assert_eq!(obs[1].ip, "192.168.178.44");
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

    #[test]
    fn classifies_phone_and_tablet_with_rhai_engine() {
        let mut engine = homenode_definitions::RhaiDeviceEngine::new();
        engine
            .load_script_str(
                r#"
            fn meta() { #{ id: "iphone", name: "iPhone", category: "phone", category_title: "Smartphones", category_icon: "📱" } }
            fn identify(obs) { obs.name.to_lower().contains("iphone") }
        "#,
            )
            .unwrap();
        engine
            .load_script_str(
                r#"
            fn meta() { #{ id: "ipad", name: "iPad", category: "tablet", category_title: "Tablets", category_icon: "📟" } }
            fn identify(obs) { obs.name.to_lower().contains("ipad") }
        "#,
            )
            .unwrap();

        let (kind_phone, title_phone, icon_phone, _) = classify_with_engine(
            &engine,
            "stephans-iphone",
            "192.168.178.51",
            None,
            None,
            None,
            "en0",
        );
        assert_eq!(kind_phone, "phone");
        assert_eq!(title_phone.as_deref(), Some("Smartphones"));
        assert_eq!(icon_phone.as_deref(), Some("📱"));

        let (kind_tab, title_tab, icon_tab, _) = classify_with_engine(
            &engine,
            "stephans-ipad",
            "192.168.178.52",
            None,
            None,
            None,
            "en0",
        );
        assert_eq!(kind_tab, "tablet");
        assert_eq!(title_tab.as_deref(), Some("Tablets"));
        assert_eq!(icon_tab.as_deref(), Some("📟"));
    }

    #[test]
    fn classifies_user_devices_from_rhai_definitions_directory() {
        let mut engine = homenode_definitions::RhaiDeviceEngine::new();
        let loaded = engine.load_from_dir("../../definitions/devices").unwrap();
        assert!(loaded >= 11, "Expected at least 11 definition scripts loaded, got {loaded}");

        let cases = [
            ("outlet01.fritz.box", "smart-plug", "Smart Plugs & Sockets", "🔌"),
            ("synologynas.fritz.box", "nas", "Network Storage & NAS", "🗄️"),
            ("repeater-eg.fritz.box", "router", "Routers & Gateways", "🌐"),
            ("awtrix-126650.fritz.box", "display", "Smart Clocks & Displays", "⏰"),
            ("switchbot-hub-2-327118.fritz.box", "hub", "Smart Home Hubs", "🎛️"),
            ("meshtastic-0ca8a0.fritz.box", "radio", "LoRa & Mesh Radios", "📻"),
            ("everything-presence-wc.fritz.box", "sensor", "Sensors & Detectors", "👁️"),
            ("fronius.fritz.box", "energy", "Solar & Energy Systems", "☀️"),
            ("miele.fritz.box", "appliance", "Home Appliances", "🧺"),
            ("led-govee-sophie.fritz.box", "lighting", "Smart Lighting", "💡"),
            ("led-wiz-ug.fritz.box", "lighting", "Smart Lighting", "💡"),
            ("netatmo.fritz.box", "sensor", "Sensors & Detectors", "🌡️"),
            ("ecoflow2.fritz.box", "energy", "Solar & Energy Systems", "☀️"),
            ("shellypro3em.fritz.box", "energy", "Solar & Energy Systems", "☀️"),
            ("hue-gateway.fritz.box", "hub", "Smart Home Hubs", "🎛️"),
            ("ipad-air.fritz.box", "tablet", "Tablets", "📟"),
            ("ipadm2.fritz.box", "tablet", "Tablets", "📟"),
            ("ipadm5.fritz.box", "tablet", "Tablets", "📟"),
            ("hensoldt-steffi.fritz.box", "computer", "Computers & Laptops", "💻"),
            ("edgy0020071074.fritz.box", "energy", "Solar & Energy Systems", "☀️"),
            ("lwip0.fritz.box", "energy", "Solar & Energy Systems", "☀️"),
            ("Centauri-Carbon.fritz.box", "3d-printer", "3D Printers & Makers", "🧊"),
            ("iphonestephan.fritz.box", "vpn", "VPN & Virtual Devices", "🛡️"),
            ("wireguard-client.fritz.box", "vpn", "VPN & Virtual Devices", "🛡️"),
        ];

        for (host, expected_cat, expected_title, expected_icon) in cases {
            let vendor = if host.contains("repeater") {
                Some("AVM Fritz!Box")
            } else if host.contains("hensoldt") {
                Some("HP Inc.")
            } else if host.contains("wireguard") || host.contains("iphonestephan") {
                Some("WireGuard / FRITZ!Box VPN")
            } else {
                None
            };
            let (cat, title, icon, _) = classify_with_engine(
                &engine,
                host,
                "192.168.178.100",
                None,
                Some(host),
                vendor,
                "en0",
            );
            assert_eq!(cat, expected_cat, "Mismatch for {host}: got {cat} expected {expected_cat}");
            assert_eq!(title.as_deref(), Some(expected_title), "Title mismatch for {host}");
            assert_eq!(icon.as_deref(), Some(expected_icon), "Icon mismatch for {host}");
        }
    }

    #[test]
    fn parses_arp_filters_multicast_and_broadcast() {
        let arp_output = r#"
? (192.168.178.1) at b4:fc:7d:53:b2:89 on en0 ifscope [ethernet]
? (224.0.0.251) at 1:0:5e:0:0:fb on en0 ifscope [ethernet]
? (239.255.255.250) at 1:0:5e:7f:ff:fa on en0 ifscope [ethernet]
? (192.168.178.255) at ff:ff:ff:ff:ff:ff on en0 ifscope [ethernet]
? (192.168.178.202) at b4:fc:7d:53:b2:89 on en0 ifscope [ethernet]
"#;
        let obs = parse_macos_arp(arp_output);
        assert_eq!(obs.len(), 2, "Multicast and broadcast IPs must be filtered out");
        assert_eq!(obs[0].ip, "192.168.178.1");
        assert_eq!(obs[1].ip, "192.168.178.202");
    }

    #[test]
    fn detects_proxy_arp_vpn_peer() {
        let catalog = homenode_definitions::CatalogDatabase::load_from_path("../../definitions/catalog.json")
            .expect("bundle catalog");
        let engine = homenode_definitions::RhaiDeviceEngine::new();

        let obs = vec![
            RawObservation {
                ip: "192.168.178.1".to_string(),
                mac: Some("b4:fc:7d:53:b2:89".to_string()),
                interface: "en0".to_string(),
                hostname: Some("fritz.box".to_string()),
                source: "arp".to_string(),
                is_active_hint: None,
            },
            RawObservation {
                ip: "192.168.178.202".to_string(),
                mac: Some("b4:fc:7d:53:b2:89".to_string()),
                interface: "en0".to_string(),
                hostname: None,
                source: "arp".to_string(),
                is_active_hint: None,
            },
        ];

        let devices = aggregate_and_classify(obs, &engine, &catalog);
        assert_eq!(devices.len(), 2);

        // Router
        let router = &devices[0];
        assert_eq!(router.ip, "192.168.178.1");
        assert_eq!(router.kind, "router");

        // VPN Peer
        let vpn = &devices[1];
        assert_eq!(vpn.ip, "192.168.178.202");
        assert_eq!(vpn.kind, "vpn");
        assert_eq!(vpn.category_title.as_deref(), Some("VPN & Virtual Devices"));
        assert_eq!(vpn.category_icon.as_deref(), Some("🛡️"));
        assert_eq!(vpn.interface, "vpn");
        assert_eq!(vpn.display_name, "iPhone Stephan (WireGuard VPN)");
        assert_eq!(vpn.product_id.as_deref(), Some("wireguard_vpn_peer"));
    }

    #[test]
    fn resolves_vendor_and_product_from_catalog() {
        let catalog = homenode_definitions::CatalogDatabase::load_from_path("../../definitions/catalog.json")
            .expect("bundle catalog");
        let engine = homenode_definitions::RhaiDeviceEngine::new();

        let obs = vec![
            RawObservation {
                ip: "192.168.178.95".to_string(),
                mac: Some("c4:5b:be:aa:bb:cc".to_string()),
                interface: "en0".to_string(),
                hostname: Some("shellypro3em.fritz.box".to_string()),
                source: "arp".to_string(),
                is_active_hint: None,
            },
            RawObservation {
                ip: "192.168.178.153".to_string(),
                mac: Some("4c:cf:7c:ca:69:be".to_string()),
                interface: "en0".to_string(),
                hostname: Some("hensoldt-steffi.fritz.box".to_string()),
                source: "arp".to_string(),
                is_active_hint: None,
            },
            RawObservation {
                ip: "192.168.178.154".to_string(),
                mac: Some("98:a4:4e:11:22:33".to_string()),
                interface: "en0".to_string(),
                hostname: Some("ipadair.fritz.box".to_string()),
                source: "arp".to_string(),
                is_active_hint: None,
            },
        ];

        let devices = aggregate_and_classify(obs, &engine, &catalog);
        assert_eq!(devices.len(), 3);

        let dev0 = &devices[0];
        assert_eq!(dev0.vendor_id.as_deref(), Some("shelly"));
        assert!(dev0.vendor.as_deref().unwrap().contains("Shelly"));
        assert_eq!(dev0.product_id.as_deref(), Some("shelly_pro_3em"));
        assert_eq!(dev0.product_name.as_deref(), Some("Shelly Pro 3EM"));

        let dev1 = &devices[1];
        assert_eq!(dev1.vendor_id.as_deref(), Some("hp"));
        assert_eq!(dev1.vendor.as_deref(), Some("HP Inc."));
        assert_eq!(dev1.product_id.as_deref(), Some("hp_business_laptop"));
        assert_eq!(dev1.product_name.as_deref(), Some("HP EliteBook / ProBook Laptop"));

        let dev2 = &devices[2];
        assert_eq!(dev2.vendor_id.as_deref(), Some("apple"));
        assert_eq!(dev2.product_id.as_deref(), Some("apple_ipad_air"));
        assert_eq!(dev2.product_name.as_deref(), Some("Apple iPad Air"));
    }

    #[test]
    fn classifies_l2_and_managed_switches() {
        let catalog = homenode_definitions::CatalogDatabase::load_from_path("../../definitions/catalog.json")
            .expect("bundle catalog");
        let mut engine = homenode_definitions::RhaiDeviceEngine::new();
        let _ = engine.load_from_dir("../../definitions/devices");

        let obs = vec![
            RawObservation {
                ip: "".to_string(),
                mac: Some("FA:CE:48:D1:6F:B1".to_string()),
                interface: "lan".to_string(),
                hostname: Some("Switch".to_string()),
                source: "fritzbox-tr064".to_string(),
                is_active_hint: Some(false),
            },
            RawObservation {
                ip: "".to_string(),
                mac: Some("FA:CE:00:B9:8C:9E".to_string()),
                interface: "lan".to_string(),
                hostname: Some("Switch".to_string()),
                source: "fritzbox-tr064".to_string(),
                is_active_hint: Some(true),
            },
            RawObservation {
                ip: "192.168.178.4".to_string(),
                mac: Some("b0:b9:8a:6f:b3:2e".to_string()),
                interface: "en0".to_string(),
                hostname: Some("switch-og".to_string()),
                source: "fritzbox-tr064".to_string(),
                is_active_hint: Some(true),
            },
        ];

        let devices = aggregate_and_classify(obs, &engine, &catalog);
        assert_eq!(devices.len(), 3);

        let sw_managed = &devices[0]; // 192.168.178.4 comes first in IP sorting
        assert_eq!(sw_managed.kind, "switch");
        assert_eq!(sw_managed.category_title.as_deref(), Some("Network Switches"));
        assert_eq!(sw_managed.category_icon.as_deref(), Some("🔀"));
        assert_eq!(sw_managed.display_name, "switch-og");
        assert_eq!(sw_managed.vendor_id.as_deref(), Some("ubiquiti"));

        let sw_l2_1 = devices.iter().find(|d| d.mac.as_deref() == Some("FA:CE:48:D1:6F:B1")).unwrap();
        assert_eq!(sw_l2_1.kind, "switch");
        assert_eq!(sw_l2_1.category_title.as_deref(), Some("Network Switches"));
        assert_eq!(sw_l2_1.category_icon.as_deref(), Some("🔀"));
        assert_eq!(sw_l2_1.is_active, Some(false));

        let sw_l2_2 = devices.iter().find(|d| d.mac.as_deref() == Some("FA:CE:00:B9:8C:9E")).unwrap();
        assert_eq!(sw_l2_2.kind, "switch");
        assert_eq!(sw_l2_2.category_title.as_deref(), Some("Network Switches"));
        assert_eq!(sw_l2_2.category_icon.as_deref(), Some("🔀"));
        assert_eq!(sw_l2_2.is_active, Some(true));
    }
}
