use std::{sync::Arc, time::Duration};

use dashmap::DashMap;
use rand::Rng;
use synaptex_types::{
    capability::DeviceCommand,
    device::DeviceId,
    DeviceState,
    plugin::{BoxedPlugin, PluginError, PluginResult, StateBusSender},
};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::cache::StateCache;

// ── Poll interval ─────────────────────────────────────────────────────────────

/// Base poll interval.  A random jitter of ±15 s is added per device so that
/// all devices don't query the network simultaneously.
const POLL_INTERVAL_SECS: u64 = 60;

// ── Registry ─────────────────────────────────────────────────────────────────

/// Stores the plugin handle alongside its supervisor `JoinHandle` so that
/// `deregister` can abort the task without waiting for the next poll cycle.
struct Entry {
    plugin:     BoxedPlugin,
    supervisor: JoinHandle<()>,
}

pub struct PluginRegistry {
    entries: DashMap<DeviceId, Entry>,
    cache:   Arc<StateCache>,
    #[allow(dead_code)]
    bus_tx:  StateBusSender,
}

impl PluginRegistry {
    pub fn new(cache: Arc<StateCache>, bus_tx: StateBusSender) -> Self {
        Self {
            entries: DashMap::new(),
            cache,
            bus_tx,
        }
    }

    /// Register a plugin and immediately start its periodic poll supervisor.
    ///
    /// The supervisor handles the first `poll_state()` call (after a small
    /// random startup jitter) and all subsequent periodic polls — `register`
    /// itself returns instantly.
    pub fn register(&self, plugin: BoxedPlugin) {
        let id         = *plugin.device_id();
        let supervisor = Self::spawn_supervisor(plugin.clone(), self.cache.clone());
        self.entries.insert(id, Entry { plugin, supervisor });
        debug!(%id, "plugin registered");
    }

    /// Dispatch a command to the plugin owning `id`.
    pub async fn execute_command(
        &self,
        id:  &DeviceId,
        cmd: DeviceCommand,
    ) -> PluginResult<()> {
        match self.entries.get(id) {
            Some(e) => e.plugin.execute_command(cmd).await,
            None    => Err(PluginError::Unreachable(format!(
                "no plugin registered for {id}"
            ))),
        }
    }

    /// Poll the device for fresh state, update the cache, and return the result.
    /// Returns an error if no plugin is registered or the device is unreachable.
    pub async fn poll_device(&self, id: &DeviceId) -> PluginResult<DeviceState> {
        match self.entries.get(id) {
            Some(e) => {
                let state = e.plugin.poll_state().await?;
                self.cache.insert(state.clone());
                Ok(state)
            }
            None => Err(PluginError::Unreachable(format!(
                "no plugin registered for {id}"
            ))),
        }
    }

    /// Stop the supervisor, disconnect the plugin, and evict from the cache.
    pub async fn deregister(&self, id: &DeviceId) {
        if let Some((_, entry)) = self.entries.remove(id) {
            entry.supervisor.abort();
            entry.plugin.disconnect().await;
            self.cache.remove(id);
            warn!(%id, "plugin deregistered");
        }
    }


    // ── Supervisor ────────────────────────────────────────────────────────────

    /// Spawn a task that periodically calls `poll_state()` and inserts the
    /// result into the state cache.  Connections are opened on-demand by the
    /// plugin — no persistent TCP session is maintained here.
    ///
    /// Startup jitter staggers initial polls so all devices don't hit the
    /// network at the same moment after daemon start.
    fn spawn_supervisor(plugin: BoxedPlugin, cache: Arc<StateCache>) -> JoinHandle<()> {
        tokio::spawn(async move {
            // Stagger startup so all devices don't poll simultaneously.
            let initial_jitter = rand::thread_rng().gen_range(0u64..10);
            tokio::time::sleep(Duration::from_secs(initial_jitter)).await;

            loop {
                match plugin.poll_state().await {
                    Ok(state) => {
                        cache.insert(state);
                    }
                    Err(e) => {
                        debug!(device = %plugin.device_id(), "poll failed: {e}");
                    }
                }

                let secs = rand::thread_rng().gen_range(
                    (POLL_INTERVAL_SECS - 15)..=(POLL_INTERVAL_SECS + 15),
                );
                tokio::time::sleep(Duration::from_secs(secs)).await;
            }
        })
    }
}
