use synaptex_types::plugin::StateBusSender;

/// Broadcast channel capacity.  Events are dropped for lagging consumers.
const BUS_CAPACITY: usize = 256;

/// Create the shared state-change broadcast bus, returning the sender.
/// Receivers are created on-demand via `StateBusSender::subscribe()`.
pub fn new_bus() -> StateBusSender {
    let (tx, _rx) = tokio::sync::broadcast::channel(BUS_CAPACITY);
    tx
}

/// Spawn a background task that forwards events from the bus into the sled
/// `state` tree, keeping the persistent layer in sync without blocking plugins.
pub fn spawn_persist_task(
    bus_tx:  StateBusSender,
    trees:   std::sync::Arc<crate::db::Trees>,
    cache:   std::sync::Arc<crate::cache::StateCache>,
) {
    let mut rx = bus_tx.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    cache.merge(event.state.clone());
                    if let Err(e) = crate::db::put(&trees.state, &event.device_id, &event.state) {
                        tracing::error!(device = %event.device_id, "failed to persist state: {e}");
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("persist task lagged, dropped {n} events");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}
