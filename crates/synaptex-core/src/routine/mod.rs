use std::{sync::Arc, time::Duration};

use dashmap::DashMap;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::{
    db::{self, Routine, RoutineStep, RoutineTarget, Trees},
    plugin::PluginRegistry,
    room,
};

// ─── RoutineRunner ────────────────────────────────────────────────────────────

/// Manages running routine executions and scheduled cron tasks.
pub struct RoutineRunner {
    /// In-flight execution handles: routine_id → JoinHandle.
    running:    DashMap<String, JoinHandle<()>>,
    /// Cron loop handles: routine_id → JoinHandle.
    cron_tasks: DashMap<String, JoinHandle<()>>,
}

impl RoutineRunner {
    pub fn new() -> Self {
        Self {
            running:    DashMap::new(),
            cron_tasks: DashMap::new(),
        }
    }

    /// Cancel any in-flight run for this routine and start a fresh execution
    /// from step 1 (cancel-and-restart policy).
    pub fn trigger(
        &self,
        routine:  Routine,
        registry: Arc<PluginRegistry>,
        trees:    Arc<Trees>,
    ) {
        // Abort any existing run.
        if let Some((_, h)) = self.running.remove(&routine.id) {
            h.abort();
        }

        let id     = routine.id.clone();
        let handle = tokio::spawn(execute_routine(routine.steps, registry, trees));
        info!(routine_id = %id, "routine triggered");
        self.running.insert(id, handle);
    }

    /// Abort any in-flight execution without affecting the cron schedule.
    pub fn cancel(&self, routine_id: &str) {
        if let Some((_, h)) = self.running.remove(routine_id) {
            h.abort();
            info!(routine_id = %routine_id, "routine cancelled");
        }
    }

    /// Parse `routine.schedule` and spawn a cron loop that calls `trigger` at
    /// each scheduled time.  Aborts any previously-running cron task for the
    /// same routine ID.
    pub fn start_cron(
        self:     &Arc<Self>,
        routine:  Routine,
        registry: Arc<PluginRegistry>,
        trees:    Arc<Trees>,
    ) -> anyhow::Result<()> {
        use std::str::FromStr;

        let schedule_str = routine
            .schedule
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("routine has no schedule"))?;

        let schedule = cron::Schedule::from_str(schedule_str)
            .map_err(|e| anyhow::anyhow!("invalid cron expression '{schedule_str}': {e}"))?;

        let id   = routine.id.clone();
        let this = Arc::clone(self);

        let handle = tokio::spawn(async move {
            use chrono::Local;
            loop {
                let next = schedule.upcoming(Local).next();
                match next {
                    None => {
                        warn!(routine_id = %routine.id, "cron schedule exhausted");
                        break;
                    }
                    Some(dt) => {
                        let now  = Local::now();
                        let wait = (dt - now).to_std().unwrap_or(Duration::ZERO);
                        tokio::time::sleep(wait).await;
                        this.trigger(routine.clone(), Arc::clone(&registry), Arc::clone(&trees));
                    }
                }
            }
        });

        if let Some(old) = self.cron_tasks.insert(id, handle) {
            old.abort();
        }

        Ok(())
    }

    /// Abort the cron loop for this routine (does not affect in-flight runs).
    pub fn stop_cron(&self, routine_id: &str) {
        if let Some((_, h)) = self.cron_tasks.remove(routine_id) {
            h.abort();
        }
    }
}

// ─── Step execution ───────────────────────────────────────────────────────────

async fn execute_routine(
    steps:    Vec<RoutineStep>,
    registry: Arc<PluginRegistry>,
    trees:    Arc<Trees>,
) {
    for step in steps {
        match step {
            RoutineStep::Wait { secs } => {
                tokio::time::sleep(Duration::from_secs(secs)).await;
            }
            RoutineStep::Command { target, command } => {
                execute_step_command(target, command, &registry, &trees).await;
            }
        }
    }
}

async fn execute_step_command(
    target:   RoutineTarget,
    command:  synaptex_types::capability::DeviceCommand,
    registry: &Arc<PluginRegistry>,
    trees:    &Arc<Trees>,
) {
    match target {
        RoutineTarget::Device(device_id) => {
            if let Err(e) = registry.execute_command(&device_id, command).await {
                warn!(device = %device_id, error = %e, "routine step: device command failed");
            }
        }
        RoutineTarget::Room(room_id) => {
            match db::get_room(trees, &room_id) {
                Ok(Some(room)) => {
                    if let Err(e) = room::execute_room_command(&room, command, registry).await {
                        warn!(room_id = %room_id, error = %e, "routine step: room command failed");
                    }
                }
                Ok(None) => {
                    warn!(room_id = %room_id, "routine step: room not found");
                }
                Err(e) => {
                    warn!(room_id = %room_id, error = %e, "routine step: failed to load room");
                }
            }
        }
    }
}
