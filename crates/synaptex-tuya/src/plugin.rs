/// `DevicePlugin` implementation for the Tuya local protocol.
///
/// Protocol version (v3.3 ECB vs v3.4 CBC with session-key negotiation) is
/// detected automatically on every [`connect`] attempt by probing the 0x03
/// session-key handshake with a short timeout.  This means the plugin handles
/// firmware upgrades transparently without any manual reconfiguration.
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
use tracing::{debug, info, warn, error};

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
    protocol::{self, CommandWord},
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
    /// Its raw bytes are the AES-128 key.
    pub local_key: String,
    pub dp_map:    DpMap,
}

impl TuyaConfig {
    /// Extract the 16 raw bytes used as the AES key.
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
    info:             DeviceInfo,
    config:           TuyaConfig,
    bus_tx:           StateBusSender,
    seq_no:           AtomicU32,
    /// Flipped to `false` by the reader task on connection drop.
    connected:        Arc<AtomicBool>,
    /// Set to `true` after a successful v3.4 negotiation; `false` for v3.3.
    /// Used only for the `protocol()` display string.
    detected_as_v34:  AtomicBool,
    /// Write half of the active TCP connection, or `None` when disconnected.
    writer:           Arc<Mutex<Option<tokio::net::tcp::OwnedWriteHalf>>>,
    /// Active session key for v3.4 (`None` when using v3.3 / not connected).
    session_key:      Arc<Mutex<Option<[u8; 16]>>>,
}

impl TuyaPlugin {
    pub fn new(info: DeviceInfo, config: TuyaConfig, bus_tx: StateBusSender) -> Self {
        Self {
            info,
            config,
            bus_tx,
            seq_no:           AtomicU32::new(1),
            connected:        Arc::new(AtomicBool::new(false)),
            detected_as_v34:  AtomicBool::new(false),
            writer:           Arc::new(Mutex::new(None)),
            session_key:      Arc::new(Mutex::new(None)),
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

    /// Attempt the v3.4 session-key exchange on a fresh, unsplit `TcpStream`.
    ///
    /// Returns the derived 16-byte session key on success.
    async fn negotiate_session_key_v34(
        &self,
        stream: &mut TcpStream,
        key: &[u8; 16],
    ) -> Result<[u8; 16], TuyaError> {
        // ── Step 1: generate local nonce ──────────────────────────────────────
        let mut local_nonce = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut local_nonce);

        // ── Step 2: send 0x03 SessKeyNegStart ────────────────────────────────
        let enc_nonce   = cipher::ecb_encrypt_raw(key, &local_nonce)?;
        let start_frame = protocol::build_frame(
            self.next_seq(),
            CommandWord::SessKeyNegStart,
            &enc_nonce,
        );
        stream.write_all(&start_frame).await?;

        // ── Step 3: receive 0x04 SessKeyNegResp ──────────────────────────────
        let mut ring = Vec::<u8>::with_capacity(256);
        let mut buf  = [0u8; 256];
        let resp = loop {
            let n = stream.read(&mut buf).await?;
            if n == 0 {
                return Err(TuyaError::Protocol(
                    "connection closed during session key negotiation".into(),
                ));
            }
            ring.extend_from_slice(&buf[..n]);
            match protocol::parse_frame(&ring)? {
                Some((frame, consumed)) => {
                    ring.drain(..consumed);
                    break frame;
                }
                None => continue,
            }
        };

        if resp.command != CommandWord::SessKeyNegResp {
            return Err(TuyaError::Protocol(format!(
                "expected SessKeyNegResp (0x04), got {:?}",
                resp.command
            )));
        }

        // ── Step 4: decrypt response, verify HMAC ────────────────────────────
        // Plaintext: remote_nonce[0..16] || HMAC-SHA256(local_key, local_nonce)[16..48]
        let decrypted = cipher::ecb_decrypt_raw(key, &resp.payload)?;
        if decrypted.len() < 48 {
            return Err(TuyaError::Protocol(format!(
                "SessKeyNegResp decrypted length {} < 48",
                decrypted.len()
            )));
        }
        let remote_nonce: [u8; 16] = decrypted[0..16].try_into().unwrap();
        let recv_hmac              = &decrypted[16..48];
        let expected_hmac          = hmac_sha256(key, &local_nonce);
        if &expected_hmac[..] != recv_hmac {
            return Err(TuyaError::Cipher(
                "session key negotiation HMAC verification failed".into(),
            ));
        }

        // ── Step 5: derive session key ────────────────────────────────────────
        let sk_full                = hmac_sha256(key, &remote_nonce);
        let session_key: [u8; 16]  = sk_full[0..16].try_into().unwrap();

        // ── Step 6: send 0x05 SessKeyNegFinish ───────────────────────────────
        let enc_sk       = cipher::ecb_encrypt_raw(key, &session_key)?;
        let finish_frame = protocol::build_frame(
            self.next_seq(),
            CommandWord::SessKeyNegFinish,
            &enc_sk,
        );
        stream.write_all(&finish_frame).await?;

        Ok(session_key)
    }

    async fn send_raw(&self, payload: &[u8], cmd: CommandWord) -> Result<(), TuyaError> {
        let key = self.config.key_bytes()?;
        let seq = self.next_seq();

        // Use v3.4 CBC if a session key is present, otherwise v3.3 ECB.
        //
        // For v3.3:
        //   - Encrypt the JSON payload first (no prefix inside the AES block).
        //   - Control (0x07) frames then get a 15-byte version header prepended
        //     to the ciphertext: b"3.3" + 12×\x00 + ciphertext.
        //   - All other commands (DpQuery, Heartbeat, …) send raw ciphertext only.
        let encrypted = {
            let sk_guard = self.session_key.lock().await;
            match sk_guard.as_ref() {
                Some(sk) => cipher::cbc_encrypt(sk, &[0u8; 16], payload),
                None => {
                    let ct = cipher::encrypt(&key, payload);
                    if protocol::v33_needs_prefix(cmd) {
                        protocol::v33_prepend_version(&ct)
                    } else {
                        ct
                    }
                }
            }
        };

        let frame = protocol::build_frame(seq, cmd, &encrypted);

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
        let dp_map = &self.config.dp_map;
        match cmd {
            DeviceCommand::SetPower(on) => Some(json!({
                dp_map.power_dp.to_string(): on
            })),
            DeviceCommand::SetBrightness(v) => {
                let (dp, val) = dp_map.brightness_dp_value(*v);
                Some(json!({ dp.to_string(): val }))
            }
            DeviceCommand::SetColorTemp(k) => {
                let (dp, val) = dp_map.color_temp_dp_value(*k);
                Some(json!({ dp.to_string(): val }))
            }
            DeviceCommand::SetRgb(_, _, _) => None,
            DeviceCommand::SetSwitch { index, state } => Some(json!({
                index.to_string(): state
            })),
        }
    }
}

// ─── DevicePlugin impl ───────────────────────────────────────────────────────

#[async_trait]
impl DevicePlugin for TuyaPlugin {
    fn device_id(&self)    -> &DeviceId     { &self.info.id }
    fn name(&self)         -> &str          { &self.info.name }
    fn capabilities(&self) -> &[Capability] { &self.info.capabilities }

    fn protocol(&self) -> &str {
        if self.detected_as_v34.load(Ordering::Acquire) {
            "tuya_local_3.4"
        } else {
            "tuya_local_3.3"
        }
    }

    async fn connect(&self) -> PluginResult<()> {
        let addr = format!("{}:{}", self.config.ip, self.config.port);
        info!(device = %self.info.id, %addr, "connecting");

        let mut stream = TcpStream::connect(&addr)
            .await
            .map_err(|e| PluginError::Unreachable(e.to_string()))?;
        stream.set_nodelay(true)?;

        let key = self.config.key_bytes().map_err(PluginError::from)?;

        // ── Auto-detect protocol version ──────────────────────────────────────
        // Send the v3.4 session-key negotiation and wait up to 500 ms.
        // v3.4 devices respond with 0x04; v3.3 devices ignore 0x03 (or send
        // an error), causing the timeout to fire.  Either way we continue on
        // the same TCP stream — there is no reconnect needed.
        let session_key_opt: Option<[u8; 16]> = match timeout(
            Duration::from_millis(500),
            self.negotiate_session_key_v34(&mut stream, &key),
        )
        .await
        {
            Ok(Ok(sk)) => {
                info!(device = %self.info.id, "detected v3.4");
                *self.session_key.lock().await = Some(sk);
                self.detected_as_v34.store(true, Ordering::Release);
                Some(sk)
            }
            Ok(Err(e)) => {
                debug!(device = %self.info.id, "v3.4 probe rejected ({e}), using v3.3");
                *self.session_key.lock().await = None;
                self.detected_as_v34.store(false, Ordering::Release);
                None
            }
            Err(_elapsed) => {
                debug!(device = %self.info.id, "v3.4 probe timed out, using v3.3");
                *self.session_key.lock().await = None;
                self.detected_as_v34.store(false, Ordering::Release);
                None
            }
        };

        let (mut reader, writer) = stream.into_split();
        *self.writer.lock().await = Some(writer);
        self.connected.store(true, Ordering::Release);

        // ── Spawn reader task ─────────────────────────────────────────────────
        let plugin_id     = self.info.id;
        let bus_tx        = self.bus_tx.clone();
        let writer_ref    = self.writer.clone();
        let connected_ref = self.connected.clone();
        let dp_map        = self.config.dp_map.clone();
        // key and session_key_opt are [u8;16] / Option<[u8;16]> — Copy.

        tokio::spawn(async move {
            let mut buf  = vec![0u8; 4096];
            let mut ring: Vec<u8> = Vec::new();

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
                            match protocol::parse_frame(&ring) {
                                Ok(Some((frame, consumed))) => {
                                    ring.drain(..consumed);

                                    debug!(
                                        device = %plugin_id,
                                        seq = frame.seq_no,
                                        cmd = ?frame.command,
                                        payload_len = frame.payload.len(),
                                        "← rx"
                                    );

                                    // All v3.3 device→client frames start with a
                                    // 4-byte return code (tinytuya always strips it).
                                    // If the frame is ≤4 bytes it's a retcode-only ACK
                                    // with no payload to decrypt (e.g. Control ACK).
                                    if frame.payload.len() <= 4 { continue; }
                                    let rc = u32::from_be_bytes(
                                        frame.payload[0..4].try_into().unwrap()
                                    );
                                    if rc != 0 {
                                        debug!(
                                            device = %plugin_id,
                                            return_code = rc,
                                            cmd = ?frame.command,
                                            "non-zero return code"
                                        );
                                    }
                                    let after_rc = &frame.payload[4..];

                                    // Some frames (e.g. StatusPush) also carry a
                                    // 15-byte version header ("3.3" + 12 bytes) between
                                    // the return code and the ciphertext.
                                    let enc: &[u8] = if after_rc.len() >= 15
                                        && after_rc.starts_with(b"3.3")
                                    {
                                        debug!(device = %plugin_id, "stripping v3.3 version header");
                                        &after_rc[15..]
                                    } else {
                                        after_rc
                                    };

                                    if enc.is_empty() { continue; }
                                    if enc.len() % 16 != 0 {
                                        debug!(
                                            device = %plugin_id,
                                            len = enc.len(),
                                            cmd = ?frame.command,
                                            "ciphertext not block-aligned, skipping"
                                        );
                                        continue;
                                    }

                                    let plain = match session_key_opt {
                                        Some(sk) => {
                                            match cipher::cbc_decrypt(&sk, &[0u8; 16], enc) {
                                                Ok(p)  => p,
                                                Err(e) => {
                                                    debug!(device = %plugin_id, "cbc decrypt failed ({e}), trying raw");
                                                    enc.to_vec()
                                                }
                                            }
                                        }
                                        None => {
                                            match cipher::decrypt(&key, enc) {
                                                Ok(p)  => p,
                                                Err(e) => {
                                                    debug!(device = %plugin_id, "ecb decrypt failed ({e}), trying raw");
                                                    enc.to_vec()
                                                }
                                            }
                                        }
                                    };

                                    match serde_json::from_slice::<Value>(&plain) {
                                        Err(_) => {
                                            debug!(
                                                device = %plugin_id,
                                                cmd = ?frame.command,
                                                "← non-JSON payload (heartbeat?)"
                                            );
                                        }
                                        Ok(val) => {
                                            match val.get("dps").and_then(Value::as_object) {
                                                None => {
                                                    debug!(
                                                        device = %plugin_id,
                                                        payload = %val,
                                                        "← JSON without dps"
                                                    );
                                                }
                                                Some(dps_obj) => {
                                                    let dps: HashMap<String, Value> =
                                                        dps_obj.clone().into_iter().collect();

                                                    info!(
                                                        device = %plugin_id,
                                                        dps = %val["dps"],
                                                        "← dps update"
                                                    );

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

        info!(device = %self.info.id, "connected");
        Ok(())
    }

    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }

    async fn disconnect(&self) {
        self.connected.store(false, Ordering::Release);
        *self.writer.lock().await = None;
        *self.session_key.lock().await = None;
        info!(device = %self.info.id, "disconnected");
    }

    async fn poll_state(&self) -> PluginResult<DeviceState> {
        // Subscribe to the bus BEFORE sending the query so the response
        // can't slip past us between send and recv.
        let mut rx = self.bus_tx.subscribe();

        let t = (Self::epoch_ms() / 1000).to_string();
        let payload = json!({
            "gwId":  self.config.tuya_id,
            "devId": self.config.tuya_id,
            "uid":   self.config.tuya_id,
            "t":     t,
        })
        .to_string();
        self.send_raw(payload.as_bytes(), CommandWord::DpQuery)
            .await
            .map_err(PluginError::from)?;

        // Wait up to 2 s for the reader task to push a state event for this device.
        let id = self.info.id;
        let state = timeout(Duration::from_secs(2), async move {
            loop {
                match rx.recv().await {
                    Ok(ev) if ev.device_id == id => return ev.state,
                    Ok(_)                        => continue, // different device
                    Err(_)                       => break,    // bus closed
                }
            }
            DeviceState {
                device_id: id, online: false,
                updated_at_ms: SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap_or_default().as_millis() as u64,
                power: None, brightness: None, color_temp_k: None,
                rgb: None, switches: HashMap::new(),
            }
        })
        .await
        .unwrap_or_else(|_| DeviceState {
            device_id: id, online: false,
            updated_at_ms: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default().as_millis() as u64,
            power: None, brightness: None, color_temp_k: None,
            rgb: None, switches: HashMap::new(),
        });

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

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let mut mac = Hmac::<Sha256>::new_from_slice(key)
        .expect("HMAC accepts keys of any length");
    mac.update(data);
    let result = mac.finalize().into_bytes();
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&result);
    arr
}
