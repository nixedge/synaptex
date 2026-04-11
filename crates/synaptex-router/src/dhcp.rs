/// Kea reservation client via the hook cmd channel.
///
/// The Kea hook's cmd thread connects to synaptex-router's classification
/// socket with {"type":"cmd"} as the opening message.  synaptex-router stores
/// that connection in `CmdState`; this client sends reservation-add/del
/// commands over it and reads the hook's responses.
///
/// No new sockets, no /run/kea permission changes — the existing
/// /run/synaptex-router/kea-hook.sock is reused.

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

use crate::db::RouterDb;
use crate::kea::{CmdConn, CmdState};

use tokio::io::AsyncBufReadExt as _;
use tokio::io::AsyncWriteExt as _;

// ─── Client ───────────────────────────────────────────────────────────────────

pub struct KeaClient {
    cmd:       CmdState,
    subnet_id: u32,
}

impl KeaClient {
    pub fn new(cmd: CmdState, subnet_id: u32) -> Self {
        Self { cmd, subnet_id }
    }

    /// Add or refresh a host reservation (MAC → IP).  No-op if cmd channel not yet connected.
    pub async fn reservation_add(&self, hw_address: &str, ip: &str) -> Result<()> {
        let mac = hw_address.to_ascii_lowercase();
        let cmd = serde_json::json!({
            "cmd":       "reservation-add",
            "mac":       &mac,
            "ip":        ip,
            "subnet_id": self.subnet_id,
        })
        .to_string();

        self.send(&cmd).await.map(|_| {
            debug!(%mac, %ip, "dhcp: reservation added");
        })
    }

    /// Remove a reservation by MAC address.  Non-fatal if not found.
    pub async fn reservation_del(&self, hw_address: &str) -> Result<()> {
        let mac = hw_address.to_ascii_lowercase();
        let cmd = serde_json::json!({
            "cmd":       "reservation-del",
            "mac":       &mac,
            "subnet_id": self.subnet_id,
        })
        .to_string();

        if let Err(e) = self.send(&cmd).await {
            warn!(%mac, "dhcp: reservation-del: {e}");
        }
        Ok(())
    }

    /// Re-push reservations for every device in the router DB at startup.
    pub async fn sync_from_db(&self, db: &RouterDb) -> Result<()> {
        let devices = db.list_all()?;
        let total   = devices.len();
        let mut pushed = 0usize;
        for d in &devices {
            let kea_ip = match d.managed_ip.as_deref() {
                Some(ip) => ip.to_string(),
                None     => continue,
            };
            if d.mac.is_empty() { continue; }
            match self.reservation_add(&d.mac, &kea_ip).await {
                Ok(())  => pushed += 1,
                Err(e)  => warn!(mac = %d.mac, ip = kea_ip, "dhcp: sync failed: {e:#}"),
            }
        }
        info!(total, pushed, "dhcp: startup reservation sync complete");
        Ok(())
    }

    // ── Internals ─────────────────────────────────────────────────────────────

    async fn send(&self, cmd: &str) -> Result<serde_json::Value> {
        let mut guard = self.cmd.lock().await;
        let conn: &mut CmdConn = guard
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("kea hook cmd channel not connected"))?;

        let mut line = cmd.to_string();
        line.push('\n');

        if let Err(e) = conn.write.write_all(line.as_bytes()).await {
            *guard = None;  // clear on write error so next connect restores it
            return Err(e).context("dhcp: write to hook cmd channel");
        }

        let mut resp_line = String::new();
        let result = conn.lines.get_mut().read_line(&mut resp_line).await;
        match result {
            Ok(0) | Err(_) => {
                *guard = None;
                anyhow::bail!("dhcp: hook cmd channel closed");
            }
            _ => {}
        }

        let resp: serde_json::Value =
            serde_json::from_str(resp_line.trim()).context("dhcp: parse hook response")?;

        debug!(result = ?resp["result"], text = ?resp["text"], "dhcp: hook response");

        let result = resp["result"].as_i64().unwrap_or(-1);
        let text   = resp["text"].as_str().unwrap_or("(no text)");

        if result == 0 {
            Ok(resp)
        } else {
            anyhow::bail!("result={result}: {text}")
        }
    }
}
