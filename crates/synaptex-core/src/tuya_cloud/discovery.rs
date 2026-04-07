use std::{
    collections::HashMap,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::Result;
use serde::Deserialize;
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;

// ─── UDP broadcast decryption key ─────────────────────────────────────────────

/// AES-128-ECB key used by Tuya v3.3 devices for UDP broadcast payloads.
/// = MD5("yGAdlopoPVldABfn")
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
    // Try PKCS7 first; fall back to stripping null/control bytes (some Tuya firmware).
    let last = *out.last()?;
    if last > 0 && last <= 16 && out.ends_with(&vec![last; last as usize]) {
        out.truncate(out.len() - last as usize);
    } else {
        out.retain(|&b| b >= 0x20); // strip nulls and control chars
    }
    Some(out)
}

/// Decrypt a Tuya v3.5 UDP broadcast frame (0x6699 prefix).
///
/// Layout: header(18) | iv(12) | ciphertext | tag(16) | suffix(4)
///   header = prefix(4) + unknown(2) + seq(4) + cmd(4) + payload_len(4)
///   aad    = header[4..18]  (unknown + seq + cmd + payload_len, 14 bytes)
///
/// Key is MD5("yGAdlopoPVldABfn") — the same shared UDP key as v3.3, but used
/// as the raw 16-byte digest for AES-128-GCM instead of as an ECB key.
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

    let aad         = &data[4..HEADER_LEN];
    let iv          = &data[HEADER_LEN..HEADER_LEN + IV_LEN];
    let ct          = &data[HEADER_LEN + IV_LEN..msg_len - SUFFIX_LEN - TAG_LEN];
    let tag         = &data[msg_len - SUFFIX_LEN - TAG_LEN..msg_len - SUFFIX_LEN];

    // aes-gcm expects ciphertext with tag appended.
    let mut ct_with_tag = ct.to_vec();
    ct_with_tag.extend_from_slice(tag);

    let key_bytes = udp_key();
    let key    = Key::<Aes128Gcm>::from_slice(&key_bytes);
    let cipher = Aes128Gcm::new(key);
    let nonce  = Nonce::from_slice(iv);

    cipher.decrypt(nonce, Payload { msg: &ct_with_tag, aad }).ok()
}

// ─── Frame / payload parsing ──────────────────────────────────────────────────

const PREFIX:      [u8; 4] = [0x00, 0x00, 0x55, 0xAA];
const SUFFIX:      [u8; 4] = [0x00, 0x00, 0xAA, 0x55];
const PREFIX_6699: [u8; 4] = [0x00, 0x00, 0x66, 0x99];

/// Extract the inner payload bytes from a Tuya UDP frame.
///
/// UDP broadcast frames have a 20-byte header:
///   prefix(4) + seq(4) + cmd(4) + len(4) + retcode(4)
/// followed by the payload, then CRC(4) + suffix(4) at the end.
fn extract_payload(data: &[u8]) -> Option<&[u8]> {
    if data.len() < 28 || data[0..4] != PREFIX {
        return None;
    }
    // payload = data[20 .. len-8]
    let end = data.len().saturating_sub(8);
    if end <= 20 {
        return None;
    }
    Some(&data[20..end])
}

/// Build a minimal Tuya UDP scan packet (cmd 0x12, empty payload) to trigger
/// device responses on the local network.
fn build_scan_packet() -> [u8; 20] {
    let mut pkt = [0u8; 20];
    pkt[0..4].copy_from_slice(&PREFIX);
    // seq = 0, cmd = 0x12 (18 = UDP broadcast/scan)
    pkt[8..12].copy_from_slice(&[0x00, 0x00, 0x00, 0x12]);
    // len = 8 (CRC 4 + suffix 4, no payload)
    pkt[12..16].copy_from_slice(&[0x00, 0x00, 0x00, 0x08]);
    // CRC32 of empty payload = 0
    pkt[16..20].copy_from_slice(&SUFFIX);
    pkt
}

#[derive(Deserialize)]
struct TuyaBroadcast {
    #[serde(rename = "gwId", alias = "devId")]
    gw_id: String,
    /// Device's own IP address, reported in the broadcast payload.
    #[serde(default)]
    ip: Option<String>,
}

/// Try every reasonable decode of a raw UDP datagram.
/// Returns `(tuya_id, payload_ip)` where `payload_ip` is the IP reported inside
/// the broadcast JSON (more reliable than ARP for discovering all devices).
/// Logs each attempted step at DEBUG so failures can be diagnosed.
fn parse_broadcast(src: Ipv4Addr, data: &[u8]) -> Option<(String, Option<Ipv4Addr>)> {
    // ── v3.5 (0x6699) — AES-128-GCM with shared MD5 key ──────────────────────
    if data.starts_with(&PREFIX_6699) {
        match gcm_decrypt_v35(data) {
            Some(ref plain) => {
                // GCM plaintext is retcode(4) + JSON.  Try with and without the
                // 4-byte retcode prefix (tinytuya strips it when plain[0] != '{').
                let candidates: &[&[u8]] = if plain.first() != Some(&b'{') && plain.get(4) == Some(&b'{') {
                    &[&plain[4..], plain.as_slice()]
                } else {
                    &[plain.as_slice()]
                };
                for candidate in candidates {
                    match serde_json::from_slice::<TuyaBroadcast>(candidate) {
                        Ok(b) => {
                            let payload_ip = b.ip.as_deref().and_then(|s| s.parse().ok());
                            tracing::debug!(src = %src, "discovery: parsed v3.5 GCM JSON");
                            return Some((b.gw_id, payload_ip));
                        }
                        Err(e) => tracing::debug!(src = %src, err = %e, "discovery: v3.5 GCM JSON parse failed"),
                    }
                }
            }
            None => tracing::debug!(src = %src, "discovery: v3.5 GCM decrypt failed"),
        }
        // v3.5 frames use only GCM; no ECB fallback applies.
        return None;
    }

    let key = udp_key();

    let payload = extract_payload(data);
    tracing::debug!(
        src = %src,
        total_bytes = data.len(),
        header_ok = payload.is_some(),
        payload_bytes = payload.map(|p| p.len()),
        first16 = %hex::encode(&data[..data.len().min(16)]),
        "discovery: raw packet",
    );

    let candidates: Vec<(&[u8], &str)> = match payload {
        Some(p) => vec![(p, "frame-payload"), (data, "raw")],
        None    => vec![(data, "raw")],
    };

    for (candidate, label) in &candidates {
        // 1. Plaintext JSON.
        match serde_json::from_slice::<TuyaBroadcast>(candidate) {
            Ok(b) => {
                let payload_ip = b.ip.as_deref().and_then(|s| s.parse().ok());
                tracing::debug!(src = %src, via = label, "discovery: parsed plain JSON");
                return Some((b.gw_id, payload_ip));
            }
            Err(e) => tracing::debug!(src = %src, via = label, err = %e, "discovery: plain JSON failed"),
        }

        // 2. Plaintext with 15-byte version prefix stripped.
        if candidate.len() > 15 {
            match serde_json::from_slice::<TuyaBroadcast>(&candidate[15..]) {
                Ok(b) => {
                    let payload_ip = b.ip.as_deref().and_then(|s| s.parse().ok());
                    tracing::debug!(src = %src, via = label, "discovery: parsed plain JSON (15-byte strip)");
                    return Some((b.gw_id, payload_ip));
                }
                Err(e) => tracing::debug!(src = %src, via = label, err = %e, "discovery: 15-strip JSON failed"),
            }
        }

        // 3. AES-128-ECB with the shared UDP key (v3.3 broadcast encryption).
        match aes_ecb_decrypt(&key, candidate) {
            None => tracing::debug!(src = %src, via = label,
                len = candidate.len(), "discovery: AES decrypt skipped (bad length or empty)"),
            Some(ref dec) => {
                tracing::debug!(src = %src, via = label,
                    decrypted_len = dec.len(),
                    decrypted_preview = %String::from_utf8_lossy(&dec[..dec.len().min(64)]),
                    "discovery: AES decrypted");
                match serde_json::from_slice::<TuyaBroadcast>(dec) {
                    Ok(b) => {
                        let payload_ip = b.ip.as_deref().and_then(|s| s.parse().ok());
                        tracing::debug!(src = %src, via = label, "discovery: parsed AES JSON");
                        return Some((b.gw_id, payload_ip));
                    }
                    Err(e) => tracing::debug!(src = %src, via = label, err = %e, "discovery: AES JSON failed"),
                }
                if dec.len() > 15 {
                    match serde_json::from_slice::<TuyaBroadcast>(&dec[15..]) {
                        Ok(b) => {
                            let payload_ip = b.ip.as_deref().and_then(|s| s.parse().ok());
                            tracing::debug!(src = %src, via = label, "discovery: parsed AES JSON (15-byte strip)");
                            return Some((b.gw_id, payload_ip));
                        }
                        Err(e) => tracing::debug!(src = %src, via = label, err = %e, "discovery: AES+15-strip JSON failed"),
                    }
                }
            }
        }
    }

    tracing::debug!(src = %src, "discovery: all decode attempts failed");
    None
}

// ─── Socket helpers ───────────────────────────────────────────────────────────

/// Bind a UDP socket with SO_REUSEPORT + SO_REUSEADDR + SO_BROADCAST so that
/// the bind succeeds even if another process is already on that port.
fn bind_udp(port: u16) -> Result<UdpSocket> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_reuse_address(true)?;
    #[cfg(unix)]
    sock.set_reuse_port(true)?;
    sock.set_broadcast(true)?;
    sock.set_nonblocking(true)?;
    sock.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port).into())?;
    let std_sock: std::net::UdpSocket = sock.into();
    Ok(UdpSocket::from_std(std_sock)?)
}

// ─── ARP table ────────────────────────────────────────────────────────────────

/// Parse /proc/net/arp and return IP → MAC (uppercase, colon-separated).
pub fn arp_table() -> HashMap<Ipv4Addr, String> {
    let mut map = HashMap::new();
    let Ok(content) = std::fs::read_to_string("/proc/net/arp") else {
        return map;
    };
    for line in content.lines().skip(1) {
        // Columns: IP  HW-type  Flags  HW-address  Mask  Device
        let mut cols = line.split_whitespace();
        let ip_str  = cols.next().unwrap_or("");
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

// ─── Public API ───────────────────────────────────────────────────────────────

pub struct DiscoveredDevice {
    pub tuya_id: String,
    pub ip:      Ipv4Addr,
    pub mac:     String,
}

/// Discover Tuya devices on the local network.
///
/// Binds to UDP ports 6666 and 6667 (with SO_REUSEPORT so existing listeners
/// don't block us), sends an active scan broadcast to wake devices up, then
/// collects device IDs for `duration`. Returns devices whose MAC is in the
/// host ARP table.
pub async fn discover(duration: Duration) -> Result<Vec<DiscoveredDevice>> {
    let seen: Arc<Mutex<HashMap<String, Ipv4Addr>>> = Arc::new(Mutex::new(HashMap::new()));

    // Bind sockets — log clearly on failure so it shows up in daemon logs.
    let sock6666 = match bind_udp(6666) {
        Ok(s)  => Some(s),
        Err(e) => { tracing::warn!("UDP discovery: cannot bind 6666: {e}"); None }
    };
    let sock6667 = match bind_udp(6667) {
        Ok(s)  => Some(s),
        Err(e) => { tracing::warn!("UDP discovery: cannot bind 6667: {e}"); None }
    };

    if sock6666.is_none() && sock6667.is_none() {
        anyhow::bail!("could not bind either UDP discovery port (6666/6667)");
    }

    // Send a probe broadcast to port 6666 to wake up any devices that don't
    // broadcast continuously.
    let probe = build_scan_packet();
    if let Some(ref s) = sock6666 {
        let dst = SocketAddr::from(([255, 255, 255, 255], 6666u16));
        if let Err(e) = s.send_to(&probe, dst).await {
            tracing::debug!("UDP probe send failed (non-fatal): {e}");
        }
    }

    async fn listen(sock: UdpSocket, seen: Arc<Mutex<HashMap<String, Ipv4Addr>>>) {
        let mut buf = [0u8; 4096];
        loop {
            let Ok((n, addr)) = sock.recv_from(&mut buf).await else { break };
            let src_ip = match addr {
                SocketAddr::V4(a) => *a.ip(),
                _                 => continue,
            };
            let data = &buf[..n];
            match parse_broadcast(src_ip, data) {
                Some((id, payload_ip)) => {
                    // Prefer the IP reported inside the JSON payload; it's more
                    // reliable than the UDP source when the device is behind NAT
                    // or when the server can't talk back to src_ip directly.
                    let ip = payload_ip.unwrap_or(src_ip);
                    tracing::debug!(src = %src_ip, resolved_ip = %ip, tuya_id = %id,
                        "UDP discovery: identified device");
                    seen.lock().unwrap().insert(id, ip);
                }
                None => {} // already logged inside parse_broadcast
            }
        }
    }

    let seen_a = seen.clone();
    let seen_b = seen.clone();

    let _ = tokio::time::timeout(duration, async move {
        match (sock6666, sock6667) {
            (Some(a), Some(b)) => { tokio::join!(listen(a, seen_a), listen(b, seen_b)); }
            (Some(a), None)    => { listen(a, seen_a).await; }
            (None,    Some(b)) => { listen(b, seen_b).await; }
            (None,    None)    => {}
        }
    })
    .await;

    // ── ARP population ────────────────────────────────────────────────────────
    // The kernel ARP table only contains IPs the server has recently talked to.
    // For each discovered device IP that isn't in the ARP table, send a tiny
    // UDP datagram to it — this triggers the kernel to perform ARP resolution
    // and populate the cache.  Wait briefly for replies, then re-read the table.
    {
        let mut arp = arp_table();
        let snapshot: Vec<(String, Ipv4Addr)> = seen.lock().unwrap()
            .iter()
            .map(|(id, ip)| (id.clone(), *ip))
            .collect();

        let missing: Vec<Ipv4Addr> = snapshot.iter()
            .filter(|(_, ip)| !arp.contains_key(ip))
            .map(|(_, ip)| *ip)
            .collect();

        if !missing.is_empty() {
            tracing::debug!(count = missing.len(), "UDP discovery: pinging unknown IPs to populate ARP");
            // Use a throw-away std UdpSocket — just needs to cause the kernel to ARP.
            if let Ok(probe_sock) = std::net::UdpSocket::bind("0.0.0.0:0") {
                probe_sock.set_nonblocking(true).ok();
                for ip in &missing {
                    let dst = std::net::SocketAddrV4::new(*ip, 6666);
                    probe_sock.send_to(b"\x00", dst).ok();
                }
            }
            // Give the kernel ~200 ms to resolve all the ARPs.
            tokio::time::sleep(Duration::from_millis(200)).await;
            arp = arp_table();
            tracing::debug!(arp_entries = arp.len(), "UDP discovery: ARP table after probe");
        }

        tracing::debug!(arp_entries = arp.len(), seen = snapshot.len(),
            "UDP discovery: scan complete");

        let devices = snapshot.iter()
            .filter_map(|(tuya_id, ip)| {
                let mac = arp.get(ip)?.clone();
                Some(DiscoveredDevice { tuya_id: tuya_id.clone(), ip: *ip, mac })
            })
            .collect();

        return Ok(devices);
    }
}
