/// Kea DHCP hook integration — Unix domain socket listener.
///
/// The Kea hook shim (.so) connects here at `load()` time and sends one
/// JSON line per `pkt4_receive` callout.  We classify the device and reply
/// with a list of Kea client-class names for the shim to apply via
/// `pkt->addClass(...)` before returning `NEXT_STEP_CONTINUE`.
///
/// # IoT VLAN gating
///
/// Only requests whose `giaddr` (relay agent IP) matches one of the
/// configured `iot_relay_ips` are classified.  All other requests receive
/// an empty `classes` list, so Kea falls through to the subnet definitions
/// in your NixOS configuration unchanged.
///
/// # Wire format (newline-delimited JSON)
///
/// Request (shim → router):
/// ```json
/// {
///   "mac":          "fc:65:de:aa:bb:cc",
///   "giaddr":       "10.10.20.1",
///   "msg_type":     1,
///   "hostname":     "Amazon-Echo-A1B2C3",   // option 12, omitted if absent
///   "vendor_class": "udhcp 1.23.2",          // option 60, omitted if absent
///   "prl":          [1,3,6,12,15,28]         // option 55, omitted if absent
/// }
/// ```
///
/// Response (router → shim):
/// ```json
/// { "classes": ["IOT_DEVICE", "AMAZON"] }
/// ```
/// or `{ "classes": [] }` for non-IoT-VLAN requests.

use std::{net::Ipv4Addr, path::Path, sync::Arc};

use crate::db::RouterDb;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    net::{
        unix::{OwnedReadHalf, OwnedWriteHalf},
        UnixListener, UnixStream,
    },
    sync::Mutex,
};
use tracing::{debug, info, warn};

// ─── Cmd channel state ────────────────────────────────────────────────────────

/// One end of the persistent reservation-command channel to the Kea hook.
///
/// Stored in `CmdState`; accessed exclusively by `KeaClient` methods and
/// cleared automatically when the hook disconnects.
pub struct CmdConn {
    pub write: OwnedWriteHalf,
    pub lines: Lines<BufReader<OwnedReadHalf>>,
}

/// Shared state for the cmd channel.  `None` until the hook's cmd thread
/// has connected; cleared again on disconnection.
pub type CmdState = Arc<Mutex<Option<CmdConn>>>;

// ─── Wire types ───────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct PktRequest {
    pub mac:          String,
    /// Relay agent IP — identifies the source VLAN/subnet.
    pub giaddr:       Option<String>,
    /// DHCP message type: 1=DISCOVER, 3=REQUEST, 8=INFORM.
    pub msg_type:     u8,
    /// Option 12 — client-supplied hostname.
    pub hostname:     Option<String>,
    /// Option 60 — vendor class identifier.
    pub vendor_class: Option<String>,
    /// Option 55 — parameter request list (DHCP fingerprint, reserved for future use).
    #[serde(default)]
    #[allow(dead_code)]
    pub prl:          Vec<u8>,
}

#[derive(Debug, Serialize)]
pub struct PktResponse {
    pub classes: Vec<&'static str>,
}

// ─── Classification ───────────────────────────────────────────────────────────

/// Classify a DHCP packet.
///
/// Always checks the router DB first — if the device is known, adds
/// `SYNAPTEX_KNOWN` so Kea (and future nftables hooks) can distinguish
/// managed devices from unknown ones.  Its IP reservation is handled
/// separately by the discovery pipeline pushing to Kea on device upsert.
///
/// Additionally gates on `giaddr` for IoT VLAN classification: if the
/// request arrived via a relay in `iot_relay_ips`, the device also gets
/// `IOT_DEVICE` and any applicable vendor class.  Non-IoT traffic that
/// is also unknown returns an empty list so Kea falls through to its
/// statically-configured subnet rules unchanged.
pub fn classify(req: &PktRequest, iot_relay_ips: &[Ipv4Addr], db: &RouterDb) -> Vec<&'static str> {
    let mut classes: Vec<&'static str> = vec![];

    // Known device check — O(1) sled lookup via MAC secondary index.
    match db.get_by_mac(&req.mac) {
        Ok(Some(_)) => {
            debug!(mac = %req.mac, "kea: known device");
            classes.push("SYNAPTEX_KNOWN");
        }
        Ok(None)    => {}
        Err(e)      => warn!(mac = %req.mac, "kea: db lookup: {e}"),
    }

    // IoT VLAN classification (giaddr gating).
    let on_iot_vlan = req.giaddr.as_deref()
        .and_then(|s| s.parse::<Ipv4Addr>().ok())
        .map(|ip| iot_relay_ips.contains(&ip))
        .unwrap_or(false);

    if on_iot_vlan {
        classes.push("IOT_DEVICE");
        if let Some(vendor) = classify_vendor(&req.mac, req.hostname.as_deref(), req.vendor_class.as_deref()) {
            classes.push(vendor);
        }
    }

    classes
}

/// Identify the vendor/device-type from MAC OUI, hostname, and VCI.
fn classify_vendor(mac: &str, hostname: Option<&str>, vendor_class: Option<&str>) -> Option<&'static str> {
    // ── MAC OUI (most reliable) ───────────────────────────────────────────────
    if oui_matches(mac, &[
        "fc:65:de", "44:65:0d", "84:d6:d0", "f0:27:2d", "00:fc:8b",
        "74:c2:46", "a4:08:f5", "18:74:2e", "40:b4:cd", "68:37:e9",
        "ac:63:be", "50:dc:e7", "f0:4f:7c", "34:d2:70", "b4:7c:9c",
        "cc:9e:a2", "fc:a6:67", "28:ef:01", "88:71:e5", "e8:9f:80",
    ]) {
        return Some("AMAZON");
    }

    if oui_matches(mac, &[
        "f4:f5:d8", "48:d6:d5", "54:60:09", "6c:ad:f8", "a4:77:33",
        "d4:f5:47", "1c:f2:9a", "b0:e0:3b", "20:df:b9", "48:bf:6b",
    ]) {
        return Some("GOOGLE");
    }

    if oui_matches(mac, &[
        "a8:be:27", "f0:18:98", "3c:06:30", "8c:85:90", "dc:a4:ca",
        "b8:78:2e", "40:33:1a", "a4:b1:97", "28:6a:b8", "00:cd:fe",
    ]) {
        return Some("APPLE");
    }

    // ── Hostname prefix (secondary signal) ───────────────────────────────────
    if let Some(h) = hostname {
        let h = h.to_ascii_lowercase();
        if h.starts_with("amazon-") || h.starts_with("echo-") || h.starts_with("alexa-") {
            return Some("AMAZON");
        }
        if h.starts_with("google-") || h.starts_with("chromecast") || h.starts_with("nest-") {
            return Some("GOOGLE");
        }
    }

    // ── Vendor Class Identifier (tertiary signal) ─────────────────────────────
    if let Some(vc) = vendor_class {
        let vc = vc.to_ascii_lowercase();
        if vc.contains("amazon") || vc.contains("alexa") || vc.contains("fire") {
            return Some("AMAZON");
        }
    }

    None
}

/// Returns true if the MAC's OUI matches any of the given prefixes.
/// `mac` is colon-separated hex, case-insensitive ("fc:65:de:aa:bb:cc").
fn oui_matches(mac: &str, prefixes: &[&str]) -> bool {
    let mac = mac.to_ascii_lowercase();
    let oui = &mac[..mac.len().min(8)];
    prefixes.iter().any(|&p| oui == p)
}

// ─── Socket listener ──────────────────────────────────────────────────────────

/// Spawn the Kea hook domain socket listener.
///
/// Removes any stale socket file, binds a new `UnixListener`, and accepts
/// connections indefinitely.  Classification connections (one per Kea worker
/// thread) are handled in their own tasks.  The single cmd-channel connection
/// (opened by the hook at load time with `{"type":"cmd"}`) is stored in
/// `cmd_state` for `KeaClient` to push reservation commands through.
pub fn spawn(
    path:          impl AsRef<Path> + Send + 'static,
    iot_relay_ips: Vec<Ipv4Addr>,
    db:            Arc<RouterDb>,
    cmd_state:     CmdState,
) {
    let iot_relay_ips = Arc::new(iot_relay_ips);
    tokio::spawn(async move {
        if let Err(e) = run(path.as_ref(), &iot_relay_ips, &db, cmd_state).await {
            warn!("kea socket listener exited: {e}");
        }
    });
}

async fn run(
    path:          &Path,
    iot_relay_ips: &Arc<Vec<Ipv4Addr>>,
    db:            &Arc<RouterDb>,
    cmd_state:     CmdState,
) -> Result<()> {
    let _ = tokio::fs::remove_file(path).await;
    let listener = UnixListener::bind(path)?;
    info!(path = %path.display(), "kea: listening for hook connections");

    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let ips = iot_relay_ips.clone();
                let db  = db.clone();
                let cmd = cmd_state.clone();
                tokio::spawn(handle_connection(stream, ips, db, cmd));
            }
            Err(e) => warn!("kea: accept error: {e}"),
        }
    }
}

async fn handle_connection(
    stream:        UnixStream,
    iot_relay_ips: Arc<Vec<Ipv4Addr>>,
    db:            Arc<RouterDb>,
    cmd_state:     CmdState,
) {
    let (read_half, write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();

    // Peek at the first line to distinguish connection types.
    let first = match lines.next_line().await {
        Ok(Some(l)) => l,
        _ => return,
    };

    // cmd-channel connection: hook announces {"type":"cmd"}.
    if serde_json::from_str::<serde_json::Value>(&first)
        .ok()
        .and_then(|v| v["type"].as_str().map(str::to_string))
        .as_deref() == Some("cmd")
    {
        info!("kea: cmd channel connected");
        *cmd_state.lock().await = Some(CmdConn { write: write_half, lines });
        // Task exits here; KeaClient owns the connection from this point.
        // When the hook disconnects, KeaClient clears cmd_state on the next error.
        return;
    }

    // Classification connection: process first line then loop.
    let mut write_half = write_half;
    classify_and_reply(&first, &iot_relay_ips, &db, &mut write_half).await;
    while let Ok(Some(line)) = lines.next_line().await {
        classify_and_reply(&line, &iot_relay_ips, &db, &mut write_half).await;
    }
    debug!("kea: shim disconnected");
}

async fn classify_and_reply(
    line:          &str,
    iot_relay_ips: &[Ipv4Addr],
    db:            &RouterDb,
    write_half:    &mut OwnedWriteHalf,
) {
    let response = match serde_json::from_str::<PktRequest>(line) {
        Ok(req) => {
            let classes = classify(&req, iot_relay_ips, db);
            debug!(
                mac      = %req.mac,
                giaddr   = ?req.giaddr,
                msg_type = req.msg_type,
                ?classes,
                "kea: classified",
            );
            serde_json::to_string(&PktResponse { classes }).unwrap_or_default()
        }
        Err(e) => {
            warn!("kea: malformed request: {e}  raw={line:?}");
            serde_json::to_string(&PktResponse { classes: vec![] }).unwrap_or_default()
        }
    };

    let mut buf = response.into_bytes();
    buf.push(b'\n');
    if let Err(e) = write_half.write_all(&buf).await {
        warn!("kea: write error: {e}");
    }
}
