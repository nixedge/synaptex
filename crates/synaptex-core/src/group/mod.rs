use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use rand::RngCore;
use tokio::{sync::Mutex, task::JoinHandle};
use tracing::warn;

use synaptex_types::{
    capability::{Capability, DeviceCommand},
    device::{DeviceId, DeviceInfo},
    plugin::{DevicePlugin, DeviceState, PluginError, PluginResult, StateBusSender},
};

use crate::{cache::StateCache, plugin::PluginRegistry};

// ─── Group ID ────────────────────────────────────────────────────────────────

/// Generate a random locally-administered unicast MAC for use as a group ID.
pub fn new_group_id() -> DeviceId {
    let mut bytes = [0u8; 6];
    rand::thread_rng().fill_bytes(&mut bytes);
    // Set locally-administered bit (bit 1), clear multicast bit (bit 0).
    bytes[0] = (bytes[0] | 0x02) & 0xFE;
    DeviceId(bytes)
}

// ─── State synthesis ─────────────────────────────────────────────────────────

/// Synthesize a group `DeviceState` from the current member states in the cache.
pub fn compute_group_state(
    group_id:   DeviceId,
    member_ids: &[DeviceId],
    cache:      &StateCache,
) -> DeviceState {
    let states: Vec<DeviceState> = member_ids
        .iter()
        .filter_map(|id| cache.get(id))
        .collect();

    let online  = states.iter().any(|s| s.online);
    let primary = states.iter().find(|s| s.online).or_else(|| states.first());

    DeviceState {
        device_id:     group_id,
        online,
        updated_at_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64,
        power:        primary.and_then(|s| s.power),
        brightness:   primary.and_then(|s| s.brightness),
        color_temp_k: primary.and_then(|s| s.color_temp_k),
        rgb:          primary.and_then(|s| s.rgb),
        switches:     primary.map(|s| s.switches.clone()).unwrap_or_default(),
    }
}

// ─── GroupPlugin ─────────────────────────────────────────────────────────────

pub struct GroupPlugin {
    info:           DeviceInfo,
    member_ids:     Vec<DeviceId>,
    registry:       Arc<PluginRegistry>,
    cache:          Arc<StateCache>,
    bus_tx:         StateBusSender,
    connected:      AtomicBool,
    watcher_handle: Mutex<Option<JoinHandle<()>>>,
}

impl GroupPlugin {
    pub fn new(
        info:       DeviceInfo,
        member_ids: Vec<DeviceId>,
        registry:   Arc<PluginRegistry>,
        cache:      Arc<StateCache>,
        bus_tx:     StateBusSender,
    ) -> Self {
        Self {
            info,
            member_ids,
            registry,
            cache,
            bus_tx,
            connected:      AtomicBool::new(false),
            watcher_handle: Mutex::new(None),
        }
    }
}

#[async_trait]
impl DevicePlugin for GroupPlugin {
    fn device_id(&self)    -> &DeviceId  { &self.info.id }
    fn name(&self)         -> &str       { &self.info.name }
    fn protocol(&self)     -> &str       { "group" }
    fn capabilities(&self) -> &[Capability] { &self.info.capabilities }
    fn is_connected(&self) -> bool       { self.connected.load(Ordering::Relaxed) }

    async fn connect(&self) -> PluginResult<()> {
        self.connected.store(true, Ordering::Relaxed);

        // Spawn watcher task: subscribe to bus, re-publish synthetic state
        // whenever any member emits an event.
        let group_id   = self.info.id;
        let member_ids = self.member_ids.clone();
        let cache      = self.cache.clone();
        let bus_tx     = self.bus_tx.clone();
        let mut rx     = self.bus_tx.subscribe();

        let handle = tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        if member_ids.contains(&event.device_id) {
                            let state = compute_group_state(group_id, &member_ids, &cache);
                            let _ = bus_tx.send(synaptex_types::plugin::StateChangeEvent {
                                device_id: group_id,
                                state,
                            });
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!(group = %group_id, dropped = n, "group watcher lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        *self.watcher_handle.lock().await = Some(handle);
        Ok(())
    }

    async fn disconnect(&self) {
        self.connected.store(false, Ordering::Relaxed);
        if let Some(h) = self.watcher_handle.lock().await.take() {
            h.abort();
        }
    }

    async fn poll_state(&self) -> PluginResult<DeviceState> {
        Ok(compute_group_state(self.info.id, &self.member_ids, &self.cache))
    }

    async fn execute_command(&self, cmd: DeviceCommand) -> PluginResult<()> {
        let mut handles: Vec<JoinHandle<(DeviceId, Result<PluginResult<()>, tokio::time::error::Elapsed>)>> =
            Vec::with_capacity(self.member_ids.len());

        for &member_id in &self.member_ids {
            let registry = self.registry.clone();
            let cmd      = cmd.clone();
            handles.push(tokio::spawn(async move {
                let result = tokio::time::timeout(
                    Duration::from_secs(5),
                    registry.execute_command(&member_id, cmd),
                )
                .await;
                (member_id, result)
            }));
        }

        let mut errors = Vec::new();
        for handle in handles {
            match handle.await {
                Ok((_id, Ok(Ok(()))))  => { /* success */ }
                Ok((id, Ok(Err(e))))   => errors.push(format!("{id}: {e}")),
                Ok((id, Err(_)))       => errors.push(format!("{id}: timed out")),
                Err(_)                 => errors.push("task panicked".into()),
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(PluginError::Protocol(errors.join("; ")))
        }
    }
}

