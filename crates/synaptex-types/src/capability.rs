use serde::{Deserialize, Serialize};

/// Capabilities a device can advertise to the core.
/// The registry validates commands against this list before dispatch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Capability {
    Power,
    Dimmer    { min: u16, max: u16 },
    ColorTemp { min_k: u16, max_k: u16 },
    Rgb,
    Switch    { index: u8 },
}

/// Commands the core may dispatch to a plugin.
#[derive(Debug, Clone)]
pub enum DeviceCommand {
    SetPower(bool),
    SetBrightness(u16),
    SetColorTemp(u16),
    SetRgb(u8, u8, u8),
    SetSwitch { index: u8, state: bool },
}

impl DeviceCommand {
    /// Returns true if this command requires the given capability.
    pub fn requires(&self, cap: &Capability) -> bool {
        match (self, cap) {
            (DeviceCommand::SetPower(_),     Capability::Power)        => true,
            (DeviceCommand::SetBrightness(_), Capability::Dimmer { .. }) => true,
            (DeviceCommand::SetColorTemp(_), Capability::ColorTemp { .. }) => true,
            (DeviceCommand::SetRgb(_, _, _), Capability::Rgb)          => true,
            (DeviceCommand::SetSwitch { index, .. }, Capability::Switch { index: cap_idx }) => {
                index == cap_idx
            }
            _ => false,
        }
    }
}
