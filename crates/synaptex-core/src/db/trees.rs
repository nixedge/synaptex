use anyhow::Result;
use sled::{Db, Tree};

/// Strongly-typed handles to every sled tree used by synaptex-core.
pub struct Trees {
    /// Static device metadata: `DeviceId` → `postcard(DeviceInfo)`.
    pub registry: Tree,
    /// Live device state: `DeviceId` → `postcard(DeviceState)`.
    pub state:    Tree,
    /// Protocol-specific plugin configs: `DeviceId` → `postcard(PluginConfig)`.
    pub configs:  Tree,
    /// Named rooms: `room_id (UUID string bytes)` → `postcard(Room)`.
    pub rooms:    Tree,
    /// Named routines: `routine_id (UUID string bytes)` → `postcard(Routine)`.
    pub routines: Tree,
    /// Global config items: string key → `postcard(blob)`.
    pub config:   Tree,
}

impl Trees {
    pub fn open(db: &Db) -> Result<Self> {
        Ok(Self {
            registry:    db.open_tree("registry")?,
            state:       db.open_tree("state")?,
            configs:     db.open_tree("configs")?,
            rooms:       db.open_tree("rooms")?,
            routines:    db.open_tree("routines")?,
            config:      db.open_tree("config")?,
        })
    }
}
