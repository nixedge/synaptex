/// Kea reservation client via the synaptex hook command socket.
///
/// Instead of connecting to Kea's own control socket (which Kea enforces
/// strict permissions on), synaptex-router connects to a socket created by
/// the synaptex_hook.so shared library running inside the Kea process.
/// The hook handles reservation-add / reservation-del commands using Kea's
/// in-process HostMgr API.
///
/// Configure in kea-dhcp4.conf:
/// ```json
/// "hooks-libraries": [{
///   "library": "/path/to/synaptex_hook.so",
///   "parameters": {
///     "socket":     "/run/synaptex-router/kea-hook.sock",
///     "cmd_socket": "/run/kea/synaptex-cmd.sock"
///   }
/// }]
/// ```

use std::path::PathBuf;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
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
    /// Upsert semantics: duplicate detection is handled in the hook via
    /// DuplicateHost exception (delete + re-add).
    pub async fn reservation_add(&self, hw_address: &str, ip: &str) -> Result<()> {
        let mac = hw_address.to_ascii_lowercase();
        let cmd = serde_json::json!({
            "cmd":       "reservation-add",
            "mac":       mac,
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
            "mac":       mac,
            "subnet_id": self.subnet_id,
        })
        .to_string();
        if let Err(e) = self.send(&cmd).await {
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

    /// Send one JSON command to the hook cmd socket and check the result.
    ///
    /// Opens a fresh connection per call.
    async fn send(&self, cmd: &str) -> Result<serde_json::Value> {
        let stream = UnixStream::connect(&self.socket_path)
            .await
            .with_context(|| format!("connect to {}", self.socket_path.display()))?;

        let (read_half, mut write_half) = stream.into_split();

        let mut line = cmd.to_string();
        line.push('\n');
        write_half.write_all(line.as_bytes()).await.context("write")?;

        let mut resp_line = String::new();
        BufReader::new(read_half)
            .read_line(&mut resp_line)
            .await
            .context("read")?;

        let resp: serde_json::Value =
            serde_json::from_str(resp_line.trim()).context("parse hook response")?;

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
