use anyhow::{bail, Result};
use clap::Subcommand;
use tonic::transport::Channel;

use synaptex_proto::{
    device_service_client::DeviceServiceClient,
    Capability as ProtoCapability,
    CreateGroupRequest,
    DeviceId as ProtoDeviceId,
    DeviceInfo as ProtoDeviceInfo,
    GetDeviceStateRequest,
    ListDevicesRequest,
    RegisterDeviceRequest,
    SendIrCommand,
    SetDeviceStateRequest,
    SetDpCommand,
    TuyaConfig as ProtoTuyaConfig,
    UnregisterDeviceRequest,
    UpdateGroupRequest,
    WatchDeviceStateRequest,
    set_device_state_request::Command,
    set_dp_command::Value as DpValue,
};

// ─── Group subcommand ─────────────────────────────────────────────────────────

#[derive(Debug, Subcommand)]
pub enum GroupCmd {
    /// Create a new group from comma-separated member MACs.
    Create {
        /// Human-readable name for the group.
        #[arg(long)]
        name: String,

        /// Optional model string.
        #[arg(long, default_value = "group")]
        model: String,

        /// Comma-separated list of member MAC addresses.
        #[arg(long, value_name = "MAC,MAC,...")]
        members: String,
    },

    /// Update a group's name and/or members.
    Update {
        /// Group MAC address.
        #[arg(long, value_name = "MAC")]
        mac: String,

        /// New name (omit to keep current).
        #[arg(long)]
        name: Option<String>,

        /// Replacement comma-separated member MACs (omit to keep current).
        #[arg(long, value_name = "MAC,MAC,...")]
        members: Option<String>,
    },
}

// ─── Subcommands ─────────────────────────────────────────────────────────────

#[derive(Debug, Subcommand)]
pub enum DeviceCmd {
    /// Print the current state of a device.
    Get {
        #[arg(long, value_name = "MAC")]
        mac: String,
    },

    /// Send a command to a device.
    Set {
        #[arg(long, value_name = "MAC")]
        mac: String,

        /// Turn on (true) or off (false).
        #[arg(long, value_name = "BOOL", group = "cmd")]
        power: Option<bool>,

        /// Set brightness 0–1000.
        #[arg(long, value_name = "0-1000", group = "cmd")]
        brightness: Option<u32>,

        /// Set colour temperature in Kelvin.
        #[arg(long, value_name = "KELVIN", group = "cmd")]
        color_temp: Option<u32>,

        /// Set RGB colour, e.g. `255,128,0`.
        #[arg(long, value_name = "R,G,B", group = "cmd")]
        rgb: Option<String>,

        /// Send an IR code. Format: `HEAD:KEY` (HEAD may be empty, e.g. `:KEY`).
        #[arg(long, value_name = "HEAD:KEY", group = "cmd")]
        send_ir: Option<String>,

        /// Write a raw DP. Format: `DP:TYPE:VALUE` where TYPE is bool|int|str.
        /// Example: `--set-dp 3:str:low`
        #[arg(long, value_name = "DP:TYPE:VALUE", group = "cmd")]
        set_dp: Option<String>,
    },

    /// List all registered devices.
    List {
        /// Show only group devices (protocol == "group").
        #[arg(long)]
        groups: bool,
    },

    /// Register a new Tuya device and start its plugin.
    Add {
        /// Device MAC address (`AA:BB:CC:DD:EE:FF`).
        #[arg(long, value_name = "MAC")]
        mac: String,

        /// Human-readable name, e.g. "Living Room Lamp".
        #[arg(long)]
        name: String,

        /// Device IP address on the local network.
        #[arg(long, value_name = "IP")]
        ip: String,

        /// Tuya cloud device ID.
        #[arg(long, value_name = "ID")]
        tuya_id: String,

        /// 16-character ASCII local key from the Tuya API.
        #[arg(long, value_name = "KEY")]
        local_key: String,

        /// Tuya device model string (informational only).
        #[arg(long, default_value = "generic")]
        model: String,

        /// Tuya local API port (almost always 6668).
        #[arg(long, default_value_t = 6668u32)]
        port: u32,

        /// DP profile preset: bulb_a | bulb_b | switch | fan | ir1 | ir2
        #[arg(long, default_value = "bulb_b")]
        dp_profile: String,

        /// Override capabilities (comma-separated): power,dimmer,colortemp,rgb,fan,ir
        /// If omitted the server derives them from --dp-profile automatically.
        #[arg(long, value_name = "CAP,...")]
        capabilities: Option<String>,
    },

    /// Unregister a device and stop its plugin.
    Remove {
        #[arg(long, value_name = "MAC")]
        mac: String,
    },

    /// Stream live state updates from a device (or all devices if --mac is omitted).
    Watch {
        #[arg(long, value_name = "MAC")]
        mac: Option<String>,
    },

    /// Manage device groups.
    #[command(subcommand)]
    Group(GroupCmd),
}

// ─── Dispatch ────────────────────────────────────────────────────────────────

pub async fn run(cmd: DeviceCmd, client: &mut DeviceServiceClient<Channel>) -> Result<()> {
    match cmd {
        DeviceCmd::Get { mac }                                                             => get(mac, client).await,
        DeviceCmd::Set { mac, power, brightness, color_temp, rgb, send_ir, set_dp }       => {
            set(mac, power, brightness, color_temp, rgb, send_ir, set_dp, client).await
        }
        DeviceCmd::List { groups }                                                         => list(groups, client).await,
        DeviceCmd::Add { mac, name, ip, tuya_id, local_key, model, port, dp_profile, capabilities } => {
            add(mac, name, ip, tuya_id, local_key, model, port, dp_profile, capabilities, client).await
        }
        DeviceCmd::Remove { mac }                                                          => remove(mac, client).await,
        DeviceCmd::Watch { mac }                                                           => watch(mac, client).await,
        DeviceCmd::Group(GroupCmd::Create { name, model, members })                       => {
            group_create(name, model, members, client).await
        }
        DeviceCmd::Group(GroupCmd::Update { mac, name, members })                         => {
            group_update(mac, name, members, client).await
        }
    }
}

// ─── Handlers ────────────────────────────────────────────────────────────────

async fn get(mac: String, client: &mut DeviceServiceClient<Channel>) -> Result<()> {
    let resp = client
        .get_device_state(GetDeviceStateRequest {
            device_id: Some(ProtoDeviceId { mac }),
        })
        .await?
        .into_inner();

    match resp.state {
        Some(s) => {
            println!("device:      {}", s.device_id.map(|d| d.mac).unwrap_or_default());
            println!("online:      {}", s.online);
            println!("updated_at:  {} ms", s.updated_at_ms);
            if let Some(p)  = s.power        { println!("power:       {}", if p { "on" } else { "off" }); }
            if let Some(b)  = s.brightness   { println!("brightness:  {b}/1000"); }
            if let Some(ct) = s.color_temp_k { println!("color_temp:  {ct} K"); }
            if let Some(c)  = s.rgb          { println!("rgb:         ({},{},{})", c.r, c.g, c.b); }
        }
        None => bail!("no state returned for device"),
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn set(
    mac:        String,
    power:      Option<bool>,
    brightness: Option<u32>,
    color_temp: Option<u32>,
    rgb:        Option<String>,
    send_ir:    Option<String>,
    set_dp:     Option<String>,
    client:     &mut DeviceServiceClient<Channel>,
) -> Result<()> {
    let command = build_command(power, brightness, color_temp, rgb, send_ir, set_dp)?;

    let resp = client
        .set_device_state(SetDeviceStateRequest {
            device_id: Some(ProtoDeviceId { mac: mac.clone() }),
            command:   Some(command),
        })
        .await?
        .into_inner();

    if !resp.ok {
        bail!("command failed: {}", resp.error_message);
    }

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    get(mac, client).await
}

async fn list(groups_only: bool, client: &mut DeviceServiceClient<Channel>) -> Result<()> {
    let resp = client
        .list_devices(ListDevicesRequest {})
        .await?
        .into_inner();

    let devices: Vec<_> = resp.devices
        .into_iter()
        .filter(|d| !groups_only || d.protocol == "group")
        .collect();

    if devices.is_empty() {
        println!("no devices registered");
        return Ok(());
    }

    for d in &devices {
        let mac = d.id.as_ref().map(|id| id.mac.as_str()).unwrap_or("?");
        println!("{mac}  {:32}  {}", d.name, d.protocol);
    }
    Ok(())
}

async fn add(
    mac:          String,
    name:         String,
    ip:           String,
    tuya_id:      String,
    local_key:    String,
    model:        String,
    port:         u32,
    dp_profile:   String,
    capabilities: Option<String>,
    client:       &mut DeviceServiceClient<Channel>,
) -> Result<()> {
    if local_key.len() != 16 {
        bail!("--local-key must be exactly 16 characters (got {})", local_key.len());
    }

    // Parse explicit capability overrides, or send empty (server derives from dp_profile).
    let capability_ints = match capabilities {
        Some(caps_str) => parse_capabilities(&caps_str)?,
        None           => vec![],
    };

    let resp = client
        .register_device(RegisterDeviceRequest {
            info: Some(ProtoDeviceInfo {
                id:           Some(ProtoDeviceId { mac }),
                name,
                model,
                protocol:     "tuya_local".into(),
                capabilities: capability_ints,
            }),
            tuya_config: Some(ProtoTuyaConfig {
                ip,
                port,
                tuya_id,
                local_key,
                dp_profile,
            }),
        })
        .await?
        .into_inner();

    if resp.ok {
        println!("device registered — plugin connecting in background");
    } else {
        bail!("registration failed: {}", resp.error_message);
    }
    Ok(())
}

/// Parse a comma-separated capability string into proto `Capability` integers.
fn parse_capabilities(s: &str) -> Result<Vec<i32>> {
    s.split(',')
        .map(|tok| match tok.trim() {
            "power"     => Ok(ProtoCapability::Power     as i32),
            "dimmer"    => Ok(ProtoCapability::Dimmer    as i32),
            "colortemp" => Ok(ProtoCapability::ColorTemp as i32),
            "rgb"       => Ok(ProtoCapability::Rgb       as i32),
            "fan"       => Ok(ProtoCapability::Fan       as i32),
            "ir"        => Ok(ProtoCapability::Ir        as i32),
            other       => bail!("unknown capability '{other}'; valid values: power,dimmer,colortemp,rgb,fan,ir"),
        })
        .collect()
}

async fn remove(mac: String, client: &mut DeviceServiceClient<Channel>) -> Result<()> {
    let resp = client
        .unregister_device(UnregisterDeviceRequest {
            device_id: Some(ProtoDeviceId { mac }),
        })
        .await?
        .into_inner();

    if resp.ok {
        println!("device unregistered");
    } else {
        bail!("unregister failed");
    }
    Ok(())
}

async fn watch(mac: Option<String>, client: &mut DeviceServiceClient<Channel>) -> Result<()> {
    let device_ids = mac
        .map(|m| vec![ProtoDeviceId { mac: m }])
        .unwrap_or_default();

    let mut stream = client
        .watch_device_state(WatchDeviceStateRequest { device_ids })
        .await?
        .into_inner();

    println!("watching device state (Ctrl-C to stop)…");
    loop {
        match stream.message().await? {
            None => {
                println!("stream closed by server");
                break;
            }
            Some(ev) => {
                if let Some(s) = ev.state {
                    let mac = s.device_id.as_ref().map(|d| d.mac.as_str()).unwrap_or("?");
                    let online = if s.online { "online" } else { "offline" };
                    print!("{mac} [{online}]");
                    if let Some(p)  = s.power        { print!("  power={}", if p { "on" } else { "off" }); }
                    if let Some(b)  = s.brightness   { print!("  bri={b}"); }
                    if let Some(ct) = s.color_temp_k { print!("  ct={ct}K"); }
                    if let Some(c)  = s.rgb          { print!("  rgb=({},{},{})", c.r, c.g, c.b); }
                    println!();
                }
            }
        }
    }
    Ok(())
}

async fn group_create(
    name:    String,
    model:   String,
    members: String,
    client:  &mut DeviceServiceClient<Channel>,
) -> Result<()> {
    let member_ids = parse_mac_list(&members)?;

    let resp = client
        .create_group(CreateGroupRequest { name, model, member_ids })
        .await?
        .into_inner();

    if resp.ok {
        let id = resp.id.map(|d| d.mac).unwrap_or_default();
        println!("group created — id: {id}");
    } else {
        bail!("group creation failed: {}", resp.error_message);
    }
    Ok(())
}

async fn group_update(
    mac:     String,
    name:    Option<String>,
    members: Option<String>,
    client:  &mut DeviceServiceClient<Channel>,
) -> Result<()> {
    let member_ids = members.map(|m| parse_mac_list(&m)).transpose()?
        .unwrap_or_default();

    let resp = client
        .update_group(UpdateGroupRequest {
            group_id:   Some(ProtoDeviceId { mac }),
            name:       name.unwrap_or_default(),
            member_ids,
        })
        .await?
        .into_inner();

    if resp.ok {
        println!("group updated");
    } else {
        bail!("group update failed: {}", resp.error_message);
    }
    Ok(())
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Parse a comma-separated list of MAC addresses into `ProtoDeviceId`s.
fn parse_mac_list(s: &str) -> Result<Vec<ProtoDeviceId>> {
    s.split(',')
        .map(|m| {
            let m = m.trim().to_string();
            if m.is_empty() {
                bail!("empty MAC address in list");
            }
            Ok(ProtoDeviceId { mac: m })
        })
        .collect()
}

/// Build a `Command` oneof from the CLI flag set (shared by `device set` and
/// `room set`).
pub fn build_command(
    power:      Option<bool>,
    brightness: Option<u32>,
    color_temp: Option<u32>,
    rgb:        Option<String>,
    send_ir:    Option<String>,
    set_dp:     Option<String>,
) -> Result<Command> {
    if let Some(v) = power {
        Ok(Command::SetPower(v))
    } else if let Some(v) = brightness {
        Ok(Command::SetBrightness(v))
    } else if let Some(v) = color_temp {
        Ok(Command::SetColorTempK(v))
    } else if let Some(s) = rgb {
        let parts: Vec<u32> = s
            .split(',')
            .map(|x| x.trim().parse::<u32>())
            .collect::<Result<_, _>>()
            .map_err(|_| anyhow::anyhow!("--rgb requires three comma-separated integers, e.g. 255,128,0"))?;
        if parts.len() != 3 {
            bail!("--rgb requires exactly 3 components");
        }
        Ok(Command::SetRgb(synaptex_proto::RgbValue { r: parts[0], g: parts[1], b: parts[2] }))
    } else if let Some(ir) = send_ir {
        let (head, key) = if let Some(pos) = ir.find(':') {
            (ir[..pos].to_string(), ir[pos + 1..].to_string())
        } else {
            bail!("--send-ir requires format HEAD:KEY (HEAD may be empty, e.g. \":KEY\")");
        };
        Ok(Command::SendIr(SendIrCommand { head, key }))
    } else if let Some(dp_spec) = set_dp {
        let parts: Vec<&str> = dp_spec.splitn(3, ':').collect();
        if parts.len() != 3 {
            bail!("--set-dp requires format DP:TYPE:VALUE, e.g. 3:str:low");
        }
        let dp: u32 = parts[0].parse().map_err(|_| anyhow::anyhow!("DP must be a number"))?;
        let value = match parts[1] {
            "bool" => {
                let b: bool = parts[2].parse().map_err(|_| anyhow::anyhow!("bool value must be true/false"))?;
                DpValue::BoolVal(b)
            }
            "int" => {
                let i: i64 = parts[2].parse().map_err(|_| anyhow::anyhow!("int value must be a number"))?;
                DpValue::IntVal(i)
            }
            "str" => DpValue::StringVal(parts[2].to_string()),
            t => bail!("unknown DP type: {t}; use bool, int, or str"),
        };
        Ok(Command::SetDp(SetDpCommand { dp, value: Some(value) }))
    } else {
        bail!("provide one of --power, --brightness, --color-temp, --rgb, --send-ir, or --set-dp");
    }
}
