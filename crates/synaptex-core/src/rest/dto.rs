use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use synaptex_types::{
    capability::{Capability, DeviceCommand, FanSpeed},
    device::DeviceInfo,
    plugin::DeviceState,
};

use crate::{
    db::{Routine, RoutineStep, RoutineTarget},
    tuya_cloud::CloudDevice,
};

// ─── Device ──────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
pub struct DeviceDto {
    pub mac:          String,
    pub name:         String,
    pub model:        String,
    pub protocol:     String,
    pub ip:           Option<String>,
    /// Tuya local protocol version ("3.3" | "3.4" | "3.5"), None for group devices.
    pub tuya_version: Option<String>,
    pub capabilities: Vec<CapabilityDto>,
    pub state:        Option<DeviceStateDto>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DeviceStateDto {
    pub online:        bool,
    pub updated_at_ms: u64,
    pub power:         Option<bool>,
    pub brightness:    Option<u16>,
    pub color_temp_k:  Option<u16>,
    pub rgb:           Option<[u8; 3]>,
    pub switches:      HashMap<u8, bool>,
    pub fan_speed:     Option<FanSpeed>,
    pub temp_current:     Option<u16>,
    pub temp_set:         Option<u16>,
    pub temp_calibration: Option<i16>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CapabilityDto {
    Power,
    Light,
    Dimmer   { min: u16, max: u16 },
    ColorTemp { min_k: u16, max_k: u16 },
    Rgb,
    Switch   { index: u8 },
    Fan,
    Ir,
    Thermostat { min: u16, max: u16 },
}

impl From<&Capability> for CapabilityDto {
    fn from(c: &Capability) -> Self {
        match c {
            Capability::Power                    => CapabilityDto::Power,
            Capability::Light                    => CapabilityDto::Light,
            Capability::Dimmer { min, max }      => CapabilityDto::Dimmer { min: *min, max: *max },
            Capability::ColorTemp { min_k, max_k } =>
                CapabilityDto::ColorTemp { min_k: *min_k, max_k: *max_k },
            Capability::Rgb                      => CapabilityDto::Rgb,
            Capability::Switch { index }         => CapabilityDto::Switch { index: *index },
            Capability::Fan                      => CapabilityDto::Fan,
            Capability::Ir                       => CapabilityDto::Ir,
            Capability::Thermostat { min, max }  => CapabilityDto::Thermostat { min: *min, max: *max },
        }
    }
}

pub fn device_dto(info: &DeviceInfo, state: Option<DeviceState>, ip: Option<String>, tuya_version: Option<String>) -> DeviceDto {
    DeviceDto {
        mac:          info.id.to_string(),
        name:         info.name.clone(),
        model:        info.model.clone(),
        protocol:     info.protocol.clone(),
        ip,
        tuya_version,
        capabilities: info.capabilities.iter().map(CapabilityDto::from).collect(),
        state:        state.map(|s| DeviceStateDto {
            online:        s.online,
            updated_at_ms: s.updated_at_ms,
            power:         s.power,
            brightness:    s.brightness,
            color_temp_k:  s.color_temp_k,
            rgb:           s.rgb.map(|(r, g, b)| [r, g, b]),
            switches:      s.switches,
            fan_speed:     s.fan_speed,
            temp_current:     s.temp_current,
            temp_set:         s.temp_set,
            temp_calibration: s.temp_calibration,
        }),
    }
}

// ─── Command ─────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CommandDto {
    SetPower      { on: bool },
    SetBrightness { level: u16 },
    SetColorTemp  { kelvin: u16 },
    SetRgb        { r: u8, g: u8, b: u8 },
    SetSwitch     { index: u8, on: bool },
    SetFanSpeed   { speed: FanSpeed },
    SetTargetTemp { temp: u16 },
    SendIr        { key: String, #[serde(default)] head: Option<String> },
    SetDp {
        dp:       u16,
        bool_val: Option<bool>,
        int_val:  Option<i64>,
        str_val:  Option<String>,
    },
    /// Patch-style light command — only the `Some` fields are applied.
    SetLight {
        #[serde(default)] power:      Option<bool>,
        #[serde(default)] brightness: Option<u16>,
        /// Colour temperature in Kelvin.
        #[serde(default)] color_temp: Option<u16>,
        #[serde(default)] r:          Option<u8>,
        #[serde(default)] g:          Option<u8>,
        #[serde(default)] b:          Option<u8>,
        /// Mode override: `"white"` | `"colour"`.
        #[serde(default)] color_mode: Option<String>,
    },
}

impl TryFrom<CommandDto> for DeviceCommand {
    type Error = &'static str;

    fn try_from(dto: CommandDto) -> Result<Self, Self::Error> {
        Ok(match dto {
            CommandDto::SetPower      { on }               => DeviceCommand::SetPower(on),
            CommandDto::SetBrightness { level }            => DeviceCommand::SetBrightness(level),
            CommandDto::SetColorTemp  { kelvin }           => DeviceCommand::SetColorTemp(kelvin),
            CommandDto::SetRgb        { r, g, b }          => DeviceCommand::SetRgb(r, g, b),
            CommandDto::SetSwitch     { index, on }        =>
                DeviceCommand::SetSwitch { index, state: on },
            CommandDto::SetFanSpeed   { speed }            => DeviceCommand::SetFanSpeed(speed),
            CommandDto::SetTargetTemp { temp }             => DeviceCommand::SetTargetTemp(temp),
            CommandDto::SendIr        { key, head }        => DeviceCommand::SendIr { head, key },

            CommandDto::SetDp { dp, bool_val: Some(v), .. } =>
                DeviceCommand::SetDpBool { dp, value: v },
            CommandDto::SetDp { dp, int_val:  Some(v), .. } =>
                DeviceCommand::SetDpInt  { dp, value: v },
            CommandDto::SetDp { dp, str_val:  Some(v), .. } =>
                DeviceCommand::SetDpStr  { dp, value: v },
            CommandDto::SetDp { .. } =>
                return Err("set_dp requires exactly one of bool_val, int_val, str_val"),
            CommandDto::SetLight { power, brightness, color_temp, r, g, b, color_mode } => {
                let rgb = match (r, g, b) {
                    (Some(r), Some(g), Some(b)) => Some((r, g, b)),
                    _                           => None,
                };
                DeviceCommand::SetLight { power, brightness, color_temp, rgb, color_mode }
            }
        })
    }
}

// ─── RegisterBody ────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct RegisterBody {
    pub mac:        String,
    pub name:       String,
    pub ip:         String,
    pub tuya_id:    String,
    pub local_key:  String,
    #[serde(default)] pub model:      Option<String>,
    #[serde(default)] pub port:       Option<u16>,
    #[serde(default)] pub dp_profile: Option<String>,
}

// ─── Group ───────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
pub struct GroupDto {
    pub mac:     String,
    pub name:    String,
    pub model:   String,
    pub members: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateGroupBody {
    pub name:    String,
    #[serde(default)] pub model:   Option<String>,
    pub members: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct PatchGroupBody {
    pub name:    Option<String>,
    pub members: Option<Vec<String>>,
}

// ─── Room ────────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
pub struct RoomDto {
    pub id:      String,
    pub name:    String,
    pub devices: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateRoomBody {
    pub name:    String,
    pub devices: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct PatchRoomBody {
    pub name:    Option<String>,
    pub devices: Option<Vec<String>>,
}

// ─── Routine ─────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
pub struct RoutineDto {
    pub id:       String,
    pub name:     String,
    pub schedule: Option<String>,
    pub steps:    Vec<RoutineStepDto>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RoutineStepDto {
    Command { target: RoutineTargetDto, command: CommandDto },
    Wait    { secs: u64 },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RoutineTargetDto {
    Room   { id:  String },
    Device { mac: String },
}

#[derive(Debug, Deserialize)]
pub struct RoutineBody {
    pub name:     String,
    pub schedule: Option<String>,
    pub steps:    Vec<RoutineStepDto>,
}

impl TryFrom<RoutineStepDto> for RoutineStep {
    type Error = String;

    fn try_from(dto: RoutineStepDto) -> Result<Self, Self::Error> {
        Ok(match dto {
            RoutineStepDto::Wait { secs } => RoutineStep::Wait { secs },
            RoutineStepDto::Command { target, command } => {
                let target = match target {
                    RoutineTargetDto::Room { id } => RoutineTarget::Room(id),
                    RoutineTargetDto::Device { mac } => {
                        let id = synaptex_types::device::DeviceId::from_mac_str(&mac)
                            .map_err(|e| e.to_string())?;
                        RoutineTarget::Device(id)
                    }
                };
                let cmd = DeviceCommand::try_from(command).map_err(|e| e.to_string())?;
                RoutineStep::Command { target, command: cmd }
            }
        })
    }
}

impl From<&RoutineStep> for RoutineStepDto {
    fn from(step: &RoutineStep) -> Self {
        match step {
            RoutineStep::Wait { secs } => RoutineStepDto::Wait { secs: *secs },
            RoutineStep::Command { target, command } => {
                let target_dto = match target {
                    RoutineTarget::Room(id) => RoutineTargetDto::Room { id: id.clone() },
                    RoutineTarget::Device(id) =>
                        RoutineTargetDto::Device { mac: id.to_string() },
                };
                let cmd_dto = device_command_to_dto(command);
                RoutineStepDto::Command { target: target_dto, command: cmd_dto }
            }
        }
    }
}

fn device_command_to_dto(cmd: &DeviceCommand) -> CommandDto {
    match cmd {
        DeviceCommand::SetPower(on)         => CommandDto::SetPower { on: *on },
        DeviceCommand::SetBrightness(lvl)   => CommandDto::SetBrightness { level: *lvl },
        DeviceCommand::SetColorTemp(k)      => CommandDto::SetColorTemp { kelvin: *k },
        DeviceCommand::SetRgb(r, g, b)      => CommandDto::SetRgb { r: *r, g: *g, b: *b },
        DeviceCommand::SetSwitch { index, state } =>
            CommandDto::SetSwitch { index: *index, on: *state },
        DeviceCommand::SetFanSpeed(speed)   => CommandDto::SetFanSpeed { speed: *speed },
        DeviceCommand::SetTargetTemp(t)     => CommandDto::SetTargetTemp { temp: *t },
        DeviceCommand::SendIr { head, key } =>
            CommandDto::SendIr { key: key.clone(), head: head.clone() },
        DeviceCommand::SetDpBool { dp, value } =>
            CommandDto::SetDp { dp: *dp, bool_val: Some(*value), int_val: None, str_val: None },
        DeviceCommand::SetDpInt { dp, value } =>
            CommandDto::SetDp { dp: *dp, bool_val: None, int_val: Some(*value), str_val: None },
        DeviceCommand::SetDpStr { dp, value } =>
            CommandDto::SetDp { dp: *dp, bool_val: None, int_val: None, str_val: Some(value.clone()) },
        DeviceCommand::SetLight { power, brightness, color_temp, rgb, color_mode } =>
            CommandDto::SetLight {
                power:      *power,
                brightness: *brightness,
                color_temp: *color_temp,
                r:          rgb.map(|(r, _, _)| r),
                g:          rgb.map(|(_, g, _)| g),
                b:          rgb.map(|(_, _, b)| b),
                color_mode: color_mode.clone(),
            },
    }
}

pub fn routine_to_dto(r: &Routine) -> RoutineDto {
    RoutineDto {
        id:       r.id.clone(),
        name:     r.name.clone(),
        schedule: r.schedule.clone(),
        steps:    r.steps.iter().map(RoutineStepDto::from).collect(),
    }
}

// ─── Config ──────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct ConfigDto {
    pub tuya_cloud:  Option<TuyaCloudInfoDto>,
    pub api_key_set: bool,
}

#[derive(Debug, Serialize)]
pub struct TuyaCloudInfoDto {
    pub client_id: String,
    pub region:    String,
}

#[derive(Debug, Deserialize)]
pub struct SetTuyaCloudBody {
    pub client_id:      String,
    pub client_secret:  String,
    pub region:         String,
    /// Any device ID in the account — used once to resolve the owner UID.
    pub seed_device_id: String,
}

#[derive(Debug, Deserialize)]
pub struct SetApiKeyBody {
    pub key: Option<String>,
}

// ─── Pairing / Cloud devices ─────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct CloudDeviceDto {
    pub id:         String,
    pub name:       String,
    pub category:   String,
    pub product_id: String,
    pub online:     bool,
    pub firmware:   Option<String>,
    pub local_key:  String,
}

impl From<CloudDevice> for CloudDeviceDto {
    fn from(d: CloudDevice) -> Self {
        CloudDeviceDto {
            id:         d.id,
            name:       d.name,
            category:   d.category,
            product_id: d.product_id,
            online:     d.online,
            firmware:   d.firmware,
            local_key:  d.local_key,
        }
    }
}

// ─── Import ───────────────────────────────────────────────────────────────────

/// Summary of a single successfully imported device.
#[derive(Debug, Serialize)]
pub struct ImportedDeviceDto {
    pub mac:        String,
    pub name:       String,
    pub tuya_id:    String,
    pub ip:         String,
    pub dp_profile: String,
}

/// Response from POST /pairing/import.
#[derive(Debug, Serialize)]
pub struct ImportResultDto {
    /// Devices discovered on the local network and registered.
    pub registered:           Vec<ImportedDeviceDto>,
    /// Already-registered devices whose name or local_key changed and were synced.
    pub updated_registration: Vec<ImportedDeviceDto>,
    /// Devices already registered with no changes.
    pub already_registered:   Vec<ImportedDeviceDto>,
    /// Online cloud devices that could not be found or resolved locally.
    pub not_discovered:       Vec<CloudDeviceDto>,
    /// Cloud-only virtual devices skipped (vdevo* IDs, gateway sub-devices with
    /// non-hex suffixes like *mu29/*ayps — these have no local TCP endpoint).
    pub skipped_virtual:      Vec<CloudDeviceDto>,
}
