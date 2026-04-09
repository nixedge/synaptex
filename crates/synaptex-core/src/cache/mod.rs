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

    /// Merge a (possibly partial) state update into the cache.
    ///
    /// `online` and `updated_at_ms` are always overwritten.  All `Option`
    /// fields are only overwritten when the incoming value is `Some`, so a
    /// partial echo-back (e.g. only the mode DP changed) preserves the full
    /// state that was built by the last complete poll.
    pub fn merge(&self, new: DeviceState) {
        self.0
            .entry(new.device_id)
            .and_modify(|s| {
                s.online        = new.online;
                s.updated_at_ms = new.updated_at_ms;
                if let Some(v) = new.power        { s.power        = Some(v); }
                if let Some(v) = new.brightness   { s.brightness   = Some(v); }
                if let Some(v) = new.color_temp_k { s.color_temp_k = Some(v); }
                if let Some(v) = new.rgb          { s.rgb          = Some(v); }
                if !new.switches.is_empty()       { s.switches     = new.switches.clone(); }
                if let Some(v) = new.fan_speed    { s.fan_speed    = Some(v); }
                if let Some(v) = new.temp_current     { s.temp_current     = Some(v); }
                if let Some(v) = new.temp_set         { s.temp_set         = Some(v); }
                if let Some(v) = new.temp_calibration { s.temp_calibration = Some(v); }
            })
            .or_insert(new);
    }

    pub fn get(&self, id: &DeviceId) -> Option<DeviceState> {
        self.0.get(id).map(|r| r.clone())
    }

    #[allow(dead_code)]
    pub fn all(&self) -> Vec<DeviceState> {
        self.0.iter().map(|r| r.value().clone()).collect()
    }

    pub fn remove(&self, id: &DeviceId) {
        self.0.remove(id);
    }
}
