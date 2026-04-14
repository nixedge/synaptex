use std::net::Ipv4Addr;

use anyhow::Result;
use serde::{Deserialize, Serialize};


// ─── Device kind ──────────────────────────────────────────────────────────────

/// Protocol-specific device identity and metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DeviceKind {
    Tuya   { tuya_id: String, version: String },
    Matter { node_id: u64 },
    Bond   { bond_id: String, bond_token: String },
    Alexa,
    Sense,
    Mysa,
    Roku,
    Wled,
    Dvr,
    Other(String),
}

impl DeviceKind {
    /// Returns the protocol-native ID used as the `native_to_id` index key.
    pub fn native_id(&self) -> Option<String> {
        match self {
            DeviceKind::Tuya   { tuya_id, .. } => Some(tuya_id.clone()),
            DeviceKind::Bond   { bond_id, .. }  => Some(bond_id.clone()),
            DeviceKind::Matter { node_id }     => Some(node_id.to_string()),
            DeviceKind::Alexa | DeviceKind::Sense | DeviceKind::Mysa | DeviceKind::Roku | DeviceKind::Wled | DeviceKind::Dvr | DeviceKind::Other(_) => None,
        }
    }
}

// ─── Network policy ───────────────────────────────────────────────────────────

/// Governs what nftables rules are generated for a device.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum NetPolicy {
    /// Pairing mode — temporary internet access to reach cloud APIs.
    Provisioning,
    /// Fully provisioned — only the controller may initiate connections.
    Provisioned,
    /// Firmware update in progress — short-term internet access.
    FirmwareUpdate,
    /// Inherently cloud-dependent (e.g. Alexa, Google Home).
    CloudDependent,
}

impl Default for NetPolicy {
    fn default() -> Self {
        NetPolicy::Provisioning
    }
}

// ─── Router device ────────────────────────────────────────────────────────────

/// A network-layer device record persisted by synaptex-router.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouterDevice {
    /// UUID v4 — stable primary key, protocol-agnostic.
    pub device_id:  String,
    /// Current IP observed from UDP broadcast or DHCP.  May differ from
    /// `managed_ip` until the device renews its DHCP lease.
    pub ip:         String,
    /// MAC address (uppercase colon-separated).
    pub mac:        String,
    /// Synaptex-allocated IP in the managed range (.20–.223).
    /// `None` if not yet allocated (allocation failure or pre-migration device).
    /// This is the IP pushed to Kea as a reservation.
    pub managed_ip: Option<String>,
    /// Protocol-specific identity and metadata.
    pub kind:       DeviceKind,
    /// Governs nftables access policy for this device.
    pub net_policy: NetPolicy,
}

// ─── Database ─────────────────────────────────────────────────────────────────

/// Persistent store for router-managed devices.
///
/// Four sled trees:
/// - `devices`       — device_id (UUID) → postcard RouterDevice  (primary)
/// - `mac_to_id`     — lowercase MAC → device_id                 (O(1) Kea hook lookup)
/// - `native_to_id`  — protocol-native ID → device_id            (discovery dedup)
/// - `allocated_ips` — host octet (u8) → device_id               (IP allocator state)
pub struct RouterDb {
    devices:           sled::Tree,
    mac_to_id:         sled::Tree,
    native_to_id:      sled::Tree,
    allocated_ips:     sled::Tree,
    managed_subnet:    [u8; 3],
    managed_host_start: u8,
    managed_host_end:   u8,
}

impl RouterDb {
    pub fn open(
        db:                &sled::Db,
        managed_subnet:    [u8; 3],
        managed_host_start: u8,
        managed_host_end:   u8,
    ) -> Result<Self> {
        anyhow::ensure!(
            managed_host_start <= managed_host_end,
            "managed-host-start ({managed_host_start}) must be ≤ managed-host-end ({managed_host_end})",
        );
        Ok(Self {
            devices:            db.open_tree("devices")?,
            mac_to_id:          db.open_tree("mac_to_id")?,
            native_to_id:       db.open_tree("native_to_id")?,
            allocated_ips:      db.open_tree("allocated_ips")?,
            managed_subnet,
            managed_host_start,
            managed_host_end,
        })
    }

    /// Upsert a device.  Returns `true` if the record was new or any field changed.
    pub fn upsert(&self, device: &RouterDevice) -> Result<bool> {
        let new_bytes = postcard::to_allocvec(device)?;
        let changed = match self.devices.get(device.device_id.as_bytes())? {
            Some(existing) => existing.as_ref() != new_bytes.as_slice(),
            None           => true,
        };
        if changed {
            self.devices.insert(device.device_id.as_bytes(), new_bytes)?;
            if !device.mac.is_empty() {
                let mac_key = device.mac.to_ascii_lowercase();
                self.mac_to_id.insert(mac_key.as_bytes(), device.device_id.as_bytes())?;
            }
            if let Some(native_id) = device.kind.native_id() {
                self.native_to_id.insert(native_id.as_bytes(), device.device_id.as_bytes())?;
            }
        }
        Ok(changed)
    }

    /// Look up a device by MAC address (case-insensitive).
    pub fn get_by_mac(&self, mac: &str) -> Result<Option<RouterDevice>> {
        let mac_key = mac.to_ascii_lowercase();
        let Some(id_ivec) = self.mac_to_id.get(mac_key.as_bytes())? else {
            return Ok(None);
        };
        let device_id = std::str::from_utf8(&id_ivec).unwrap_or_default();
        match self.devices.get(device_id.as_bytes())? {
            Some(bytes) => Ok(postcard::from_bytes(&bytes).ok()),
            None        => Ok(None),
        }
    }

    /// Look up a device by protocol-native ID (Tuya gwId, Bond device ID, etc.).
    pub fn get_by_native_id(&self, native_id: &str) -> Result<Option<RouterDevice>> {
        let Some(id_ivec) = self.native_to_id.get(native_id.as_bytes())? else {
            return Ok(None);
        };
        let device_id = std::str::from_utf8(&id_ivec).unwrap_or_default();
        match self.devices.get(device_id.as_bytes())? {
            Some(bytes) => Ok(postcard::from_bytes(&bytes).ok()),
            None        => Ok(None),
        }
    }

    pub fn list_all(&self) -> Result<Vec<RouterDevice>> {
        let mut out = Vec::new();
        for item in self.devices.iter() {
            let (_, v) = item?;
            if let Ok(dev) = postcard::from_bytes(&v) {
                out.push(dev);
            }
        }
        Ok(out)
    }

    // ── IP allocator ──────────────────────────────────────────────────────────

    /// Allocate the next free managed IP for `device_id`.
    ///
    /// Scans host octets `managed_host_start`–`managed_host_end` sequentially
    /// and claims the first that is not already in `allocated_ips`.
    ///
    /// Returns an error if the range is exhausted.  This should be treated as
    /// a hard operational limit (~204 devices on a /24).
    pub fn allocate_ip(&self, device_id: &str) -> Result<Ipv4Addr> {
        for host in self.managed_host_start..=self.managed_host_end {
            let key = [host];
            if self.allocated_ips.get(key)?.is_none() {
                self.allocated_ips.insert(key, device_id.as_bytes())?;
                let [a, b, c] = self.managed_subnet;
                return Ok(Ipv4Addr::new(a, b, c, host));
            }
        }
        let [a, b, c] = self.managed_subnet;
        let s = self.managed_host_start;
        let e = self.managed_host_end;
        anyhow::bail!("managed IP range {a}.{b}.{c}.{s}–{a}.{b}.{c}.{e} exhausted")
    }

    /// Pin a device to a specific managed IP.
    ///
    /// The IP must be within the managed range.  Returns an error if the host
    /// octet is already claimed by a *different* device; re-claiming the same
    /// device_id is a no-op and succeeds.
    pub fn reserve_ip(&self, ip: &str, device_id: &str) -> Result<Ipv4Addr> {
        let addr: Ipv4Addr = ip.parse()
            .map_err(|_| anyhow::anyhow!("invalid IP address: {ip}"))?;
        let [a, b, c, host] = addr.octets();
        anyhow::ensure!(
            [a, b, c] == self.managed_subnet,
            "IP {ip} is not in the managed subnet",
        );
        anyhow::ensure!(
            (self.managed_host_start..=self.managed_host_end).contains(&host),
            "IP {ip} is outside the managed host range ({}.{}.{}.{}–{}.{}.{}.{})",
            a, b, c, self.managed_host_start,
            a, b, c, self.managed_host_end,
        );
        let key = [host];
        match self.allocated_ips.get(key)? {
            Some(existing) if existing.as_ref() != device_id.as_bytes() => {
                let owner = std::str::from_utf8(&existing).unwrap_or("?");
                anyhow::bail!("IP {ip} is already allocated to device {owner}");
            }
            _ => {
                self.allocated_ips.insert(key, device_id.as_bytes())?;
                Ok(addr)
            }
        }
    }

    /// Release a previously allocated managed IP back to the free pool.
    ///
    /// No-op if the IP is outside the managed range or was never allocated.
    pub fn release_ip(&self, ip: &str) -> Result<()> {
        if let Ok(addr) = ip.parse::<Ipv4Addr>() {
            let host = addr.octets()[3];
            if (self.managed_host_start..=self.managed_host_end).contains(&host) {
                self.allocated_ips.remove([host])?;
            }
        }
        Ok(())
    }
}
