use serde::{Deserialize, Serialize};
use synaptex_types::device::DeviceId;

// ─── Persisted configs ───────────────────────────────────────────────────────

/// Account-level config stored in the `config` sled tree under key `mysa_account`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MysaAccountConfig {
    pub username: String,
    pub password: String,
}

/// Per-device config stored in the `configs` sled tree as `PluginConfig::Mysa`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MysaConfig {
    /// Stable synaptex device ID derived from SHA-256(mysa_id)[0..6] with LA bit.
    pub device_id:    DeviceId,
    /// Lowercase MAC address with no colons (e.g. "aabbcc112233").
    pub mysa_id:      String,
    pub name:         String,
    /// Device model string, e.g. "BB-V1-0".
    pub model:        String,
    /// Minimum setpoint in tenths of °C (e.g. 50 = 5.0°C).
    pub min_setpoint: u16,
    /// Maximum setpoint in tenths of °C (e.g. 300 = 30.0°C).
    pub max_setpoint: u16,
}

// ─── Hot-cache state ─────────────────────────────────────────────────────────

/// Current device state held in the in-memory cache.
#[derive(Debug, Clone)]
pub struct MysaDeviceState {
    /// Current ambient temperature in tenths of °C.
    pub temp_current: u16,
    /// Target setpoint in tenths of °C.
    pub temp_set:     u16,
    /// Heating mode: 0 = off, 3 = heat.
    pub mode:         u8,
}

// ─── REST wire types ─────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct DeviceListWrapper {
    pub data: Option<Vec<MysaDeviceInfo>>,
}

#[derive(Debug, Deserialize)]
pub struct MysaDeviceInfo {
    pub id:   String,
    pub name: String,
    #[serde(rename = "productType", default)]
    pub product_type: String,
    #[serde(rename = "setpointMin")]
    pub setpoint_min: Option<f32>,
    #[serde(rename = "setpointMax")]
    pub setpoint_max: Option<f32>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct MysaRawState {
    pub temperature:  Option<f32>,
    #[serde(rename = "setPoint")]
    pub set_point:    Option<f32>,
    #[serde(rename = "heatingMode")]
    pub heating_mode: Option<u8>,
}

// ─── MQTT wire types ─────────────────────────────────────────────────────────

/// Envelope for MsgType 44 messages on `/v1/dev/{id}/out`.
#[derive(Debug, Deserialize)]
pub struct MqttOutMsg {
    #[serde(rename = "msgType")]
    pub msg_type: u8,
    #[serde(default)]
    pub body:     MqttOutBody,
}

#[derive(Debug, Default, Deserialize)]
pub struct MqttOutBody {
    pub state: Option<MqttState>,
}

#[derive(Debug, Deserialize)]
pub struct MqttState {
    #[serde(rename = "heatingMode")]
    pub heating_mode: Option<u8>,
    /// Can be integer (centidegrees) or float (°C).
    pub temperature:  Option<serde_json::Value>,
    #[serde(rename = "setPoint")]
    pub set_point:    Option<serde_json::Value>,
}

// ─── Temperature conversion ──────────────────────────────────────────────────

/// Convert an MQTT/REST temperature value (float °C) to tenths of °C.
pub fn rest_temp_to_tenths(celsius: f32) -> u16 {
    (celsius * 10.0).round() as u16
}

/// Convert an MQTT numeric temperature to tenths of °C.
/// Values > 100 are treated as centidegrees (÷10); ≤100 as whole °C (×10).
pub fn mqtt_temp_to_tenths(value: u16) -> u16 {
    if value > 100 {
        value / 10
    } else {
        value * 10
    }
}

/// Convert a serde_json::Value temperature (int centidegrees or float °C) to tenths of °C.
pub fn json_temp_to_tenths(v: &serde_json::Value) -> Option<u16> {
    if let Some(n) = v.as_u64() {
        Some(mqtt_temp_to_tenths(n as u16))
    } else if let Some(f) = v.as_f64() {
        Some((f * 10.0).round() as u16)
    } else {
        None
    }
}
