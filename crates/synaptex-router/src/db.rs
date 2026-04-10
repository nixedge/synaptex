use anyhow::Result;
use serde::{Deserialize, Serialize};

/// A Tuya device record persisted by the router.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceRecord {
    pub tuya_id: String,
    pub mac:     String,
    pub ip:      String,
    pub version: String,
}

/// Persistent store for discovered Tuya devices.
///
/// Two sled trees:
/// - `devices`   — tuya_id → postcard-serialised DeviceRecord (primary)
/// - `mac_to_id` — lowercase MAC → tuya_id bytes (secondary index for O(1)
///                 MAC lookups from the Kea hook)
pub struct RouterDb {
    devices:   sled::Tree,
    mac_to_id: sled::Tree,
}

impl RouterDb {
    pub fn open(db: &sled::Db) -> Result<Self> {
        Ok(Self {
            devices:   db.open_tree("devices")?,
            mac_to_id: db.open_tree("mac_to_id")?,
        })
    }

    /// Upsert a device record.  Returns `true` if the record was new or any
    /// field changed, `false` if nothing changed.
    ///
    /// Both trees are updated atomically when the record changes so that
    /// `get_by_mac` is always consistent with `devices`.
    pub fn upsert(&self, record: &DeviceRecord) -> Result<bool> {
        let new_bytes = postcard::to_allocvec(record)?;
        let changed = match self.devices.get(record.tuya_id.as_bytes())? {
            Some(existing) => existing.as_ref() != new_bytes.as_slice(),
            None           => true,
        };
        if changed {
            self.devices.insert(record.tuya_id.as_bytes(), new_bytes)?;
            if !record.mac.is_empty() {
                let mac_key = record.mac.to_ascii_lowercase();
                self.mac_to_id.insert(mac_key.as_bytes(), record.tuya_id.as_bytes())?;
            }
        }
        Ok(changed)
    }

    /// Look up a device record by MAC address (case-insensitive).
    ///
    /// Used by the Kea hook handler to classify DHCP packets from known
    /// devices without scanning all records.
    pub fn get_by_mac(&self, mac: &str) -> Result<Option<DeviceRecord>> {
        let mac_key = mac.to_ascii_lowercase();
        let Some(id_ivec) = self.mac_to_id.get(mac_key.as_bytes())? else {
            return Ok(None);
        };
        let tuya_id = std::str::from_utf8(&id_ivec).unwrap_or_default();
        match self.devices.get(tuya_id.as_bytes())? {
            Some(bytes) => Ok(postcard::from_bytes(&bytes).ok()),
            None        => Ok(None),
        }
    }

    pub fn list_all(&self) -> Result<Vec<DeviceRecord>> {
        let mut out = Vec::new();
        for item in self.devices.iter() {
            let (_, v) = item?;
            if let Ok(rec) = postcard::from_bytes(&v) {
                out.push(rec);
            }
        }
        Ok(out)
    }
}
