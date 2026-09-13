use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum DedupKey {
    PacketId { mac: String, packet_id: u8 },
    PayloadHash { mac: String, hash: u64 },
}

#[derive(Debug, Clone)]
struct DedupRecord {
    seen_at: Instant,
    best_rssi: i16,
    gateway: String,
}

pub struct BTHomeDedup {
    window: Duration,
    records: Mutex<HashMap<DedupKey, DedupRecord>>,
}

impl BTHomeDedup {
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            records: Mutex::new(HashMap::new()),
        }
    }

    /// Checks whether an incoming BLE broadcast is a duplicate from another gateway.
    /// Returns `true` if duplicate (already seen within window), `false` if fresh/new.
    pub fn is_duplicate(
        &self,
        mac: &str,
        packet_id: Option<u8>,
        raw_payload: &[u8],
        rssi: i16,
        gateway: &str,
    ) -> bool {
        let norm_mac = mac.trim().to_lowercase();
        let key = match packet_id {
            Some(pid) => DedupKey::PacketId {
                mac: norm_mac,
                packet_id: pid,
            },
            None => {
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                raw_payload.hash(&mut hasher);
                DedupKey::PayloadHash {
                    mac: norm_mac,
                    hash: hasher.finish(),
                }
            }
        };

        let now = Instant::now();
        let mut guard = self.records.lock().unwrap();

        // Prune entries older than 2x window to keep memory footprint minimal
        let prune_threshold = self.window * 2;
        guard.retain(|_, v| now.duration_since(v.seen_at) < prune_threshold);

        if let Some(entry) = guard.get_mut(&key) {
            if now.duration_since(entry.seen_at) < self.window {
                // Update best RSSI / gateway if this gateway heard it louder
                if rssi > entry.best_rssi {
                    entry.best_rssi = rssi;
                    entry.gateway = gateway.to_string();
                }
                return true;
            }
        }

        guard.insert(
            key,
            DedupRecord {
                seen_at: now,
                best_rssi: rssi,
                gateway: gateway.to_string(),
            },
        );
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deduplicates_same_packet_across_multiple_gateways() {
        let dedup = BTHomeDedup::new(Duration::from_millis(500));
        let mac = "38:39:8f:11:22:33";
        let payload = [0x40, 0x00, 0x12, 0x01, 0x64]; // packet_id = 0x12

        // First gateway reports packet
        assert!(!dedup.is_duplicate(mac, Some(0x12), &payload, -75, "shelly-living-room"));

        // Second gateway reports exact same packet 50ms later
        assert!(dedup.is_duplicate(mac, Some(0x12), &payload, -65, "shelly-hallway"));

        // Third gateway reports exact same packet 100ms later
        assert!(dedup.is_duplicate(mac, Some(0x12), &payload, -80, "shelly-kitchen"));
    }

    #[test]
    fn test_accepts_next_packet_id_immediately() {
        let dedup = BTHomeDedup::new(Duration::from_millis(500));
        let mac = "38:39:8f:11:22:33";
        let payload1 = [0x40, 0x00, 0x12, 0x01, 0x64];
        let payload2 = [0x40, 0x00, 0x13, 0x01, 0x64];

        assert!(!dedup.is_duplicate(mac, Some(0x12), &payload1, -70, "shelly-living-room"));
        // Incrementing packet_id arrives immediately
        assert!(!dedup.is_duplicate(mac, Some(0x13), &payload2, -70, "shelly-living-room"));
    }

    #[test]
    fn test_accepts_different_mac_with_same_packet_id() {
        let dedup = BTHomeDedup::new(Duration::from_millis(500));
        let payload = [0x40, 0x00, 0x05, 0x01, 0x64];

        assert!(!dedup.is_duplicate("aa:bb:cc:01:02:03", Some(0x05), &payload, -70, "shelly-1"));
        assert!(!dedup.is_duplicate("aa:bb:cc:04:05:06", Some(0x05), &payload, -70, "shelly-1"));
    }

    #[test]
    fn test_deduplicates_without_packet_id_via_payload_hash() {
        let dedup = BTHomeDedup::new(Duration::from_millis(500));
        let mac = "aa:bb:cc:01:02:03";
        let payload = [0x40, 0x01, 0x64, 0x02, 0x10, 0x09];

        assert!(!dedup.is_duplicate(mac, None, &payload, -70, "shelly-1"));
        assert!(dedup.is_duplicate(mac, None, &payload, -65, "shelly-2"));
    }
}
