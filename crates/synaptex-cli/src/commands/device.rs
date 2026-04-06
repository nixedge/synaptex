use anyhow::{bail, Result};
use clap::Subcommand;
use tonic::transport::Channel;

use synaptex_proto::{
    device_service_client::DeviceServiceClient,
    Capability as ProtoCapability,
    DeviceId as ProtoDeviceId,
    DeviceInfo as ProtoDeviceInfo,
    GetDeviceStateRequest,
    ListDevicesRequest,
    RegisterDeviceRequest,
    SetDeviceStateRequest,
    TuyaConfig as ProtoTuyaConfig,
    UnregisterDeviceRequest,
    set_device_state_request::Command,
};

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
    },

    /// List all registered devices.
    List,

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

        /// Tuya cloud device ID (the `id`/`uuid` field from the API, e.g. `"2470245270039f12abbf"`).
        #[arg(long, value_name = "ID")]
        tuya_id: String,

        /// 16-character ASCII local key (the `key` field from the Tuya API).
        #[arg(long, value_name = "KEY")]
        local_key: String,

        /// Tuya device model string (informational only).
        #[arg(long, default_value = "generic")]
        model: String,

        /// Tuya local API port (almost always 6668).
        #[arg(long, default_value_t = 6668u32)]
        port: u32,
    },

    /// Unregister a device and stop its plugin.
    Remove {
        #[arg(long, value_name = "MAC")]
        mac: String,
    },
}

// ─── Dispatch ────────────────────────────────────────────────────────────────

pub async fn run(cmd: DeviceCmd, client: &mut DeviceServiceClient<Channel>) -> Result<()> {
    match cmd {
        DeviceCmd::Get { mac }                                          => get(mac, client).await,
        DeviceCmd::Set { mac, power, brightness, color_temp, rgb }     => {
            set(mac, power, brightness, color_temp, rgb, client).await
        }
        DeviceCmd::List                                                 => list(client).await,
        DeviceCmd::Add { mac, name, ip, tuya_id, local_key, model, port } => {
            add(mac, name, ip, tuya_id, local_key, model, port, client).await
        }
        DeviceCmd::Remove { mac }                                       => remove(mac, client).await,
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

async fn set(
    mac:        String,
    power:      Option<bool>,
    brightness: Option<u32>,
    color_temp: Option<u32>,
    rgb:        Option<String>,
    client:     &mut DeviceServiceClient<Channel>,
) -> Result<()> {
    let command = if let Some(v) = power {
        Command::SetPower(v)
    } else if let Some(v) = brightness {
        Command::SetBrightness(v)
    } else if let Some(v) = color_temp {
        Command::SetColorTempK(v)
    } else if let Some(s) = rgb {
        let parts: Vec<u32> = s
            .split(',')
            .map(|x| x.trim().parse::<u32>())
            .collect::<Result<_, _>>()
            .map_err(|_| anyhow::anyhow!("--rgb requires three comma-separated integers, e.g. 255,128,0"))?;
        if parts.len() != 3 {
            bail!("--rgb requires exactly 3 components");
        }
        Command::SetRgb(synaptex_proto::RgbValue { r: parts[0], g: parts[1], b: parts[2] })
    } else {
        bail!("provide one of --power, --brightness, --color-temp, or --rgb");
    };

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

    // Give the device a moment to push its state update before we read it.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    get(mac, client).await
}

async fn list(client: &mut DeviceServiceClient<Channel>) -> Result<()> {
    let resp = client
        .list_devices(ListDevicesRequest {})
        .await?
        .into_inner();

    if resp.devices.is_empty() {
        println!("no devices registered");
        return Ok(());
    }

    for d in &resp.devices {
        let mac = d.id.as_ref().map(|id| id.mac.as_str()).unwrap_or("?");
        println!("{mac}  {:32}  {}", d.name, d.protocol);
    }
    Ok(())
}

async fn add(
    mac:       String,
    name:      String,
    ip:        String,
    tuya_id:   String,
    local_key: String,
    model:     String,
    port:      u32,
    client:    &mut DeviceServiceClient<Channel>,
) -> Result<()> {
    if local_key.len() != 16 {
        bail!("--local-key must be exactly 16 characters (got {})", local_key.len());
    }

    let resp = client
        .register_device(RegisterDeviceRequest {
            info: Some(ProtoDeviceInfo {
                id:           Some(ProtoDeviceId { mac }),
                name,
                model,
                protocol:     "tuya_local".into(), // version detected at runtime
                capabilities: vec![
                    ProtoCapability::Power as i32,
                    ProtoCapability::Dimmer as i32,
                    ProtoCapability::ColorTemp as i32,
                    ProtoCapability::Rgb as i32,
                ],
            }),
            tuya_config: Some(ProtoTuyaConfig {
                ip,
                port,
                tuya_id,
                local_key,
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
