use anyhow::{bail, Context, Result};
use clap::Subcommand;

// ─── Subcommands ─────────────────────────────────────────────────────────────

#[derive(Debug, Subcommand)]
pub enum RoutineCmd {
    /// Create a new routine.
    Create {
        /// Human-readable name for the routine.
        #[arg(long)]
        name: String,

        /// 6-field cron schedule (e.g. "0 30 22 * * Mon-Fri").
        /// Omit for a manual-only routine.
        #[arg(long, value_name = "CRON")]
        schedule: Option<String>,

        /// One or more steps. Repeat the flag for multiple steps.
        /// Formats:
        ///   wait/SECS
        ///   room/UUID/power/true|false
        ///   room/UUID/brightness/0-1000
        ///   room/UUID/colortemp/KELVIN
        ///   room/UUID/rgb/R,G,B
        ///   room/UUID/switch/INDEX/true|false
        ///   room/UUID/ir/HEAD:KEY
        ///   room/UUID/dp/DP:TYPE:VALUE
        ///   device/MAC/power/true|false  (same variants as room)
        #[arg(long = "step", value_name = "STEP")]
        steps: Vec<String>,
    },

    /// Update a routine's name, schedule, and/or steps.
    Update {
        /// Routine UUID.
        #[arg(long)]
        id: String,

        /// New name (omit to keep current).
        #[arg(long)]
        name: Option<String>,

        /// New cron schedule (omit to keep current; use empty string to clear).
        #[arg(long, value_name = "CRON")]
        schedule: Option<String>,

        /// Replacement step list (omit to keep current; provide to replace all steps).
        #[arg(long = "step", value_name = "STEP")]
        steps: Vec<String>,
    },

    /// Delete a routine.
    Delete {
        /// Routine UUID.
        #[arg(long)]
        id: String,
    },

    /// List all routines.
    List,

    /// Show details of a single routine.
    Get {
        /// Routine UUID.
        #[arg(long)]
        id: String,
    },

    /// Trigger a routine immediately (cancel-and-restart if already running).
    Trigger {
        /// Routine UUID.
        #[arg(long)]
        id: String,
    },

    /// Cancel a currently-running routine execution.
    Cancel {
        /// Routine UUID.
        #[arg(long)]
        id: String,
    },
}

// ─── Dispatch ────────────────────────────────────────────────────────────────

pub async fn run(cmd: RoutineCmd, http_url: &str, api_key: Option<&str>) -> Result<()> {
    match cmd {
        RoutineCmd::Create  { name, schedule, steps }       => create(name, schedule, steps, http_url, api_key).await,
        RoutineCmd::Update  { id, name, schedule, steps }   => update(id, name, schedule, steps, http_url, api_key).await,
        RoutineCmd::Delete  { id }                          => delete(id, http_url, api_key).await,
        RoutineCmd::List                                     => list(http_url, api_key).await,
        RoutineCmd::Get     { id }                          => get(id, http_url, api_key).await,
        RoutineCmd::Trigger { id }                          => trigger(id, http_url, api_key).await,
        RoutineCmd::Cancel  { id }                          => cancel(id, http_url, api_key).await,
    }
}

// ─── Handlers ────────────────────────────────────────────────────────────────

async fn create(
    name:     String,
    schedule: Option<String>,
    steps:    Vec<String>,
    http_url: &str,
    api_key:  Option<&str>,
) -> Result<()> {
    if steps.is_empty() {
        bail!("at least one --step is required");
    }
    let step_jsons: Result<Vec<_>> = steps.iter().map(|s| parse_step(s)).collect();
    let body = serde_json::json!({
        "name":     name,
        "schedule": schedule,
        "steps":    step_jsons?,
    });

    let client = reqwest::Client::new();
    let mut req = client.post(format!("{http_url}/api/v1/routines")).json(&body);
    if let Some(key) = api_key { req = req.header("Authorization", format!("Bearer {key}")); }
    let resp = req.send().await.context("POST /api/v1/routines")?;
    if !resp.status().is_success() {
        bail!("routine creation failed: {}", resp.text().await?);
    }
    let result: serde_json::Value = resp.json().await?;
    println!("routine created — id: {}", result["id"].as_str().unwrap_or("?"));
    Ok(())
}

async fn update(
    id:       String,
    name:     Option<String>,
    schedule: Option<String>,
    steps:    Vec<String>,
    http_url: &str,
    api_key:  Option<&str>,
) -> Result<()> {
    // First fetch the existing routine so we can fill in unchanged fields.
    let client = reqwest::Client::new();
    let mut get_req = client.get(format!("{http_url}/api/v1/routines/{id}"));
    if let Some(key) = api_key { get_req = get_req.header("Authorization", format!("Bearer {key}")); }
    let get_resp = get_req.send().await.context("GET /api/v1/routines/{id}")?;
    if get_resp.status() == reqwest::StatusCode::NOT_FOUND { bail!("routine not found"); }
    if !get_resp.status().is_success() { bail!("server error: {}", get_resp.text().await?); }
    let existing: serde_json::Value = get_resp.json().await?;

    let final_name     = name.unwrap_or_else(|| existing["name"].as_str().unwrap_or("").to_string());
    let final_schedule = schedule.or_else(|| existing["schedule"].as_str().map(str::to_string));
    let final_steps = if steps.is_empty() {
        existing["steps"].clone()
    } else {
        let parsed: Result<Vec<_>> = steps.iter().map(|s| parse_step(s)).collect();
        serde_json::json!(parsed?)
    };

    let body = serde_json::json!({
        "name":     final_name,
        "schedule": final_schedule,
        "steps":    final_steps,
    });

    let mut put_req = client.put(format!("{http_url}/api/v1/routines/{id}")).json(&body);
    if let Some(key) = api_key { put_req = put_req.header("Authorization", format!("Bearer {key}")); }
    let resp = put_req.send().await.context("PUT /api/v1/routines/{id}")?;
    if !resp.status().is_success() {
        bail!("routine update failed: {}", resp.text().await?);
    }
    println!("routine updated");
    Ok(())
}

async fn delete(id: String, http_url: &str, api_key: Option<&str>) -> Result<()> {
    let client = reqwest::Client::new();
    let mut req = client.delete(format!("{http_url}/api/v1/routines/{id}"));
    if let Some(key) = api_key { req = req.header("Authorization", format!("Bearer {key}")); }
    let resp = req.send().await.context("DELETE /api/v1/routines/{id}")?;
    if !resp.status().is_success() {
        bail!("routine deletion failed: {}", resp.text().await?);
    }
    println!("routine deleted");
    Ok(())
}

async fn list(http_url: &str, api_key: Option<&str>) -> Result<()> {
    let client = reqwest::Client::new();
    let mut req = client.get(format!("{http_url}/api/v1/routines"));
    if let Some(key) = api_key { req = req.header("Authorization", format!("Bearer {key}")); }
    let resp = req.send().await.context("GET /api/v1/routines")?;
    if !resp.status().is_success() {
        bail!("server error: {}", resp.text().await?);
    }

    let routines: Vec<serde_json::Value> = resp.json().await?;
    if routines.is_empty() {
        println!("no routines");
        return Ok(());
    }
    for r in &routines {
        let sched = r["schedule"].as_str().unwrap_or("manual");
        let step_count = r["steps"].as_array().map(Vec::len).unwrap_or(0);
        println!("{}  {:32}  {:30}  {} step(s)",
            r["id"].as_str().unwrap_or("?"),
            r["name"].as_str().unwrap_or("?"),
            sched,
            step_count,
        );
    }
    Ok(())
}

async fn get(id: String, http_url: &str, api_key: Option<&str>) -> Result<()> {
    let client = reqwest::Client::new();
    let mut req = client.get(format!("{http_url}/api/v1/routines/{id}"));
    if let Some(key) = api_key { req = req.header("Authorization", format!("Bearer {key}")); }
    let resp = req.send().await.context("GET /api/v1/routines/{id}")?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND { bail!("routine not found"); }
    if !resp.status().is_success() { bail!("server error: {}", resp.text().await?); }

    let r: serde_json::Value = resp.json().await?;
    println!("id:       {}", r["id"].as_str().unwrap_or("?"));
    println!("name:     {}", r["name"].as_str().unwrap_or("?"));
    println!("schedule: {}", r["schedule"].as_str().unwrap_or("manual"));
    if let Some(steps) = r["steps"].as_array() {
        println!("steps ({}):", steps.len());
        for (i, step) in steps.iter().enumerate() {
            println!("  {}. {}", i + 1, describe_step(step));
        }
    }
    Ok(())
}

async fn trigger(id: String, http_url: &str, api_key: Option<&str>) -> Result<()> {
    let client = reqwest::Client::new();
    let mut req = client.post(format!("{http_url}/api/v1/routines/{id}/trigger"));
    if let Some(key) = api_key { req = req.header("Authorization", format!("Bearer {key}")); }
    let resp = req.send().await.context("POST /api/v1/routines/{id}/trigger")?;
    if !resp.status().is_success() {
        bail!("trigger failed: {}", resp.text().await?);
    }
    println!("routine triggered");
    Ok(())
}

async fn cancel(id: String, http_url: &str, api_key: Option<&str>) -> Result<()> {
    let client = reqwest::Client::new();
    let mut req = client.delete(format!("{http_url}/api/v1/routines/{id}/run"));
    if let Some(key) = api_key { req = req.header("Authorization", format!("Bearer {key}")); }
    let resp = req.send().await.context("DELETE /api/v1/routines/{id}/run")?;
    if !resp.status().is_success() {
        bail!("cancel failed: {}", resp.text().await?);
    }
    println!("routine cancelled");
    Ok(())
}

// ─── Step parsing ─────────────────────────────────────────────────────────────

/// Parse a `/`-separated step string into a `RoutineStepDto`-compatible JSON value.
///
/// Formats:
///   wait/SECS
///   room/UUID/COMMAND[/ARG...]
///   device/MAC/COMMAND[/ARG...]
///
/// COMMAND variants: power, brightness, colortemp, rgb, switch, ir, dp
pub fn parse_step(s: &str) -> Result<serde_json::Value> {
    let mut iter = s.splitn(4, '/');
    let kind = iter.next().ok_or_else(|| anyhow::anyhow!("empty step"))?;

    if kind == "wait" {
        let secs_str = iter.next().ok_or_else(|| anyhow::anyhow!("wait: expected SECS"))?;
        let secs: u64 = secs_str.parse()
            .map_err(|_| anyhow::anyhow!("wait: SECS must be a non-negative integer"))?;
        return Ok(serde_json::json!({ "type": "wait", "secs": secs }));
    }

    let id   = iter.next().ok_or_else(|| anyhow::anyhow!("'{kind}': expected ID"))?;
    let cmd  = iter.next().ok_or_else(|| anyhow::anyhow!("'{kind}/{id}': expected COMMAND"))?;
    let rest = iter.next();

    let target = match kind {
        "room"   => serde_json::json!({ "type": "room",   "id":  id }),
        "device" => serde_json::json!({ "type": "device", "mac": id }),
        other    => bail!("unknown step kind '{other}'; expected wait, room, or device"),
    };

    let command = if cmd == "switch" {
        let rest = rest.ok_or_else(|| anyhow::anyhow!("switch: expected INDEX/STATE"))?;
        let mut sw = rest.splitn(2, '/');
        let index: u8 = sw.next()
            .ok_or_else(|| anyhow::anyhow!("switch: expected index"))?
            .parse()
            .map_err(|_| anyhow::anyhow!("switch: INDEX must be a number"))?;
        let state: bool = sw.next()
            .ok_or_else(|| anyhow::anyhow!("switch: expected state after index"))?
            .parse()
            .map_err(|_| anyhow::anyhow!("switch: STATE must be true or false"))?;
        serde_json::json!({ "type": "set_switch", "index": index, "on": state })
    } else {
        let arg = rest.ok_or_else(|| anyhow::anyhow!("'{cmd}': expected argument"))?;
        parse_cmd_arg(cmd, arg)?
    };

    Ok(serde_json::json!({ "type": "command", "target": target, "command": command }))
}

fn parse_cmd_arg(cmd: &str, arg: &str) -> Result<serde_json::Value> {
    match cmd {
        "power" => {
            let v: bool = arg.parse()
                .map_err(|_| anyhow::anyhow!("power: expected true or false"))?;
            Ok(serde_json::json!({ "type": "set_power", "on": v }))
        }
        "brightness" => {
            let v: u16 = arg.parse()
                .map_err(|_| anyhow::anyhow!("brightness: expected a number 0-1000"))?;
            Ok(serde_json::json!({ "type": "set_brightness", "level": v }))
        }
        "colortemp" => {
            let v: u16 = arg.parse()
                .map_err(|_| anyhow::anyhow!("colortemp: expected a Kelvin value"))?;
            Ok(serde_json::json!({ "type": "set_color_temp", "kelvin": v }))
        }
        "rgb" => {
            let parts: Vec<u8> = arg.split(',')
                .map(|x| x.trim().parse::<u8>())
                .collect::<std::result::Result<_, _>>()
                .map_err(|_| anyhow::anyhow!("rgb: three comma-separated integers 0-255"))?;
            if parts.len() != 3 { bail!("rgb: expected exactly 3 components (R,G,B)"); }
            Ok(serde_json::json!({ "type": "set_rgb", "r": parts[0], "g": parts[1], "b": parts[2] }))
        }
        "ir" => {
            let pos = arg.find(':').ok_or_else(|| {
                anyhow::anyhow!("ir: expected HEAD:KEY format (HEAD may be empty, e.g. :KEY)")
            })?;
            let head = &arg[..pos];
            let key  = &arg[pos + 1..];
            Ok(serde_json::json!({ "type": "send_ir", "head": head, "key": key }))
        }
        "dp" => {
            let parts: Vec<&str> = arg.splitn(3, ':').collect();
            if parts.len() != 3 { bail!("dp: expected DP:TYPE:VALUE format, e.g. 3:str:low"); }
            let dp: u16 = parts[0].parse()
                .map_err(|_| anyhow::anyhow!("dp: DP must be a number"))?;
            match parts[1] {
                "bool" => {
                    let b: bool = parts[2].parse()
                        .map_err(|_| anyhow::anyhow!("dp: bool value must be true or false"))?;
                    Ok(serde_json::json!({ "type": "set_dp", "dp": dp, "bool_val": b }))
                }
                "int" => {
                    let i: i64 = parts[2].parse()
                        .map_err(|_| anyhow::anyhow!("dp: int value must be a number"))?;
                    Ok(serde_json::json!({ "type": "set_dp", "dp": dp, "int_val": i }))
                }
                "str" => Ok(serde_json::json!({ "type": "set_dp", "dp": dp, "str_val": parts[2] })),
                t => bail!("dp: unknown type '{t}'; use bool, int, or str"),
            }
        }
        other => bail!("unknown command '{other}'; expected power, brightness, colortemp, rgb, switch, ir, or dp"),
    }
}

// ─── Display helpers ──────────────────────────────────────────────────────────

fn describe_step(step: &serde_json::Value) -> String {
    match step["type"].as_str().unwrap_or("?") {
        "wait" => format!("wait {}s", step["secs"].as_u64().unwrap_or(0)),
        "command" => {
            let target = match step["target"]["type"].as_str().unwrap_or("?") {
                "room"   => format!("room:{}", step["target"]["id"].as_str().unwrap_or("?")),
                "device" => format!("device:{}", step["target"]["mac"].as_str().unwrap_or("?")),
                t        => format!("?:{t}"),
            };
            let cmd = format_command(& step["command"]);
            format!("{target}  {cmd}")
        }
        t => format!("?:{t}"),
    }
}

fn format_command(cmd: &serde_json::Value) -> String {
    match cmd["type"].as_str().unwrap_or("?") {
        "set_power"      => format!("power={}", cmd["on"]),
        "set_brightness" => format!("brightness={}", cmd["level"]),
        "set_color_temp" => format!("colortemp={}K", cmd["kelvin"]),
        "set_rgb"        => format!("rgb=({},{},{})", cmd["r"], cmd["g"], cmd["b"]),
        "set_switch"     => format!("switch[{}]={}", cmd["index"], cmd["on"]),
        "send_ir"        => format!("ir={}:{}", cmd["head"].as_str().unwrap_or(""), cmd["key"].as_str().unwrap_or("")),
        "set_dp"         => format!("dp={}", cmd["dp"]),
        t                => format!("?:{t}"),
    }
}
