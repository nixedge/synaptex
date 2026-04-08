use anyhow::{bail, Context, Result};
use clap::Subcommand;

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
        #[arg(long, default_value_t = 6668u16)]
        port: u16,

        /// DP profile preset: bulb_a | bulb_b | switch | fan | ir1 | ir2
        #[arg(long, default_value = "bulb_b")]
        dp_profile: String,
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

    /// Import all Tuya Cloud devices found on the local network.
    Import,
}

// ─── Dispatch ────────────────────────────────────────────────────────────────

pub async fn run(cmd: DeviceCmd, http_url: &str, api_key: Option<&str>) -> Result<()> {
    match cmd {
        DeviceCmd::Get { mac } =>
            get(mac, http_url, api_key).await,
        DeviceCmd::Set { mac, power, brightness, color_temp, rgb, send_ir, set_dp } =>
            set(mac, power, brightness, color_temp, rgb, send_ir, set_dp, http_url, api_key).await,
        DeviceCmd::List { groups } =>
            list(groups, http_url, api_key).await,
        DeviceCmd::Add { mac, name, ip, tuya_id, local_key, model, port, dp_profile } =>
            add(mac, name, ip, tuya_id, local_key, model, port, dp_profile, http_url, api_key).await,
        DeviceCmd::Remove { mac } =>
            remove(mac, http_url, api_key).await,
        DeviceCmd::Watch { mac } =>
            watch(mac, http_url, api_key).await,
        DeviceCmd::Group(GroupCmd::Create { name, model, members }) =>
            group_create(name, model, members, http_url, api_key).await,
        DeviceCmd::Group(GroupCmd::Update { mac, name, members }) =>
            group_update(mac, name, members, http_url, api_key).await,
        DeviceCmd::Import =>
            import(http_url, api_key).await,
    }
}

// ─── Handlers ────────────────────────────────────────────────────────────────

async fn get(mac: String, http_url: &str, api_key: Option<&str>) -> Result<()> {
    let resp = rest_get(&format!("{http_url}/api/v1/devices/{mac}"), api_key).await
        .context("GET /api/v1/devices/{mac}")?;

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        bail!("device {mac} not found");
    }
    if !resp.status().is_success() {
        bail!("server error: {}", resp.text().await?);
    }

    let d: serde_json::Value = resp.json().await?;
    println!("device:    {}", d["mac"].as_str().unwrap_or("?"));
    println!("name:      {}", d["name"].as_str().unwrap_or("?"));
    println!("protocol:  {}", d["protocol"].as_str().unwrap_or("?"));
    println!("ip:        {}", d["ip"].as_str().unwrap_or("-"));

    if let Some(state) = d["state"].as_object() {
        let online = state["online"].as_bool().unwrap_or(false);
        println!("online:    {}", online);
        if let Some(p) = state["power"].as_bool() {
            println!("power:     {}", if p { "on" } else { "off" });
        }
        if let Some(b) = state["brightness"].as_u64() {
            println!("brightness:{b}/1000");
        }
        if let Some(ct) = state["color_temp_k"].as_u64() {
            println!("color_temp:{ct} K");
        }
        if let Some(rgb) = state["rgb"].as_array() {
            if rgb.len() == 3 {
                println!("rgb:       ({},{},{})", rgb[0], rgb[1], rgb[2]);
            }
        }
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
    http_url:   &str,
    api_key:    Option<&str>,
) -> Result<()> {
    let cmd_json = build_command_json(power, brightness, color_temp, rgb, send_ir, set_dp)?;

    let client = reqwest::Client::new();
    let mut req = client
        .post(format!("{http_url}/api/v1/devices/{mac}/command"))
        .json(&cmd_json);
    if let Some(key) = api_key {
        req = req.header("Authorization", format!("Bearer {key}"));
    }
    let resp = req.send().await.context("POST /api/v1/devices/{mac}/command")?;
    if !resp.status().is_success() {
        bail!("command failed: {}", resp.text().await?);
    }

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    get(mac, http_url, api_key).await
}

async fn list(groups_only: bool, http_url: &str, api_key: Option<&str>) -> Result<()> {
    let resp = rest_get(&format!("{http_url}/api/v1/devices"), api_key).await
        .context("GET /api/v1/devices")?;
    if !resp.status().is_success() {
        bail!("server error: {}", resp.text().await?);
    }

    let devices: Vec<serde_json::Value> = resp.json().await?;
    let devices: Vec<_> = devices.into_iter()
        .filter(|d| !groups_only || d["protocol"].as_str() == Some("group"))
        .collect();

    if devices.is_empty() {
        println!("no devices registered");
        return Ok(());
    }

    for d in &devices {
        let mac      = d["mac"].as_str().unwrap_or("?");
        let name     = d["name"].as_str().unwrap_or("?");
        let ip       = d["ip"].as_str().unwrap_or("-");
        let protocol = d["protocol"].as_str().unwrap_or("?");
        let version  = d["tuya_version"].as_str().unwrap_or("-");
        println!("{mac}  {ip:15}  {:32}  {protocol:12}  v{version}", name);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn add(
    mac:        String,
    name:       String,
    ip:         String,
    tuya_id:    String,
    local_key:  String,
    model:      String,
    port:       u16,
    dp_profile: String,
    http_url:   &str,
    api_key:    Option<&str>,
) -> Result<()> {
    if local_key.len() != 16 {
        bail!("--local-key must be exactly 16 characters (got {})", local_key.len());
    }

    let body = serde_json::json!({
        "mac":        mac,
        "name":       name,
        "ip":         ip,
        "tuya_id":    tuya_id,
        "local_key":  local_key,
        "model":      model,
        "port":       port,
        "dp_profile": dp_profile,
    });

    let client = reqwest::Client::new();
    let mut req = client.post(format!("{http_url}/api/v1/devices")).json(&body);
    if let Some(key) = api_key {
        req = req.header("Authorization", format!("Bearer {key}"));
    }
    let resp = req.send().await.context("POST /api/v1/devices")?;
    if !resp.status().is_success() {
        bail!("registration failed: {}", resp.text().await?);
    }
    println!("device registered — plugin connecting in background");
    Ok(())
}

async fn remove(mac: String, http_url: &str, api_key: Option<&str>) -> Result<()> {
    let client = reqwest::Client::new();
    let mut req = client.delete(format!("{http_url}/api/v1/devices/{mac}"));
    if let Some(key) = api_key {
        req = req.header("Authorization", format!("Bearer {key}"));
    }
    let resp = req.send().await.context("DELETE /api/v1/devices/{mac}")?;
    if !resp.status().is_success() {
        bail!("unregister failed: {}", resp.text().await?);
    }
    println!("device unregistered");
    Ok(())
}

async fn watch(mac: Option<String>, http_url: &str, api_key: Option<&str>) -> Result<()> {
    let client = reqwest::Client::new();
    let mut req = client
        .get(format!("{http_url}/api/v1/events"))
        .header("Accept", "text/event-stream");
    if let Some(key) = api_key {
        req = req.header("Authorization", format!("Bearer {key}"));
    }
    let mut resp = req.send().await.context("GET /api/v1/events")?;
    if !resp.status().is_success() {
        bail!("server error: {}", resp.text().await?);
    }

    println!("watching device state (Ctrl-C to stop)…");

    let mut buf = String::new();
    while let Some(chunk) = resp.chunk().await? {
        buf.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(nl) = buf.find('\n') {
            let line = buf[..nl].trim_end_matches('\r').to_string();
            buf = buf[nl + 1..].to_string();

            let Some(data) = line.strip_prefix("data: ") else { continue };
            let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else { continue };

            if let Some(ref filter_mac) = mac {
                if v["mac"].as_str() != Some(filter_mac.as_str()) {
                    continue;
                }
            }

            let device_mac = v["mac"].as_str().unwrap_or("?");
            let online     = v["online"].as_bool().unwrap_or(false);
            print!("{device_mac} [{}]", if online { "online" } else { "offline" });
            if let Some(p)  = v["power"].as_bool()       { print!("  power={}", if p { "on" } else { "off" }); }
            if let Some(b)  = v["brightness"].as_u64()   { print!("  bri={b}"); }
            if let Some(ct) = v["color_temp_k"].as_u64() { print!("  ct={ct}K"); }
            if let Some(rgb) = v["rgb"].as_array() {
                if rgb.len() == 3 { print!("  rgb=({},{},{})", rgb[0], rgb[1], rgb[2]); }
            }
            println!();
        }
    }
    Ok(())
}

async fn group_create(
    name:     String,
    model:    String,
    members:  String,
    http_url: &str,
    api_key:  Option<&str>,
) -> Result<()> {
    let member_macs: Vec<&str> = members.split(',').map(str::trim).collect();
    let body = serde_json::json!({ "name": name, "model": model, "members": member_macs });

    let client = reqwest::Client::new();
    let mut req = client.post(format!("{http_url}/api/v1/groups")).json(&body);
    if let Some(key) = api_key {
        req = req.header("Authorization", format!("Bearer {key}"));
    }
    let resp = req.send().await.context("POST /api/v1/groups")?;
    if !resp.status().is_success() {
        bail!("group creation failed: {}", resp.text().await?);
    }
    let result: serde_json::Value = resp.json().await?;
    println!("group created — mac: {}", result["mac"].as_str().unwrap_or("?"));
    Ok(())
}

async fn group_update(
    mac:      String,
    name:     Option<String>,
    members:  Option<String>,
    http_url: &str,
    api_key:  Option<&str>,
) -> Result<()> {
    let mut body = serde_json::json!({});
    if let Some(n) = name    { body["name"]    = serde_json::json!(n); }
    if let Some(m) = members {
        let macs: Vec<&str> = m.split(',').map(str::trim).collect();
        body["members"] = serde_json::json!(macs);
    }

    let client = reqwest::Client::new();
    let mut req = client.patch(format!("{http_url}/api/v1/groups/{mac}")).json(&body);
    if let Some(key) = api_key {
        req = req.header("Authorization", format!("Bearer {key}"));
    }
    let resp = req.send().await.context("PATCH /api/v1/groups/{mac}")?;
    if !resp.status().is_success() {
        bail!("group update failed: {}", resp.text().await?);
    }
    println!("group updated");
    Ok(())
}

async fn import(http_url: &str, api_key: Option<&str>) -> Result<()> {
    let client = reqwest::Client::new();
    let mut req = client.post(format!("{http_url}/api/v1/pairing/import"));
    if let Some(key) = api_key {
        req = req.header("Authorization", format!("Bearer {key}"));
    }

    println!("Scanning local network for 5 seconds…");
    let resp = req.send().await.context("POST /api/v1/pairing/import")?;
    if !resp.status().is_success() {
        bail!("server error: {}", resp.text().await?);
    }

    let result: serde_json::Value = resp.json().await?;
    let registered      = result["registered"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let already         = result["already_registered"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let not_discovered  = result["not_discovered"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let skipped_virtual = result["skipped_virtual"].as_array().map(Vec::as_slice).unwrap_or(&[]);

    if registered.is_empty() && already.is_empty() {
        println!("No devices discovered on the local network.");
    }

    if !registered.is_empty() {
        println!("\nRegistered ({}):", registered.len());
        for d in registered {
            println!("  {}  {:32}  {}  ({})",
                d["mac"].as_str().unwrap_or("?"),
                d["name"].as_str().unwrap_or("?"),
                d["ip"].as_str().unwrap_or("?"),
                d["dp_profile"].as_str().unwrap_or("?"),
            );
        }
    }
    if !already.is_empty() {
        println!("\nAlready registered ({}):", already.len());
        for d in already {
            println!("  {}  {}", d["mac"].as_str().unwrap_or("?"), d["name"].as_str().unwrap_or("?"));
        }
    }
    if !not_discovered.is_empty() {
        println!("\nOnline but not found locally ({}):", not_discovered.len());
        for d in not_discovered {
            println!("  {}  {}", d["id"].as_str().unwrap_or("?"), d["name"].as_str().unwrap_or("?"));
        }
    }
    if !skipped_virtual.is_empty() {
        println!("\nSkipped (virtual/sub-device) ({}):", skipped_virtual.len());
        for d in skipped_virtual {
            println!("  {}  {}", d["id"].as_str().unwrap_or("?"), d["name"].as_str().unwrap_or("?"));
        }
    }
    Ok(())
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

async fn rest_get(url: &str, api_key: Option<&str>) -> Result<reqwest::Response> {
    let client = reqwest::Client::new();
    let mut req = client.get(url);
    if let Some(key) = api_key {
        req = req.header("Authorization", format!("Bearer {key}"));
    }
    Ok(req.send().await?)
}

/// Build a `CommandDto`-compatible JSON value from CLI flags.
pub fn build_command_json(
    power:      Option<bool>,
    brightness: Option<u32>,
    color_temp: Option<u32>,
    rgb:        Option<String>,
    send_ir:    Option<String>,
    set_dp:     Option<String>,
) -> Result<serde_json::Value> {
    if let Some(v) = power {
        Ok(serde_json::json!({ "type": "set_power", "on": v }))
    } else if let Some(v) = brightness {
        Ok(serde_json::json!({ "type": "set_brightness", "level": v }))
    } else if let Some(v) = color_temp {
        Ok(serde_json::json!({ "type": "set_color_temp", "kelvin": v }))
    } else if let Some(s) = rgb {
        let parts: Vec<u8> = s
            .split(',')
            .map(|x| x.trim().parse::<u8>())
            .collect::<std::result::Result<_, _>>()
            .map_err(|_| anyhow::anyhow!("--rgb: three comma-separated 0–255 values, e.g. 255,128,0"))?;
        if parts.len() != 3 { bail!("--rgb requires exactly 3 components"); }
        Ok(serde_json::json!({ "type": "set_rgb", "r": parts[0], "g": parts[1], "b": parts[2] }))
    } else if let Some(ir) = send_ir {
        let pos = ir.find(':')
            .ok_or_else(|| anyhow::anyhow!("--send-ir: expected HEAD:KEY (HEAD may be empty, e.g. \":KEY\")"))?;
        let head = &ir[..pos];
        let key  = &ir[pos + 1..];
        Ok(serde_json::json!({ "type": "send_ir", "head": head, "key": key }))
    } else if let Some(dp_spec) = set_dp {
        let parts: Vec<&str> = dp_spec.splitn(3, ':').collect();
        if parts.len() != 3 { bail!("--set-dp requires format DP:TYPE:VALUE, e.g. 3:str:low"); }
        let dp: u16 = parts[0].parse().map_err(|_| anyhow::anyhow!("DP must be a number"))?;
        match parts[1] {
            "bool" => {
                let b: bool = parts[2].parse()
                    .map_err(|_| anyhow::anyhow!("bool value must be true or false"))?;
                Ok(serde_json::json!({ "type": "set_dp", "dp": dp, "bool_val": b }))
            }
            "int" => {
                let i: i64 = parts[2].parse()
                    .map_err(|_| anyhow::anyhow!("int value must be a number"))?;
                Ok(serde_json::json!({ "type": "set_dp", "dp": dp, "int_val": i }))
            }
            "str" => Ok(serde_json::json!({ "type": "set_dp", "dp": dp, "str_val": parts[2] })),
            t => bail!("unknown DP type '{t}'; use bool, int, or str"),
        }
    } else {
        bail!("provide one of --power, --brightness, --color-temp, --rgb, --send-ir, or --set-dp");
    }
}
