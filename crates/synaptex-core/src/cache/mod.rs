use dashmap::DashMap;
use synaptex_types::{device::DeviceId, plugin::DeviceState};

/// In-memory hot cache backed by a lock-free concurrent hash map.
///
/// All state reads go through this cache; sled is the write-behind store
/// updated by the `persist_task` in `bus.rs`.
#[derive(Default)]
pub struct StateCache(DashMap<DeviceId, DeviceState>);

impl StateCache {
    pub fn new() -> Self {
        Self(DashMap::new())
    }

    pub fn insert(&self, state: DeviceState) {
        self.0.insert(state.device_id, state);
    }

    pub fn get(&self, id: &DeviceId) -> Option<DeviceState> {
        self.0.get(id).map(|r| r.clone())
    }

    pub fn all(&self) -> Vec<DeviceState> {
        self.0.iter().map(|r| r.value().clone()).collect()
    }

    pub fn remove(&self, id: &DeviceId) {
        self.0.remove(id);
    }
}
