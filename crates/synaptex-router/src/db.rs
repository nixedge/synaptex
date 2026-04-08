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
pub struct RouterDb {
    devices: sled::Tree,
}

impl RouterDb {
    pub fn open(db: &sled::Db) -> Result<Self> {
        Ok(Self { devices: db.open_tree("devices")? })
    }

    /// Upsert a device record.  Returns `true` if the record was new or any
    /// field changed (IP or version update), `false` if nothing changed.
    pub fn upsert(&self, record: &DeviceRecord) -> Result<bool> {
        let new_bytes = postcard::to_allocvec(record)?;
        let changed = match self.devices.get(record.tuya_id.as_bytes())? {
            Some(existing) => existing.as_ref() != new_bytes.as_slice(),
            None           => true,
        };
        if changed {
            self.devices.insert(record.tuya_id.as_bytes(), new_bytes)?;
        }
        Ok(changed)
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
