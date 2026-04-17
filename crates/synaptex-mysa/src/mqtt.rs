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
    types::{json_temp_to_tenths, mqtt_temp_to_tenths, MqttOutMsg, MysaDeviceState},
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

fn dispatch_batch(device_id: Option<&str>, payload: &[u8], account: &Arc<MysaAccount>) {
    let id = match device_id {
        Some(d) => d,
        None    => return,
    };
    // Binary telemetry: 0xCA 0xA0 + temp(2) + setpoint(2) + duty(2)
    if payload.len() < 8 || payload[0] != 0xCA || payload[1] != 0xA0 {
        return;
    }
    let temp_raw = u16::from_be_bytes([payload[2], payload[3]]);
    let sp_raw   = u16::from_be_bytes([payload[4], payload[5]]);

    let temp = mqtt_temp_to_tenths(temp_raw);
    let sp   = mqtt_temp_to_tenths(sp_raw);

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
