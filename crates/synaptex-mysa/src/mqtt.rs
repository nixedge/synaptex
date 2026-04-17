//! Manual MQTT 3.1.1 framing over tokio-tungstenite WebSocket + worker task.

use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::{bail, Context, Result};
use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{http::Request, Message};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::{
    plugin::MysaAccount,
    types::{json_temp_to_tenths, MqttOutMsg, MysaDeviceState},
};
use synaptex_types::plugin::{DeviceState, StateChangeEvent};

// ─── Worker command ───────────────────────────────────────────────────────────

pub(crate) enum WorkerCmd {
    Publish { topic: String, payload: Vec<u8>, qos: u8 },
    Subscribe { device_id: String },
}

// ─── Worker entry point ───────────────────────────────────────────────────────

/// Spawned by `MysaAccount::start_mqtt_worker`.  Runs forever, reconnecting as needed.
pub(crate) async fn run_mqtt_worker(account: Arc<MysaAccount>, mut cmd_rx: mpsc::Receiver<WorkerCmd>) {
    loop {
        // Ensure we have valid credentials.
        let session = match account.ensure_auth().await {
            Ok(s)  => s,
            Err(e) => {
                warn!("mysa: auth failed, retrying in 30s: {e:#}");
                tokio::time::sleep(Duration::from_secs(30)).await;
                continue;
            }
        };

        let url = crate::sigv4::presign_mqtt_url(
            &session.aws_key_id, &session.aws_secret, &session.aws_session,
        );

        match run_session(&url, &account, &mut cmd_rx).await {
            Ok(())  => debug!("mysa: MQTT session ended cleanly"),
            Err(e)  => warn!("mysa: MQTT session error: {e:#}"),
        }

        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

// ─── Single MQTT session ──────────────────────────────────────────────────────

async fn run_session(
    wss_url:  &str,
    account:  &Arc<MysaAccount>,
    cmd_rx:   &mut mpsc::Receiver<WorkerCmd>,
) -> Result<()> {
    // WebSocket connect with mqtt subprotocol.
    let request = Request::builder()
        .uri(wss_url)
        .header("Sec-WebSocket-Protocol", "mqtt")
        .body(())
        .context("build WebSocket request")?;

    let (ws, _) = tokio_tungstenite::connect_async(request).await
        .context("WebSocket connect")?;
    let (mut sink, mut stream) = ws.split();

    // Send CONNECT.
    let client_id = Uuid::new_v4().to_string();
    sink.send(Message::Binary(build_connect(&client_id, 60))).await
        .context("send CONNECT")?;

    // Wait for CONNACK.
    let connack = recv_next(&mut stream).await.context("await CONNACK")?;
    if connack.first() != Some(&0x20) {
        bail!("expected CONNACK (0x20), got {:?}", connack.first());
    }
    let rc = connack.get(3).copied().unwrap_or(0xFF);
    if rc != 0 {
        bail!("CONNACK return code: {rc}");
    }
    info!("mysa: MQTT connected (client_id={client_id})");

    // Subscribe to all device topics in chunks of 2.
    subscribe_all(&mut sink, account).await?;

    // Ping timer every 25 seconds.
    let mut ping = tokio::time::interval(Duration::from_secs(25));
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ping.tick().await; // consume the immediate tick

    let mut packet_id: u16 = 1;

    loop {
        tokio::select! {
            msg = stream.next() => {
                let msg = match msg {
                    Some(Ok(m))  => m,
                    Some(Err(e)) => { warn!("mysa: ws error: {e}"); break; }
                    None         => break,
                };
                if let Message::Binary(data) = msg {
                    handle_packet(&data, account);
                } else if let Message::Close(_) = msg {
                    break;
                }
            }

            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(WorkerCmd::Publish { topic, payload, qos }) => {
                        let pid = if qos > 0 { Some(next_pid(&mut packet_id)) } else { None };
                        let pkt = build_publish(&topic, &payload, qos, pid);
                        if let Err(e) = sink.send(Message::Binary(pkt)).await {
                            warn!("mysa: publish failed: {e}");
                            break;
                        }
                    }
                    Some(WorkerCmd::Subscribe { device_id }) => {
                        let topics_owned = device_topics(&device_id);
                        let topics: Vec<(&str, u8)> = topics_owned.iter().map(|(t, q)| (t.as_str(), *q)).collect();
                        let pkt = build_subscribe(next_pid(&mut packet_id), &topics);
                        if let Err(e) = sink.send(Message::Binary(pkt)).await {
                            warn!("mysa: subscribe failed: {e}");
                        }
                    }
                    None => break,
                }
            }

            _ = ping.tick() => {
                if let Err(e) = sink.send(Message::Binary(vec![0xC0, 0x00])).await {
                    warn!("mysa: PINGREQ failed: {e}");
                    break;
                }
            }
        }
    }

    let _ = sink.send(Message::Binary(vec![0xE0, 0x00])).await; // DISCONNECT
    Ok(())
}

// ─── Subscribe helpers ────────────────────────────────────────────────────────

async fn subscribe_all(
    sink:    &mut (impl SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin),
    account: &Arc<MysaAccount>,
) -> Result<()> {
    let ids: Vec<String> = account.device_ids.iter().map(|r| r.key().clone()).collect();
    let mut pid: u16 = 100;

    for chunk in ids.chunks(2) {
        let mut topics: Vec<(&str, u8)> = Vec::new();
        let mut owned: Vec<(String, u8)> = Vec::new();
        for id in chunk {
            owned.push((format!("/v1/dev/{id}/out"),   1));
            owned.push((format!("/v1/dev/{id}/batch"), 0));
        }
        for (t, qos) in &owned {
            topics.push((t.as_str(), *qos));
        }
        let pkt = build_subscribe(pid, &topics);
        pid = pid.wrapping_add(1);
        sink.send(Message::Binary(pkt)).await
            .context("send SUBSCRIBE")?;
    }
    Ok(())
}

fn device_topics(id: &str) -> Vec<(String, u8)> {
    vec![
        (format!("/v1/dev/{id}/out"),   1),
        (format!("/v1/dev/{id}/batch"), 0),
    ]
}

// ─── Packet dispatch ──────────────────────────────────────────────────────────

fn handle_packet(data: &[u8], account: &Arc<MysaAccount>) {
    if data.is_empty() {
        return;
    }
    let packet_type = (data[0] >> 4) & 0x0F;
    match packet_type {
        3  => handle_publish(data, account),
        4  => { /* PUBACK — no-op */ }
        13 => { /* PINGRESP — no-op */ }
        _  => {}
    }
}

fn handle_publish(data: &[u8], account: &Arc<MysaAccount>) {
    // Parse PUBLISH fixed header.
    if data.len() < 2 {
        return;
    }
    let qos = (data[0] >> 1) & 0x03;
    let (rem_len, hdr_bytes) = decode_remaining_len(&data[1..]);
    let var_start = 1 + hdr_bytes;
    if data.len() < var_start + rem_len {
        return;
    }
    let var = &data[var_start..var_start + rem_len];

    // Topic name (2-byte length prefix).
    if var.len() < 2 {
        return;
    }
    let topic_len = u16::from_be_bytes([var[0], var[1]]) as usize;
    if var.len() < 2 + topic_len {
        return;
    }
    let topic = match std::str::from_utf8(&var[2..2 + topic_len]) {
        Ok(t)  => t,
        Err(_) => return,
    };

    // Skip packet identifier for QoS>0.
    let payload_start = 2 + topic_len + if qos > 0 { 2 } else { 0 };
    if var.len() < payload_start {
        return;
    }
    let payload = &var[payload_start..];

    // Extract device ID from topic.
    let device_id = extract_device_id(topic);

    if topic.ends_with("/out") {
        dispatch_json(device_id, payload, account);
    } else if topic.ends_with("/batch") {
        dispatch_batch(device_id, payload, account);
    }
}

fn dispatch_json(device_id: Option<&str>, payload: &[u8], account: &Arc<MysaAccount>) {
    let id = match device_id {
        Some(d) => d,
        None    => return,
    };
    let msg: MqttOutMsg = match serde_json::from_slice(payload) {
        Ok(m)  => m,
        Err(_) => return,
    };
    if msg.msg_type != 44 {
        return;
    }
    if let Some(state) = msg.body.state {
        let mut updated = false;
        let mut entry = account.state_cache.entry(id.to_string()).or_insert_with(|| {
            MysaDeviceState { temp_current: 0, temp_set: 200, mode: 0 }
        });

        if let Some(mode) = state.heating_mode {
            entry.mode = mode;
            updated = true;
        }
        if let Some(ref v) = state.temperature {
            if let Some(t) = json_temp_to_tenths(v) {
                entry.temp_current = t;
                updated = true;
            }
        }
        if let Some(ref v) = state.set_point {
            if let Some(t) = json_temp_to_tenths(v) {
                entry.temp_set = t;
                updated = true;
            }
        }

        if updated {
            let snap = entry.clone();
            drop(entry);
            push_state_event(id, snap, account);
        }
    }
}

/// Parse the binary batch telemetry frame emitted by BB-V1-0 devices on the
/// `/v1/dev/{id}/batch` topic.
///
/// Frame layout (little-endian):
/// ```text
/// Offset  Size  Field
///  0-1     2    Magic: 0xCA 0xA0
///  2       1    Version (0, 1, 3, …)
///  3-6     4    Unix timestamp (u32 LE)
///  7-8     2    Setpoint in tenths of °C (i16 LE)
///  9-10    2    Ambient temperature in tenths of °C (i16 LE)
///  …       …    Remaining fields vary by version
/// ```
///
/// Returns `(setpoint_tenths, ambient_temp_tenths)` on success, `None` if the
/// payload is too short or the magic bytes do not match.
pub(crate) fn parse_batch_frame(payload: &[u8]) -> Option<(u16, u16)> {
    if payload.len() < 11 || payload[0] != 0xCA || payload[1] != 0xA0 {
        return None;
    }
    // Raw values are already in tenths of °C — no conversion needed.
    let sp   = i16::from_le_bytes([payload[7], payload[8]])  as u16;
    let temp = i16::from_le_bytes([payload[9], payload[10]]) as u16;
    Some((sp, temp))
}

fn dispatch_batch(device_id: Option<&str>, payload: &[u8], account: &Arc<MysaAccount>) {
    let id = match device_id {
        Some(d) => d,
        None    => return,
    };
    let (sp, temp) = match parse_batch_frame(payload) {
        Some(v) => v,
        None    => return,
    };

    let mut entry = account.state_cache.entry(id.to_string()).or_insert_with(|| {
        MysaDeviceState { temp_current: 0, temp_set: 200, mode: 0 }
    });
    entry.temp_current = temp;
    entry.temp_set     = sp;
    let snap = entry.clone();
    drop(entry);
    push_state_event(id, snap, account);
}

fn push_state_event(mysa_id: &str, state: MysaDeviceState, account: &Arc<MysaAccount>) {
    // Look up the synaptex DeviceId for this mysa_id.
    let device_id = match account.mysa_to_device_id.get(mysa_id) {
        Some(r) => *r,
        None    => return,
    };

    let ds = DeviceState {
        device_id,
        online:           true,
        updated_at_ms:    now_ms(),
        power:            Some(state.mode != 0),
        brightness:       None,
        color_temp_k:     None,
        rgb:              None,
        mode:             None,
        switches:         HashMap::new(),
        fan_speed:        None,
        temp_current:     Some(state.temp_current),
        temp_set:         Some(state.temp_set),
        temp_calibration: None,
    };

    let event = StateChangeEvent {
        device_id,
        state:   ds,
        raw_dps: HashMap::new(),
    };
    account.bus_tx.send(event).ok();
}

fn extract_device_id(topic: &str) -> Option<&str> {
    // /v1/dev/{id}/out  or  /v1/dev/{id}/batch
    let parts: Vec<&str> = topic.split('/').collect();
    if parts.len() >= 4 && parts[1] == "v1" && parts[2] == "dev" {
        Some(parts[3])
    } else {
        None
    }
}

// ─── MQTT 3.1.1 packet builders ───────────────────────────────────────────────

fn build_connect(client_id: &str, keepalive: u16) -> Vec<u8> {
    let mut var = Vec::new();
    // Protocol Name "MQTT"
    var.extend_from_slice(&encode_str("MQTT"));
    // Protocol Level 4 (3.1.1), Connect Flags: clean session
    var.push(4);
    var.push(0x02);
    // Keepalive
    var.push((keepalive >> 8) as u8);
    var.push(keepalive as u8);
    // Client ID
    var.extend_from_slice(&encode_str(client_id));

    let mut pkt = vec![0x10];
    pkt.extend_from_slice(&encode_remaining_len(var.len()));
    pkt.extend_from_slice(&var);
    pkt
}

fn build_subscribe(packet_id: u16, topics: &[(&str, u8)]) -> Vec<u8> {
    let mut var = Vec::new();
    var.push((packet_id >> 8) as u8);
    var.push(packet_id as u8);
    for (topic, qos) in topics {
        var.extend_from_slice(&encode_str(topic));
        var.push(*qos);
    }
    let mut pkt = vec![0x82];
    pkt.extend_from_slice(&encode_remaining_len(var.len()));
    pkt.extend_from_slice(&var);
    pkt
}

pub(crate) fn build_publish(topic: &str, payload: &[u8], qos: u8, packet_id: Option<u16>) -> Vec<u8> {
    let mut var = Vec::new();
    var.extend_from_slice(&encode_str(topic));
    if qos > 0 {
        let pid = packet_id.unwrap_or(1);
        var.push((pid >> 8) as u8);
        var.push(pid as u8);
    }
    var.extend_from_slice(payload);

    let header = 0x30 | (qos << 1);
    let mut pkt = vec![header];
    pkt.extend_from_slice(&encode_remaining_len(var.len()));
    pkt.extend_from_slice(&var);
    pkt
}

fn encode_str(s: &str) -> Vec<u8> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(2 + bytes.len());
    out.push((bytes.len() >> 8) as u8);
    out.push(bytes.len() as u8);
    out.extend_from_slice(bytes);
    out
}

fn encode_remaining_len(mut len: usize) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let mut byte = (len % 128) as u8;
        len /= 128;
        if len > 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if len == 0 {
            break;
        }
    }
    out
}

fn decode_remaining_len(bytes: &[u8]) -> (usize, usize) {
    let mut multiplier = 1usize;
    let mut value      = 0usize;
    let mut consumed   = 0usize;
    for &byte in bytes.iter().take(4) {
        consumed += 1;
        value    += (byte & 0x7F) as usize * multiplier;
        multiplier *= 128;
        if byte & 0x80 == 0 {
            break;
        }
    }
    (value, consumed)
}

fn next_pid(pid: &mut u16) -> u16 {
    let current = *pid;
    *pid = pid.wrapping_add(1).max(1);
    current
}

async fn recv_next(
    stream: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
) -> Result<Vec<u8>> {
    loop {
        match stream.next().await {
            Some(Ok(Message::Binary(data))) => return Ok(data),
            Some(Ok(_)) => {}  // skip text/ping frames
            Some(Err(e)) => bail!("WebSocket error: {e}"),
            None         => bail!("WebSocket closed unexpectedly"),
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── encode_remaining_len / decode_remaining_len ───────────────────────────

    #[test]
    fn test_encode_remaining_len_single_byte() {
        assert_eq!(encode_remaining_len(0),   vec![0x00]);
        assert_eq!(encode_remaining_len(10),  vec![0x0A]);
        assert_eq!(encode_remaining_len(127), vec![0x7F]);
    }

    #[test]
    fn test_encode_remaining_len_two_bytes() {
        // 128: LSB = 128%128=0 | 0x80 continuation = 0x80, next byte = 128/128 = 1
        assert_eq!(encode_remaining_len(128), vec![0x80, 0x01]);
        // 200: 200%128=72=0x48 | 0x80 = 0xC8, 200/128=1
        assert_eq!(encode_remaining_len(200), vec![0xC8, 0x01]);
    }

    #[test]
    fn test_encode_decode_roundtrip() {
        for len in [0, 1, 127, 128, 200, 16383, 16384, 2_097_151] {
            let enc = encode_remaining_len(len);
            let (val, consumed) = decode_remaining_len(&enc);
            assert_eq!(val, len, "round-trip failed for len={len}");
            assert_eq!(consumed, enc.len(), "consumed mismatch for len={len}");
        }
    }

    // ── build_connect ─────────────────────────────────────────────────────────

    #[test]
    fn test_build_connect_type_and_magic() {
        let pkt = build_connect("test_client", 60);
        assert_eq!(pkt[0], 0x10, "CONNECT packet type must be 0x10");
        assert!(pkt.windows(4).any(|w| w == b"MQTT"), "packet must contain 'MQTT'");
    }

    #[test]
    fn test_build_connect_keepalive_bytes() {
        let pkt = build_connect("id", 120);
        let hi = (120u16 >> 8) as u8;
        let lo = (120u16 & 0xFF) as u8;
        assert!(pkt.windows(2).any(|w| w == [hi, lo]), "keepalive 120 not found in packet");
    }

    // ── build_subscribe ───────────────────────────────────────────────────────

    #[test]
    fn test_build_subscribe_type() {
        let pkt = build_subscribe(1, &[("/v1/dev/abc/out", 1)]);
        assert_eq!(pkt[0], 0x82, "SUBSCRIBE type must be 0x82");
    }

    #[test]
    fn test_build_subscribe_contains_topic() {
        let topic = "/v1/dev/abc/out";
        let pkt = build_subscribe(1, &[(topic, 1)]);
        assert!(pkt.windows(topic.len()).any(|w| w == topic.as_bytes()),
                "topic bytes not found in SUBSCRIBE packet");
    }

    #[test]
    fn test_build_subscribe_packet_id() {
        // Packet ID 0x00, 0x05 = 5 should appear in the variable header.
        let pkt = build_subscribe(5, &[("/v1/dev/x/out", 0)]);
        assert_eq!(pkt[0], 0x82);
        // After fixed header (type + remaining len), first two bytes are packet ID.
        let rem_len_bytes = if pkt[1] & 0x80 == 0 { 1 } else { 2 };
        let pid_hi = pkt[1 + rem_len_bytes];
        let pid_lo = pkt[1 + rem_len_bytes + 1];
        assert_eq!(u16::from_be_bytes([pid_hi, pid_lo]), 5);
    }

    // ── build_publish ─────────────────────────────────────────────────────────

    #[test]
    fn test_build_publish_qos0() {
        let pkt = build_publish("/v1/dev/abc/in", b"payload", 0, None);
        assert_eq!(pkt[0], 0x30, "QoS 0 PUBLISH must have type byte 0x30");
    }

    #[test]
    fn test_build_publish_qos1() {
        let pkt = build_publish("/v1/dev/abc/in", b"payload", 1, Some(42));
        assert_eq!(pkt[0] & 0x06, 0x02, "QoS 1 bits must be set in header");
    }

    #[test]
    fn test_build_publish_contains_payload() {
        let payload = b"hello world";
        let pkt = build_publish("/v1/dev/abc/in", payload, 0, None);
        assert!(pkt.windows(payload.len()).any(|w| w == payload),
                "payload bytes not found in PUBLISH packet");
    }

    // ── extract_device_id ─────────────────────────────────────────────────────

    #[test]
    fn test_extract_device_id_out_topic() {
        assert_eq!(extract_device_id("/v1/dev/abc123/out"), Some("abc123"));
    }

    #[test]
    fn test_extract_device_id_batch_topic() {
        assert_eq!(extract_device_id("/v1/dev/abc123/batch"), Some("abc123"));
    }

    #[test]
    fn test_extract_device_id_in_topic() {
        assert_eq!(extract_device_id("/v1/dev/abc123/in"), Some("abc123"));
    }

    #[test]
    fn test_extract_device_id_invalid_topics() {
        assert_eq!(extract_device_id("/other/path"), None);
        assert_eq!(extract_device_id(""),            None);
        assert_eq!(extract_device_id("/v1/dev"),     None);
    }

    // ── parse_batch_frame ─────────────────────────────────────────────────────

    #[test]
    fn test_parse_batch_frame_too_short() {
        assert_eq!(parse_batch_frame(b""), None);
        assert_eq!(parse_batch_frame(&[0xCA, 0xA0, 0x00]), None);
        // 10 bytes — correct magic but one byte short of the required 11
        let almost = [0xCA, 0xA0, 0x00, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(parse_batch_frame(&almost), None);
    }

    #[test]
    fn test_parse_batch_frame_wrong_magic() {
        let data = [0u8; 11];
        assert_eq!(parse_batch_frame(&data), None);
        let mut data = [0u8; 11];
        data[0] = 0xCA; // only first magic byte
        assert_eq!(parse_batch_frame(&data), None);
    }

    /// Real V3 data captured from a BB-V1-0 device (from Mysa_HA test suite).
    /// Header: CA A0 03
    /// Timestamp (LE u32): DE 55 79 69
    /// Setpoint  (LE i16): E9 00  → 233 tenths = 23.3 °C
    /// Amb. temp (LE i16): CE 00  → 206 tenths = 20.6 °C
    #[test]
    fn test_parse_batch_frame_real_v3_data() {
        let data = hex::decode(
            "caa003de557969e900ce00cd002c007e00b7000e01c81a1c01f5000002cd000007"
        ).unwrap();
        let (sp, temp) = parse_batch_frame(&data).unwrap();
        assert_eq!(sp,   233, "setpoint should be 233 tenths (23.3°C)");
        assert_eq!(temp, 206, "ambient temp should be 206 tenths (20.6°C)");
    }

    /// Constructed V0 vector matching test_parse_batch_v0 from Mysa_HA:
    /// struct.pack("<LhhhbbhhhHbb", 1769542000, 236, 211, 210, 44, …) + checksum
    /// ambTemp = 211 / 10 = 21.1°C.  setpoint field = 236 / 10 = 23.6°C.
    #[test]
    fn test_parse_batch_frame_v0_constructed() {
        let mut data: Vec<u8> = vec![0xCA, 0xA0, 0x00];       // magic + version 0
        data.extend_from_slice(&1_769_542_000u32.to_le_bytes()); // timestamp
        data.extend_from_slice(&236i16.to_le_bytes());           // h1 setpoint: 23.6°C
        data.extend_from_slice(&211i16.to_le_bytes());           // h2 ambTemp:  21.1°C
        data.extend_from_slice(&210i16.to_le_bytes());           // h3 floor sensor
        data.push(44);                                           // duty cycle
        data.push(0);
        data.extend_from_slice(&10i16.to_le_bytes());
        data.extend_from_slice(&10i16.to_le_bytes());
        data.extend_from_slice(&300i16.to_le_bytes());
        data.extend_from_slice(&5000u16.to_le_bytes());
        data.push(29);
        data.push(0);
        data.push(0x01);                                         // checksum

        let (sp, temp) = parse_batch_frame(&data).unwrap();
        assert_eq!(sp,   236, "setpoint should be 236 tenths (23.6°C)");
        assert_eq!(temp, 211, "ambient temp should be 211 tenths (21.1°C)");
    }
}
