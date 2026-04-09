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

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};
use tracing::{debug, info, warn};

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
/// Returns an empty list if `giaddr` is absent or not in `iot_relay_ips`,
/// so Kea uses its NixOS-defined rules unmodified for non-IoT traffic.
pub fn classify(req: &PktRequest, iot_relay_ips: &[Ipv4Addr]) -> Vec<&'static str> {
    // Gate on giaddr — only act on IoT VLAN traffic.
    let on_iot_vlan = req.giaddr.as_deref()
        .and_then(|s| s.parse::<Ipv4Addr>().ok())
        .map(|ip| iot_relay_ips.contains(&ip))
        .unwrap_or(false);

    if !on_iot_vlan {
        return vec![];
    }

    let mut classes = vec!["IOT_DEVICE"];
    if let Some(vendor) = classify_vendor(&req.mac, req.hostname.as_deref(), req.vendor_class.as_deref()) {
        classes.push(vendor);
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
/// connections indefinitely.  Each connection (one per Kea worker) is
/// handled in its own task and is persistent for the shim's lifetime.
pub fn spawn(path: impl AsRef<Path> + Send + 'static, iot_relay_ips: Vec<Ipv4Addr>) {
    let iot_relay_ips = Arc::new(iot_relay_ips);
    tokio::spawn(async move {
        if let Err(e) = run(path.as_ref(), &iot_relay_ips).await {
            warn!("kea socket listener exited: {e}");
        }
    });
}

async fn run(path: &Path, iot_relay_ips: &Arc<Vec<Ipv4Addr>>) -> Result<()> {
    let _ = tokio::fs::remove_file(path).await;
    let listener = UnixListener::bind(path)?;
    info!(path = %path.display(), "kea: listening for hook connections");

    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let ips = iot_relay_ips.clone();
                tokio::spawn(handle_connection(stream, ips));
            }
            Err(e) => warn!("kea: accept error: {e}"),
        }
    }
}

async fn handle_connection(stream: UnixStream, iot_relay_ips: Arc<Vec<Ipv4Addr>>) {
    debug!("kea: shim connected");
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();

    while let Ok(Some(line)) = lines.next_line().await {
        let response = match serde_json::from_str::<PktRequest>(&line) {
            Ok(req) => {
                let classes = classify(&req, &iot_relay_ips);
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
            break;
        }
    }

    debug!("kea: shim disconnected");
}
