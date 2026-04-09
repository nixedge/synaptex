/// `DevicePlugin` implementation for the Tuya local protocol (on-demand connections).
///
/// Every `poll_state()` and `execute_command()` opens a fresh TCP connection,
/// negotiates the session key, does its work, and drops the socket.
/// A periodic supervisor in the registry polls every ~60 s to keep the state cache fresh.
use std::{
    collections::HashMap,
    net::IpAddr,
    sync::atomic::{AtomicU32, AtomicU8, Ordering},
    time::{Duration, SystemTime},
};

use async_trait::async_trait;
use rand::RngCore;
use serde_json::{json, Value};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::timeout,
};
use tracing::{debug, info};

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
    pub ip:            IpAddr,
    /// Usually 6668.
    pub port:          u16,
    /// Tuya cloud device ID — placed in the `"devId"` field of every payload.
    pub tuya_id:       String,
    /// 16-character ASCII string from the Tuya API.
    pub local_key:     String,
    pub dp_map:        DpMap,
    /// Protocol version hint ("3.3" | "3.4" | "3.5").
    /// When set, skips the dual-probe and connects directly with this version.
    pub protocol_version: Option<String>,
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

// ─── Protocol detection result ────────────────────────────────────────────────

enum ProtocolResult {
    V33,
    V34([u8; 16]),
    V35([u8; 16]),
}

// ─── Ephemeral connection ─────────────────────────────────────────────────────

/// A single open TCP session — created by `open_connection()`, used for one
/// query or command, then dropped (closing the socket).
struct Connection {
    stream:   TcpStream,
    proto:    ProtocolResult,
    ecb_key:  [u8; 16],
    leftover: Vec<u8>,
    seq_no:   u32,
}

impl Connection {
    /// Returns the DP-set command word.
    ///
    /// v3.4 and v3.5 devices use `ControlNew` (0x0D); v3.3 uses `Control` (0x07).
    /// tinytuya applies a `command_override: 13` for these protocol versions.
    fn control_cmd(&self) -> CommandWord {
        match &self.proto {
            ProtocolResult::V34(_) | ProtocolResult::V35(_) => CommandWord::ControlNew,
            ProtocolResult::V33                              => CommandWord::Control,
        }
    }

    /// Build and write one command frame using the negotiated protocol.
    async fn send(&mut self, cmd: CommandWord, payload: &[u8]) -> Result<(), TuyaError> {
        let seq = self.seq_no;
        self.seq_no += 1;

        let frame = {
            // Borrow proto / ecb_key in a block so they're released before write_all.
            match &self.proto {
                ProtocolResult::V35(sk) => {
                    let sk = *sk;
                    let mut iv = [0u8; 12];
                    rand::thread_rng().fill_bytes(&mut iv);
                    // Data commands (Control, ControlNew) require the 15-byte
                    // version prefix before GCM encryption.  Query/heartbeat/
                    // session-key commands do not (tinytuya NO_PROTOCOL_HEADER_CMDS).
                    let prefixed: Vec<u8> = if matches!(cmd, CommandWord::Control | CommandWord::ControlNew) {
                        let mut p = protocol::V35_DATA_PREFIX.to_vec();
                        p.extend_from_slice(payload);
                        p
                    } else {
                        payload.to_vec()
                    };
                    protocol::build_frame_v35(seq, cmd, &prefixed, &sk, &iv)?
                }
                ProtocolResult::V34(sk) => {
                    let sk = *sk;
                    // ControlNew (0x0D) data frames get the 15-byte version prefix
                    // before ECB encryption (tinytuya NO_PROTOCOL_HEADER_CMDS).
                    let plaintext: Vec<u8> = if cmd == CommandWord::ControlNew {
                        let mut p = protocol::V34_DATA_PREFIX.to_vec();
                        p.extend_from_slice(payload);
                        p
                    } else {
                        payload.to_vec()
                    };
                    let encrypted = cipher::encrypt(&sk, &plaintext);
                    protocol::build_frame(seq, cmd, &encrypted, TrailerKind::Hmac(&sk))
                }
                ProtocolResult::V33 => {
                    let ct = cipher::encrypt(&self.ecb_key, payload);
                    let ct = if protocol::v33_needs_prefix(cmd) {
                        protocol::v33_prepend_version(&ct)
                    } else {
                        ct
                    };
                    protocol::build_frame(seq, cmd, &ct, TrailerKind::Crc32)
                }
            }
        };

        debug!(cmd = ?cmd, seq, payload_len = payload.len(), "→ tx");
        self.stream.write_all(&frame).await?;
        Ok(())
    }

    /// Read frames from the socket (processing any leftover bytes first) until
    /// one contains a `"dps"` object.  The caller is responsible for wrapping
    /// this in a `timeout`.
    async fn recv_state(
        &mut self,
        dp_map:    &DpMap,
        device_id: DeviceId,
    ) -> Result<(DeviceState, HashMap<String, Value>), TuyaError> {
        let mut buf = [0u8; 4096];
        let is_v35  = matches!(self.proto, ProtocolResult::V35(_));

        loop {
            // ── Process whatever bytes are already buffered ────────────────────
            loop {
                let result = {
                    match &self.proto {
                        ProtocolResult::V35(sk) =>
                            protocol::parse_frame_any(&self.leftover, Some((sk, TrailerKind::Crc32))),
                        ProtocolResult::V34(sk) =>
                            protocol::parse_frame_any(&self.leftover, Some((sk, TrailerKind::Hmac(sk)))),
                        ProtocolResult::V33 =>
                            protocol::parse_frame_any(&self.leftover, None),
                    }
                }?; // borrows of self.proto / self.leftover end here

                let (frame, consumed) = match result {
                    None => break, // need more data from socket
                    Some(pair) => pair,
                };
                self.leftover.drain(..consumed);

                debug!(
                    device = %device_id,
                    seq    = frame.seq_no,
                    cmd    = ?frame.command,
                    "← rx"
                );

                // ── Strip and check return code ───────────────────────────────
                if frame.payload.len() < 4 { continue; }
                let rc = u32::from_be_bytes(frame.payload[0..4].try_into().unwrap());
                if rc != 0 {
                    return Err(TuyaError::Protocol(format!(
                        "device returned error code {rc} (0x{rc:08X})"
                    )));
                }
                let after_rc = &frame.payload[4..];

                // ── Decrypt payload ───────────────────────────────────────────
                let plain: Vec<u8> = if is_v35 {
                    // GCM payload already decrypted by parse_frame_any.
                    if after_rc.is_empty() { continue; }
                    // Echo-back from ControlNew includes a "3.5\0...\0" prefix
                    // (tinytuya's version header) before the JSON.  Find the first
                    // '{' or '[' and discard everything before it.
                    let json_start = after_rc
                        .iter()
                        .position(|&b| b == b'{' || b == b'[')
                        .unwrap_or(0);
                    after_rc[json_start..].to_vec()
                } else {
                    let enc = if after_rc.len() >= 15
                        && (after_rc.starts_with(b"3.3") || after_rc.starts_with(b"3.4"))
                    {
                        &after_rc[15..]
                    } else {
                        after_rc
                    };
                    if enc.is_empty() { continue; }
                    if enc.len() % 16 != 0 {
                        debug!(device = %device_id, len = enc.len(), "ciphertext not block-aligned, skipping");
                        continue;
                    }
                    match &self.proto {
                        ProtocolResult::V34(sk) => {
                            let sk = *sk;
                            match cipher::decrypt(&sk, enc) {
                                Ok(p)  => p,
                                Err(e) => { debug!(device = %device_id, "decrypt failed ({e})"); continue; }
                            }
                        }
                        _ => match cipher::decrypt(&self.ecb_key, enc) {
                            Ok(p)  => p,
                            Err(e) => { debug!(device = %device_id, "decrypt failed ({e})"); continue; }
                        },
                    }
                };

                // ── Parse DPS ────────────────────────────────────────────────
                let Ok(val) = serde_json::from_slice::<Value>(&plain) else {
                    debug!(device = %device_id, "non-JSON payload");
                    continue;
                };
                // Accept both the legacy flat format {"dps":{...}} and the
                // protocol-4/5 envelope {"protocol":N,"data":{"dps":{...}}}
                // used by v3.4/v3.5 echo-backs.
                let dps_obj = val.get("dps").and_then(Value::as_object)
                    .or_else(|| val.pointer("/data/dps").and_then(Value::as_object));
                let Some(dps_obj) = dps_obj else {
                    debug!(device = %device_id, payload = %val, "JSON without dps");
                    continue;
                };
                let dps: HashMap<String, Value> = dps_obj.clone().into_iter().collect();

                debug!(device = %device_id, dps = %val["dps"], "← dps update");

                let mut state = DeviceState {
                    device_id,
                    online:        true,
                    updated_at_ms: SystemTime::now()
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64,
                    power:        None,
                    brightness:   None,
                    color_temp_k: None,
                    rgb:          None,
                    switches:          HashMap::new(),
                    fan_speed:         None,
                    temp_current:      None,
                    temp_set:          None,
                    temp_calibration:  None,
                };
                dp_map.apply_dps(&dps, &mut state);
                return Ok((state, dps));
            }

            // ── Read more bytes from the socket ───────────────────────────────
            let n = self.stream.read(&mut buf).await?;
            if n == 0 {
                return Err(TuyaError::Protocol("connection closed while awaiting state".into()));
            }
            self.leftover.extend_from_slice(&buf[..n]);
        }
    }
}

// ─── Plugin ──────────────────────────────────────────────────────────────────

pub struct TuyaPlugin {
    info:         DeviceInfo,
    config:       TuyaConfig,
    bus_tx:       StateBusSender,
    seq_no:       AtomicU32,
    /// Last detected protocol version: 0=unknown, 3=v3.3, 4=v3.4, 5=v3.5.
    last_version: AtomicU8,
}

impl TuyaPlugin {
    pub fn new(info: DeviceInfo, config: TuyaConfig, bus_tx: StateBusSender) -> Self {
        Self {
            info,
            config,
            bus_tx,
            seq_no:       AtomicU32::new(1),
            last_version: AtomicU8::new(0),
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

    // ── Open an ephemeral connection ──────────────────────────────────────────

    async fn open_connection(&self) -> Result<Connection, TuyaError> {
        let addr = format!("{}:{}", self.config.ip, self.config.port);
        let mut stream = TcpStream::connect(&addr).await?;
        stream.set_nodelay(true)?;

        let key  = self.config.key_bytes()?;
        let hint = self.config.protocol_version.as_deref();
        let (proto, leftover) = self.probe_protocol(&mut stream, &key, hint).await?;

        let version_byte = match &proto {
            ProtocolResult::V33    => 3u8,
            ProtocolResult::V34(_) => 4u8,
            ProtocolResult::V35(_) => 5u8,
        };
        self.last_version.store(version_byte, Ordering::Release);

        // Continue sequence numbering from where the negotiation left off so
        // data frames don't reuse seq numbers the device already saw.
        let seq_no = self.next_seq();

        Ok(Connection { stream, proto, ecb_key: key, leftover, seq_no })
    }

    // ── Protocol detection & negotiation ─────────────────────────────────────

    /// Probe the device by sending simultaneous v3.5 and v3.4 handshake
    /// frames, then wait up to 500 ms for a response.
    async fn probe_protocol(
        &self,
        stream: &mut TcpStream,
        key:    &[u8; 16],
        hint:   Option<&str>,
    ) -> Result<(ProtocolResult, Vec<u8>), TuyaError> {
        let mut nonce35 = [0u8; 16];
        let mut nonce34 = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut nonce35);
        rand::thread_rng().fill_bytes(&mut nonce34);

        let send_v35 = hint.map_or(true, |h| h == "3.5");
        let send_v34 = hint.map_or(true, |h| h == "3.4");

        // ── v3.5 probe ────────────────────────────────────────────────────────
        if send_v35 {
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

        // ── v3.4 probe ────────────────────────────────────────────────────────
        if send_v34 {
            let enc34   = cipher::encrypt(key, &nonce34);
            let probe34 = protocol::build_frame(
                self.next_seq(),
                CommandWord::SessKeyNegStart,
                &enc34,
                TrailerKind::Hmac(key),
            );
            stream.write_all(&probe34).await?;
        }

        if !send_v35 && !send_v34 {
            return Ok((ProtocolResult::V33, vec![]));
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

                if ring.len() < 4 { continue; }
                let trailer = if ring.starts_with(&protocol::PREFIX_6699) {
                    TrailerKind::Crc32
                } else {
                    TrailerKind::Hmac(key)
                };
                let result = protocol::parse_frame_any(&ring, Some((key, trailer)))?;
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
                    let sk = self.finish_v35_negotiation(stream, key, &nonce35, frame).await?;
                    Ok((ProtocolResult::V35(sk), ring))
                } else {
                    let sk = self.finish_v34_negotiation(stream, key, &nonce34, frame).await?;
                    Ok((ProtocolResult::V34(sk), ring))
                }
            }
            Ok(Err(e)) => {
                if hint.is_some() {
                    Err(e)
                } else {
                    debug!("probe error ({e}), falling back to v3.3");
                    Ok((ProtocolResult::V33, ring))
                }
            }
            Err(_elapsed) => {
                if hint.is_some() {
                    Err(TuyaError::Protocol(format!(
                        "protocol probe timed out for explicit hint {:?}",
                        hint
                    )))
                } else {
                    debug!("probe timed out, falling back to v3.3");
                    Ok((ProtocolResult::V33, ring))
                }
            }
        }
    }

    /// Complete v3.4 negotiation after receiving the 0x04 response frame.
    async fn finish_v34_negotiation(
        &self,
        stream:  &mut TcpStream,
        key:     &[u8; 16],
        nonce34: &[u8; 16],
        resp:    protocol::TuyaFrame,
    ) -> Result<[u8; 16], TuyaError> {
        if resp.command != CommandWord::SessKeyNegResp {
            return Err(TuyaError::Protocol(format!(
                "expected SessKeyNegResp (0x04), got {:?}",
                resp.command
            )));
        }
        if resp.payload.len() < 4 {
            return Err(TuyaError::Protocol(format!(
                "SessKeyNegResp payload too short: {} bytes",
                resp.payload.len()
            )));
        }
        let decrypted = cipher::decrypt(key, &resp.payload[4..])?;
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
        let mut xor_nonces = [0u8; 16];
        for i in 0..16 {
            xor_nonces[i] = nonce34[i] ^ remote_nonce[i];
        }
        let sk_bytes = cipher::ecb_encrypt_raw(key, &xor_nonces)?;
        let session_key: [u8; 16] = sk_bytes.try_into().unwrap();

        let step5_hmac = cipher::hmac_sha256(key, &remote_nonce);
        let enc5 = cipher::encrypt(key, &step5_hmac);
        let finish = protocol::build_frame(
            self.next_seq(),
            CommandWord::SessKeyNegFinish,
            &enc5,
            TrailerKind::Hmac(key),
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
        // resp.payload = retcode(4) + remote_nonce(16) + [hmac(32)]
        if resp.payload.len() < 20 {
            return Err(TuyaError::Protocol(format!(
                "v3.5 SessKeyNegResp payload too short: {} bytes",
                resp.payload.len()
            )));
        }
        let remote_nonce: [u8; 16] = resp.payload[4..20].try_into().unwrap();
        let mut xor_nonces = [0u8; 16];
        for i in 0..16 {
            xor_nonces[i] = nonce35[i] ^ remote_nonce[i];
        }
        let iv35: &[u8; 12] = nonce35[..12].try_into().unwrap();
        let gcm_out     = cipher::gcm_encrypt(key, iv35, &[], &xor_nonces);
        let session_key: [u8; 16] = gcm_out[12..28].try_into().unwrap();

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

    // ── Command → DPS mapping ─────────────────────────────────────────────────

    fn command_to_dps(&self, cmd: &DeviceCommand) -> Option<Value> {
        let dm = &self.config.dp_map;
        match cmd {
            DeviceCommand::SetPower(on) => {
                let target_dp = dm.light_power_dp.unwrap_or(dm.power_dp);
                Some(json!({ target_dp.to_string(): on }))
            }
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
            DeviceCommand::SetFanSpeed(speed)   => dm.fan_speed_dps(*speed),
            DeviceCommand::SetTargetTemp(temp)  => dm.set_temp_dps(*temp),
            DeviceCommand::SetLight { power, brightness, color_temp, rgb, color_mode } => {
                let dps = dm.patch_light_dps(
                    *power,
                    *brightness,
                    *color_temp,
                    *rgb,
                    color_mode.as_deref(),
                );
                // If nothing was set (all None), return None to skip the command.
                if dps.as_object().map(|m| m.is_empty()).unwrap_or(true) {
                    None
                } else {
                    Some(dps)
                }
            }
        }
    }

    // ── Diagnostic helper ─────────────────────────────────────────────────────

    /// Send a raw DP map directly to the device.  Used by the probe tool.
    pub async fn send_dps(&self, dps: &HashMap<String, Value>) -> PluginResult<()> {
        let t = (Self::epoch_ms() / 1000).to_string();
        let payload = json!({
            "devId": self.config.tuya_id,
            "uid":   self.config.tuya_id,
            "t":     t,
            "dps":   dps,
        })
        .to_string();

        let id = self.info.id;
        let mut conn = self.open_connection().await.map_err(PluginError::from)?;
        let ctrl = conn.control_cmd();
        conn.send(ctrl, payload.as_bytes())
            .await
            .map_err(PluginError::from)?;

        // Best-effort: receive the echo-back and push to bus.
        let dp_map = self.config.dp_map.clone();
        let bus_tx = self.bus_tx.clone();
        if let Ok(Ok((state, raw_dps))) =
            timeout(Duration::from_secs(1), conn.recv_state(&dp_map, id)).await
        {
            let _ = bus_tx.send(StateChangeEvent { device_id: id, state, raw_dps });
        }
        Ok(())
    }
}

// ─── DevicePlugin impl ────────────────────────────────────────────────────────

#[async_trait]
impl DevicePlugin for TuyaPlugin {
    fn device_id(&self)    -> &DeviceId     { &self.info.id }
    fn name(&self)         -> &str          { &self.info.name }
    fn capabilities(&self) -> &[Capability] { &self.info.capabilities }

    fn protocol(&self) -> &str {
        match self.last_version.load(Ordering::Acquire) {
            5 => "tuya_local_3.5",
            4 => "tuya_local_3.4",
            3 => "tuya_local_3.3",
            _ => "tuya_local",
        }
    }

    /// No-op — connections are opened on demand per operation.
    async fn connect(&self) -> PluginResult<()> { Ok(()) }

    /// Always reports connected — the registry supervisor uses `poll_state` health instead.
    fn is_connected(&self) -> bool { true }

    /// No-op — ephemeral connections close themselves on drop.
    async fn disconnect(&self) {}

    async fn poll_state(&self) -> PluginResult<DeviceState> {
        let id = self.info.id;
        let mut conn = match self.open_connection().await {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(device = %id, "open_connection failed: {e}");
                return Ok(offline_state(id));
            }
        };

        let t = (Self::epoch_ms() / 1000).to_string();
        let payload = json!({
            "gwId":  self.config.tuya_id,
            "devId": self.config.tuya_id,
            "uid":   self.config.tuya_id,
            "t":     t,
        })
        .to_string();

        // v3.5 uses DpQueryNew (0x10); v3.3 and v3.4 use DpQuery (0x0A).
        let cmd = match &conn.proto {
            ProtocolResult::V35(_) => CommandWord::DpQueryNew,
            _                      => CommandWord::DpQuery,
        };

        if let Err(e) = conn.send(cmd, payload.as_bytes()).await {
            debug!(device = %id, "send failed: {e}");
            return Ok(offline_state(id));
        }

        let dp_map = self.config.dp_map.clone();
        let bus_tx = self.bus_tx.clone();

        match timeout(Duration::from_secs(2), conn.recv_state(&dp_map, id)).await {
            Ok(Ok((state, raw_dps))) => {
                let _ = bus_tx.send(StateChangeEvent {
                    device_id: id,
                    state: state.clone(),
                    raw_dps,
                });
                Ok(state)
            }
            Ok(Err(e)) => {
                tracing::error!(device = %id, "recv_state error: {e}");
                Ok(offline_state(id))
            }
            Err(_elapsed) => {
                debug!(device = %id, "poll_state timeout");
                Ok(offline_state(id))
            }
        }
    }

    async fn execute_command(&self, cmd: DeviceCommand) -> PluginResult<()> {
        let id = self.info.id;

        let dps = match self.command_to_dps(&cmd) {
            Some(d) => d,
            None => {
                tracing::warn!(device = %id, cmd = ?cmd, "command not supported by this device's DP map");
                return Err(PluginError::UnsupportedCommand);
            }
        };

        info!(device = %id, cmd = ?cmd, dps = %dps, "→ control");

        let mut conn = match self.open_connection().await {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(device = %id, "execute_command: open_connection failed: {e}");
                return Err(PluginError::from(e));
            }
        };

        // v3.4/v3.5 use the protocol-5 envelope; v3.3 uses the legacy devId/uid format.
        let t = Self::epoch_ms() / 1000;
        let payload = match &conn.proto {
            ProtocolResult::V34(_) | ProtocolResult::V35(_) => json!({
                "protocol": 5,
                "t":        t,
                "data":     { "dps": dps },
            })
            .to_string(),
            ProtocolResult::V33 => json!({
                "devId": self.config.tuya_id,
                "uid":   self.config.tuya_id,
                "t":     t.to_string(),
                "dps":   dps,
            })
            .to_string(),
        };

        let ctrl = conn.control_cmd();
        conn.send(ctrl, payload.as_bytes())
            .await
            .map_err(PluginError::from)?;

        // Best-effort: receive the state echo-back and push to bus.
        let dp_map = self.config.dp_map.clone();
        let bus_tx = self.bus_tx.clone();
        match timeout(Duration::from_secs(1), conn.recv_state(&dp_map, id)).await {
            Ok(Ok((state, raw_dps))) => {
                let _ = bus_tx.send(StateChangeEvent { device_id: id, state, raw_dps });
            }
            Ok(Err(e)) => {
                tracing::warn!(device = %id, "device rejected command: {e}");
            }
            Err(_elapsed) => {
                tracing::warn!(device = %id, "no echo-back within 1 s — device may have rejected the command");
            }
        }
        Ok(())
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
        rgb: None, switches: HashMap::new(), fan_speed: None,
        temp_current: None, temp_set: None, temp_calibration: None,
    }
}
