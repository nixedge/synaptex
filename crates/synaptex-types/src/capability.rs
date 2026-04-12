use serde::{Deserialize, Serialize};

/// Universal fan speed levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FanSpeed {
    Off,
    Low,
    Medium,
    High,
}

/// Capabilities a device can advertise to the core.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Capability {
    /// Main device power (on/off).  All devices have this.
    Power,
    Dimmer    { min: u16, max: u16 },
    ColorTemp { min_k: u16, max_k: u16 },
    Rgb,
    Switch    { index: u8 },
    Fan,
    Ir,
    /// Separately controlled on/off light component (e.g. fan+light combo).
    /// When present, `SetPower` targets this light rather than the main power DP.
    Light,
    /// Thermostat: read current temp, set target temp.
    /// Temperature values are in the device's native unit (usually °F or °C).
    Thermostat { min: u16, max: u16 },
}

/// Commands the core may dispatch to a plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DeviceCommand {
    SetPower(bool),
    SetBrightness(u16),
    SetColorTemp(u16),
    SetRgb(u8, u8, u8),
    SetSwitch { index: u8, state: bool },
    /// Write a raw boolean DP (for generic devices).
    SetDpBool { dp: u16, value: bool },
    /// Write a raw integer DP (for generic devices).
    SetDpInt  { dp: u16, value: i64 },
    /// Write a raw string DP (for generic devices).
    SetDpStr  { dp: u16, value: String },
    /// Send an IR code.  `head` is an optional device header string;
    /// `key` is the IR key code.
    SendIr { head: Option<String>, key: String },
    /// Set fan speed.  `Off` also cuts power to the fan.
    SetFanSpeed(FanSpeed),
    /// Set the target/set-point temperature (device-native unit).
    SetTargetTemp(u16),
    /// Patch any combination of light attributes in a single command.
    /// Fields that are `None` are left unchanged on the device.
    SetLight {
        power:      Option<bool>,
        brightness: Option<u16>,
        color_temp: Option<u16>,
        /// RGB as (r, g, b) 0–255.
        rgb:        Option<(u8, u8, u8)>,
        /// Mode override: `"white"` or `"colour"`.  When `None` the plugin
        /// auto-derives the mode from which fields are set.
        mode: Option<String>,
    },
}

impl DeviceCommand {
    pub fn requires(&self, cap: &Capability) -> bool {
        match (self, cap) {
            (DeviceCommand::SetPower(_),      Capability::Power)          => true,
            (DeviceCommand::SetPower(_),      Capability::Light)          => true,
            (DeviceCommand::SetBrightness(_), Capability::Dimmer { .. })  => true,
            (DeviceCommand::SetColorTemp(_),  Capability::ColorTemp { .. }) => true,
            (DeviceCommand::SetRgb(_, _, _),  Capability::Rgb)            => true,
            (DeviceCommand::SetSwitch { index, .. }, Capability::Switch { index: cap_idx }) => {
                index == cap_idx
            }
            (DeviceCommand::SendIr { .. },        Capability::Ir)  => true,
            (DeviceCommand::SetFanSpeed(_),       Capability::Fan) => true,
            (DeviceCommand::SetTargetTemp(_),     Capability::Thermostat { .. }) => true,
            // SetLight targets any device with a power or light DP.
            (DeviceCommand::SetLight { .. },    Capability::Power) => true,
            (DeviceCommand::SetLight { .. },    Capability::Light) => true,
            // SetDpBool/Int/Str are pass-through; no capability gate.
            _ => false,
        }
    }
}
