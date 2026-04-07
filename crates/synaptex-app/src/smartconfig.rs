/// Tuya EZ SmartConfig implementation.
///
/// Encodes SSID + password into a sequence of UDP packet lengths and broadcasts
/// them until the device ACKs with its new IP and Tuya ID.
///
/// Protocol reference: Tuya EZ mode as documented in open-source Tuya IoT SDK
/// and community reverse-engineering (e.g. tinytuya provision module).
/// Verify encoding details against a known working implementation before shipping.
use std::{
    net::{Ipv4Addr, SocketAddrV4},
    time::{Duration, Instant},
};

use anyhow::{bail, Result};
use tokio::net::UdpSocket;

// ─── EZ-mode constants ───────────────────────────────────────────────────────

/// Broadcast targets for EZ frames.
const BCAST:    &str = "255.255.255.255:6668";
const MCAST:    &str = "239.255.255.251:1234";
/// Port the device sends its ACK to.
const ACK_PORT: u16  = 7000;
/// Preamble frame count and length.
const PREAMBLE_COUNT: usize = 16;
const PREAMBLE_LEN:   usize = 60;

// ─── Session ─────────────────────────────────────────────────────────────────

pub struct SmartConfigSession {
    ssid:     String,
    password: String,
    /// BSSID of the target AP.  All-zero is accepted by most devices in EZ mode.
    bssid:    [u8; 6],
}

impl SmartConfigSession {
    pub fn new(ssid: String, password: String) -> Self {
        Self { ssid, password, bssid: [0u8; 6] }
    }

    /// Broadcast EZ frames until device ACK or `timeout`.
    /// Returns `(device_ip, tuya_id)` on success.
    pub async fn run(&self, timeout: Duration) -> Result<(Ipv4Addr, String)> {
        let frames = self.build_frames();

        let deadline = Instant::now() + timeout;

        // ACK listener socket.
        let ack_sock = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, ACK_PORT))
            .await?;
        ack_sock.set_broadcast(true)?;

        // Broadcast socket.
        let tx = UdpSocket::bind("0.0.0.0:0").await?;
        tx.set_broadcast(true)?;

        loop {
            if Instant::now() >= deadline {
                bail!("SmartConfig timed out after {timeout:?}");
            }

            // Send one full frame cycle.
            for frame_len in &frames {
                let dummy = vec![0u8; *frame_len];
                // Errors are non-fatal — network may not be ready yet.
                let _ = tx.send_to(&dummy, BCAST).await;
                let _ = tx.send_to(&dummy, MCAST).await;
                tokio::time::sleep(Duration::from_millis(4)).await;
            }

            // Non-blocking check for ACK.
            let mut buf = [0u8; 128];
            match tokio::time::timeout(Duration::from_millis(50), ack_sock.recv_from(&mut buf)).await {
                Ok(Ok((n, addr))) => {
                    if let Some((ip, tuya_id)) = parse_ack(&buf[..n]) {
                        return Ok((ip, tuya_id));
                    }
                    // Got some packet from the device IP but couldn't parse — use sender IP.
                    if let std::net::SocketAddr::V4(v4) = addr {
                        return Ok((*v4.ip(), String::new()));
                    }
                }
                _ => {} // timeout or error — keep broadcasting
            }
        }
    }

    // ── Frame encoding ────────────────────────────────────────────────────────

    fn build_frames(&self) -> Vec<usize> {
        let payload = self.build_payload();
        let mut frames = Vec::new();

        // Preamble: 16 frames of length 60.
        for _ in 0..PREAMBLE_COUNT {
            frames.push(PREAMBLE_LEN);
        }

        // Guide code: encodes total payload length in 4 frames.
        let total = payload.len();
        frames.push(40 + ((total >> 4) & 0xF));
        frames.push(40 + (total & 0xF));
        frames.push(40 + ((total >> 12) & 0xF));
        frames.push(40 + ((total >> 8) & 0xF));

        // Data frames: each byte → two nibble frames.
        for (i, &byte) in payload.iter().enumerate() {
            let hi = (byte >> 4) & 0xF;
            let lo = byte & 0xF;
            // Sequence number nibble appended to differentiate adjacent identical bytes.
            let seq = (i & 0xF) as u8;
            frames.push(40 + hi as usize);
            frames.push(40 + lo as usize);
            // Sequence frame.
            frames.push(40 + seq as usize);
        }

        frames
    }

    fn build_payload(&self) -> Vec<u8> {
        // Payload: [ssid_len][password_len][bssid(6)][ssid_bytes][password_bytes][checksum]
        let ssid_bytes  = self.ssid.as_bytes();
        let pass_bytes  = self.password.as_bytes();

        let mut payload = Vec::new();
        payload.push(ssid_bytes.len() as u8);
        payload.push(pass_bytes.len() as u8);
        payload.extend_from_slice(&self.bssid);
        payload.extend_from_slice(ssid_bytes);
        payload.extend_from_slice(pass_bytes);

        // Simple XOR checksum.
        let checksum = payload.iter().fold(0u8, |acc, &b| acc ^ b);
        payload.push(checksum);

        payload
    }
}

// ─── ACK parsing ─────────────────────────────────────────────────────────────

/// Parse ACK packet from device.
/// Tuya ACK format (approximate): "TUYARP=<ip>,<tuya_id>" or just an IP string.
fn parse_ack(buf: &[u8]) -> Option<(Ipv4Addr, String)> {
    let s = std::str::from_utf8(buf).ok()?;
    // Try "ip,tuya_id" format.
    if let Some(rest) = s.strip_prefix("TUYARP=") {
        let mut parts = rest.splitn(2, ',');
        let ip_str    = parts.next()?.trim();
        let tuya_id   = parts.next().unwrap_or("").trim().to_string();
        let ip: Ipv4Addr = ip_str.parse().ok()?;
        return Some((ip, tuya_id));
    }
    // Fallback: bare IP string.
    let ip: Ipv4Addr = s.trim().parse().ok()?;
    Some((ip, String::new()))
}
