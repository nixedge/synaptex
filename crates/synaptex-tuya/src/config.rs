/// Serializable Tuya device configuration stored in the sled `configs` tree.
use std::net::IpAddr;

use serde::{Deserialize, Serialize};
use synaptex_types::device::DeviceId;

use crate::dp_map::DpMap;

/// Everything synaptex-core needs to construct a `TuyaPlugin` for a device.
/// Persisted via postcard in the sled `configs` tree, keyed by `DeviceId`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TuyaDeviceConfig {
    pub device_id:  DeviceId,
    pub ip:         IpAddr,
    /// Tuya local API port — almost always 6668.
    pub port:       u16,
    /// Tuya cloud device ID (e.g. `"bfabc123456789012345"`).
    pub tuya_id:    String,
    /// 16-character ASCII string from the Tuya API.
    pub local_key:  String,
    /// Named DP profile: "bulb_a" | "bulb_b" | "switch" | "fan" | "ir1" | "ir2" | "custom".
    /// When "custom" (or empty), `dp_map_override` is used if present.
    pub dp_profile:     String,
    /// Per-device DP map override.  Takes precedence over `dp_profile` when `Some`.
    pub dp_map:         Option<DpMap>,
    /// Protocol version hint from discovery ("3.3" | "3.4" | "3.5").
    /// When set, skips the dual-probe and connects with this version directly.
    #[serde(default)]
    pub protocol_version:  Option<String>,
}

impl TuyaDeviceConfig {
    /// Resolve the effective `DpMap` for this device.
    ///
    /// Resolution order:
    /// 1. `dp_map` override (if `Some`)
    /// 2. Preset named by `dp_profile`
    /// 3. `DpMap::default()` (Type-B bulb)
    pub fn dp_map(&self) -> DpMap {
        if let Some(ref m) = self.dp_map {
            return m.clone();
        }
        if !self.dp_profile.is_empty() {
            return DpMap::from_profile(&self.dp_profile);
        }
        DpMap::default()
    }
}
