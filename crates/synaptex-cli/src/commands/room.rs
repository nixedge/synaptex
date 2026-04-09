use anyhow::{bail, Context, Result};
use clap::Subcommand;

use super::device::{build_command_json, parse_power};

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

        /// Turn on or off.
        #[arg(long, value_name = "on|off", value_parser = parse_power)]
        power: Option<bool>,

        /// Set brightness 0–1000.
        #[arg(long, value_name = "0-1000")]
        brightness: Option<u32>,

        /// Set colour temperature in Kelvin.
        #[arg(long, value_name = "KELVIN")]
        color_temp: Option<u32>,

        /// Set RGB colour, e.g. `255,128,0`.
        #[arg(long, value_name = "R,G,B")]
        rgb: Option<String>,

        /// Override colour mode: white | colour.
        #[arg(long, value_name = "MODE")]
        color_mode: Option<String>,

        /// Send an IR code. Format: `HEAD:KEY`.
        #[arg(long, value_name = "HEAD:KEY", group = "cmd")]
        send_ir: Option<String>,

        /// Write a raw DP. Format: `DP:TYPE:VALUE`.
        #[arg(long, value_name = "DP:TYPE:VALUE", group = "cmd")]
        set_dp: Option<String>,

        /// Set fan speed: off | low | medium | high.
        #[arg(long, value_name = "SPEED", group = "cmd")]
        fan_speed: Option<String>,
    },
}

// ─── Dispatch ────────────────────────────────────────────────────────────────

pub async fn run(cmd: RoomCmd, http_url: &str, api_key: Option<&str>) -> Result<()> {
    match cmd {
        RoomCmd::Create { name, devices }                                         => create(name, devices, http_url, api_key).await,
        RoomCmd::Update { id, name, devices }                                     => update(id, name, devices, http_url, api_key).await,
        RoomCmd::Delete { id }                                                    => delete(id, http_url, api_key).await,
        RoomCmd::List                                                             => list(http_url, api_key).await,
        RoomCmd::Get { id }                                                       => get(id, http_url, api_key).await,
        RoomCmd::Set { id, power, brightness, color_temp, rgb, color_mode, send_ir, set_dp, fan_speed } =>
            set(id, power, brightness, color_temp, rgb, color_mode, send_ir, set_dp, fan_speed, http_url, api_key).await,
    }
}

// ─── Handlers ────────────────────────────────────────────────────────────────

async fn create(name: String, devices: String, http_url: &str, api_key: Option<&str>) -> Result<()> {
    let macs: Vec<&str> = devices.split(',').map(str::trim).collect();
    let body = serde_json::json!({ "name": name, "devices": macs });

    let client = reqwest::Client::new();
    let mut req = client.post(format!("{http_url}/api/v1/rooms")).json(&body);
    if let Some(key) = api_key { req = req.header("Authorization", format!("Bearer {key}")); }
    let resp = req.send().await.context("POST /api/v1/rooms")?;
    if !resp.status().is_success() {
        bail!("room creation failed: {}", resp.text().await?);
    }
    let result: serde_json::Value = resp.json().await?;
    println!("room created — id: {}", result["id"].as_str().unwrap_or("?"));
    Ok(())
}

async fn update(
    id:       String,
    name:     Option<String>,
    devices:  Option<String>,
    http_url: &str,
    api_key:  Option<&str>,
) -> Result<()> {
    let mut body = serde_json::json!({});
    if let Some(n) = name    { body["name"]    = serde_json::json!(n); }
    if let Some(d) = devices {
        let macs: Vec<&str> = d.split(',').map(str::trim).collect();
        body["devices"] = serde_json::json!(macs);
    }

    let client = reqwest::Client::new();
    let mut req = client.patch(format!("{http_url}/api/v1/rooms/{id}")).json(&body);
    if let Some(key) = api_key { req = req.header("Authorization", format!("Bearer {key}")); }
    let resp = req.send().await.context("PATCH /api/v1/rooms/{id}")?;
    if !resp.status().is_success() {
        bail!("room update failed: {}", resp.text().await?);
    }
    println!("room updated");
    Ok(())
}

async fn delete(id: String, http_url: &str, api_key: Option<&str>) -> Result<()> {
    let client = reqwest::Client::new();
    let mut req = client.delete(format!("{http_url}/api/v1/rooms/{id}"));
    if let Some(key) = api_key { req = req.header("Authorization", format!("Bearer {key}")); }
    let resp = req.send().await.context("DELETE /api/v1/rooms/{id}")?;
    if !resp.status().is_success() {
        bail!("room deletion failed: {}", resp.text().await?);
    }
    println!("room deleted");
    Ok(())
}

async fn list(http_url: &str, api_key: Option<&str>) -> Result<()> {
    let client = reqwest::Client::new();
    let mut req = client.get(format!("{http_url}/api/v1/rooms"));
    if let Some(key) = api_key { req = req.header("Authorization", format!("Bearer {key}")); }
    let resp = req.send().await.context("GET /api/v1/rooms")?;
    if !resp.status().is_success() {
        bail!("server error: {}", resp.text().await?);
    }

    let rooms: Vec<serde_json::Value> = resp.json().await?;
    if rooms.is_empty() {
        println!("no rooms");
        return Ok(());
    }
    for r in &rooms {
        let device_count = r["devices"].as_array().map(Vec::len).unwrap_or(0);
        println!("{}  {:32}  {} device(s)",
            r["id"].as_str().unwrap_or("?"),
            r["name"].as_str().unwrap_or("?"),
            device_count,
        );
    }
    Ok(())
}

async fn get(id: String, http_url: &str, api_key: Option<&str>) -> Result<()> {
    let client = reqwest::Client::new();
    let mut req = client.get(format!("{http_url}/api/v1/rooms/{id}"));
    if let Some(key) = api_key { req = req.header("Authorization", format!("Bearer {key}")); }
    let resp = req.send().await.context("GET /api/v1/rooms/{id}")?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND { bail!("room not found"); }
    if !resp.status().is_success() { bail!("server error: {}", resp.text().await?); }

    let r: serde_json::Value = resp.json().await?;
    println!("id:      {}", r["id"].as_str().unwrap_or("?"));
    println!("name:    {}", r["name"].as_str().unwrap_or("?"));
    println!("devices:");
    if let Some(devs) = r["devices"].as_array() {
        for d in devs {
            println!("  {}", d.as_str().unwrap_or("?"));
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
    color_mode: Option<String>,
    send_ir:    Option<String>,
    set_dp:     Option<String>,
    fan_speed:  Option<String>,
    http_url:   &str,
    api_key:    Option<&str>,
) -> Result<()> {
    let cmd_json = build_command_json(power, brightness, color_temp, rgb, color_mode, send_ir, set_dp, fan_speed, None, None)?;

    let client = reqwest::Client::new();
    let mut req = client
        .post(format!("{http_url}/api/v1/rooms/{id}/command"))
        .json(&cmd_json);
    if let Some(key) = api_key { req = req.header("Authorization", format!("Bearer {key}")); }
    let resp = req.send().await.context("POST /api/v1/rooms/{id}/command")?;
    if !resp.status().is_success() {
        bail!("room command failed: {}", resp.text().await?);
    }
    println!("room command sent");
    Ok(())
}
