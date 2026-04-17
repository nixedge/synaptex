/// Mysa cloud device sync: calls the REST API, persists new devices to sled,
/// and registers `MysaPlugin` instances with the registry.
///
/// Triggered on daemon start (for devices added since last run) and on Mysa
/// hub registration via `POST /api/v1/hubs`.
use std::sync::Arc;

use synaptex_mysa::{MysaAccount, MysaPlugin, discovery};

use crate::{
    db::{self, PluginConfig},
    plugin::PluginRegistry,
};

/// Discover Mysa cloud devices and register any that are new.
///
/// Uses the current session's id_token for REST API authentication.
pub async fn sync_account(
    account:  Arc<MysaAccount>,
    trees:    Arc<crate::db::Trees>,
    registry: Arc<PluginRegistry>,
) {
    let session = match account.ensure_auth().await {
        Ok(s)  => s,
        Err(e) => {
            tracing::warn!("mysa: sync_account: auth failed: {e:#}");
            return;
        }
    };

    let devices = match discovery::discover_devices(&session.id_token).await {
        Ok(d)  => d,
        Err(e) => {
            tracing::warn!("mysa: sync_account: discovery failed: {e:#}");
            return;
        }
    };

    let mut new_count = 0usize;
    for (info, cfg) in devices {
        if registry.is_registered(&info.id) {
            continue;
        }

        // Persist DeviceInfo (if new).
        let already_in_sled = db::get::<synaptex_types::DeviceInfo>(&trees.registry, &info.id)
            .unwrap_or(None)
            .is_some();
        if !already_in_sled {
            if let Err(e) = db::register_device(&trees, &info) {
                tracing::warn!(name = %info.name, "mysa: failed to save device info: {e}");
                continue;
            }
        }

        // Persist plugin config.
        if let Err(e) = db::save_plugin_config(&trees, &info.id, &PluginConfig::Mysa(cfg.clone())) {
            tracing::warn!(name = %info.name, "mysa: failed to save plugin config: {e}");
            continue;
        }

        account.add_device(cfg.mysa_id.clone(), cfg.device_id);
        let plugin = MysaPlugin::new(info.clone(), cfg, account.clone());
        registry.register(Arc::new(plugin));

        tracing::info!(
            name      = %info.name,
            device_id = %info.id,
            "mysa: registered device",
        );
        new_count += 1;
    }

    if new_count > 0 {
        tracing::info!(new_count, "mysa: sync complete");
    }
}
