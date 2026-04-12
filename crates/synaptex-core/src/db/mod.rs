pub mod trees;
pub use trees::Trees;

use anyhow::Result;
use postcard::{from_bytes, to_allocvec};
use serde::{de::DeserializeOwned, Serialize};
use sled::Tree;
use synaptex_types::{device::DeviceId, DeviceInfo};
use synaptex_tuya::TuyaDeviceConfig;

// ─── Generic helpers ─────────────────────────────────────────────────────────

/// Postcard-encode `val` and insert into `tree` keyed by `id`.
pub fn put<V: Serialize>(tree: &Tree, id: &DeviceId, val: &V) -> Result<()> {
    let encoded = to_allocvec(val)?;
    tree.insert(id.0, encoded)?;
    Ok(())
}

/// Fetch and postcard-decode a value from `tree` keyed by `id`.
pub fn get<V: DeserializeOwned>(tree: &Tree, id: &DeviceId) -> Result<Option<V>> {
    match tree.get(id.0)? {
        Some(bytes) => Ok(Some(from_bytes(&bytes)?)),
        None        => Ok(None),
    }
}

// ─── Registry helpers ────────────────────────────────────────────────────────

/// Persist device metadata to the registry tree.
pub fn register_device(trees: &Trees, info: &DeviceInfo) -> Result<()> {
    put(&trees.registry, &info.id, info)
}

/// Remove device metadata from the registry tree.
pub fn remove_device(trees: &Trees, id: &DeviceId) -> Result<()> {
    trees.registry.remove(id.0)?;
    Ok(())
}

/// Retrieve all `DeviceInfo` records from the registry tree.
pub fn list_all_devices(trees: &Trees) -> Result<Vec<DeviceInfo>> {
    let mut devices = Vec::new();
    for item in trees.registry.iter() {
        let (_k, v) = item?;
        let info: DeviceInfo = from_bytes(&v)?;
        devices.push(info);
    }
    Ok(devices)
}

// ─── Plugin config helpers ────────────────────────────────────────────────────

/// Config for a group device (synthetic MAC, fan-out to members).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GroupConfig {
    pub device_id:  DeviceId,
    pub member_ids: Vec<DeviceId>,
}

pub use synaptex_bond::BondConfig;

/// Discriminated union of all per-protocol configs stored in the `configs` tree.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum PluginConfig {
    Tuya(TuyaDeviceConfig),
    Group(GroupConfig),
    Bond(BondConfig),
}

/// A named room containing a set of devices (physical and/or group).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Room {
    pub id:         String,        // UUID
    pub name:       String,
    pub device_ids: Vec<DeviceId>,
}

/// Persist a plugin config to the `configs` tree.
pub fn save_plugin_config(trees: &Trees, id: &DeviceId, cfg: &PluginConfig) -> Result<()> {
    put(&trees.configs, id, cfg)
}

/// Remove a plugin config from the `configs` tree.
pub fn remove_plugin_config(trees: &Trees, id: &DeviceId) -> Result<()> {
    trees.configs.remove(id.0)?;
    Ok(())
}

/// Load a single plugin config by device ID.
pub fn load_plugin_config(trees: &Trees, id: &DeviceId) -> Result<Option<PluginConfig>> {
    get(&trees.configs, id)
}

// ─── Legacy migration types ───────────────────────────────────────────────────

/// `TuyaDeviceConfig` as it existed before the `protocol_version` field was added.
/// Used as a fallback deserializer for DB entries written by older builds.
#[derive(serde::Deserialize)]
struct TuyaDeviceConfigV0 {
    device_id:      DeviceId,
    ip:             std::net::IpAddr,
    port:           u16,
    tuya_id:        String,
    local_key:      String,
    dp_profile:     String,
    dp_map:         Option<synaptex_tuya::dp_map::DpMap>,
}

#[derive(serde::Deserialize)]
enum PluginConfigV0 {
    Tuya(TuyaDeviceConfigV0),
    Group(GroupConfig),
}

impl From<PluginConfigV0> for PluginConfig {
    fn from(v0: PluginConfigV0) -> Self {
        match v0 {
            PluginConfigV0::Tuya(t) => PluginConfig::Tuya(TuyaDeviceConfig {
                device_id:     t.device_id,
                ip:            t.ip,
                port:          t.port,
                tuya_id:       t.tuya_id,
                local_key:     t.local_key,
                dp_profile:    t.dp_profile,
                dp_map:        t.dp_map,
                protocol_version: None,
            }),
            PluginConfigV0::Group(g) => PluginConfig::Group(g),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────

/// Load all `PluginConfig` entries from the `configs` tree.
/// Entries written by older builds (missing `protocol_version`) are migrated
/// in-place on first load.
pub fn load_all_plugin_configs(trees: &Trees) -> Result<Vec<PluginConfig>> {
    let mut configs = Vec::new();
    for item in trees.configs.iter() {
        let (k, v) = item?;
        let cfg = match from_bytes::<PluginConfig>(&v) {
            Ok(cfg) => cfg,
            Err(_) => match from_bytes::<PluginConfigV0>(&v) {
                Ok(v0) => {
                    let migrated: PluginConfig = v0.into();
                    // Re-save in the current format so we don't migrate again.
                    let new_bytes = to_allocvec(&migrated)?;
                    trees.configs.insert(k, new_bytes)?;
                    tracing::info!("migrated config entry to current schema");
                    migrated
                }
                Err(e) => {
                    tracing::warn!("skipping corrupt config entry: {e}");
                    continue;
                }
            },
        };
        configs.push(cfg);
    }
    Ok(configs)
}

// ─── String-keyed generic helpers ─────────────────────────────────────────────

fn put_str<V: Serialize>(tree: &Tree, key: &str, val: &V) -> Result<()> {
    let encoded = to_allocvec(val)?;
    tree.insert(key.as_bytes(), encoded)?;
    Ok(())
}

fn get_str<V: DeserializeOwned>(tree: &Tree, key: &str) -> Result<Option<V>> {
    match tree.get(key.as_bytes())? {
        Some(bytes) => Ok(Some(from_bytes(&bytes)?)),
        None        => Ok(None),
    }
}

// ─── Room helpers ─────────────────────────────────────────────────────────────

pub fn save_room(trees: &Trees, room: &Room) -> Result<()> {
    put_str(&trees.rooms, &room.id, room)
}

pub fn get_room(trees: &Trees, room_id: &str) -> Result<Option<Room>> {
    get_str(&trees.rooms, room_id)
}

pub fn list_rooms(trees: &Trees) -> Result<Vec<Room>> {
    let mut rooms = Vec::new();
    for item in trees.rooms.iter() {
        let (_k, v) = item?;
        match from_bytes::<Room>(&v) {
            Ok(r)  => rooms.push(r),
            Err(e) => tracing::warn!("skipping corrupt room entry: {e}"),
        }
    }
    Ok(rooms)
}

pub fn remove_room(trees: &Trees, room_id: &str) -> Result<()> {
    trees.rooms.remove(room_id.as_bytes())?;
    Ok(())
}

// ─── Routine types ────────────────────────────────────────────────────────────

/// Target of a routine command step: either a room (by UUID) or a single device.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum RoutineTarget {
    Room(String),       // room UUID
    Device(synaptex_types::device::DeviceId),
}

/// A single step in a routine.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum RoutineStep {
    Command {
        target:  RoutineTarget,
        command: synaptex_types::capability::DeviceCommand,
    },
    Wait { secs: u64 },
}

/// A named automation routine.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Routine {
    pub id:       String,          // UUID v4
    pub name:     String,
    pub schedule: Option<String>,  // 6-field cron expression; None = manual only
    pub steps:    Vec<RoutineStep>,
}

// ─── Routine helpers ──────────────────────────────────────────────────────────

pub fn save_routine(trees: &Trees, routine: &Routine) -> Result<()> {
    put_str(&trees.routines, &routine.id, routine)
}

pub fn get_routine(trees: &Trees, routine_id: &str) -> Result<Option<Routine>> {
    get_str(&trees.routines, routine_id)
}

pub fn list_routines(trees: &Trees) -> Result<Vec<Routine>> {
    let mut routines = Vec::new();
    for item in trees.routines.iter() {
        let (_k, v) = item?;
        match from_bytes::<Routine>(&v) {
            Ok(r)  => routines.push(r),
            Err(e) => tracing::warn!("skipping corrupt routine entry: {e}"),
        }
    }
    Ok(routines)
}

pub fn remove_routine(trees: &Trees, routine_id: &str) -> Result<()> {
    trees.routines.remove(routine_id.as_bytes())?;
    Ok(())
}

// ─── Global config helpers ─────────────────────────────────────────────────

const KEY_TUYA_CLOUD: &str = "tuya_cloud";
const KEY_API_KEY:    &str = "api_key";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum TuyaRegion { Us, Eu, Cn, In }

impl TuyaRegion {
    pub fn base_url(&self) -> &'static str {
        match self {
            TuyaRegion::Us => "https://openapi.tuyaus.com",
            TuyaRegion::Eu => "https://openapi.tuyaeu.com",
            TuyaRegion::Cn => "https://openapi.tuyacn.com",
            TuyaRegion::In => "https://openapi.tuyain.com",
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TuyaCloudConfig {
    pub client_id:     String,
    pub client_secret: String,
    pub region:        TuyaRegion,
    /// Account owner UID — resolved from any owned device at config-save time.
    pub uid:           String,
}

pub fn save_tuya_cloud_config(trees: &Trees, cfg: &TuyaCloudConfig) -> Result<()> {
    put_str(&trees.config, KEY_TUYA_CLOUD, cfg)
}

pub fn get_tuya_cloud_config(trees: &Trees) -> Result<Option<TuyaCloudConfig>> {
    get_str(&trees.config, KEY_TUYA_CLOUD)
}

pub fn save_api_key(trees: &Trees, key: &str) -> Result<()> {
    put_str(&trees.config, KEY_API_KEY, &key.to_string())
}

pub fn get_api_key(trees: &Trees) -> Result<Option<String>> {
    get_str(&trees.config, KEY_API_KEY)
}

pub fn remove_api_key(trees: &Trees) -> Result<()> {
    trees.config.remove(KEY_API_KEY)?;
    Ok(())
}
