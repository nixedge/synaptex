//! Cloud device enumeration — pure; callers handle persistence and registration.

use anyhow::Result;
use sha2::{Digest, Sha256};
use synaptex_types::{capability::Capability, device::DeviceId, DeviceInfo};

use crate::{client::MysaHttpClient, types::MysaConfig};

/// Derive a stable, locally-administered DeviceId from a Mysa device ID (MAC).
pub fn device_id_for(mysa_id: &str) -> DeviceId {
    let mut h = Sha256::new();
    h.update(mysa_id.as_bytes());
    let hash = h.finalize();
    let mut bytes = [0u8; 6];
    bytes.copy_from_slice(&hash[..6]);
    // Set locally-administered bit (b1), clear multicast bit (b0).
    bytes[0] = (bytes[0] | 0x02) & !0x01;
    DeviceId(bytes)
}

/// Contact the Mysa REST API and return `(DeviceInfo, MysaConfig)` for every
/// device on the account.  Pure — callers are responsible for sled + registry.
pub async fn discover_devices(
    id_token: &str,
) -> Result<Vec<(DeviceInfo, MysaConfig)>> {
    let http     = MysaHttpClient::new();
    let raw_devs = http.list_devices(id_token).await?;

    let mut results = Vec::new();
    for dev in raw_devs {
        let device_id  = device_id_for(&dev.id);
        // Setpoint range is not returned by the device-list endpoint; use
        // hardware defaults (5–30 °C, expressed in tenths).
        let min_sp: u16 = 50;
        let max_sp: u16 = 300;

        let info = DeviceInfo {
            id:           device_id,
            name:         dev.name.clone(),
            model:        dev.model.clone(),
            protocol:     "mysa_cloud".to_string(),
            capabilities: vec![
                Capability::Power,
                Capability::Thermostat { min: min_sp, max: max_sp },
            ],
        };

        let cfg = MysaConfig {
            device_id,
            mysa_id:      dev.id,
            name:         dev.name,
            model:        dev.model,
            min_setpoint: min_sp,
            max_setpoint: max_sp,
        };

        results.push((info, cfg));
    }
    Ok(results)
}
