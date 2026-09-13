use serde::{Deserialize, Serialize};

/// BTHome V2 Service Data 16-bit UUID (0xFCD2, little-endian: 0xD2, 0xFC)
#[allow(dead_code)]
pub const BTHOME_V2_SERVICE_UUID: u16 = 0xFCD2;

/// Measurement Object types specified by BTHome V2
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value")]
pub enum BTHomeMeasurement {
    PacketId(u8),
    Battery(u8),                  // % (0x01)
    Temperature(f32),             // °C, 0.01 factor (0x02)
    Humidity(f32),                // %, 0.01 factor (0x03)
    Pressure(f32),                // hPa, 0.01 factor (0x04)
    Illuminance(f32),             // lux, 0.01 factor (0x05)
    Power(f32),                   // W, 0.01 factor (0x0B)
    Energy(f32),                  // kWh, 0.001 factor (0x0C)
    Voltage(f32),                 // V, 0.001 factor (0x0D)
    Current(f32),                 // A, 0.001 factor (0x0E)
    GenericBoolean(bool),         // true/false (0x0F)
    DoorWindow(bool),             // true=open, false=closed (0x1A)
    Motion(bool),                 // true=detected, false=clear (0x21)
    ButtonEvent(ButtonEventType), // 0x3A (Shelly BLU Button / BTHome)
    Unknown { id: u8, data: Vec<u8> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ButtonEventType {
    None,
    Press,
    DoublePress,
    TriplePress,
    LongPress,
    LongDoublePress,
    LongTriplePress,
    Hold,
    Unknown(u8),
}

impl From<u8> for ButtonEventType {
    fn from(val: u8) -> Self {
        match val {
            0 => ButtonEventType::None,
            1 => ButtonEventType::Press,
            2 => ButtonEventType::DoublePress,
            3 => ButtonEventType::TriplePress,
            4 => ButtonEventType::LongPress,
            5 => ButtonEventType::LongDoublePress,
            6 => ButtonEventType::LongTriplePress,
            128 => ButtonEventType::Hold,
            other => ButtonEventType::Unknown(other),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BTHomePacket {
    pub version: u8,
    pub encrypted: bool,
    pub trigger_based: bool,
    pub measurements: Vec<BTHomeMeasurement>,
}

impl BTHomePacket {
    pub fn packet_id(&self) -> Option<u8> {
        for m in &self.measurements {
            if let BTHomeMeasurement::PacketId(pid) = m {
                return Some(*pid);
            }
        }
        None
    }

    pub fn battery(&self) -> Option<u8> {
        for m in &self.measurements {
            if let BTHomeMeasurement::Battery(b) = m {
                return Some(*b);
            }
        }
        None
    }

    pub fn temperature(&self) -> Option<f32> {
        for m in &self.measurements {
            if let BTHomeMeasurement::Temperature(t) = m {
                return Some(*t);
            }
        }
        None
    }

    pub fn humidity(&self) -> Option<f32> {
        for m in &self.measurements {
            if let BTHomeMeasurement::Humidity(h) = m {
                return Some(*h);
            }
        }
        None
    }

    pub fn illuminance(&self) -> Option<f32> {
        for m in &self.measurements {
            if let BTHomeMeasurement::Illuminance(lux) = m {
                return Some(*lux);
            }
        }
        None
    }

    pub fn door_window(&self) -> Option<bool> {
        for m in &self.measurements {
            if let BTHomeMeasurement::DoorWindow(open) = m {
                return Some(*open);
            }
        }
        None
    }

    pub fn motion(&self) -> Option<bool> {
        for m in &self.measurements {
            if let BTHomeMeasurement::Motion(detected) = m {
                return Some(*detected);
            }
        }
        None
    }

    pub fn button_event(&self) -> Option<ButtonEventType> {
        for m in &self.measurements {
            if let BTHomeMeasurement::ButtonEvent(ev) = m {
                return Some(*ev);
            }
        }
        None
    }
}

/// Parses an unencrypted BTHome V2 payload
pub fn parse_bthome_v2(payload: &[u8]) -> Result<BTHomePacket, &'static str> {
    if payload.is_empty() {
        return Err("Empty BTHome payload");
    }

    let b_ext = payload[0];
    let encrypted = (b_ext & 0x01) != 0;
    let trigger_based = (b_ext & 0x04) != 0;
    let version = (b_ext >> 5) & 0x07;

    if encrypted {
        return Ok(BTHomePacket {
            version,
            encrypted: true,
            trigger_based,
            measurements: Vec::new(),
        });
    }

    let mut measurements = Vec::new();
    let mut idx = 1;

    while idx < payload.len() {
        let obj_id = payload[idx];
        idx += 1;

        match obj_id {
            // Packet ID (uint8)
            0x00 => {
                if idx >= payload.len() {
                    break;
                }
                measurements.push(BTHomeMeasurement::PacketId(payload[idx]));
                idx += 1;
            }
            // Battery % (uint8)
            0x01 => {
                if idx >= payload.len() {
                    break;
                }
                measurements.push(BTHomeMeasurement::Battery(payload[idx]));
                idx += 1;
            }
            // Temperature: sint16 (factor 0.01 °C)
            0x02 => {
                if idx + 2 > payload.len() {
                    break;
                }
                let raw = i16::from_le_bytes([payload[idx], payload[idx + 1]]);
                let val = (raw as f32) * 0.01;
                measurements.push(BTHomeMeasurement::Temperature((val * 100.0).round() / 100.0));
                idx += 2;
            }
            // Humidity: uint16 (factor 0.01 %)
            0x03 => {
                if idx + 2 > payload.len() {
                    break;
                }
                let raw = u16::from_le_bytes([payload[idx], payload[idx + 1]]);
                let val = (raw as f32) * 0.01;
                measurements.push(BTHomeMeasurement::Humidity((val * 100.0).round() / 100.0));
                idx += 2;
            }
            // Pressure: uint24 (factor 0.01 hPa)
            0x04 => {
                if idx + 3 > payload.len() {
                    break;
                }
                let raw = (payload[idx] as u32)
                    | ((payload[idx + 1] as u32) << 8)
                    | ((payload[idx + 2] as u32) << 16);
                let val = (raw as f32) * 0.01;
                measurements.push(BTHomeMeasurement::Pressure((val * 100.0).round() / 100.0));
                idx += 3;
            }
            // Illuminance: uint24 (factor 0.01 lux)
            0x05 => {
                if idx + 3 > payload.len() {
                    break;
                }
                let raw = (payload[idx] as u32)
                    | ((payload[idx + 1] as u32) << 8)
                    | ((payload[idx + 2] as u32) << 16);
                let val = (raw as f32) * 0.01;
                measurements.push(BTHomeMeasurement::Illuminance((val * 100.0).round() / 100.0));
                idx += 3;
            }
            // Power: uint24 (factor 0.01 W)
            0x0B => {
                if idx + 3 > payload.len() {
                    break;
                }
                let raw = (payload[idx] as u32)
                    | ((payload[idx + 1] as u32) << 8)
                    | ((payload[idx + 2] as u32) << 16);
                let val = (raw as f32) * 0.01;
                measurements.push(BTHomeMeasurement::Power(val));
                idx += 3;
            }
            // Energy: uint24 (factor 0.001 kWh)
            0x0C => {
                if idx + 3 > payload.len() {
                    break;
                }
                let raw = (payload[idx] as u32)
                    | ((payload[idx + 1] as u32) << 8)
                    | ((payload[idx + 2] as u32) << 16);
                let val = (raw as f32) * 0.001;
                measurements.push(BTHomeMeasurement::Energy(val));
                idx += 3;
            }
            // Voltage: uint16 (factor 0.001 V)
            0x0D => {
                if idx + 2 > payload.len() {
                    break;
                }
                let raw = u16::from_le_bytes([payload[idx], payload[idx + 1]]);
                let val = (raw as f32) * 0.001;
                measurements.push(BTHomeMeasurement::Voltage(val));
                idx += 2;
            }
            // Generic boolean (uint8: 0 or 1)
            0x0F => {
                if idx >= payload.len() {
                    break;
                }
                measurements.push(BTHomeMeasurement::GenericBoolean(payload[idx] != 0));
                idx += 1;
            }
            // Door / Window contact (uint8: 0 = closed, 1 = open)
            0x1A => {
                if idx >= payload.len() {
                    break;
                }
                measurements.push(BTHomeMeasurement::DoorWindow(payload[idx] != 0));
                idx += 1;
            }
            // Motion (uint8: 0 = clear, 1 = detected)
            0x21 => {
                if idx >= payload.len() {
                    break;
                }
                measurements.push(BTHomeMeasurement::Motion(payload[idx] != 0));
                idx += 1;
            }
            // Button Event (uint8)
            0x3A => {
                if idx >= payload.len() {
                    break;
                }
                measurements.push(BTHomeMeasurement::ButtonEvent(ButtonEventType::from(payload[idx])));
                idx += 1;
            }
            // Fallback for unknown object IDs: read 1 byte
            _ => {
                if idx < payload.len() {
                    measurements.push(BTHomeMeasurement::Unknown {
                        id: obj_id,
                        data: vec![payload[idx]],
                    });
                    idx += 1;
                }
            }
        }
    }

    Ok(BTHomePacket {
        version,
        encrypted: false,
        trigger_based,
        measurements,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_shelly_blu_button_event_and_battery() {
        // BTHome V2 packet:
        // Byte 0: 0x40 (unencrypted, version 2)
        // 0x01, 0x5F -> Battery 95%
        // 0x3A, 0x01 -> Button press (single press)
        let payload = [0x40, 0x01, 0x5F, 0x3A, 0x01];
        let packet = parse_bthome_v2(&payload).expect("Valid payload");
        assert_eq!(packet.version, 2);
        assert!(!packet.encrypted);
        assert_eq!(packet.measurements.len(), 2);
        assert_eq!(packet.measurements[0], BTHomeMeasurement::Battery(95));
        assert_eq!(packet.measurements[1], BTHomeMeasurement::ButtonEvent(ButtonEventType::Press));
    }

    #[test]
    fn parses_shelly_blu_door_window_sensor() {
        // BTHome V2 packet:
        // Byte 0: 0x40
        // 0x01, 0x64 -> Battery 100%
        // 0x1A, 0x01 -> Window Open
        // 0x05, 0xE8, 0x03, 0x00 -> 1000 * 0.01 = 10.00 lux
        let payload = [0x40, 0x01, 0x64, 0x1A, 0x01, 0x05, 0xE8, 0x03, 0x00];
        let packet = parse_bthome_v2(&payload).expect("Valid payload");
        assert_eq!(packet.measurements[0], BTHomeMeasurement::Battery(100));
        assert_eq!(packet.measurements[1], BTHomeMeasurement::DoorWindow(true));
        assert_eq!(packet.measurements[2], BTHomeMeasurement::Illuminance(10.0));
    }

    #[test]
    fn parses_temperature_and_humidity() {
        // Temp: 21.50 °C -> 2150 = 0x0866 -> 0x66, 0x08
        // Humidity: 45.25 % -> 4525 = 0x11AD -> 0xAD, 0x11
        let payload = [0x40, 0x02, 0x66, 0x08, 0x03, 0xAD, 0x11];
        let packet = parse_bthome_v2(&payload).expect("Valid payload");
        assert_eq!(packet.measurements[0], BTHomeMeasurement::Temperature(21.50));
        assert_eq!(packet.measurements[1], BTHomeMeasurement::Humidity(45.25));
    }
}
