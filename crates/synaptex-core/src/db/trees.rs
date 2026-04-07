use anyhow::Result;
use sled::{Db, Tree};

/// Strongly-typed handles to every sled tree used by synaptex-core.
pub struct Trees {
    /// Static device metadata: `DeviceId` → `postcard(DeviceInfo)`.
    pub registry: Tree,
    /// Live device state: `DeviceId` → `postcard(DeviceState)`.
    pub state:    Tree,
    /// Authentication material: `DeviceId` → `postcard(Vec<u8>)`.
    pub auth:     Tree,
    /// Protocol-specific plugin configs: `DeviceId` → `postcard(PluginConfig)`.
    pub configs:  Tree,
    /// Named rooms: `room_id (UUID string bytes)` → `postcard(Room)`.
    pub rooms:    Tree,
    /// Named routines: `routine_id (UUID string bytes)` → `postcard(Routine)`.
    pub routines: Tree,
    /// Global config items: string key → `postcard(blob)`.
    pub config:      Tree,
    /// Tuya product_id → bool (soft_reset_supported).
    pub probe_cache: Tree,
}

impl Trees {
    pub fn open(db: &Db) -> Result<Self> {
        Ok(Self {
            registry:    db.open_tree("registry")?,
            state:       db.open_tree("state")?,
            auth:        db.open_tree("auth")?,
            configs:     db.open_tree("configs")?,
            rooms:       db.open_tree("rooms")?,
            routines:    db.open_tree("routines")?,
            config:      db.open_tree("config")?,
            probe_cache: db.open_tree("probe_cache")?,
        })
    }
}
