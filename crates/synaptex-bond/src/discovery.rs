/// Pure discovery helpers — no sled or registry interaction.
/// `synaptex-core::bond_sync` calls these and handles persistence.
use anyhow::Result;
use sha2::{Digest, Sha256};
use synaptex_types::{capability::Capability, device::DeviceId, DeviceInfo};

use crate::{client::BondClient, types::BondConfig};

/// Derive a stable, locally-administered DeviceId from the hub MAC address
/// and Bond's own device ID.  The result is deterministic across restarts.
pub fn device_id_for(hub_mac: &str, bond_device_id: &str) -> DeviceId {
    let mut h = Sha256::new();
    h.update(hub_mac.as_bytes());
    h.update(b":");
    h.update(bond_device_id.as_bytes());
    let hash = h.finalize();
    let mut bytes = [0u8; 6];
    bytes.copy_from_slice(&hash[..6]);
    // Set locally-administered bit (b1), clear multicast bit (b0) so the ID
    // is a valid unicast address that won't collide with real hardware MACs.
    bytes[0] = (bytes[0] | 0x02) & !0x01;
    DeviceId(bytes)
}

/// Build the capability list for a Bond device from its type and action list.
pub fn capabilities_for(device_type: &str, actions: &[String]) -> Vec<Capability> {
    let mut caps = vec![Capability::Power];
    if device_type == "CF" {
        caps.push(Capability::Fan);
        let has_light = actions.iter().any(|a| a == "TurnLightOn" || a == "TurnLightOff");
        if has_light {
            caps.push(Capability::Light);
        }
    }
    caps
}

/// Contact the Bond bridge and return `(DeviceInfo, BondConfig)` for every
/// sub-device it controls.  Pure — callers are responsible for sled + registry.
///
/// `connect_ip`  — IP to use right now (may be the pre-DHCP-renewal address).
/// `hub_mac`     — MAC of the hub (used to derive stable DeviceIds).
/// `bond_token`  — BOND-Token header value.
/// `managed_ip`  — Router-allocated IP stored in BondConfig (stable post-renewal).
pub async fn discover_hub_devices(
    connect_ip: &str,
    hub_mac:    &str,
    bond_token: &str,
    managed_ip: &str,
) -> Result<Vec<(DeviceInfo, BondConfig)>> {
    let client = BondClient::new(connect_ip, bond_token);
    let ids = client.list_device_ids().await?;

    let mut results = Vec::new();
    for bond_device_id in ids {
        let dev = match client.get_device(&bond_device_id).await {
            Ok(d)  => d,
            Err(e) => {
                tracing::warn!(bond_device_id, "bond: failed to fetch device: {e}");
                continue;
            }
        };

        let device_id = device_id_for(hub_mac, &bond_device_id);
        let caps      = capabilities_for(&dev.device_type, &dev.actions);

        let info = DeviceInfo {
            id:           device_id,
            name:         dev.name.clone(),
            model:        dev.device_type.clone(),
            protocol:     "bond_local".to_string(),
            capabilities: caps,
        };

        let cfg = BondConfig {
            device_id,
            hub_mac:        hub_mac.to_string(),
            hub_ip:         managed_ip.to_string(),
            bond_token:     bond_token.to_string(),
            bond_device_id: bond_device_id.clone(),
            device_type:    dev.device_type,
            name:           dev.name,
            actions:        dev.actions,
            max_speed:      dev.max_speed,
        };

        results.push((info, cfg));
    }
    Ok(results)
}
