/// Automatic Bond bridge discovery: contacts the hub, enumerates sub-devices,
/// persists them to sled, and registers `BondPlugin` instances with the registry.
///
/// Triggered automatically on Bond hub registration and repeated on a 5-minute
/// interval to pick up newly added devices.
use std::{collections::HashSet, sync::Arc, time::Duration};

use synaptex_bond::{discovery, BondPlugin};
use synaptex_types::plugin::StateBusSender;

use crate::{
    db::{self, PluginConfig},
    plugin::PluginRegistry,
};

const SYNC_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Discover and register all Bond sub-devices for a single hub.
///
/// `connect_ip`  — IP currently reachable (may differ from `managed_ip` before
///                 DHCP renewal).
/// `hub_mac`     — MAC of the Bond bridge (used for stable DeviceId derivation).
/// `bond_token`  — BOND-Token header value.
/// `managed_ip`  — Router-allocated IP stored in the BondConfig for future use.
pub async fn sync_hub(
    connect_ip: &str,
    hub_mac:    &str,
    bond_token: &str,
    managed_ip: &str,
    trees:      Arc<crate::db::Trees>,
    registry:   Arc<PluginRegistry>,
    bus_tx:     StateBusSender,
) {
    let devices = match discovery::discover_hub_devices(
        connect_ip, hub_mac, bond_token, managed_ip,
    ).await {
        Ok(d)  => d,
        Err(e) => {
            tracing::warn!(hub_ip = connect_ip, "bond: discovery failed: {e:#}");
            return;
        }
    };

    // Collect device IDs already present in sled so we don't re-register.
    let existing: HashSet<synaptex_types::device::DeviceId> = db::list_all_devices(&trees)
        .unwrap_or_default()
        .into_iter()
        .map(|info| info.id)
        .collect();

    let mut new_count = 0usize;
    for (info, cfg) in devices {
        if existing.contains(&info.id) {
            continue;
        }

        if let Err(e) = db::register_device(&trees, &info) {
            tracing::warn!(name = %info.name, "bond: failed to save device info: {e}");
            continue;
        }

        if let Err(e) = db::save_plugin_config(&trees, &info.id, &PluginConfig::Bond(cfg.clone())) {
            tracing::warn!(name = %info.name, "bond: failed to save plugin config: {e}");
            continue;
        }

        let plugin = BondPlugin::new(info.clone(), cfg, bus_tx.clone());
        registry.register(Arc::new(plugin));
        tracing::info!(
            name      = %info.name,
            device_id = %info.id,
            hub_ip    = connect_ip,
            "bond: registered virtual device",
        );
        new_count += 1;
    }

    if new_count > 0 {
        tracing::info!(new_count, hub_ip = connect_ip, "bond: sync complete");
    }
}

/// Spawn a background task that re-syncs the hub every `SYNC_INTERVAL`.
/// Picks up devices added to the hub after the initial registration.
pub fn spawn_periodic_sync(
    connect_ip: String,
    hub_mac:    String,
    bond_token: String,
    managed_ip: String,
    trees:      Arc<crate::db::Trees>,
    registry:   Arc<PluginRegistry>,
    bus_tx:     StateBusSender,
) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(SYNC_INTERVAL).await;
            sync_hub(
                &connect_ip, &hub_mac, &bond_token, &managed_ip,
                trees.clone(), registry.clone(), bus_tx.clone(),
            ).await;
        }
    });
}
