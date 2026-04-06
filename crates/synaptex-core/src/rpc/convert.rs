/// Conversions between internal domain types and protobuf-generated types.
use std::{collections::HashMap, net::IpAddr, str::FromStr};

use synaptex_proto::{
    Capability as ProtoCapability,
    DeviceId as ProtoDeviceId,
    DeviceInfo as ProtoDeviceInfo,
    DeviceState as ProtoDeviceState,
    RegisterDeviceRequest,
    RgbValue,
    TuyaConfig as ProtoTuyaConfig,
    set_device_state_request::Command as ProtoCommand,
};
use synaptex_types::{
    capability::{Capability, DeviceCommand},
    device::{DeviceId, DeviceInfo},
    plugin::DeviceState,
};
use synaptex_tuya::{TuyaDeviceConfig, plugin::TuyaConfig};
use tonic::Status;

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
        Capability::Power        => ProtoCapability::Power as i32,
        Capability::Dimmer { .. }    => ProtoCapability::Dimmer as i32,
        Capability::ColorTemp { .. } => ProtoCapability::ColorTemp as i32,
        Capability::Rgb          => ProtoCapability::Rgb as i32,
        Capability::Switch { .. }    => ProtoCapability::Switch as i32,
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

// ─── Registration ────────────────────────────────────────────────────────────

/// Validate and convert a `RegisterDeviceRequest` into the internal pair
/// `(DeviceInfo, TuyaDeviceConfig)` required by the plugin factory.
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
    Ok(DeviceInfo {
        id,
        name:         p.name,
        model:        p.model,
        protocol:     p.protocol,
        capabilities: vec![], // populated by the plugin at runtime
    })
}

fn proto_tuya_config_to_internal(
    id:  &DeviceId,
    p:   &ProtoTuyaConfig,
) -> Result<TuyaDeviceConfig, Status> {
    let ip = IpAddr::from_str(&p.ip)
        .map_err(|_| Status::invalid_argument(format!("invalid IP address: {}", p.ip)))?;

    if p.tuya_id.is_empty() {
        return Err(Status::invalid_argument("tuya_id is required"));
    }

    // The local_key is a 16-character ASCII string; its raw bytes are the AES key.
    if p.local_key.len() != 16 {
        return Err(Status::invalid_argument(format!(
            "local_key must be exactly 16 characters, got {}",
            p.local_key.len()
        )));
    }

    let port = if p.port == 0 { 6668 } else { p.port as u16 };

    Ok(TuyaDeviceConfig {
        device_id: *id,
        ip,
        port,
        tuya_id:   p.tuya_id.clone(),
        local_key: p.local_key.clone(),
        dp_map:    None, // use defaults
    })
}

// ─── Command ─────────────────────────────────────────────────────────────────

pub fn proto_command_to_device_command(cmd: ProtoCommand) -> DeviceCommand {
    match cmd {
        ProtoCommand::SetPower(v)       => DeviceCommand::SetPower(v),
        ProtoCommand::SetBrightness(v)  => DeviceCommand::SetBrightness(v as u16),
        ProtoCommand::SetColorTempK(v)  => DeviceCommand::SetColorTemp(v as u16),
        ProtoCommand::SetRgb(rgb)       => DeviceCommand::SetRgb(
            rgb.r as u8, rgb.g as u8, rgb.b as u8,
        ),
        ProtoCommand::SetSwitch(sw)     => DeviceCommand::SetSwitch {
            index: sw.index as u8,
            state: sw.state,
        },
    }
}
