use std::{sync::Arc, time::Duration};

use synaptex_types::{
    capability::DeviceCommand,
    plugin::{PluginError, PluginResult},
};

use crate::{db::Room, plugin::PluginRegistry};

/// Fan out `cmd` to every device in `room` that advertises the required capability.
///
/// - Devices that return `PluginError::UnsupportedCommand` are **silently skipped**.
/// - Devices not found in the registry are silently skipped (they may have been
///   removed without the room being updated).
/// - All other errors or timeouts are aggregated and returned as a single error
///   string `"MAC: reason; MAC: timed out"`.
///
/// Returns `Ok(())` if all eligible devices succeeded (or there were none).
pub async fn execute_room_command(
    room:     &Room,
    cmd:      DeviceCommand,
    registry: &Arc<PluginRegistry>,
) -> PluginResult<()> {
    let mut handles = Vec::new();

    for &device_id in &room.device_ids {
        let registry = registry.clone();
        let cmd      = cmd.clone();
        handles.push(tokio::spawn(async move {
            let result = tokio::time::timeout(
                Duration::from_secs(5),
                registry.execute_command(&device_id, cmd),
            )
            .await;
            (device_id, result)
        }));
    }

    let mut errors = Vec::new();
    for handle in handles {
        match handle.await {
            Ok((_, Ok(Ok(()))))                           => { /* success */ }
            Ok((_, Ok(Err(PluginError::UnsupportedCommand)))) => { /* skip silently */ }
            Ok((_id, Ok(Err(PluginError::Unreachable(_))))) => { /* not registered; skip */ }
            Ok((id, Ok(Err(e))))                          => errors.push(format!("{id}: {e}")),
            Ok((id, Err(_timeout)))                       => errors.push(format!("{id}: timed out")),
            Err(_panic)                                   => errors.push("task panicked".into()),
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(PluginError::Protocol(errors.join("; ")))
    }
}
