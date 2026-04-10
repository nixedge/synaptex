/// Kea DHCP4 control socket client.
///
/// Pushes host reservations into Kea's in-memory host cache via the
/// `host_cmds` hook library.  Requires in kea-dhcp4.conf:
///
/// ```json
/// "control-socket": { "socket-type": "unix", "socket-name": "/run/kea/kea4.sock" },
/// "hooks-libraries": [{ "library": "/path/to/libdhcp_host_cmds.so" }]
/// ```
///
/// Reservations survive Kea SIGHUP (config reload) but are lost on a full
/// daemon restart.  `sync_from_db` re-pushes all known devices at router
/// startup to cover that case.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tracing::{debug, info, warn};

use crate::db::RouterDb;

// ─── Client ───────────────────────────────────────────────────────────────────

pub struct KeaClient {
    socket_path: PathBuf,
    subnet_id:   u32,
}

impl KeaClient {
    pub fn new(socket_path: PathBuf, subnet_id: u32) -> Self {
        Self { socket_path, subnet_id }
    }

    /// Add or refresh a host reservation (MAC → IP).
    ///
    /// If a reservation already exists for this MAC (e.g. the device is
    /// renewing with a different IP), deletes it first then re-adds so the
    /// entry stays current (upsert semantics).
    pub async fn reservation_add(&self, hw_address: &str, ip: &str) -> Result<()> {
        let mac = hw_address.to_ascii_lowercase();
        match self.send(&self.add_cmd(&mac, ip)).await {
            Ok(()) => {
                debug!(%mac, %ip, "dhcp: reservation added");
                Ok(())
            }
            Err(e) if is_duplicate(&e) => {
                warn!(%mac, %ip, "dhcp: duplicate reservation — refreshing");
                self.del_inner(&mac).await.ok();
                self.send(&self.add_cmd(&mac, ip)).await
            }
            Err(e) => Err(e),
        }
    }

    /// Remove a reservation by MAC address.  Non-fatal if the reservation
    /// does not exist.
    pub async fn reservation_del(&self, hw_address: &str) -> Result<()> {
        let mac = hw_address.to_ascii_lowercase();
        if let Err(e) = self.del_inner(&mac).await {
            warn!(%mac, "dhcp: reservation-del: {e}");
        }
        Ok(())
    }

    /// Re-push reservations for every device in the router DB.
    ///
    /// Called at startup because Kea's in-memory host cache does not survive
    /// a daemon restart.  Errors are logged but do not abort the sync.
    pub async fn sync_from_db(&self, db: &RouterDb) -> Result<()> {
        let devices = db.list_all()?;
        let total   = devices.len();
        let mut pushed = 0usize;
        for d in &devices {
            if d.mac.is_empty() || d.ip.is_empty() {
                continue;
            }
            match self.reservation_add(&d.mac, &d.ip).await {
                Ok(())  => pushed += 1,
                Err(e)  => warn!(mac = %d.mac, ip = %d.ip, "dhcp: sync: {e}"),
            }
        }
        info!(total, pushed, "dhcp: startup reservation sync complete");
        Ok(())
    }

    // ── Internals ─────────────────────────────────────────────────────────────

    fn add_cmd(&self, mac: &str, ip: &str) -> String {
        json!({
            "command": "reservation-add",
            "service": ["dhcp4"],
            "arguments": {
                "reservation": {
                    "hw-address": mac,
                    "ip-address": ip,
                    "subnet-id":  self.subnet_id
                }
            }
        })
        .to_string()
    }

    fn del_cmd(&self, mac: &str) -> String {
        json!({
            "command": "reservation-del",
            "service": ["dhcp4"],
            "arguments": {
                "identifier-type": "hw-address",
                "identifier":      mac,
                "subnet-id":       self.subnet_id
            }
        })
        .to_string()
    }

    async fn del_inner(&self, mac: &str) -> Result<()> {
        self.send(&self.del_cmd(mac)).await
    }

    /// Send one JSON command to the Kea control socket and check the result.
    ///
    /// Opens a fresh connection per call — Kea closes the socket after each
    /// command anyway.
    async fn send(&self, cmd: &str) -> Result<()> {
        let mut stream = UnixStream::connect(&self.socket_path)
            .await
            .with_context(|| format!("connect to {}", self.socket_path.display()))?;

        stream.write_all(cmd.as_bytes()).await.context("write")?;
        stream.shutdown().await.context("shutdown write")?;

        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.context("read")?;

        // Response: [{"result": N, "text": "..."}]
        let resp: serde_json::Value =
            serde_json::from_slice(&buf).context("parse Kea response")?;

        let result = resp[0]["result"].as_i64().unwrap_or(-1);
        let text   = resp[0]["text"].as_str().unwrap_or("(no text)");

        debug!(result, text, "dhcp: kea response");

        if result == 0 {
            Ok(())
        } else {
            anyhow::bail!("result={result}: {text}")
        }
    }
}

fn is_duplicate(e: &anyhow::Error) -> bool {
    let s = e.to_string().to_ascii_lowercase();
    s.contains("already") || s.contains("duplicate") || s.contains("exist")
}
