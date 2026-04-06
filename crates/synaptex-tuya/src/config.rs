/// Serializable Tuya device configuration stored in the sled `configs` tree.
use std::net::IpAddr;

use serde::{Deserialize, Serialize};
use synaptex_types::device::DeviceId;

use crate::dp_map::DpMap;

/// Everything synaptex-core needs to construct a `TuyaPlugin` for a device.
/// Persisted via postcard in the sled `configs` tree, keyed by `DeviceId`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TuyaDeviceConfig {
    pub device_id: DeviceId,
    pub ip:        IpAddr,
    /// Tuya local API port — almost always 6668.
    pub port:      u16,
    /// Tuya cloud device ID (e.g. `"bfabc123456789012345"`).
    /// Used as the `devId` field in every JSON payload sent to the device.
    pub tuya_id:   String,
    /// 16-character ASCII string obtained from the Tuya API.
    /// The raw bytes of this string are the AES-128 key.
    pub local_key: String,
    /// `None` → use `DpMap::default()` (covers most common bulb/switch firmware).
    pub dp_map:    Option<DpMap>,
}

impl TuyaDeviceConfig {
    pub fn dp_map(&self) -> DpMap {
        self.dp_map.clone().unwrap_or_default()
    }
}
