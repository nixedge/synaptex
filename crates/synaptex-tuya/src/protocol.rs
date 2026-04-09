/// Tuya local protocol TCP framing — v3.3 (CRC32), v3.4 (HMAC), and v3.5 (AES-128-GCM).
///
/// ## v3.3 / v3.4 (0x55AA) frame layout (big-endian):
/// ```text
/// ┌──────────┬────────┬────────┬────────┬──────────────┬──────────────┬─────────┐
/// │ Prefix   │ Seq    │ Cmd    │ Len    │ Data         │ Trailer      │ Suffix  │
/// │ 0x0055AA │ (4 B)  │ (4 B)  │ (4 B)  │ encrypted    │ CRC32 or     │ 0xAA55  │
/// │  (4 B)   │        │        │ N+8    │ JSON (N B)   │ HMAC-SHA256  │  (4 B)  │
/// └──────────┴────────┴────────┴────────┴──────────────┴──────────────┴─────────┘
/// ```
/// CRC32 trailer = 4 B; HMAC-SHA256 trailer = 32 B; total `Len` field:
/// - CRC32:  N + 8  (data + CRC + suffix)
/// - HMAC:   N + 36 (data + HMAC + suffix)
///
/// ## v3.5 (0x6699) frame layout:
/// ```text
/// ┌───────────┬────────┬────────┬────────┬────────┬──────────────────────────────┬─────────┐
/// │ Prefix    │ Unk    │ Seq    │ Cmd    │ Len    │ Data                         │ Suffix  │
/// │ 0x006699  │ 0x0000 │ (4 B)  │ (4 B)  │ (4 B)  │ IV(12) + GCM_CT + tag(16)   │ 0x9966  │
/// │ (4 B)     │ (2 B)  │        │        │        │                              │ (4 B)   │
/// └───────────┴────────┴────────┴────────┴────────┴──────────────────────────────┴─────────┘
/// ```
/// Total frame = 18 + Len + 4.  Len = 12 + N + 16 = N + 28.
///
/// For **client→device** frames (commands we send): GCM plaintext = raw payload only.
/// For **device→client** frames (responses we receive): GCM plaintext = retcode(4) + payload;
/// the retcode is stripped from the decrypted payload by `parse_frame_v35`.
use bytes::{Buf, BufMut, Bytes, BytesMut};
use crc32fast::Hasher as Crc32Hasher;

use crate::{cipher, error::TuyaError};

// ─── 0x55AA constants ────────────────────────────────────────────────────────

pub const PREFIX:    [u8; 4] = [0x00, 0x00, 0x55, 0xAA];
pub const SUFFIX:    [u8; 4] = [0x00, 0x00, 0xAA, 0x55];

/// Minimum 0x55AA frame size (CRC32 trailer).
pub const MIN_FRAME_LEN: usize = 24;

pub const TRAILER_CRC_LEN:  usize = 8;   // CRC32(4) + suffix(4)
pub const TRAILER_HMAC_LEN: usize = 36;  // HMAC-SHA256(32) + suffix(4)

// ─── 0x6699 (v3.5) constants ──────────────────────────────────────────────────

pub const PREFIX_6699: [u8; 4] = [0x00, 0x00, 0x66, 0x99];
pub const SUFFIX_6699: [u8; 4] = [0x00, 0x00, 0x99, 0x66];

/// v3.3 command data prefix (15 bytes): `"3.3"` + 12 null bytes.
pub const V33_DATA_PREFIX: &[u8; 15] =
    b"3.3\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00";

/// v3.4 command data prefix (15 bytes): `"3.4"` + 12 null bytes.
pub const V34_DATA_PREFIX: &[u8; 15] =
    b"3.4\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00";

/// v3.5 command data prefix (15 bytes): `"3.5"` + 12 null bytes.
/// Must be prepended to the plaintext of data commands (Control, ControlNew)
/// before GCM encryption.  Not used for DpQuery, DpQueryNew, Heartbeat, or
/// session-key negotiation frames (those are in tinytuya's NO_PROTOCOL_HEADER_CMDS).
pub const V35_DATA_PREFIX: &[u8; 15] =
    b"3.5\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00";

// ─── Trailer kind ─────────────────────────────────────────────────────────────

/// Selects which trailer format to use when building or verifying 0x55AA frames.
#[derive(Clone, Copy)]
pub enum TrailerKind<'a> {
    /// 4-byte CRC32 (v3.3 and session-key-negotiation frames).
    Crc32,
    /// 32-byte HMAC-SHA256 keyed with the given session key (v3.4 data frames).
    Hmac(&'a [u8; 16]),
}

// ─── Command words ────────────────────────────────────────────────────────────

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandWord {
    /// v3.3 DP-set command.
    Control          = 0x07,
    StatusPush       = 0x08,
    Heartbeat        = 0x09,
    DpQuery          = 0x0A,
    /// v3.4/v3.5 DP-set command (replaces `Control` for newer protocol versions).
    ControlNew       = 0x0D,
    /// v3.4+ preferred DP-query command (FRM_QUERY_STAT_NEW).
    DpQueryNew       = 0x10,
    // v3.4 session-key negotiation
    SessKeyNegStart  = 0x03,
    SessKeyNegResp   = 0x04,
    SessKeyNegFinish = 0x05,
}

impl TryFrom<u32> for CommandWord {
    type Error = TuyaError;
    fn try_from(v: u32) -> Result<Self, TuyaError> {
        match v {
            0x03 => Ok(Self::SessKeyNegStart),
            0x04 => Ok(Self::SessKeyNegResp),
            0x05 => Ok(Self::SessKeyNegFinish),
            0x07 => Ok(Self::Control),
            0x08 => Ok(Self::StatusPush),
            0x09 => Ok(Self::Heartbeat),
            0x0A => Ok(Self::DpQuery),
            0x0D => Ok(Self::ControlNew),
            0x10 => Ok(Self::DpQueryNew),
            other => Err(TuyaError::Protocol(format!("unknown command word: 0x{other:02X}"))),
        }
    }
}

// ─── Frame ────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct TuyaFrame {
    pub seq_no:  u32,
    pub command: CommandWord,
    /// Raw payload bytes.  For 0x55AA frames this is the still-encrypted
    /// payload; the caller decrypts.  For 0x6699 frames this is already
    /// decrypted (GCM handles it inside `parse_frame_any`).
    pub payload: Bytes,
}

// ─── Build 0x55AA frame ───────────────────────────────────────────────────────

/// Encode a frame into wire bytes with either CRC32 or HMAC-SHA256 trailer.
/// `payload` must already be AES-encrypted by the caller.
pub fn build_frame(
    seq_no:  u32,
    cmd:     CommandWord,
    payload: &[u8],
    trailer: TrailerKind,
) -> Bytes {
    let trailer_len = match trailer {
        TrailerKind::Crc32   => TRAILER_CRC_LEN,
        TrailerKind::Hmac(_) => TRAILER_HMAC_LEN,
    };
    let data_len = payload.len() as u32 + trailer_len as u32;

    let mut buf = BytesMut::with_capacity(16 + payload.len() + trailer_len);
    buf.put_slice(&PREFIX);
    buf.put_u32(seq_no);
    buf.put_u32(cmd as u32);
    buf.put_u32(data_len);
    buf.put_slice(payload);

    match trailer {
        TrailerKind::Crc32 => {
            let crc = crc32_of(&buf);
            buf.put_u32(crc);
        }
        TrailerKind::Hmac(key) => {
            let hmac = cipher::hmac_sha256(key, &buf);
            buf.put_slice(&hmac);
        }
    }
    buf.put_slice(&SUFFIX);
    buf.freeze()
}

// ─── Parse 0x55AA frame ───────────────────────────────────────────────────────

/// Attempt to parse one 0x55AA `TuyaFrame` from `buf`.
///
/// Returns `Ok(Some((frame, consumed)))` when a complete frame is available,
/// `Ok(None)` when more data is needed, or an error on malformed data.
pub fn parse_frame(buf: &[u8], trailer: TrailerKind) -> Result<Option<(TuyaFrame, usize)>, TuyaError> {
    if buf.len() < MIN_FRAME_LEN {
        return Ok(None);
    }
    if &buf[..4] != PREFIX {
        return Err(TuyaError::Protocol(format!("bad prefix: {:02X?}", &buf[..4])));
    }

    let mut cursor = &buf[4..];
    let seq_no   = cursor.get_u32();
    let cmd_raw  = cursor.get_u32();
    let data_len = cursor.get_u32() as usize; // includes trailer + suffix

    let total_frame = 16 + data_len; // prefix(4)+seq(4)+cmd(4)+len(4) + data_len
    if buf.len() < total_frame {
        return Ok(None); // incomplete
    }

    match trailer {
        TrailerKind::Crc32 => {
            let payload_len = data_len.saturating_sub(8);
            let payload_end = 16 + payload_len;
            let payload     = Bytes::copy_from_slice(&buf[16..payload_end]);

            let expected_crc = u32::from_be_bytes(
                buf[payload_end..payload_end + 4].try_into().unwrap(),
            );
            let actual_crc = crc32_of(&buf[..payload_end]);
            if expected_crc != actual_crc {
                return Err(TuyaError::Protocol(format!(
                    "CRC mismatch: expected {expected_crc:#010X}, got {actual_crc:#010X}"
                )));
            }
            if &buf[payload_end + 4..payload_end + 8] != SUFFIX {
                return Err(TuyaError::Protocol("bad suffix".into()));
            }
            let command = CommandWord::try_from(cmd_raw)?;
            Ok(Some((TuyaFrame { seq_no, command, payload }, total_frame)))
        }

        TrailerKind::Hmac(key) => {
            let payload_len = data_len.saturating_sub(36);
            let payload_end = 16 + payload_len;
            let payload     = Bytes::copy_from_slice(&buf[16..payload_end]);

            let expected_hmac = &buf[payload_end..payload_end + 32];
            let actual_hmac   = cipher::hmac_sha256(key, &buf[..payload_end]);
            if expected_hmac != &actual_hmac {
                return Err(TuyaError::Cipher("HMAC verification failed".into()));
            }
            if &buf[payload_end + 32..payload_end + 36] != SUFFIX {
                return Err(TuyaError::Protocol("bad v3.4 suffix".into()));
            }
            let command = CommandWord::try_from(cmd_raw)?;
            Ok(Some((TuyaFrame { seq_no, command, payload }, total_frame)))
        }
    }
}

// ─── Build v3.5 (0x6699) frame ────────────────────────────────────────────────

/// Build a v3.5 (0x6699) AES-128-GCM frame (client→device direction).
///
/// `session_key` is the AES-128-GCM key.  `iv` is the 12-byte nonce (caller
/// must ensure uniqueness; for data frames use a fresh random nonce per call).
/// AAD = header bytes `[4..18]` (0u16 + seqno + cmd + len).
///
/// Wire layout: `PREFIX_6699(4) | 0u16(2) | seq(4) | cmd(4) | len(4) |`
///              `gcm_encrypt(payload)(IV(12)+CT+tag(16)) | SUFFIX_6699(4)`
///
/// No retcode is included for client→device frames.  Len = IV(12) + CT + tag(16).
pub fn build_frame_v35(
    seq_no:      u32,
    cmd:         CommandWord,
    payload:     &[u8],
    session_key: &[u8; 16],
    iv:          &[u8; 12],
) -> Result<Bytes, TuyaError> {
    // data_len = IV(12) + ciphertext + tag(16); no retcode for client→device
    let data_len = 12u32 + payload.len() as u32 + 16u32;

    // AAD = header[4..18]: 0u16(2) + seq(4) + cmd(4) + data_len(4) = 14 bytes
    let mut aad = [0u8; 14];
    aad[0..2].copy_from_slice(&0u16.to_be_bytes());
    aad[2..6].copy_from_slice(&seq_no.to_be_bytes());
    aad[6..10].copy_from_slice(&(cmd as u32).to_be_bytes());
    aad[10..14].copy_from_slice(&data_len.to_be_bytes());

    // gcm_encrypt returns IV(12) ++ ciphertext ++ tag(16)
    let gcm_out = cipher::gcm_encrypt(session_key, iv, &aad, payload);

    let total = 22 + data_len as usize; // header(18) + data + suffix(4)
    let mut buf = BytesMut::with_capacity(total);
    buf.put_slice(&PREFIX_6699);
    buf.put_u16(0u16);
    buf.put_u32(seq_no);
    buf.put_u32(cmd as u32);
    buf.put_u32(data_len);
    buf.put_slice(&gcm_out); // IV(12) + ciphertext + tag(16)
    buf.put_slice(&SUFFIX_6699);
    Ok(buf.freeze())
}

// ─── Parse any frame (0x55AA or 0x6699) ──────────────────────────────────────

/// Parse one frame from `buf`, auto-detecting 0x55AA vs 0x6699 prefix.
///
/// `session_key`:
/// - `None`                  → 0x55AA CRC32; 0x6699 returns error (no GCM key).
/// - `Some((gcm_key, tk))`   → 0x6699 GCM-decrypts with `gcm_key`;
///                             0x55AA uses `tk` (CRC32 or HMAC).
///
/// For 0x6699, the returned `TuyaFrame.payload` is already GCM-decrypted.
pub fn parse_frame_any<'a>(
    buf:         &[u8],
    session_key: Option<(&'a [u8; 16], TrailerKind<'a>)>,
) -> Result<Option<(TuyaFrame, usize)>, TuyaError> {
    if buf.len() < 4 {
        return Ok(None);
    }

    if &buf[..4] == PREFIX_6699 {
        parse_frame_v35(buf, session_key.map(|(k, _)| k))
    } else {
        let tk = session_key
            .map(|(_, tk)| tk)
            .unwrap_or(TrailerKind::Crc32);
        parse_frame(buf, tk)
    }
}

// ─── Parse a single 0x6699 frame ─────────────────────────────────────────────

fn parse_frame_v35(
    buf:         &[u8],
    session_key: Option<&[u8; 16]>,
) -> Result<Option<(TuyaFrame, usize)>, TuyaError> {
    // Header: PREFIX_6699(4) + 0u16(2) + seq(4) + cmd(4) + len(4) = 18 bytes
    if buf.len() < 18 {
        return Ok(None);
    }
    if &buf[..4] != PREFIX_6699 {
        return Err(TuyaError::Protocol(format!(
            "parse_frame_v35: bad prefix {:02X?}",
            &buf[..4]
        )));
    }

    let seq_no   = u32::from_be_bytes(buf[6..10].try_into().unwrap());
    let cmd_raw  = u32::from_be_bytes(buf[10..14].try_into().unwrap());
    let data_len = u32::from_be_bytes(buf[14..18].try_into().unwrap()) as usize;

    let total_frame = 18 + data_len + 4; // header + data + suffix
    if buf.len() < total_frame {
        return Ok(None);
    }

    // Verify suffix
    if &buf[total_frame - 4..total_frame] != SUFFIX_6699 {
        return Err(TuyaError::Protocol("bad v3.5 suffix".into()));
    }

    let data = &buf[18..18 + data_len];
    // data = IV(12) + GCM_ciphertext(retcode(4) + payload)(N) + tag(16)
    // The retcode is inside the GCM ciphertext for device→client frames.
    if data.len() < 12 + 16 {
        return Err(TuyaError::Protocol(format!(
            "v3.5 data too short: {} bytes",
            data.len()
        )));
    }

    let iv: &[u8; 12] = data[0..12].try_into().unwrap();
    let rest           = &data[12..]; // ciphertext + tag
    let ct_len         = rest.len().saturating_sub(16);
    let ciphertext     = &rest[..ct_len];
    let tag: &[u8; 16] = rest[ct_len..].try_into().unwrap();

    // AAD = buf[4..18]
    let aad = &buf[4..18];

    let key = session_key.ok_or_else(|| {
        TuyaError::Protocol("v3.5 frame received but no GCM session key available".into())
    })?;

    let plaintext = cipher::gcm_decrypt(key, iv, aad, ciphertext, tag)?;

    // Leave the 4-byte retcode in the payload so recv_state can inspect it
    // consistently across all protocol versions.
    let payload = Bytes::from(plaintext);

    let command = CommandWord::try_from(cmd_raw)?;
    Ok(Some((
        TuyaFrame { seq_no, command, payload },
        total_frame,
    )))
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn crc32_of(data: &[u8]) -> u32 {
    let mut h = Crc32Hasher::new();
    h.update(data);
    h.finalize()
}

/// Returns `true` if this v3.3 command requires the 15-byte version prefix.
pub fn v33_needs_prefix(cmd: CommandWord) -> bool {
    cmd == CommandWord::Control
}

/// Prepend the 15-byte v3.3 version header to an already-encrypted ciphertext.
pub fn v33_prepend_version(ciphertext: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(V33_DATA_PREFIX.len() + ciphertext.len());
    out.extend_from_slice(V33_DATA_PREFIX);
    out.extend_from_slice(ciphertext);
    out
}
