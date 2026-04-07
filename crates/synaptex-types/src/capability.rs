use serde::{Deserialize, Serialize};

/// Capabilities a device can advertise to the core.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Capability {
    Power,
    Dimmer    { min: u16, max: u16 },
    ColorTemp { min_k: u16, max_k: u16 },
    Rgb,
    Switch    { index: u8 },
    Fan,
    Ir,
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
}

impl DeviceCommand {
    pub fn requires(&self, cap: &Capability) -> bool {
        match (self, cap) {
            (DeviceCommand::SetPower(_),      Capability::Power)          => true,
            (DeviceCommand::SetBrightness(_), Capability::Dimmer { .. })  => true,
            (DeviceCommand::SetColorTemp(_),  Capability::ColorTemp { .. }) => true,
            (DeviceCommand::SetRgb(_, _, _),  Capability::Rgb)            => true,
            (DeviceCommand::SetSwitch { index, .. }, Capability::Switch { index: cap_idx }) => {
                index == cap_idx
            }
            (DeviceCommand::SendIr { .. }, Capability::Ir) => true,
            // SetDpBool/Int/Str and fan commands are pass-through; no capability gate.
            _ => false,
        }
    }
}
