use std::collections::HashMap;

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
        #[arg(long, value_name = "BOOL")]
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

        /// Send an IR code. Format: `HEAD:KEY` (HEAD may be empty, e.g. `:KEY`).
        #[arg(long, value_name = "HEAD:KEY", group = "cmd")]
        send_ir: Option<String>,

        /// Write a raw DP. Format: `DP:TYPE:VALUE` where TYPE is bool|int|str.
        #[arg(long, value_name = "DP:TYPE:VALUE", group = "cmd")]
        set_dp: Option<String>,

        /// Set fan speed: off | low | medium | high.
        #[arg(long, value_name = "SPEED", group = "cmd")]
        fan_speed: Option<String>,
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

        /// DP profile preset: bulb_a | bulb_b | switch | fan | fan_light | fan_light_simple | fan_light_numeric | ir1 | ir2
        #[arg(long, default_value = "bulb_b")]
        dp_profile: String,
    },

    /// Unregister a device and stop its plugin.
    Remove {
        #[arg(long, value_name = "MAC")]
        mac: String,

        /// Also remove the device from the Tuya Cloud account (full de-registration).
        /// WARNING: this is permanent — the device loses its cloud identity,
        /// Alexa routines, and HomeLife scenes.
        #[arg(long)]
        factory_reset: bool,
    },

    /// Stream live state updates from a device (or all devices if --mac is omitted).
    Watch {
        #[arg(long, value_name = "MAC")]
        mac: Option<String>,
    },

    /// Update the DP profile and/or protocol version of a registered device and reload its plugin.
    SetProfile {
        #[arg(long, value_name = "MAC")]
        mac: String,

        /// DP profile preset: bulb_a | bulb_b | switch | fan | fan_light | fan_light_simple | fan_light_numeric | ir1 | ir2
        #[arg(long, value_name = "PROFILE")]
        profile: Option<String>,

        /// Protocol version: 3.3 | 3.4 | 3.5
        #[arg(long, value_name = "VERSION")]
        protocol_version: Option<String>,
    },

    /// Directly probe a Tuya device (bypasses the daemon).
    /// Config JSON can be piped from: curl .../devices/{mac}/debug-config
    Probe {
        /// Device config JSON with tuya_id, local_key, ip, port, dp_profile.
        /// Reads from stdin if not provided.
        #[arg(long, value_name = "JSON")]
        config: Option<String>,

        /// DP key=value pairs to set, e.g. 1=bool:true 3=str:1
        /// If omitted, performs a get (status dump).
        #[arg(value_name = "DP=TYPE:VALUE")]
        set_dps: Vec<String>,
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
        DeviceCmd::Set { mac, power, brightness, color_temp, rgb, color_mode, send_ir, set_dp, fan_speed } =>
            set(mac, power, brightness, color_temp, rgb, color_mode, send_ir, set_dp, fan_speed, http_url, api_key).await,
        DeviceCmd::List { groups } =>
            list(groups, http_url, api_key).await,
        DeviceCmd::Add { mac, name, ip, tuya_id, local_key, model, port, dp_profile } =>
            add(mac, name, ip, tuya_id, local_key, model, port, dp_profile, http_url, api_key).await,
        DeviceCmd::Remove { mac, factory_reset } =>
            remove(mac, factory_reset, http_url, api_key).await,
        DeviceCmd::Watch { mac } =>
            watch(mac, http_url, api_key).await,
        DeviceCmd::Group(GroupCmd::Create { name, model, members }) =>
            group_create(name, model, members, http_url, api_key).await,
        DeviceCmd::Group(GroupCmd::Update { mac, name, members }) =>
            group_update(mac, name, members, http_url, api_key).await,
        DeviceCmd::SetProfile { mac, profile, protocol_version } =>
            set_profile(mac, profile, protocol_version, http_url, api_key).await,
        DeviceCmd::Probe { config, set_dps } =>
            probe(config, set_dps, http_url, api_key).await,
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
    color_mode: Option<String>,
    send_ir:    Option<String>,
    set_dp:     Option<String>,
    fan_speed:  Option<String>,
    http_url:   &str,
    api_key:    Option<&str>,
) -> Result<()> {
    // Look up the device type to determine whether to use SetLight or SetPower.
    let is_light = {
        let resp = rest_get(&format!("{http_url}/api/v1/devices/{mac}"), api_key).await
            .context("GET /api/v1/devices/{mac}")?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            bail!("device {mac} not found");
        }
        if !resp.status().is_success() {
            bail!("server error: {}", resp.text().await?);
        }
        let d: serde_json::Value = resp.json().await?;
        matches!(device_type(&d).as_str(),
            "rgb_bulb" | "bulb" | "fan+bulb" | "fan+rgb_bulb" | "fan+light"
        )
    };

    let cmd_json = build_command_json(power, brightness, color_temp, rgb, color_mode, send_ir, set_dp, fan_speed, Some(is_light))?;

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

    // mac (uppercase) → room name, group name
    let mut room_map:  HashMap<String, String> = HashMap::new();
    let mut group_map: HashMap<String, String> = HashMap::new();

    if let Ok(resp) = rest_get(&format!("{http_url}/api/v1/rooms"), api_key).await {
        if let Ok(rooms) = resp.json::<Vec<serde_json::Value>>().await {
            for room in &rooms {
                let room_name = room["name"].as_str().unwrap_or("?").to_string();
                for mac in room["devices"].as_array().into_iter().flatten().filter_map(|v| v.as_str()) {
                    room_map.insert(mac.to_uppercase(), room_name.clone());
                }
            }
        }
    }
    if let Ok(resp) = rest_get(&format!("{http_url}/api/v1/groups"), api_key).await {
        if let Ok(groups) = resp.json::<Vec<serde_json::Value>>().await {
            for group in &groups {
                let group_name = group["name"].as_str().unwrap_or("?").to_string();
                for mac in group["members"].as_array().into_iter().flatten().filter_map(|v| v.as_str()) {
                    group_map.insert(mac.to_uppercase(), group_name.clone());
                }
            }
        }
    }

    for d in &devices {
        let mac       = d["mac"].as_str().unwrap_or("?");
        let name      = d["name"].as_str().unwrap_or("?");
        let ip        = d["ip"].as_str().unwrap_or("-");
        let protocol  = d["protocol"].as_str().unwrap_or("?");
        let version   = match d["tuya_version"].as_str() {
            Some(v) => format!("v{v}"),
            None    => if protocol == "group" { "-".to_string() } else { "v-".to_string() },
        };
        let dtype     = device_type(d);
        let room_lbl  = room_map.get(&mac.to_uppercase())
            .map(|r| format!("  room:{r}"))
            .unwrap_or_default();
        let group_lbl = group_map.get(&mac.to_uppercase())
            .map(|g| format!("  group:{g}"))
            .unwrap_or_default();
        println!("{mac}  {ip:15}  {:32}  {dtype:10}  {protocol:12}  {version}{room_lbl}{group_lbl}", name);
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

async fn remove(mac: String, factory_reset: bool, http_url: &str, api_key: Option<&str>) -> Result<()> {
    let client = reqwest::Client::new();
    let url = if factory_reset {
        format!("{http_url}/api/v1/devices/{mac}?factory_reset=true")
    } else {
        format!("{http_url}/api/v1/devices/{mac}")
    };
    let mut req = client.delete(&url);
    if let Some(key) = api_key {
        req = req.header("Authorization", format!("Bearer {key}"));
    }
    let resp = req.send().await.context("DELETE /api/v1/devices")?;
    if !resp.status().is_success() {
        bail!("unregister failed: {}", resp.text().await?);
    }
    if factory_reset {
        println!("device unregistered and removed from Tuya Cloud");
    } else {
        println!("device unregistered");
    }
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
            if let Some(spd) = v["fan_speed"].as_str()   { print!("  fan={spd}"); }
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

async fn probe(config_arg: Option<String>, set_dps: Vec<String>, _http_url: &str, _api_key: Option<&str>) -> Result<()> {
    use std::{net::IpAddr, sync::Arc};
    use synaptex_tuya::{plugin::TuyaConfig, TuyaDeviceConfig, TuyaPlugin};
    use synaptex_types::{device::{DeviceId, DeviceInfo}, plugin::DevicePlugin};

    // Enable debug logging so reader-task traces are visible.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "synaptex_tuya=debug".into()),
        )
        .try_init();
    // Read config JSON from arg or stdin.
    let json_str = match config_arg {
        Some(s) => s,
        None => {
            let mut buf = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)?;
            buf
        }
    };
    let cfg: serde_json::Value = serde_json::from_str(&json_str)
        .context("failed to parse config JSON")?;

    let tuya_id   = cfg["tuya_id"].as_str().context("missing tuya_id")?.to_string();
    let local_key = cfg["local_key"].as_str().context("missing local_key")?.to_string();
    let ip: IpAddr = cfg["ip"].as_str().context("missing ip")?.parse()
        .context("invalid IP")?;
    let port  = cfg["port"].as_u64().unwrap_or(6668) as u16;
    let dp_profile  = cfg["dp_profile"].as_str().unwrap_or("bulb_b").to_string();
    let protocol_version = cfg["protocol_version"].as_str().map(str::to_string);

    let device_id = cfg["mac"].as_str()
        .and_then(|m| DeviceId::from_mac_str(m).ok())
        .unwrap_or_else(|| DeviceId::from_mac_str("00:00:00:00:00:00").unwrap());
    let info = DeviceInfo {
        id:           device_id,
        name:         "probe".into(),
        model:        String::new(),
        protocol:     "tuya_local".into(),
        capabilities: vec![],
    };

    let tuya_cfg = TuyaDeviceConfig {
        device_id,
        ip,
        port,
        tuya_id,
        local_key,
        dp_profile,
        dp_map: None,
        protocol_version,
    };

    let (bus_tx, _) = tokio::sync::broadcast::channel(16);
    let mut bus_rx  = bus_tx.subscribe();
    let plugin = Arc::new(TuyaPlugin::new(info, TuyaConfig {
        ip:            tuya_cfg.ip,
        port:          tuya_cfg.port,
        tuya_id:       tuya_cfg.tuya_id.clone(),
        local_key:     tuya_cfg.local_key.clone(),
        dp_map:        tuya_cfg.dp_map(),
        protocol_version: tuya_cfg.protocol_version.clone(),
    }, bus_tx));

    if set_dps.is_empty() {
        // GET: poll state, capture raw DPs from bus event.
        let id = device_id;
        let state = match plugin.poll_state().await {
            Ok(s) => s,
            Err(e) => {
                println!("\n=== parsed state ===");
                println!("  online:       false");
                println!("  reason:       {e}");
                return Ok(());
            }
        };

        // Drain any queued event from our pre-poll subscription for raw DPs.
        let raw_dps = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            async {
                loop {
                    match bus_rx.recv().await {
                        Ok(ev) if ev.device_id == id => return ev.raw_dps,
                        Ok(_)  => continue,
                        Err(_) => return HashMap::new(),
                    }
                }
            },
        )
        .await
        .unwrap_or_default();

        println!("\n=== raw DPs ===");
        let mut keys: Vec<&String> = raw_dps.keys().collect();
        keys.sort_by_key(|k| k.parse::<u32>().unwrap_or(u32::MAX));
        for k in keys {
            println!("  DP {:>3}: {}", k, raw_dps[k]);
        }

        println!("\n=== parsed state ===");
        println!("  online:       {}", state.online);
        if !state.online {
            println!("  reason:       no DPS received within timeout (wrong credentials or protocol version?)");
        }
        if let Some(v) = state.power        { println!("  power:        {v}"); }
        if let Some(v) = state.brightness   { println!("  brightness:   {v}"); }
        if let Some(v) = state.color_temp_k { println!("  color_temp_k: {v}"); }
        if let Some(v) = state.rgb          { println!("  rgb:          {:?}", v); }
        if let Some(v) = state.fan_speed    { println!("  fan_speed:    {:?}", v); }
        for (idx, on) in &state.switches    { println!("  switch[{idx}]:   {on}"); }
    } else {
        // SET: parse DP=TYPE:VALUE pairs and send.
        let mut dps: HashMap<String, serde_json::Value> = HashMap::new();
        for arg in &set_dps {
            let (dp, rest) = arg.split_once('=').context("expected DP=TYPE:VALUE")?;
            let (typ, val) = rest.split_once(':').context("expected TYPE:VALUE after =")?;
            let value = match typ {
                "bool" => serde_json::Value::Bool(val.parse::<bool>()
                    .context("bool must be true/false")?),
                "int"  => serde_json::Value::Number(val.parse::<i64>()
                    .context("int must be an integer")?.into()),
                "str"  => serde_json::Value::String(val.to_string()),
                other  => bail!("unknown type {other:?}, use bool|int|str"),
            };
            dps.insert(dp.to_string(), value);
        }
        println!("sending dps: {}", serde_json::to_string(&dps)?);
        plugin.send_dps(&dps).await?;
        println!("ok");
    }
    Ok(())
}

async fn set_profile(mac: String, profile: Option<String>, protocol_version: Option<String>, http_url: &str, api_key: Option<&str>) -> Result<()> {
    if profile.is_none() && protocol_version.is_none() {
        bail!("provide at least one of --profile or --protocol-hint");
    }
    let client = reqwest::Client::new();
    let mut body = serde_json::Map::new();
    if let Some(p) = &profile        { body.insert("dp_profile".into(),    p.clone().into()); }
    if let Some(h) = &protocol_version  { body.insert("protocol_version".into(), h.clone().into()); }
    let mut req = client
        .patch(format!("{http_url}/api/v1/devices/{mac}"))
        .json(&body);
    if let Some(key) = api_key {
        req = req.header("Authorization", format!("Bearer {key}"));
    }
    let resp = req.send().await?;
    if resp.status().is_success() {
        if let Some(p) = profile       { println!("dp_profile → {p}"); }
        if let Some(h) = protocol_version { println!("protocol_version → {h}"); }
    } else {
        let err: serde_json::Value = resp.json().await.unwrap_or_default();
        bail!("{}", err["message"].as_str().unwrap_or("unknown error"));
    }
    Ok(())
}

async fn import(http_url: &str, api_key: Option<&str>) -> Result<()> {
    let client = reqwest::Client::new();
    let mut req = client.post(format!("{http_url}/api/v1/pairing/import"));
    if let Some(key) = api_key {
        req = req.header("Authorization", format!("Bearer {key}"));
    }

    println!("Fetching devices from Tuya Cloud…");
    let resp = req.send().await.context("POST /api/v1/pairing/import")?;
    if !resp.status().is_success() {
        bail!("server error: {}", resp.text().await?);
    }

    let result: serde_json::Value = resp.json().await?;
    let registered      = result["registered"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let updated         = result["updated_registration"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let already         = result["already_registered"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let not_discovered  = result["not_discovered"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let skipped_virtual = result["skipped_virtual"].as_array().map(Vec::as_slice).unwrap_or(&[]);

    if registered.is_empty() && updated.is_empty() && already.is_empty() {
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
    if !updated.is_empty() {
        println!("\nRegistration updated ({}):", updated.len());
        for d in updated {
            println!("  {}  {}", d["mac"].as_str().unwrap_or("?"), d["name"].as_str().unwrap_or("?"));
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

/// Derive a human-readable device type label from the capability list in a DeviceDto JSON value.
fn device_type(d: &serde_json::Value) -> String {
    if d["protocol"].as_str() == Some("group") {
        return "group".to_string();
    }
    let caps: Vec<&str> = d["capabilities"]
        .as_array()
        .map(|arr| arr.iter().filter_map(|c| c["type"].as_str()).collect())
        .unwrap_or_default();
    let has_fan   = caps.contains(&"fan");
    let has_light = caps.contains(&"light");   // separate light DP
    let has_dim   = caps.contains(&"dimmer") || caps.contains(&"color_temp");
    let has_rgb   = caps.contains(&"rgb");
    if has_fan {
        return match (has_light || has_dim || has_rgb, has_rgb, has_dim) {
            (false, _, _)  => "fan".to_string(),
            (_, true,  _)  => "fan+rgb_bulb".to_string(),
            (_, _, true)   => "fan+bulb".to_string(),
            _              => "fan+light".to_string(),  // on/off only
        };
    }
    if caps.contains(&"ir")         { return "ir".to_string(); }
    if has_rgb                       { return "rgb_bulb".to_string(); }
    if has_dim                       { return "bulb".to_string(); }
    let switch_count = caps.iter().filter(|&&t| t == "switch").count();
    if switch_count > 0             { return format!("switch_{switch_count}"); }
    if caps.contains(&"power")      { return "switch".to_string(); }
    "device".to_string()
}

async fn rest_get(url: &str, api_key: Option<&str>) -> Result<reqwest::Response> {
    let client = reqwest::Client::new();
    let mut req = client.get(url);
    if let Some(key) = api_key {
        req = req.header("Authorization", format!("Bearer {key}"));
    }
    Ok(req.send().await?)
}

/// Build a `CommandDto`-compatible JSON value from CLI flags.
///
/// `is_light`: `Some(true)` = device has light capabilities (use `SetLight` for light flags),
///             `Some(false)` = device is a plain switch (reject light attributes),
///             `None` = unknown / room command (auto-detect from which flags are set).
pub fn build_command_json(
    power:      Option<bool>,
    brightness: Option<u32>,
    color_temp: Option<u32>,
    rgb:        Option<String>,
    color_mode: Option<String>,
    send_ir:    Option<String>,
    set_dp:     Option<String>,
    fan_speed:  Option<String>,
    is_light:   Option<bool>,
) -> Result<serde_json::Value> {
    let has_light_attrs = brightness.is_some() || color_temp.is_some()
        || rgb.is_some() || color_mode.is_some();
    let has_exclusive = send_ir.is_some() || set_dp.is_some() || fan_speed.is_some();

    if (power.is_some() || has_light_attrs) && has_exclusive {
        bail!("--power/--brightness/--color-temp/--rgb/--color-mode cannot be combined \
               with --send-ir, --set-dp, or --fan-speed");
    }

    // Determine whether to use SetLight based on device type (if known) or flags.
    let use_set_light = match is_light {
        Some(true)  => power.is_some() || has_light_attrs,
        Some(false) => {
            if has_light_attrs {
                bail!("this device does not support brightness/colour controls");
            }
            false // switch/outlet: fall through to SetPower
        }
        None => has_light_attrs, // room/unknown: use SetLight only when light attrs present
    };

    // Plain power toggle for non-light devices (or room with power-only).
    if power.is_some() && !use_set_light {
        return Ok(serde_json::json!({ "type": "set_power", "on": power.unwrap() }));
    }

    if use_set_light {
        // Parse optional rgb string into separate r/g/b fields.
        let (r, g, b) = if let Some(s) = rgb {
            let parts: Vec<u8> = s
                .split(',')
                .map(|x| x.trim().parse::<u8>())
                .collect::<std::result::Result<_, _>>()
                .map_err(|_| anyhow::anyhow!("--rgb: three comma-separated 0–255 values, e.g. 255,128,0"))?;
            if parts.len() != 3 { bail!("--rgb requires exactly 3 components"); }
            (Some(parts[0]), Some(parts[1]), Some(parts[2]))
        } else {
            (None, None, None)
        };
        let mut obj = serde_json::Map::new();
        obj.insert("type".into(), "set_light".into());
        if let Some(v) = power      { obj.insert("power".into(),      v.into()); }
        if let Some(v) = brightness { obj.insert("brightness".into(),  v.into()); }
        if let Some(v) = color_temp { obj.insert("color_temp".into(),  v.into()); }
        if let Some(v) = r          { obj.insert("r".into(),           v.into()); }
        if let Some(v) = g          { obj.insert("g".into(),           v.into()); }
        if let Some(v) = b          { obj.insert("b".into(),           v.into()); }
        if let Some(v) = color_mode {
            match v.as_str() {
                "white" | "colour" => {}
                _ => bail!("--color-mode must be 'white' or 'colour'"),
            }
            obj.insert("color_mode".into(), v.into());
        }
        return Ok(serde_json::Value::Object(obj));
    }

    if let Some(ir) = send_ir {
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
    } else if let Some(s) = fan_speed {
        match s.as_str() {
            "off" | "low" | "medium" | "high" =>
                Ok(serde_json::json!({ "type": "set_fan_speed", "speed": s })),
            _ => bail!("--fan-speed must be one of: off, low, medium, high"),
        }
    } else {
        bail!("provide at least one of --power, --brightness, --color-temp, --rgb, --color-mode, \
               --send-ir, --set-dp, or --fan-speed");
    }
}
