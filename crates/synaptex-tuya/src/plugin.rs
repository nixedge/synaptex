/// `DevicePlugin` implementation for the Tuya local protocol.
///
/// Protocol version is auto-detected on every `connect()`:
/// - v3.5 (0x6699 GCM) and v3.4 (0x55AA HMAC) probes are sent simultaneously.
/// - Whichever the device answers is used; timeout → v3.3 ECB.
use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc,
    },
    time::{Duration, SystemTime},
};

use async_trait::async_trait;
use rand::RngCore;
use serde_json::{json, Value};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::Mutex,
    time::timeout,
};
use tracing::{debug, error, info, warn};

use synaptex_types::{
    capability::{Capability, DeviceCommand},
    device::{DeviceId, DeviceInfo},
    plugin::{DeviceState, PluginError, PluginResult, StateBusSender, StateChangeEvent},
    DevicePlugin,
};

use crate::{
    cipher,
    dp_map::DpMap,
    error::TuyaError,
    protocol::{self, CommandWord, TrailerKind},
};

// ─── Configuration ───────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct TuyaConfig {
    pub ip:        IpAddr,
    /// Usually 6668.
    pub port:      u16,
    /// Tuya cloud device ID — placed in the `"devId"` field of every payload.
    pub tuya_id:   String,
    /// 16-character ASCII string from the Tuya API.
    pub local_key: String,
    pub dp_map:    DpMap,
}

impl TuyaConfig {
    pub fn key_bytes(&self) -> Result<[u8; 16], TuyaError> {
        self.local_key
            .as_bytes()
            .try_into()
            .map_err(|_| TuyaError::Cipher(format!(
                "local_key is {} bytes, expected 16",
                self.local_key.len()
            )))
    }
}

// ─── Plugin ──────────────────────────────────────────────────────────────────

pub struct TuyaPlugin {
    info:              DeviceInfo,
    config:            TuyaConfig,
    bus_tx:            StateBusSender,
    seq_no:            AtomicU32,
    connected:         Arc<AtomicBool>,
    detected_as_v34:   AtomicBool,
    detected_as_v35:   AtomicBool,
    writer:            Arc<Mutex<Option<tokio::net::tcp::OwnedWriteHalf>>>,
    /// Active v3.4 session key (`None` when using v3.3 or v3.5).
    session_key:       Arc<Mutex<Option<[u8; 16]>>>,
    /// Active v3.5 session key (`None` when using v3.3 or v3.4).
    session_key_v35:   Arc<Mutex<Option<[u8; 16]>>>,
}

impl TuyaPlugin {
    pub fn new(info: DeviceInfo, config: TuyaConfig, bus_tx: StateBusSender) -> Self {
        Self {
            info,
            config,
            bus_tx,
            seq_no:            AtomicU32::new(1),
            connected:         Arc::new(AtomicBool::new(false)),
            detected_as_v34:   AtomicBool::new(false),
            detected_as_v35:   AtomicBool::new(false),
            writer:            Arc::new(Mutex::new(None)),
            session_key:       Arc::new(Mutex::new(None)),
            session_key_v35:   Arc::new(Mutex::new(None)),
        }
    }

    fn next_seq(&self) -> u32 {
        self.seq_no.fetch_add(1, Ordering::Relaxed)
    }

    fn epoch_ms() -> u64 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }

    // ── Protocol detection & negotiation ─────────────────────────────────────

    /// Probe the device by sending simultaneous v3.5 and v3.4 handshake
    /// frames, then wait up to 500 ms for a response.
    ///
    /// Returns the detected session key (or `None` for v3.3) plus any
    /// leftover bytes already read past the first frame.
    async fn probe_protocol(
        &self,
        stream: &mut TcpStream,
        key:    &[u8; 16],
    ) -> Result<(ProtocolResult, Vec<u8>), TuyaError> {
        let mut nonce35 = [0u8; 16];
        let mut nonce34 = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut nonce35);
        rand::thread_rng().fill_bytes(&mut nonce34);

        // ── v3.5 probe: 0x6699 GCM frame with nonce35 as payload ─────────────
        {
            let iv35: &[u8; 12] = nonce35[..12].try_into().unwrap();
            let probe35 = protocol::build_frame_v35(
                self.next_seq(),
                CommandWord::SessKeyNegStart,
                &nonce35,
                key,
                iv35,
            )?;
            stream.write_all(&probe35).await?;
        }

        // ── v3.4 probe: 0x55AA CRC32 frame with ECB(key, nonce34) ────────────
        {
            let enc34   = cipher::ecb_encrypt_raw(key, &nonce34)?;
            let probe34 = protocol::build_frame(
                self.next_seq(),
                CommandWord::SessKeyNegStart,
                &enc34,
                TrailerKind::Crc32,
            );
            stream.write_all(&probe34).await?;
        }

        // ── Wait up to 500 ms for any response ────────────────────────────────
        let mut ring = Vec::<u8>::with_capacity(512);
        let mut tmp  = [0u8; 512];

        let probe_result = timeout(Duration::from_millis(500), async {
            loop {
                let n = stream.read(&mut tmp).await?;
                if n == 0 {
                    return Err(TuyaError::Protocol(
                        "connection closed during protocol probe".into(),
                    ));
                }
                ring.extend_from_slice(&tmp[..n]);

                // Peek at the prefix to pick the right parser.
                if ring.len() < 4 { continue; }
                let result = protocol::parse_frame_any(
                    &ring,
                    Some((key, TrailerKind::Crc32)),
                )?;
                if let Some((frame, consumed)) = result {
                    let is_v35 = ring.starts_with(&protocol::PREFIX_6699);
                    ring.drain(..consumed);
                    return Ok((frame, is_v35, nonce34, nonce35));
                }
            }
        })
        .await;

        match probe_result {
            Ok(Ok((frame, is_v35, nonce34, nonce35))) => {
                if is_v35 {
                    let sk = self.finish_v35_negotiation(
                        stream, key, &nonce35, frame,
                    ).await?;
                    Ok((ProtocolResult::V35(sk), ring))
                } else {
                    let sk = self.finish_v34_negotiation(
                        stream, key, &nonce34, frame,
                    ).await?;
                    Ok((ProtocolResult::V34(sk), ring))
                }
            }
            Ok(Err(e)) => {
                debug!("probe error ({e}), falling back to v3.3");
                Ok((ProtocolResult::V33, ring))
            }
            Err(_elapsed) => {
                debug!("probe timed out, falling back to v3.3");
                Ok((ProtocolResult::V33, ring))
            }
        }
    }

    /// Complete v3.4 negotiation after receiving the 0x04 response frame.
    async fn finish_v34_negotiation(
        &self,
        stream:    &mut TcpStream,
        key:       &[u8; 16],
        nonce34:   &[u8; 16],
        resp:      protocol::TuyaFrame,
    ) -> Result<[u8; 16], TuyaError> {
        if resp.command != CommandWord::SessKeyNegResp {
            return Err(TuyaError::Protocol(format!(
                "expected SessKeyNegResp (0x04), got {:?}",
                resp.command
            )));
        }

        // Decrypt response: remote_nonce[16] || HMAC-SHA256(key, local_nonce)[32]
        let decrypted = cipher::ecb_decrypt_raw(key, &resp.payload)?;
        if decrypted.len() < 48 {
            return Err(TuyaError::Protocol(format!(
                "SessKeyNegResp decrypted length {} < 48",
                decrypted.len()
            )));
        }
        let remote_nonce: [u8; 16] = decrypted[0..16].try_into().unwrap();
        let recv_hmac              = &decrypted[16..48];
        let expected_hmac          = cipher::hmac_sha256(key, nonce34);
        if recv_hmac != &expected_hmac[..] {
            return Err(TuyaError::Cipher(
                "v3.4 session key HMAC verification failed".into(),
            ));
        }

        // Derive session key: ECB_encrypt(key, local_nonce XOR remote_nonce)
        let mut xor_nonces = [0u8; 16];
        for i in 0..16 {
            xor_nonces[i] = nonce34[i] ^ remote_nonce[i];
        }
        let sk_bytes = cipher::ecb_encrypt_raw(key, &xor_nonces)?;
        let session_key: [u8; 16] = sk_bytes.try_into().unwrap();

        // Send 0x05: HMAC-SHA256(key, remote_nonce), CRC32 trailer
        let step5_payload = cipher::hmac_sha256(key, &remote_nonce);
        let finish = protocol::build_frame(
            self.next_seq(),
            CommandWord::SessKeyNegFinish,
            &step5_payload,
            TrailerKind::Crc32,
        );
        stream.write_all(&finish).await?;

        Ok(session_key)
    }

    /// Complete v3.5 negotiation after receiving the GCM-decrypted 0x04 frame.
    async fn finish_v35_negotiation(
        &self,
        stream:  &mut TcpStream,
        key:     &[u8; 16],
        nonce35: &[u8; 16],
        resp:    protocol::TuyaFrame,
    ) -> Result<[u8; 16], TuyaError> {
        if resp.command != CommandWord::SessKeyNegResp {
            return Err(TuyaError::Protocol(format!(
                "v3.5 expected SessKeyNegResp (0x04), got {:?}",
                resp.command
            )));
        }

        // payload is already GCM-decrypted; first 16 bytes = remote_nonce
        if resp.payload.len() < 16 {
            return Err(TuyaError::Protocol(format!(
                "v3.5 SessKeyNegResp payload too short: {} bytes",
                resp.payload.len()
            )));
        }
        let remote_nonce: [u8; 16] = resp.payload[..16].try_into().unwrap();

        // XOR nonces
        let mut xor_nonces = [0u8; 16];
        for i in 0..16 {
            xor_nonces[i] = nonce35[i] ^ remote_nonce[i];
        }

        // Derive session key: gcm_encrypt(key, nonce35[..12], &[], xor_nonces)[12..28]
        // (take ciphertext portion = bytes 12–27 of the output)
        let iv35: &[u8; 12] = nonce35[..12].try_into().unwrap();
        let gcm_out     = cipher::gcm_encrypt(key, iv35, &[], &xor_nonces);
        let session_key: [u8; 16] = gcm_out[12..28].try_into().unwrap();

        // Send 0x05: HMAC-SHA256(key, remote_nonce) in a 0x6699 frame
        let step5_payload = cipher::hmac_sha256(key, &remote_nonce);
        let mut rng_iv = [0u8; 12];
        rand::thread_rng().fill_bytes(&mut rng_iv);
        let finish = protocol::build_frame_v35(
            self.next_seq(),
            CommandWord::SessKeyNegFinish,
            &step5_payload,
            key,
            &rng_iv,
        )?;
        stream.write_all(&finish).await?;

        Ok(session_key)
    }

    // ── Data transmission ─────────────────────────────────────────────────────

    async fn send_raw(&self, payload: &[u8], cmd: CommandWord) -> Result<(), TuyaError> {
        let ecb_key = self.config.key_bytes()?;
        let seq     = self.next_seq();

        // Hold the lock to read the session keys and build the frame atomically.
        let frame = {
            let sk_v35_guard = self.session_key_v35.lock().await;
            let sk_v34_guard = self.session_key.lock().await;

            if let Some(sk35) = sk_v35_guard.as_ref() {
                // v3.5: GCM encrypt
                let mut iv = [0u8; 12];
                rand::thread_rng().fill_bytes(&mut iv);
                drop(sk_v34_guard);
                protocol::build_frame_v35(seq, cmd, payload, sk35, &iv)?
            } else if let Some(sk34) = sk_v34_guard.as_ref() {
                // v3.4: CBC encrypt + HMAC trailer
                let encrypted = cipher::cbc_encrypt(sk34, &[0u8; 16], payload);
                drop(sk_v35_guard);
                protocol::build_frame(seq, cmd, &encrypted, TrailerKind::Hmac(sk34))
            } else {
                // v3.3: ECB encrypt + CRC32 trailer
                drop(sk_v35_guard);
                let ct = cipher::encrypt(&ecb_key, payload);
                let ct = if protocol::v33_needs_prefix(cmd) {
                    protocol::v33_prepend_version(&ct)
                } else {
                    ct
                };
                protocol::build_frame(seq, cmd, &ct, TrailerKind::Crc32)
            }
        };

        debug!(
            device = %self.info.id,
            seq,
            cmd = ?cmd,
            payload_len = payload.len(),
            "→ tx"
        );

        let mut guard = self.writer.lock().await;
        if let Some(w) = guard.as_mut() {
            w.write_all(&frame).await?;
        } else {
            return Err(TuyaError::Offline);
        }
        Ok(())
    }

    fn command_to_dps(&self, cmd: &DeviceCommand) -> Option<Value> {
        let dm = &self.config.dp_map;
        match cmd {
            DeviceCommand::SetPower(on) => Some(json!({
                dm.power_dp.to_string(): on
            })),
            DeviceCommand::SetBrightness(v) => {
                let (dp, val) = dm.brightness_dp_value(*v);
                Some(json!({ dp.to_string(): val }))
            }
            DeviceCommand::SetColorTemp(k) => {
                let (dp, val) = dm.color_temp_dp_value(*k);
                Some(json!({ dp.to_string(): val }))
            }
            DeviceCommand::SetRgb(r, g, b) => {
                dm.rgb_dps(*r, *g, *b)
            }
            DeviceCommand::SetSwitch { index, state } => Some(json!({
                index.to_string(): state
            })),
            DeviceCommand::SetDpBool { dp, value } => Some(json!({
                dp.to_string(): value
            })),
            DeviceCommand::SetDpInt { dp, value } => Some(json!({
                dp.to_string(): value
            })),
            DeviceCommand::SetDpStr { dp, value } => Some(json!({
                dp.to_string(): value
            })),
            DeviceCommand::SendIr { head, key } => dm.ir_dps(head.as_deref(), key),
        }
    }
}

// ─── Protocol detection result ────────────────────────────────────────────────

enum ProtocolResult {
    V33,
    V34([u8; 16]),
    V35([u8; 16]),
}

// ─── DevicePlugin impl ────────────────────────────────────────────────────────

#[async_trait]
impl DevicePlugin for TuyaPlugin {
    fn device_id(&self)    -> &DeviceId     { &self.info.id }
    fn name(&self)         -> &str          { &self.info.name }
    fn capabilities(&self) -> &[Capability] { &self.info.capabilities }

    fn protocol(&self) -> &str {
        if self.detected_as_v35.load(Ordering::Acquire) {
            "tuya_local_3.5"
        } else if self.detected_as_v34.load(Ordering::Acquire) {
            "tuya_local_3.4"
        } else {
            "tuya_local_3.3"
        }
    }

    async fn connect(&self) -> PluginResult<()> {
        let addr = format!("{}:{}", self.config.ip, self.config.port);
        debug!(device = %self.info.id, %addr, "connecting");

        let mut stream = TcpStream::connect(&addr)
            .await
            .map_err(|e| PluginError::Unreachable(e.to_string()))?;
        stream.set_nodelay(true)?;

        let key = self.config.key_bytes().map_err(PluginError::from)?;

        // Reset state from any previous connection.
        self.detected_as_v34.store(false, Ordering::Release);
        self.detected_as_v35.store(false, Ordering::Release);
        *self.session_key.lock().await     = None;
        *self.session_key_v35.lock().await = None;

        // ── Auto-detect protocol ──────────────────────────────────────────────
        let (proto_result, leftover) = self
            .probe_protocol(&mut stream, &key)
            .await
            .map_err(PluginError::from)?;

        // Session key values captured for the reader task (by value, not Arc).
        let session_key_v34_opt: Option<[u8; 16]>;
        let session_key_v35_opt: Option<[u8; 16]>;
        let is_v35: bool;

        match proto_result {
            ProtocolResult::V35(sk) => {
                info!(device = %self.info.id, "detected v3.5");
                *self.session_key_v35.lock().await = Some(sk);
                self.detected_as_v35.store(true, Ordering::Release);
                session_key_v34_opt = None;
                session_key_v35_opt = Some(sk);
                is_v35 = true;
            }
            ProtocolResult::V34(sk) => {
                info!(device = %self.info.id, "detected v3.4");
                *self.session_key.lock().await = Some(sk);
                self.detected_as_v34.store(true, Ordering::Release);
                session_key_v34_opt = Some(sk);
                session_key_v35_opt = None;
                is_v35 = false;
            }
            ProtocolResult::V33 => {
                debug!(device = %self.info.id, "using v3.3");
                session_key_v34_opt = None;
                session_key_v35_opt = None;
                is_v35 = false;
            }
        }

        let (mut reader, writer) = stream.into_split();
        *self.writer.lock().await = Some(writer);
        self.connected.store(true, Ordering::Release);

        // ── Spawn reader task ─────────────────────────────────────────────────
        let plugin_id     = self.info.id;
        let bus_tx        = self.bus_tx.clone();
        let writer_ref    = self.writer.clone();
        let connected_ref = self.connected.clone();
        let dp_map        = self.config.dp_map.clone();
        let ecb_key       = key; // Copy — used for v3.3 decryption

        tokio::spawn(async move {
            let mut buf:  Vec<u8> = vec![0u8; 4096];
            let mut ring: Vec<u8> = leftover; // start with any bytes read during probe

            loop {
                match reader.read(&mut buf).await {
                    Ok(0) => {
                        warn!(device = %plugin_id, "connection closed by device");
                        *writer_ref.lock().await = None;
                        connected_ref.store(false, Ordering::Release);
                        break;
                    }
                    Ok(n) => {
                        ring.extend_from_slice(&buf[..n]);
                        loop {
                            let parse_result = if is_v35 {
                                if let Some(sk) = &session_key_v35_opt {
                                    protocol::parse_frame_any(&ring, Some((sk, TrailerKind::Crc32)))
                                } else {
                                    break;
                                }
                            } else if let Some(sk) = &session_key_v34_opt {
                                protocol::parse_frame_any(&ring, Some((sk, TrailerKind::Hmac(sk))))
                            } else {
                                protocol::parse_frame_any(&ring, None)
                            };

                            match parse_result {
                                Ok(Some((frame, consumed))) => {
                                    ring.drain(..consumed);

                                    debug!(
                                        device = %plugin_id,
                                        seq = frame.seq_no,
                                        cmd = ?frame.command,
                                        payload_len = frame.payload.len(),
                                        "← rx"
                                    );

                                    // v3.5 frames: payload is already GCM-decrypted JSON.
                                    // v3.3/v3.4 frames: payload = retcode(4) [+ version header] + ciphertext.
                                    let plain: Vec<u8> = if is_v35 {
                                        if frame.payload.is_empty() { continue; }
                                        frame.payload.to_vec()
                                    } else {
                                        if frame.payload.len() <= 4 { continue; }
                                        let rc = u32::from_be_bytes(
                                            frame.payload[0..4].try_into().unwrap()
                                        );
                                        if rc != 0 {
                                            debug!(device = %plugin_id, return_code = rc, "non-zero return code");
                                        }
                                        let after_rc = &frame.payload[4..];

                                        // Strip optional v3.3 version header.
                                        let enc = if after_rc.len() >= 15
                                            && after_rc.starts_with(b"3.3")
                                        {
                                            &after_rc[15..]
                                        } else {
                                            after_rc
                                        };

                                        if enc.is_empty() { continue; }
                                        if enc.len() % 16 != 0 {
                                            debug!(device = %plugin_id, len = enc.len(), "ciphertext not block-aligned, skipping");
                                            continue;
                                        }

                                        match session_key_v34_opt {
                                            Some(sk) => match cipher::cbc_decrypt(&sk, &[0u8; 16], enc) {
                                                Ok(p)  => p,
                                                Err(e) => {
                                                    debug!(device = %plugin_id, "cbc decrypt failed ({e})");
                                                    enc.to_vec()
                                                }
                                            },
                                            None => match cipher::decrypt(&ecb_key, enc) {
                                                Ok(p)  => p,
                                                Err(e) => {
                                                    debug!(device = %plugin_id, "ecb decrypt failed ({e})");
                                                    enc.to_vec()
                                                }
                                            },
                                        }
                                    };

                                    match serde_json::from_slice::<Value>(&plain) {
                                        Err(_) => {
                                            debug!(device = %plugin_id, cmd = ?frame.command, "← non-JSON payload");
                                        }
                                        Ok(val) => {
                                            match val.get("dps").and_then(Value::as_object) {
                                                None => {
                                                    debug!(device = %plugin_id, payload = %val, "← JSON without dps");
                                                }
                                                Some(dps_obj) => {
                                                    let dps: HashMap<String, Value> =
                                                        dps_obj.clone().into_iter().collect();

                                                    debug!(device = %plugin_id, dps = %val["dps"], "← dps update");

                                                    let mut state = DeviceState {
                                                        device_id:     plugin_id,
                                                        online:        true,
                                                        updated_at_ms: SystemTime::now()
                                                            .duration_since(SystemTime::UNIX_EPOCH)
                                                            .unwrap_or_default()
                                                            .as_millis() as u64,
                                                        power:        None,
                                                        brightness:   None,
                                                        color_temp_k: None,
                                                        rgb:          None,
                                                        switches:     HashMap::new(),
                                                    };
                                                    dp_map.apply_dps(&dps, &mut state);
                                                    let _ = bus_tx.send(StateChangeEvent {
                                                        device_id: plugin_id,
                                                        state,
                                                    });
                                                }
                                            }
                                        }
                                    }
                                }
                                Ok(None) => break,
                                Err(e) => {
                                    error!(device = %plugin_id, "frame parse error: {e}");
                                    ring.clear();
                                    break;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!(device = %plugin_id, "read error: {e}");
                        *writer_ref.lock().await = None;
                        connected_ref.store(false, Ordering::Release);
                        break;
                    }
                }
            }
        });

        debug!(device = %self.info.id, "connected");
        Ok(())
    }

    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }

    async fn disconnect(&self) {
        self.connected.store(false, Ordering::Release);
        *self.writer.lock().await        = None;
        *self.session_key.lock().await   = None;
        *self.session_key_v35.lock().await = None;
        self.detected_as_v34.store(false, Ordering::Release);
        self.detected_as_v35.store(false, Ordering::Release);
        info!(device = %self.info.id, "disconnected");
    }

    async fn poll_state(&self) -> PluginResult<DeviceState> {
        let mut rx = self.bus_tx.subscribe();

        let t = (Self::epoch_ms() / 1000).to_string();
        let payload = json!({
            "gwId":  self.config.tuya_id,
            "devId": self.config.tuya_id,
            "uid":   self.config.tuya_id,
            "t":     t,
        })
        .to_string();

        // v3.4+ devices prefer DpQueryNew (0x0D) for status queries.
        let cmd = if self.detected_as_v34.load(Ordering::Acquire)
            || self.detected_as_v35.load(Ordering::Acquire)
        {
            CommandWord::DpQueryNew
        } else {
            CommandWord::DpQuery
        };

        self.send_raw(payload.as_bytes(), cmd)
            .await
            .map_err(PluginError::from)?;

        let id    = self.info.id;
        let state = timeout(Duration::from_secs(2), async move {
            loop {
                match rx.recv().await {
                    Ok(ev) if ev.device_id == id => return ev.state,
                    Ok(_)                        => continue,
                    Err(_)                       => break,
                }
            }
            offline_state(id)
        })
        .await
        .unwrap_or_else(|_| offline_state(id));

        Ok(state)
    }

    async fn execute_command(&self, cmd: DeviceCommand) -> PluginResult<()> {
        let dps = self
            .command_to_dps(&cmd)
            .ok_or(PluginError::UnsupportedCommand)?;

        let t = (Self::epoch_ms() / 1000).to_string();
        let payload = json!({
            "devId": self.config.tuya_id,
            "uid":   self.config.tuya_id,
            "t":     t,
            "dps":   dps,
        })
        .to_string();

        info!(device = %self.info.id, cmd = ?cmd, dps = %dps, "→ control");

        self.send_raw(payload.as_bytes(), CommandWord::Control)
            .await
            .map_err(PluginError::from)
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn offline_state(id: DeviceId) -> DeviceState {
    DeviceState {
        device_id: id, online: false,
        updated_at_ms: SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64,
        power: None, brightness: None, color_temp_k: None,
        rgb: None, switches: HashMap::new(),
    }
}
