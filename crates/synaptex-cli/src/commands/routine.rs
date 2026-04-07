use anyhow::{bail, Result};
use clap::Subcommand;
use tonic::transport::Channel;

use synaptex_proto::{
    device_service_client::DeviceServiceClient,
    CancelRoutineRequest,
    CommandStep,
    CreateRoutineRequest,
    DeleteRoutineRequest,
    DeviceId as ProtoDeviceId,
    GetRoutineRequest,
    ListRoutinesRequest,
    RgbValue,
    RoutineStep as ProtoRoutineStep,
    SendIrCommand,
    SetDpCommand,
    SwitchCommand,
    TriggerRoutineRequest,
    UpdateRoutineRequest,
    WaitStep,
    command_step::{Command as CsCommand, Target as CsTarget},
    routine_step::Step as ProtoStep,
    set_dp_command::Value as DpValue,
};

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

        /// New cron schedule (omit to keep current).
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

pub async fn run(cmd: RoutineCmd, client: &mut DeviceServiceClient<Channel>) -> Result<()> {
    match cmd {
        RoutineCmd::Create   { name, schedule, steps }       => create(name, schedule, steps, client).await,
        RoutineCmd::Update   { id, name, schedule, steps }   => update(id, name, schedule, steps, client).await,
        RoutineCmd::Delete   { id }                          => delete(id, client).await,
        RoutineCmd::List                                      => list(client).await,
        RoutineCmd::Get      { id }                          => get(id, client).await,
        RoutineCmd::Trigger  { id }                          => trigger(id, client).await,
        RoutineCmd::Cancel   { id }                          => cancel(id, client).await,
    }
}

// ─── Handlers ────────────────────────────────────────────────────────────────

async fn create(
    name:     String,
    schedule: Option<String>,
    steps:    Vec<String>,
    client:   &mut DeviceServiceClient<Channel>,
) -> Result<()> {
    if steps.is_empty() {
        bail!("at least one --step is required");
    }
    let proto_steps: Result<Vec<_>> = steps.iter().map(|s| parse_step(s)).collect();
    let resp = client
        .create_routine(CreateRoutineRequest {
            name,
            schedule: schedule.unwrap_or_default(),
            steps:    proto_steps?,
        })
        .await?
        .into_inner();

    if resp.ok {
        println!("routine created — id: {}", resp.id);
    } else {
        bail!("routine creation failed: {}", resp.error_message);
    }
    Ok(())
}

async fn update(
    id:       String,
    name:     Option<String>,
    schedule: Option<String>,
    steps:    Vec<String>,
    client:   &mut DeviceServiceClient<Channel>,
) -> Result<()> {
    let proto_steps: Result<Vec<_>> = steps.iter().map(|s| parse_step(s)).collect();
    let resp = client
        .update_routine(UpdateRoutineRequest {
            id,
            name:     name.unwrap_or_default(),
            schedule: schedule.unwrap_or_default(),
            steps:    proto_steps?,
        })
        .await?
        .into_inner();

    if resp.ok {
        println!("routine updated");
    } else {
        bail!("routine update failed: {}", resp.error_message);
    }
    Ok(())
}

async fn delete(id: String, client: &mut DeviceServiceClient<Channel>) -> Result<()> {
    let resp = client
        .delete_routine(DeleteRoutineRequest { id })
        .await?
        .into_inner();

    if resp.ok {
        println!("routine deleted");
    } else {
        bail!("routine deletion failed: {}", resp.error_message);
    }
    Ok(())
}

async fn list(client: &mut DeviceServiceClient<Channel>) -> Result<()> {
    let resp = client
        .list_routines(ListRoutinesRequest {})
        .await?
        .into_inner();

    if resp.routines.is_empty() {
        println!("no routines");
        return Ok(());
    }

    for r in &resp.routines {
        let sched = if r.schedule.is_empty() { "manual".to_string() } else { r.schedule.clone() };
        println!("{}  {:32}  {:30}  {} step(s)", r.id, r.name, sched, r.steps.len());
    }
    Ok(())
}

async fn get(id: String, client: &mut DeviceServiceClient<Channel>) -> Result<()> {
    let resp = client
        .get_routine(GetRoutineRequest { id })
        .await?
        .into_inner();

    match resp.routine {
        None => bail!("routine not found"),
        Some(r) => {
            println!("id:       {}", r.id);
            println!("name:     {}", r.name);
            println!("schedule: {}", if r.schedule.is_empty() { "manual" } else { &r.schedule });
            println!("steps ({}):", r.steps.len());
            for (i, step) in r.steps.iter().enumerate() {
                println!("  {}. {}", i + 1, describe_step(step));
            }
        }
    }
    Ok(())
}

async fn trigger(id: String, client: &mut DeviceServiceClient<Channel>) -> Result<()> {
    let resp = client
        .trigger_routine(TriggerRoutineRequest { id })
        .await?
        .into_inner();

    if resp.ok {
        println!("routine triggered");
    } else {
        bail!("trigger failed: {}", resp.error_message);
    }
    Ok(())
}

async fn cancel(id: String, client: &mut DeviceServiceClient<Channel>) -> Result<()> {
    let resp = client
        .cancel_routine(CancelRoutineRequest { id })
        .await?
        .into_inner();

    if resp.ok {
        println!("routine cancelled");
    } else {
        bail!("cancel failed: {}", resp.error_message);
    }
    Ok(())
}

// ─── Step parsing ─────────────────────────────────────────────────────────────

/// Parse a `/`-separated step string into a `ProtoRoutineStep`.
///
/// Formats:
///   wait/SECS
///   room/UUID/COMMAND[/ARG...]
///   device/MAC/COMMAND[/ARG...]
///
/// COMMAND variants: power, brightness, colortemp, rgb, switch, ir, dp
pub fn parse_step(s: &str) -> Result<ProtoRoutineStep> {
    // Split at most 4 times so the 4th element captures "rest" (handles dp values with slashes).
    let mut iter = s.splitn(4, '/');
    let kind = iter.next().ok_or_else(|| anyhow::anyhow!("empty step"))?;

    if kind == "wait" {
        let secs_str = iter
            .next()
            .ok_or_else(|| anyhow::anyhow!("wait: expected SECS"))?;
        let secs: u64 = secs_str
            .parse()
            .map_err(|_| anyhow::anyhow!("wait: SECS must be a non-negative integer"))?;
        return Ok(ProtoRoutineStep { step: Some(ProtoStep::Wait(WaitStep { secs })) });
    }

    let id  = iter.next().ok_or_else(|| anyhow::anyhow!("'{kind}': expected ID"))?;
    let cmd = iter.next().ok_or_else(|| anyhow::anyhow!("'{kind}/{id}': expected COMMAND"))?;
    let rest = iter.next(); // everything after the 3rd '/'

    let target = match kind {
        "room"   => CsTarget::RoomId(id.to_string()),
        "device" => CsTarget::DeviceId(ProtoDeviceId { mac: id.to_string() }),
        other    => bail!("unknown step kind '{other}'; expected wait, room, or device"),
    };

    let command = if cmd == "switch" {
        // Format: room/UUID/switch/INDEX/STATE → rest = "INDEX/STATE"
        let rest = rest.ok_or_else(|| anyhow::anyhow!("switch: expected INDEX/STATE"))?;
        let mut sw = rest.splitn(2, '/');
        let index: u32 = sw
            .next()
            .ok_or_else(|| anyhow::anyhow!("switch: expected index"))?
            .parse()
            .map_err(|_| anyhow::anyhow!("switch: INDEX must be a number"))?;
        let state: bool = sw
            .next()
            .ok_or_else(|| anyhow::anyhow!("switch: expected state after index"))?
            .parse()
            .map_err(|_| anyhow::anyhow!("switch: STATE must be true or false"))?;
        CsCommand::SetSwitch(SwitchCommand { index, state })
    } else {
        let arg = rest.ok_or_else(|| anyhow::anyhow!("'{cmd}': expected argument"))?;
        parse_cmd_arg(cmd, arg)?
    };

    Ok(ProtoRoutineStep {
        step: Some(ProtoStep::Command(CommandStep {
            target:  Some(target),
            command: Some(command),
        })),
    })
}

fn parse_cmd_arg(cmd: &str, arg: &str) -> Result<CsCommand> {
    match cmd {
        "power" => {
            let v: bool = arg
                .parse()
                .map_err(|_| anyhow::anyhow!("power: expected true or false"))?;
            Ok(CsCommand::SetPower(v))
        }
        "brightness" => {
            let v: u32 = arg
                .parse()
                .map_err(|_| anyhow::anyhow!("brightness: expected a number 0-1000"))?;
            Ok(CsCommand::SetBrightness(v))
        }
        "colortemp" => {
            let v: u32 = arg
                .parse()
                .map_err(|_| anyhow::anyhow!("colortemp: expected a Kelvin value"))?;
            Ok(CsCommand::SetColorTempK(v))
        }
        "rgb" => {
            let parts: Vec<u32> = arg
                .split(',')
                .map(|x| x.trim().parse::<u32>())
                .collect::<std::result::Result<_, _>>()
                .map_err(|_| anyhow::anyhow!("rgb: expected three comma-separated integers, e.g. 0,0,255"))?;
            if parts.len() != 3 {
                bail!("rgb: expected exactly 3 components (R,G,B)");
            }
            Ok(CsCommand::SetRgb(RgbValue { r: parts[0], g: parts[1], b: parts[2] }))
        }
        "ir" => {
            let pos = arg.find(':').ok_or_else(|| {
                anyhow::anyhow!("ir: expected HEAD:KEY format (HEAD may be empty, e.g. :KEY)")
            })?;
            let head = arg[..pos].to_string();
            let key  = arg[pos + 1..].to_string();
            Ok(CsCommand::SendIr(SendIrCommand { head, key }))
        }
        "dp" => {
            let parts: Vec<&str> = arg.splitn(3, ':').collect();
            if parts.len() != 3 {
                bail!("dp: expected DP:TYPE:VALUE format, e.g. 3:str:low");
            }
            let dp: u32 = parts[0]
                .parse()
                .map_err(|_| anyhow::anyhow!("dp: DP must be a number"))?;
            let value = match parts[1] {
                "bool" => {
                    let b: bool = parts[2]
                        .parse()
                        .map_err(|_| anyhow::anyhow!("dp: bool value must be true or false"))?;
                    DpValue::BoolVal(b)
                }
                "int" => {
                    let i: i64 = parts[2]
                        .parse()
                        .map_err(|_| anyhow::anyhow!("dp: int value must be a number"))?;
                    DpValue::IntVal(i)
                }
                "str" => DpValue::StringVal(parts[2].to_string()),
                t => bail!("dp: unknown type '{t}'; use bool, int, or str"),
            };
            Ok(CsCommand::SetDp(SetDpCommand { dp, value: Some(value) }))
        }
        other => bail!("unknown command '{other}'; expected power, brightness, colortemp, rgb, switch, ir, or dp"),
    }
}

// ─── Display helpers ──────────────────────────────────────────────────────────

fn describe_step(step: &ProtoRoutineStep) -> String {
    match &step.step {
        None => "(empty)".to_string(),
        Some(ProtoStep::Wait(w)) => format!("wait {}s", w.secs),
        Some(ProtoStep::Command(cs)) => {
            let target = match &cs.target {
                None                           => "?".to_string(),
                Some(CsTarget::RoomId(id))     => format!("room:{id}"),
                Some(CsTarget::DeviceId(d))    => format!("device:{}", d.mac),
            };
            let cmd = match &cs.command {
                None                                 => "?".to_string(),
                Some(CsCommand::SetPower(v))         => format!("power={v}"),
                Some(CsCommand::SetBrightness(v))    => format!("brightness={v}"),
                Some(CsCommand::SetColorTempK(v))    => format!("colortemp={v}K"),
                Some(CsCommand::SetRgb(c))           => format!("rgb=({},{},{})", c.r, c.g, c.b),
                Some(CsCommand::SetSwitch(sw))       => format!("switch[{}]={}", sw.index, sw.state),
                Some(CsCommand::SendIr(ir))          => format!("ir={}:{}", ir.head, ir.key),
                Some(CsCommand::SetDp(dp))           => format!("dp={}", dp.dp),
            };
            format!("{target}  {cmd}")
        }
    }
}
