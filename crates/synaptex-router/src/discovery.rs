/// Tuya UDP device discovery for synaptex-router.
///
/// Tuya devices broadcast device-info JSON on UDP ports 6666 (v3.3/v3.4/v3.5)
/// and 6667 (encrypted v3.3+).  This module binds both ports, decodes every
/// known wire format, and pushes each identified device onto a tokio broadcast
/// channel so that connected gRPC clients (synaptex-core) receive a live stream.
///
/// # Wire formats handled
/// - **v3.5** — `0x6699` frame prefix; payload is AES-128-GCM,
///   key = MD5("yGAdlopoPVldABfn")
/// - **v3.3** — `0x000055AA` frame; inner payload AES-128-ECB with the same key
/// - **Plaintext** — some firmware variants broadcast unencrypted JSON directly
///
/// # Active scan
/// On startup and every 30 seconds a small probe packet (cmd 0x12) is broadcast
/// to `255.255.255.255:6666` to wake devices that don't broadcast continuously.
///
/// # MAC resolution
/// Device broadcasts include an IP but no MAC.  The MAC is resolved from
/// `/proc/net/arp`.  For IPs not yet in the ARP table a tiny UDP datagram is
/// sent to trigger kernel ARP resolution; the table is re-read after 200 ms.
/// Devices whose MAC still cannot be resolved are emitted with an empty `mac`
/// field so the consumer can decide how to handle them.
///
/// # Deduplication
/// Each unique `tuya_id` is forwarded at most once every 30 seconds to avoid
/// flooding the channel from devices that announce continuously.
use std::{
    collections::HashMap,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::Arc,
    time::Duration,
};

use serde::Deserialize;
use socket2::{Domain, Protocol, Socket, Type};
use tokio::{net::UdpSocket, sync::broadcast};

use synaptex_router_proto::DiscoveredDevice;

use crate::db::{DeviceRecord, RouterDb};
use crate::dhcp::KeaClient;

// ─── Crypto ───────────────────────────────────────────────────────────────────

/// AES-128 key shared by all Tuya devices for UDP broadcast payloads.
/// Derived as MD5("yGAdlopoPVldABfn").
fn udp_key() -> [u8; 16] {
    use md5::{Digest, Md5};
    Md5::digest(b"yGAdlopoPVldABfn").into()
}

fn aes_ecb_decrypt(key: &[u8; 16], data: &[u8]) -> Option<Vec<u8>> {
    use aes::{
        cipher::{generic_array::GenericArray, BlockDecrypt, KeyInit},
        Aes128,
    };
    if data.is_empty() || data.len() % 16 != 0 {
        return None;
    }
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let mut out = data.to_vec();
    for chunk in out.chunks_exact_mut(16) {
        cipher.decrypt_block(GenericArray::from_mut_slice(chunk));
    }
    // Try PKCS7 first; fall back to stripping null/control bytes (some firmware).
    let last = *out.last()?;
    if last > 0 && last <= 16 && out.ends_with(&vec![last; last as usize]) {
        out.truncate(out.len() - last as usize);
    } else {
        out.retain(|&b| b >= 0x20);
    }
    Some(out)
}

/// Decrypt a Tuya v3.5 UDP broadcast frame (0x6699 prefix).
///
/// Layout: header(18) | iv(12) | ciphertext | tag(16) | suffix(4)
///   header = prefix(4) + unknown(2) + seq(4) + cmd(4) + payload_len(4)
///   aad    = header[4..18]
fn gcm_decrypt_v35(data: &[u8]) -> Option<Vec<u8>> {
    use aes_gcm::{
        aead::{Aead, KeyInit, Payload},
        Aes128Gcm, Key, Nonce,
    };

    const HEADER_LEN: usize = 18;
    const SUFFIX_LEN: usize = 4;
    const TAG_LEN:    usize = 16;
    const IV_LEN:     usize = 12;

    if data.len() < HEADER_LEN + IV_LEN + TAG_LEN + SUFFIX_LEN {
        return None;
    }

    let payload_len = u32::from_be_bytes(data[14..18].try_into().ok()?) as usize;
    let msg_len = HEADER_LEN + payload_len + SUFFIX_LEN;
    if data.len() < msg_len || payload_len < IV_LEN + TAG_LEN {
        return None;
    }

    let aad = &data[4..HEADER_LEN];
    let iv  = &data[HEADER_LEN..HEADER_LEN + IV_LEN];
    let ct  = &data[HEADER_LEN + IV_LEN..msg_len - SUFFIX_LEN - TAG_LEN];
    let tag = &data[msg_len - SUFFIX_LEN - TAG_LEN..msg_len - SUFFIX_LEN];

    let mut ct_with_tag = ct.to_vec();
    ct_with_tag.extend_from_slice(tag);

    let key_bytes = udp_key();
    let key    = Key::<Aes128Gcm>::from_slice(&key_bytes);
    let cipher = Aes128Gcm::new(key);
    let nonce  = Nonce::from_slice(iv);

    cipher.decrypt(nonce, Payload { msg: &ct_with_tag, aad }).ok()
}

// ─── Frame parsing ────────────────────────────────────────────────────────────

const PREFIX:      [u8; 4] = [0x00, 0x00, 0x55, 0xAA];
const SUFFIX_BYTES:[u8; 4] = [0x00, 0x00, 0xAA, 0x55];
const PREFIX_6699: [u8; 4] = [0x00, 0x00, 0x66, 0x99];

/// Extract the JSON payload from a standard Tuya UDP frame.
///
/// Frame layout: prefix(4) + seq(4) + cmd(4) + len(4) + retcode(4) + payload + CRC(4) + suffix(4)
fn extract_payload(data: &[u8]) -> Option<&[u8]> {
    if data.len() < 28 || data[0..4] != PREFIX {
        return None;
    }
    let end = data.len().saturating_sub(8);
    if end <= 20 { return None; }
    Some(&data[20..end])
}

/// Build a minimal Tuya UDP scan packet (cmd 0x12) to trigger device responses.
fn build_scan_packet() -> [u8; 20] {
    let mut pkt = [0u8; 20];
    pkt[0..4].copy_from_slice(&PREFIX);
    pkt[8..12].copy_from_slice(&[0x00, 0x00, 0x00, 0x12]);
    pkt[12..16].copy_from_slice(&[0x00, 0x00, 0x00, 0x08]);
    pkt[16..20].copy_from_slice(&SUFFIX_BYTES);
    pkt
}

#[derive(Deserialize)]
struct TuyaBroadcast {
    #[serde(rename = "gwId", alias = "devId")]
    gw_id: String,
    /// Device's own IP address as reported in the broadcast payload.
    #[serde(default)]
    ip: Option<String>,
    /// Protocol version embedded in the broadcast JSON (e.g. "3.3", "3.4", "3.5").
    #[serde(default)]
    version: Option<String>,
}

struct ParsedDevice {
    tuya_id:    String,
    payload_ip: Option<Ipv4Addr>,
    version:    String,
}

/// Try every known decode of a raw UDP datagram.
fn parse_broadcast(src: Ipv4Addr, data: &[u8]) -> Option<ParsedDevice> {
    // ── v3.5 (0x6699) — AES-128-GCM ──────────────────────────────────────────
    if data.starts_with(&PREFIX_6699) {
        match gcm_decrypt_v35(data) {
            Some(ref plain) => {
                // GCM plaintext is retcode(4) + JSON.  Try with and without the
                // 4-byte retcode prefix.
                let candidates: &[&[u8]] = if plain.first() != Some(&b'{') && plain.get(4) == Some(&b'{') {
                    &[&plain[4..], plain.as_slice()]
                } else {
                    &[plain.as_slice()]
                };
                for candidate in candidates {
                    if let Ok(b) = serde_json::from_slice::<TuyaBroadcast>(candidate) {
                        let payload_ip = b.ip.as_deref().and_then(|s| s.parse().ok());
                        let version    = b.version.unwrap_or_else(|| "3.5".into());
                        tracing::debug!(%src, "discovery: parsed v3.5 GCM JSON");
                        return Some(ParsedDevice { tuya_id: b.gw_id, payload_ip, version });
                    }
                }
                tracing::debug!(%src, "discovery: v3.5 GCM JSON parse failed");
            }
            None => tracing::debug!(%src, "discovery: v3.5 GCM decrypt failed"),
        }
        return None;
    }

    let key     = udp_key();
    let payload = extract_payload(data);
    tracing::debug!(
        %src,
        total_bytes = data.len(),
        header_ok   = payload.is_some(),
        first16     = %hex::encode(&data[..data.len().min(16)]),
        "discovery: raw packet",
    );

    let candidates: Vec<(&[u8], &str)> = match payload {
        Some(p) => vec![(p, "frame-payload"), (data, "raw")],
        None    => vec![(data, "raw")],
    };

    for (candidate, label) in &candidates {
        // 1. Plaintext JSON.
        if let Ok(b) = serde_json::from_slice::<TuyaBroadcast>(candidate) {
            let payload_ip = b.ip.as_deref().and_then(|s| s.parse().ok());
            let version    = b.version.unwrap_or_default();
            tracing::debug!(%src, via = label, "discovery: parsed plain JSON");
            return Some(ParsedDevice { tuya_id: b.gw_id, payload_ip, version });
        }

        // 2. Plaintext with 15-byte version prefix stripped.
        if candidate.len() > 15 {
            if let Ok(b) = serde_json::from_slice::<TuyaBroadcast>(&candidate[15..]) {
                let payload_ip = b.ip.as_deref().and_then(|s| s.parse().ok());
                let version    = b.version.unwrap_or_default();
                tracing::debug!(%src, via = label, "discovery: parsed plain JSON (15-byte strip)");
                return Some(ParsedDevice { tuya_id: b.gw_id, payload_ip, version });
            }
        }

        // 3. AES-128-ECB (v3.3).
        if let Some(ref dec) = aes_ecb_decrypt(&key, candidate) {
            tracing::debug!(
                %src, via = label,
                decrypted_len     = dec.len(),
                decrypted_preview = %String::from_utf8_lossy(&dec[..dec.len().min(64)]),
                "discovery: AES ECB decrypted",
            );
            if let Ok(b) = serde_json::from_slice::<TuyaBroadcast>(dec) {
                let payload_ip = b.ip.as_deref().and_then(|s| s.parse().ok());
                let version    = b.version.unwrap_or_else(|| "3.3".into());
                tracing::debug!(%src, via = label, "discovery: parsed AES ECB JSON");
                return Some(ParsedDevice { tuya_id: b.gw_id, payload_ip, version });
            }
            if dec.len() > 15 {
                if let Ok(b) = serde_json::from_slice::<TuyaBroadcast>(&dec[15..]) {
                    let payload_ip = b.ip.as_deref().and_then(|s| s.parse().ok());
                    let version    = b.version.unwrap_or_else(|| "3.3".into());
                    tracing::debug!(%src, via = label, "discovery: parsed AES ECB JSON (15-byte strip)");
                    return Some(ParsedDevice { tuya_id: b.gw_id, payload_ip, version });
                }
            }
        }
    }

    tracing::debug!(%src, "discovery: all decode attempts failed");
    None
}

// ─── Socket helpers ───────────────────────────────────────────────────────────

/// Bind a UDP socket with SO_REUSEPORT + SO_REUSEADDR + SO_BROADCAST.
/// When `iface` is `Some`, also sets SO_BINDTODEVICE to restrict traffic
/// to that network interface.
fn bind_udp(port: u16, iface: Option<&str>) -> anyhow::Result<UdpSocket> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_reuse_address(true)?;
    #[cfg(unix)]
    sock.set_reuse_port(true)?;
    sock.set_broadcast(true)?;
    sock.set_nonblocking(true)?;
    #[cfg(target_os = "linux")]
    if let Some(name) = iface {
        sock.bind_device(Some(name.as_bytes()))?;
    }
    sock.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port).into())?;
    let std_sock: std::net::UdpSocket = sock.into();
    Ok(UdpSocket::from_std(std_sock)?)
}

// ─── ARP resolution ───────────────────────────────────────────────────────────

fn arp_table() -> HashMap<Ipv4Addr, String> {
    let mut map = HashMap::new();
    let Ok(content) = std::fs::read_to_string("/proc/net/arp") else {
        return map;
    };
    for line in content.lines().skip(1) {
        // Columns: IP  HW-type  Flags  HW-address  Mask  Device
        let mut cols = line.split_whitespace();
        let ip_str   = cols.next().unwrap_or("");
        let _hw_type = cols.next();
        let _flags   = cols.next();
        let mac      = cols.next().unwrap_or("");
        if let Ok(ip) = ip_str.parse::<Ipv4Addr>() {
            if !mac.is_empty() && mac != "00:00:00:00:00:00" {
                map.insert(ip, mac.to_uppercase());
            }
        }
    }
    map
}

/// Look up a device IP in the ARP table.
///
/// If the IP is missing, sends a tiny UDP datagram to the device to trigger
/// kernel ARP resolution, waits 200 ms, then re-reads the table.
/// Returns an empty string if the MAC is still unresolvable.
async fn resolve_mac(ip: Ipv4Addr) -> String {
    if let Some(mac) = arp_table().get(&ip).cloned() {
        return mac;
    }
    // Trigger kernel ARP by sending a throw-away datagram.
    if let Ok(probe) = std::net::UdpSocket::bind("0.0.0.0:0") {
        probe.set_nonblocking(true).ok();
        probe.send_to(b"\x00", std::net::SocketAddrV4::new(ip, 6666)).ok();
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    arp_table().get(&ip).cloned().unwrap_or_default()
}

// ─── Discovery daemon ─────────────────────────────────────────────────────────

/// How often to broadcast an active scan probe.
const PROBE_INTERVAL: Duration = Duration::from_secs(30);

/// Spawn background tasks that listen for Tuya UDP broadcasts on ports 6666
/// and 6667 and forward decoded `DiscoveredDevice` messages onto `tx`.
///
/// Only pushes to the channel when a device is new or its record has changed
/// (IP or version update), using `db` as the source of truth.
///
/// When `interfaces` is `Some`, one socket per interface per port is bound
/// via SO_BINDTODEVICE.  When `None`, a single unbound socket per port
/// receives traffic from all interfaces.
pub fn spawn(
    tx:         Arc<broadcast::Sender<DiscoveredDevice>>,
    db:         Arc<RouterDb>,
    interfaces: Option<Vec<String>>,
    kea:        Option<Arc<KeaClient>>,
) {
    // Build the list of (port, Option<iface>) pairs to listen on.
    let iface_list: Vec<Option<String>> = match &interfaces {
        Some(ifaces) if !ifaces.is_empty() => {
            tracing::info!(?ifaces, "discovery: binding to specified interfaces");
            ifaces.iter().map(|i| Some(i.clone())).collect()
        }
        _ => vec![None],
    };

    for port in [6666u16, 6667] {
        for iface in &iface_list {
            let tx    = tx.clone();
            let db    = db.clone();
            let iface = iface.clone();
            let kea   = kea.clone();
            tokio::spawn(async move {
                listen_loop(port, tx, db, iface, kea).await;
            });
        }
    }

    tokio::spawn(async move {
        active_scan_loop(interfaces).await;
    });
}

async fn listen_loop(
    port:  u16,
    tx:    Arc<broadcast::Sender<DiscoveredDevice>>,
    db:    Arc<RouterDb>,
    iface: Option<String>,
    kea:   Option<Arc<KeaClient>>,
) {
    let sock = match bind_udp(port, iface.as_deref()) {
        Ok(s)  => s,
        Err(e) => {
            tracing::warn!(%port, iface = ?iface, "discovery: cannot bind UDP port: {e}");
            return;
        }
    };
    tracing::info!(%port, iface = ?iface, "discovery: UDP listener started");

    let mut buf = [0u8; 4096];
    loop {
        let (n, addr) = match sock.recv_from(&mut buf).await {
            Ok(v)  => v,
            Err(e) => { tracing::warn!(%port, "discovery: recv_from error: {e}"); continue; }
        };
        let src_ip = match addr {
            SocketAddr::V4(a) => *a.ip(),
            _                 => continue,
        };

        let Some(parsed) = parse_broadcast(src_ip, &buf[..n]) else { continue };

        let ip  = parsed.payload_ip.unwrap_or(src_ip);
        let mac = resolve_mac(ip).await;

        let record = DeviceRecord {
            tuya_id: parsed.tuya_id.clone(),
            mac:     mac.clone(),
            ip:      ip.to_string(),
            version: parsed.version.clone(),
        };

        // Persist and only forward if something changed.
        let changed = match db.upsert(&record) {
            Ok(c)  => c,
            Err(e) => {
                tracing::warn!(tuya_id = %parsed.tuya_id, "discovery: db upsert error: {e}");
                true // forward anyway on db error
            }
        };

        if !changed { continue; }

        // Push/refresh Kea reservation for new or IP-changed devices so Kea
        // assigns the same IP on the next DHCP renewal.
        if let Some(ref kea) = kea {
            if !record.mac.is_empty() && !record.ip.is_empty() {
                if let Err(e) = kea.reservation_add(&record.mac, &record.ip).await {
                    tracing::warn!(
                        mac = %record.mac,
                        ip  = %record.ip,
                        "discovery: kea reservation: {e}",
                    );
                }
            }
        }

        tracing::debug!(
            tuya_id = %parsed.tuya_id,
            %ip,
            %mac,
            version = %parsed.version,
            "discovery: device changed",
        );

        // A send error just means no current subscribers — not fatal.
        let _ = tx.send(DiscoveredDevice {
            tuya_id: parsed.tuya_id,
            ip:      ip.to_string(),
            mac,
            version: parsed.version,
        });
    }
}

/// Periodically broadcast a Tuya scan probe on each monitored interface
/// to wake devices that don't announce themselves continuously.
async fn active_scan_loop(interfaces: Option<Vec<String>>) {
    let probe = build_scan_packet();
    let dst   = SocketAddr::from(([255, 255, 255, 255], 6666u16));

    let iface_list: Vec<Option<String>> = match &interfaces {
        Some(ifaces) if !ifaces.is_empty() =>
            ifaces.iter().map(|i| Some(i.clone())).collect(),
        _ => vec![None],
    };

    loop {
        for iface in &iface_list {
            match bind_udp(0, iface.as_deref()) {
                Ok(sock) => {
                    sock.set_broadcast(true).ok();
                    match sock.send_to(&probe, dst).await {
                        Ok(_)  => tracing::debug!(iface = ?iface, "discovery: active scan probe sent"),
                        Err(e) => tracing::debug!(iface = ?iface, "discovery: active scan probe failed: {e}"),
                    }
                }
                Err(e) => tracing::debug!(iface = ?iface, "discovery: bind for probe failed: {e}"),
            }
        }
        tokio::time::sleep(PROBE_INTERVAL).await;
    }
}
