use std::{sync::Arc, time::Duration};

use dashmap::DashMap;
use synaptex_types::{
    capability::DeviceCommand,
    device::DeviceId,
    plugin::{BoxedPlugin, PluginError, PluginResult, StateBusSender},
};
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use crate::cache::StateCache;

// ── Reconnect tuning ─────────────────────────────────────────────────────────

/// How often the supervisor checks `is_connected()` when the device is up.
const HEALTH_POLL:    Duration = Duration::from_secs(5);
/// Starting backoff before the first reconnect attempt.
const INITIAL_BACKOFF: Duration = Duration::from_secs(2);
/// Ceiling for exponential backoff.
const MAX_BACKOFF:    Duration = Duration::from_secs(60);

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

    /// Register a plugin and immediately start its reconnect supervisor.
    ///
    /// The supervisor handles the first `connect()` call and all subsequent
    /// reconnects — `register` itself returns instantly.
    pub fn register(&self, plugin: BoxedPlugin) {
        let id         = *plugin.device_id();
        let supervisor = Self::spawn_supervisor(plugin.clone(), self.cache.clone());
        self.entries.insert(id, Entry { plugin, supervisor });
        info!(%id, "plugin registered");
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

    /// Stop the supervisor, disconnect the plugin, and evict from the cache.
    pub async fn deregister(&self, id: &DeviceId) {
        if let Some((_, entry)) = self.entries.remove(id) {
            entry.supervisor.abort();
            entry.plugin.disconnect().await;
            self.cache.remove(id);
            warn!(%id, "plugin deregistered");
        }
    }

    pub fn bus_sender(&self) -> &StateBusSender {
        &self.bus_tx
    }

    // ── Supervisor ────────────────────────────────────────────────────────────

    /// Spawn a task that manages the full connection lifecycle for `plugin`.
    ///
    /// On first run, and on every disconnection, the task calls `connect()`
    /// with exponential backoff, then calls `poll_state()` to hydrate the
    /// cache.  Between reconnect attempts the backoff doubles up to
    /// `MAX_BACKOFF`.  The task exits only when it is aborted (via
    /// `deregister`).
    fn spawn_supervisor(plugin: BoxedPlugin, cache: Arc<StateCache>) -> JoinHandle<()> {
        tokio::spawn(async move {
            let id          = *plugin.device_id();
            let mut backoff = INITIAL_BACKOFF;

            loop {
                if !plugin.is_connected() {
                    info!(%id, ?backoff, "attempting connect");
                    match plugin.connect().await {
                        Ok(()) => {
                            info!(%id, "connected");
                            backoff = INITIAL_BACKOFF; // reset on success

                            // Hydrate cache with a fresh state snapshot.
                            match plugin.poll_state().await {
                                Ok(state) => cache.insert(state),
                                Err(e)    => warn!(%id, "poll_state after connect failed: {e}"),
                            }
                        }
                        Err(e) => {
                            error!(%id, "connect failed: {e}; retrying in {backoff:?}");
                            tokio::time::sleep(backoff).await;
                            backoff = (backoff * 2).min(MAX_BACKOFF);
                            continue; // skip the health poll sleep
                        }
                    }
                }

                tokio::time::sleep(HEALTH_POLL).await;
            }
        })
    }
}
