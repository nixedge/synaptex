use anyhow::{bail, Result};
use clap::Subcommand;
use tonic::transport::Channel;

use synaptex_proto::{
    device_service_client::DeviceServiceClient,
    CreateRoomRequest,
    DeleteRoomRequest,
    DeviceId as ProtoDeviceId,
    GetRoomRequest,
    ListRoomsRequest,
    SendRoomCommandRequest,
    UpdateRoomRequest,
    send_room_command_request::Command,
};

use super::device::build_command;

// ─── Subcommands ─────────────────────────────────────────────────────────────

#[derive(Debug, Subcommand)]
pub enum RoomCmd {
    /// Create a new room from comma-separated device MACs.
    Create {
        /// Human-readable name for the room.
        #[arg(long)]
        name: String,

        /// Comma-separated list of device MAC addresses.
        #[arg(long, value_name = "MAC,MAC,...")]
        devices: String,
    },

    /// Update a room's name and/or device list.
    Update {
        /// Room UUID.
        #[arg(long)]
        id: String,

        /// New name (omit to keep current).
        #[arg(long)]
        name: Option<String>,

        /// Replacement comma-separated device MACs (omit to keep current).
        #[arg(long, value_name = "MAC,MAC,...")]
        devices: Option<String>,
    },

    /// Delete a room (devices are unaffected).
    Delete {
        /// Room UUID.
        #[arg(long)]
        id: String,
    },

    /// List all rooms.
    List,

    /// Show details of a single room.
    Get {
        /// Room UUID.
        #[arg(long)]
        id: String,
    },

    /// Send a command to all eligible devices in a room.
    Set {
        /// Room UUID.
        #[arg(long)]
        id: String,

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

        /// Send an IR code. Format: `HEAD:KEY`.
        #[arg(long, value_name = "HEAD:KEY", group = "cmd")]
        send_ir: Option<String>,

        /// Write a raw DP. Format: `DP:TYPE:VALUE`.
        #[arg(long, value_name = "DP:TYPE:VALUE", group = "cmd")]
        set_dp: Option<String>,
    },
}

// ─── Dispatch ────────────────────────────────────────────────────────────────

pub async fn run(cmd: RoomCmd, client: &mut DeviceServiceClient<Channel>) -> Result<()> {
    match cmd {
        RoomCmd::Create { name, devices }                                         => create(name, devices, client).await,
        RoomCmd::Update { id, name, devices }                                     => update(id, name, devices, client).await,
        RoomCmd::Delete { id }                                                    => delete(id, client).await,
        RoomCmd::List                                                             => list(client).await,
        RoomCmd::Get { id }                                                       => get(id, client).await,
        RoomCmd::Set { id, power, brightness, color_temp, rgb, send_ir, set_dp } => {
            set(id, power, brightness, color_temp, rgb, send_ir, set_dp, client).await
        }
    }
}

// ─── Handlers ────────────────────────────────────────────────────────────────

async fn create(name: String, devices: String, client: &mut DeviceServiceClient<Channel>) -> Result<()> {
    let device_ids = parse_mac_list(&devices)?;
    let resp = client
        .create_room(CreateRoomRequest { name, device_ids })
        .await?
        .into_inner();

    if resp.ok {
        println!("room created — id: {}", resp.id);
    } else {
        bail!("room creation failed: {}", resp.error_message);
    }
    Ok(())
}

async fn update(
    id:      String,
    name:    Option<String>,
    devices: Option<String>,
    client:  &mut DeviceServiceClient<Channel>,
) -> Result<()> {
    let device_ids = devices.map(|d| parse_mac_list(&d)).transpose()?
        .unwrap_or_default();

    let resp = client
        .update_room(UpdateRoomRequest {
            room_id:    id,
            name:       name.unwrap_or_default(),
            device_ids,
        })
        .await?
        .into_inner();

    if resp.ok {
        println!("room updated");
    } else {
        bail!("room update failed: {}", resp.error_message);
    }
    Ok(())
}

async fn delete(id: String, client: &mut DeviceServiceClient<Channel>) -> Result<()> {
    let resp = client
        .delete_room(DeleteRoomRequest { room_id: id })
        .await?
        .into_inner();

    if resp.ok {
        println!("room deleted");
    } else {
        bail!("room deletion failed: {}", resp.error_message);
    }
    Ok(())
}

async fn list(client: &mut DeviceServiceClient<Channel>) -> Result<()> {
    let resp = client
        .list_rooms(ListRoomsRequest {})
        .await?
        .into_inner();

    if resp.rooms.is_empty() {
        println!("no rooms");
        return Ok(());
    }

    for r in &resp.rooms {
        let device_count = r.device_ids.len();
        println!("{}  {:32}  {} device(s)", r.id, r.name, device_count);
    }
    Ok(())
}

async fn get(id: String, client: &mut DeviceServiceClient<Channel>) -> Result<()> {
    let resp = client
        .get_room(GetRoomRequest { room_id: id })
        .await?
        .into_inner();

    match resp.room {
        None => bail!("room not found"),
        Some(r) => {
            println!("id:      {}", r.id);
            println!("name:    {}", r.name);
            println!("devices:");
            for d in &r.device_ids {
                println!("  {}", d.mac);
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn set(
    id:         String,
    power:      Option<bool>,
    brightness: Option<u32>,
    color_temp: Option<u32>,
    rgb:        Option<String>,
    send_ir:    Option<String>,
    set_dp:     Option<String>,
    client:     &mut DeviceServiceClient<Channel>,
) -> Result<()> {
    // Reuse device set command builder; convert to room command variant.
    let device_cmd = build_command(power, brightness, color_temp, rgb, send_ir, set_dp)?;

    let command: Command = match device_cmd {
        synaptex_proto::set_device_state_request::Command::SetPower(v)      => Command::SetPower(v),
        synaptex_proto::set_device_state_request::Command::SetBrightness(v) => Command::SetBrightness(v),
        synaptex_proto::set_device_state_request::Command::SetColorTempK(v) => Command::SetColorTempK(v),
        synaptex_proto::set_device_state_request::Command::SetRgb(v)        => Command::SetRgb(v),
        synaptex_proto::set_device_state_request::Command::SetSwitch(v)     => Command::SetSwitch(v),
        synaptex_proto::set_device_state_request::Command::SendIr(v)        => Command::SendIr(v),
        synaptex_proto::set_device_state_request::Command::SetDp(v)         => Command::SetDp(v),
    };

    let resp = client
        .send_room_command(SendRoomCommandRequest {
            room_id: id,
            command: Some(command),
        })
        .await?
        .into_inner();

    if resp.ok {
        println!("room command sent");
    } else {
        bail!("room command failed: {}", resp.error_message);
    }
    Ok(())
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

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
