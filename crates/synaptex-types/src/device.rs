use std::fmt;

use serde::{Deserialize, Serialize};

use crate::capability::Capability;

/// Unique device identifier encoded as a 6-byte MAC address.
/// Fixed-size and hashable — converted to/from string only at the gRPC boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeviceId(pub [u8; 6]);

impl DeviceId {
    /// Parse a canonical MAC string `"AA:BB:CC:DD:EE:FF"`.
    pub fn from_mac_str(s: &str) -> Result<Self, String> {
        let parts: Vec<&str> = s.split(':').collect();
        if parts.len() != 6 {
            return Err(format!("invalid MAC address (expected 6 octets): {s}"));
        }
        let mut bytes = [0u8; 6];
        for (i, part) in parts.iter().enumerate() {
            bytes[i] = u8::from_str_radix(part, 16)
                .map_err(|_| format!("invalid hex octet '{part}' in: {s}"))?;
        }
        Ok(DeviceId(bytes))
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let b = &self.0;
        write!(
            f,
            "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
            b[0], b[1], b[2], b[3], b[4], b[5]
        )
    }
}

/// Static device metadata persisted in the `registry` sled tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub id:           DeviceId,
    pub name:         String,
    pub model:        String,
    /// Protocol identifier, e.g. `"tuya_local_3.3"`.
    pub protocol:     String,
    pub capabilities: Vec<Capability>,
}
