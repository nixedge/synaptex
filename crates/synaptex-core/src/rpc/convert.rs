/// Conversions between internal domain types and protobuf-generated types.
use std::{collections::HashMap, net::IpAddr, str::FromStr};

use synaptex_proto::{
    Capability as ProtoCapability,
    CommandStep,
    DeviceId as ProtoDeviceId,
    DeviceInfo as ProtoDeviceInfo,
    DeviceState as ProtoDeviceState,
    RegisterDeviceRequest,
    RgbValue,
    RoutineInfo as ProtoRoutineInfo,
    RoutineStep as ProtoRoutineStep,
    RoomInfo as ProtoRoomInfo,
    SendIrCommand,
    SetDpCommand,
    SwitchCommand,
    TuyaConfig as ProtoTuyaConfig,
    WaitStep,
    command_step::{Command as CsCommand, Target as CsTarget},
    routine_step::Step as ProtoStep,
    send_room_command_request::Command as ProtoRoomCommand,
    set_device_state_request::Command as ProtoCommand,
    set_dp_command::Value as ProtoDpValue,
};
use synaptex_types::{
    capability::{Capability, DeviceCommand},
    device::{DeviceId, DeviceInfo},
    plugin::DeviceState,
};
use synaptex_tuya::TuyaDeviceConfig;
use tonic::Status;

use crate::db::{Room, Routine, RoutineStep, RoutineTarget};

// ─── DeviceId ────────────────────────────────────────────────────────────────

pub fn proto_id_to_internal(id: &ProtoDeviceId) -> Result<DeviceId, Status> {
    DeviceId::from_mac_str(&id.mac)
        .map_err(|e| Status::invalid_argument(e))
}

pub fn internal_id_to_proto(id: &DeviceId) -> ProtoDeviceId {
    ProtoDeviceId { mac: id.to_string() }
}

// ─── Capability ──────────────────────────────────────────────────────────────

fn capability_to_proto(c: &Capability) -> i32 {
    match c {
        Capability::Power           => ProtoCapability::Power as i32,
        Capability::Dimmer { .. }   => ProtoCapability::Dimmer as i32,
        Capability::ColorTemp { .. }=> ProtoCapability::ColorTemp as i32,
        Capability::Rgb             => ProtoCapability::Rgb as i32,
        Capability::Switch { .. }   => ProtoCapability::Switch as i32,
        Capability::Fan             => ProtoCapability::Fan as i32,
        Capability::Ir              => ProtoCapability::Ir as i32,
    }
}

/// Convert a proto capability integer to an internal `Capability`.
/// Returns `None` for `CAPABILITY_UNSPECIFIED` or unknown values.
/// `Dimmer` and `ColorTemp` use standard normalized ranges; `Switch` defaults
/// to index 0.  Use `DpMap::capabilities()` for precise ranges from a device config.
pub fn proto_capability_to_internal(c: i32) -> Option<Capability> {
    match ProtoCapability::try_from(c).ok()? {
        ProtoCapability::Unspecified => None,
        ProtoCapability::Power       => Some(Capability::Power),
        ProtoCapability::Dimmer      => Some(Capability::Dimmer { min: 0, max: 1000 }),
        ProtoCapability::ColorTemp   => Some(Capability::ColorTemp { min_k: 2700, max_k: 6500 }),
        ProtoCapability::Rgb         => Some(Capability::Rgb),
        ProtoCapability::Switch      => Some(Capability::Switch { index: 0 }),
        ProtoCapability::Fan         => Some(Capability::Fan),
        ProtoCapability::Ir          => Some(Capability::Ir),
    }
}

// ─── DeviceInfo ───────────────────────────────────────────────────────────────

pub fn device_info_to_proto(info: DeviceInfo) -> ProtoDeviceInfo {
    ProtoDeviceInfo {
        id:           Some(internal_id_to_proto(&info.id)),
        name:         info.name,
        model:        info.model,
        protocol:     info.protocol,
        capabilities: info.capabilities.iter().map(capability_to_proto).collect(),
    }
}

// ─── DeviceState ─────────────────────────────────────────────────────────────

pub fn device_state_to_proto(s: DeviceState) -> ProtoDeviceState {
    let switches: HashMap<u32, bool> = s.switches
        .into_iter()
        .map(|(k, v)| (k as u32, v))
        .collect();

    ProtoDeviceState {
        device_id:     Some(internal_id_to_proto(&s.device_id)),
        online:        s.online,
        updated_at_ms: s.updated_at_ms,
        power:         s.power,
        brightness:    s.brightness.map(|v| v as u32),
        color_temp_k:  s.color_temp_k.map(|v| v as u32),
        rgb:           s.rgb.map(|(r, g, b)| RgbValue {
            r: r as u32,
            g: g as u32,
            b: b as u32,
        }),
        switches,
    }
}

// ─── Registration ─────────────────────────────────────────────────────────────

pub fn proto_register_to_internal(
    req: RegisterDeviceRequest,
) -> Result<(DeviceInfo, TuyaDeviceConfig), Status> {
    let proto_info = req.info.ok_or_else(|| Status::invalid_argument("info is required"))?;
    let proto_tuya = req
        .tuya_config
        .ok_or_else(|| Status::invalid_argument("tuya_config is required"))?;

    let info = proto_device_info_to_internal(proto_info)?;
    let tuya = proto_tuya_config_to_internal(&info.id, &proto_tuya)?;
    Ok((info, tuya))
}

fn proto_device_info_to_internal(p: ProtoDeviceInfo) -> Result<DeviceInfo, Status> {
    let id = proto_id_to_internal(
        p.id.as_ref()
            .ok_or_else(|| Status::invalid_argument("device_id is required"))?,
    )?;
    let capabilities = p.capabilities
        .iter()
        .filter_map(|&c| proto_capability_to_internal(c))
        .collect();
    Ok(DeviceInfo {
        id,
        name:         p.name,
        model:        p.model,
        protocol:     p.protocol,
        capabilities,
    })
}

fn proto_tuya_config_to_internal(
    id: &DeviceId,
    p:  &ProtoTuyaConfig,
) -> Result<TuyaDeviceConfig, Status> {
    let ip = IpAddr::from_str(&p.ip)
        .map_err(|_| Status::invalid_argument(format!("invalid IP address: {}", p.ip)))?;

    if p.tuya_id.is_empty() {
        return Err(Status::invalid_argument("tuya_id is required"));
    }
    if p.local_key.len() != 16 {
        return Err(Status::invalid_argument(format!(
            "local_key must be exactly 16 characters, got {}",
            p.local_key.len()
        )));
    }

    let port = if p.port == 0 { 6668 } else { p.port as u16 };

    Ok(TuyaDeviceConfig {
        device_id:  *id,
        ip,
        port,
        tuya_id:    p.tuya_id.clone(),
        local_key:  p.local_key.clone(),
        dp_profile: p.dp_profile.clone(),
        dp_map:     None,
    })
}

// ─── Room ─────────────────────────────────────────────────────────────────────

pub fn room_info_to_proto(r: Room) -> ProtoRoomInfo {
    ProtoRoomInfo {
        id:         r.id,
        name:       r.name,
        device_ids: r.device_ids.iter().map(internal_id_to_proto).collect(),
    }
}

// ─── Command ─────────────────────────────────────────────────────────────────

pub fn proto_command_to_device_command(cmd: ProtoCommand) -> Result<DeviceCommand, Status> {
    let dc = match cmd {
        ProtoCommand::SetPower(v)      => DeviceCommand::SetPower(v),
        ProtoCommand::SetBrightness(v) => DeviceCommand::SetBrightness(v as u16),
        ProtoCommand::SetColorTempK(v) => DeviceCommand::SetColorTemp(v as u16),
        ProtoCommand::SetRgb(rgb)      => DeviceCommand::SetRgb(
            rgb.r as u8, rgb.g as u8, rgb.b as u8,
        ),
        ProtoCommand::SetSwitch(sw)    => DeviceCommand::SetSwitch {
            index: sw.index as u8,
            state: sw.state,
        },
        ProtoCommand::SendIr(ir)       => DeviceCommand::SendIr {
            head: if ir.head.is_empty() { None } else { Some(ir.head) },
            key:  ir.key,
        },
        ProtoCommand::SetDp(dp)        => {
            let value = dp.value.ok_or_else(|| Status::invalid_argument("set_dp.value is required"))?;
            match value {
                ProtoDpValue::BoolVal(b)   => DeviceCommand::SetDpBool { dp: dp.dp as u16, value: b },
                ProtoDpValue::IntVal(i)    => DeviceCommand::SetDpInt  { dp: dp.dp as u16, value: i },
                ProtoDpValue::StringVal(s) => DeviceCommand::SetDpStr  { dp: dp.dp as u16, value: s },
            }
        }
    };
    Ok(dc)
}

// ─── Routine ──────────────────────────────────────────────────────────────────

pub fn routine_info_to_proto(r: Routine) -> ProtoRoutineInfo {
    ProtoRoutineInfo {
        id:       r.id,
        name:     r.name,
        schedule: r.schedule.unwrap_or_default(),
        steps:    r.steps.into_iter().map(routine_step_to_proto).collect(),
    }
}

fn routine_step_to_proto(step: RoutineStep) -> ProtoRoutineStep {
    let inner = match step {
        RoutineStep::Wait { secs } => ProtoStep::Wait(WaitStep { secs }),
        RoutineStep::Command { target, command } => {
            ProtoStep::Command(command_step_to_proto(target, command))
        }
    };
    ProtoRoutineStep { step: Some(inner) }
}

fn command_step_to_proto(target: RoutineTarget, command: DeviceCommand) -> CommandStep {
    let proto_target = match target {
        RoutineTarget::Room(id)        => CsTarget::RoomId(id),
        RoutineTarget::Device(did)     => CsTarget::DeviceId(internal_id_to_proto(&did)),
    };

    let proto_command = match command {
        DeviceCommand::SetPower(v)             => CsCommand::SetPower(v),
        DeviceCommand::SetBrightness(v)        => CsCommand::SetBrightness(v as u32),
        DeviceCommand::SetColorTemp(v)         => CsCommand::SetColorTempK(v as u32),
        DeviceCommand::SetRgb(r, g, b)         => CsCommand::SetRgb(RgbValue {
            r: r as u32, g: g as u32, b: b as u32,
        }),
        DeviceCommand::SetSwitch { index, state } => CsCommand::SetSwitch(SwitchCommand {
            index: index as u32, state,
        }),
        DeviceCommand::SendIr { head, key }    => CsCommand::SendIr(SendIrCommand {
            head: head.unwrap_or_default(), key,
        }),
        DeviceCommand::SetDpBool { dp, value } => CsCommand::SetDp(SetDpCommand {
            dp: dp as u32, value: Some(ProtoDpValue::BoolVal(value)),
        }),
        DeviceCommand::SetDpInt  { dp, value } => CsCommand::SetDp(SetDpCommand {
            dp: dp as u32, value: Some(ProtoDpValue::IntVal(value)),
        }),
        DeviceCommand::SetDpStr  { dp, value } => CsCommand::SetDp(SetDpCommand {
            dp: dp as u32, value: Some(ProtoDpValue::StringVal(value)),
        }),
    };

    CommandStep {
        target:  Some(proto_target),
        command: Some(proto_command),
    }
}

/// Convert a proto `RoutineStep` to the internal representation.
pub fn proto_routine_step_to_internal(step: ProtoRoutineStep) -> Result<RoutineStep, Status> {
    let inner = step.step.ok_or_else(|| Status::invalid_argument("routine step is empty"))?;
    match inner {
        ProtoStep::Wait(w) => Ok(RoutineStep::Wait { secs: w.secs }),
        ProtoStep::Command(cs) => {
            let target = match cs
                .target
                .ok_or_else(|| Status::invalid_argument("command step target is required"))?
            {
                CsTarget::RoomId(id)   => RoutineTarget::Room(id),
                CsTarget::DeviceId(did) => RoutineTarget::Device(proto_id_to_internal(&did)?),
            };

            let cmd_proto = cs
                .command
                .ok_or_else(|| Status::invalid_argument("command step command is required"))?;
            let command = proto_command_step_to_device_command(cmd_proto)?;

            Ok(RoutineStep::Command { target, command })
        }
    }
}

fn proto_command_step_to_device_command(cmd: CsCommand) -> Result<DeviceCommand, Status> {
    let dc = match cmd {
        CsCommand::SetPower(v)      => DeviceCommand::SetPower(v),
        CsCommand::SetBrightness(v) => DeviceCommand::SetBrightness(v as u16),
        CsCommand::SetColorTempK(v) => DeviceCommand::SetColorTemp(v as u16),
        CsCommand::SetRgb(rgb)      => DeviceCommand::SetRgb(
            rgb.r as u8, rgb.g as u8, rgb.b as u8,
        ),
        CsCommand::SetSwitch(sw)    => DeviceCommand::SetSwitch {
            index: sw.index as u8, state: sw.state,
        },
        CsCommand::SendIr(ir)       => DeviceCommand::SendIr {
            head: if ir.head.is_empty() { None } else { Some(ir.head) },
            key:  ir.key,
        },
        CsCommand::SetDp(dp)        => {
            let value = dp
                .value
                .ok_or_else(|| Status::invalid_argument("set_dp.value is required"))?;
            match value {
                ProtoDpValue::BoolVal(b)   => DeviceCommand::SetDpBool { dp: dp.dp as u16, value: b },
                ProtoDpValue::IntVal(i)    => DeviceCommand::SetDpInt  { dp: dp.dp as u16, value: i },
                ProtoDpValue::StringVal(s) => DeviceCommand::SetDpStr  { dp: dp.dp as u16, value: s },
            }
        }
    };
    Ok(dc)
}

pub fn proto_send_room_command_to_device_command(
    cmd: Option<ProtoRoomCommand>,
) -> Result<DeviceCommand, Status> {
    let cmd = cmd.ok_or_else(|| Status::invalid_argument("command is required"))?;
    let dc = match cmd {
        ProtoRoomCommand::SetPower(v)      => DeviceCommand::SetPower(v),
        ProtoRoomCommand::SetBrightness(v) => DeviceCommand::SetBrightness(v as u16),
        ProtoRoomCommand::SetColorTempK(v) => DeviceCommand::SetColorTemp(v as u16),
        ProtoRoomCommand::SetRgb(rgb)      => DeviceCommand::SetRgb(
            rgb.r as u8, rgb.g as u8, rgb.b as u8,
        ),
        ProtoRoomCommand::SetSwitch(sw)    => DeviceCommand::SetSwitch {
            index: sw.index as u8,
            state: sw.state,
        },
        ProtoRoomCommand::SendIr(ir)       => DeviceCommand::SendIr {
            head: if ir.head.is_empty() { None } else { Some(ir.head) },
            key:  ir.key,
        },
        ProtoRoomCommand::SetDp(dp)        => {
            let value = dp.value.ok_or_else(|| Status::invalid_argument("set_dp.value is required"))?;
            match value {
                ProtoDpValue::BoolVal(b)   => DeviceCommand::SetDpBool { dp: dp.dp as u16, value: b },
                ProtoDpValue::IntVal(i)    => DeviceCommand::SetDpInt  { dp: dp.dp as u16, value: i },
                ProtoDpValue::StringVal(s) => DeviceCommand::SetDpStr  { dp: dp.dp as u16, value: s },
            }
        }
    };
    Ok(dc)
}
