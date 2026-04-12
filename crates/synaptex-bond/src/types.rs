use serde::{Deserialize, Serialize};
use synaptex_types::device::DeviceId;

/// Per-device config stored in the `configs` sled tree as `PluginConfig::Bond`.
/// Each Bond bridge sub-device (fan, fireplace, etc.) gets its own entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BondConfig {
    /// Stable synaptex device ID — derived deterministically from hub MAC +
    /// Bond device ID so it survives restarts.
    pub device_id: DeviceId,

    // ── Hub-level ────────────────────────────────────────────────────────────
    /// MAC address of the Bond bridge hub (hub registration key).
    pub hub_mac: String,
    /// IP to use when connecting to the Bond bridge.
    /// Stored as the router-allocated managed IP for stability post-DHCP-renewal.
    pub hub_ip: String,
    /// BOND-Token header value for local API auth.
    pub bond_token: String,

    // ── Sub-device ───────────────────────────────────────────────────────────
    /// Bond's own opaque device ID (e.g. "aabbccdd").
    pub bond_device_id: String,
    /// Bond device type: "CF" (ceiling fan), "FP" (fireplace), "GX" (generic), etc.
    pub device_type: String,
    /// Human-readable name from the Bond bridge.
    pub name: String,
    /// Actions supported by this device (used for capability mapping).
    pub actions: Vec<String>,
    /// Maximum fan speed reported by the Bond bridge (CF devices only).
    /// Used to map Low/Medium/High proportionally.  Default 3.
    #[serde(default = "default_max_speed")]
    pub max_speed: u8,
}

fn default_max_speed() -> u8 { 3 }

/// Info returned by `GET /v2/devices/{id}`.
#[derive(Debug, Clone)]
pub struct BondDeviceInfo {
    pub id:          String,
    pub name:        String,
    pub device_type: String,
    pub actions:     Vec<String>,
    pub max_speed:   u8,
}

/// State returned by `GET /v2/devices/{id}/state`.
#[derive(Debug, Clone, Deserialize)]
pub struct BondDeviceState {
    #[serde(default)]
    pub power: u8,
    #[serde(default)]
    pub speed: u8,
    #[serde(default)]
    pub light: u8,
}
