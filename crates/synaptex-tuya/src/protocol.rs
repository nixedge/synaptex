/// Tuya local protocol v3.3 / v3.4 TCP framing.
///
/// Message layout (all multi-byte fields are big-endian):
///
/// ```text
/// ┌─────────────┬──────────┬──────────┬──────────┬──────────────┬──────────┬─────────────┐
/// │ Prefix (4B) │ Seq (4B) │ Cmd (4B) │ Len (4B) │ Data (N B)   │ CRC (4B) │ Suffix (4B) │
/// │ 0x0055AA    │          │          │ N+8      │ encrypted JSON│ crc32    │ 0x0055AA    │
/// └─────────────┴──────────┴──────────┴──────────┴──────────────┴──────────┴─────────────┘
/// ```
///
/// For v3.3 the data field of **commands** is prefixed with the 12-byte
/// version string `"3.3\0\0\0\0\0\0\0\0\0"` before encryption.
use bytes::{Buf, BufMut, Bytes, BytesMut};
use crc32fast::Hasher as Crc32Hasher;

use crate::error::TuyaError;

// ─── Constants ───────────────────────────────────────────────────────────────

pub const PREFIX: [u8; 4] = [0x00, 0x00, 0x55, 0xAA];
pub const SUFFIX: [u8; 4] = [0x00, 0x00, 0xAA, 0x55];

/// Minimum frame size: prefix(4) + seq(4) + cmd(4) + len(4) + crc(4) + suffix(4)
pub const MIN_FRAME_LEN: usize = 24;

/// v3.3 command data prefix (15 bytes): `"3.3"` + 12 null bytes.
///
/// This is prepended to the **ciphertext** (after AES-ECB encryption) only for
/// Control (0x07) commands.  DpQuery and all other commands send the ciphertext
/// directly with no prefix.
pub const V33_DATA_PREFIX: &[u8; 15] = b"3.3\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00";

// ─── Command words ───────────────────────────────────────────────────────────

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandWord {
    Heartbeat  = 0x09,
    DpQuery    = 0x0A,
    Control    = 0x07,
    StatusPush = 0x08,
    // v3.4 session key negotiation
    SessKeyNegStart  = 0x03,
    SessKeyNegResp   = 0x04,
    SessKeyNegFinish = 0x05,
}

impl TryFrom<u32> for CommandWord {
    type Error = TuyaError;
    fn try_from(v: u32) -> Result<Self, TuyaError> {
        match v {
            0x09 => Ok(Self::Heartbeat),
            0x0A => Ok(Self::DpQuery),
            0x07 => Ok(Self::Control),
            0x08 => Ok(Self::StatusPush),
            0x03 => Ok(Self::SessKeyNegStart),
            0x04 => Ok(Self::SessKeyNegResp),
            0x05 => Ok(Self::SessKeyNegFinish),
            other => Err(TuyaError::Protocol(format!("unknown command word: 0x{other:02X}"))),
        }
    }
}

// ─── Frame ───────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct TuyaFrame {
    pub seq_no:  u32,
    pub command: CommandWord,
    /// Raw (decrypted) payload bytes.
    pub payload: Bytes,
}

// ─── Build a command frame ────────────────────────────────────────────────────

/// Encode a `TuyaFrame` into wire bytes, including CRC and prefix/suffix.
/// `encrypted_payload` must already be AES-128-ECB encrypted.
pub fn build_frame(seq_no: u32, cmd: CommandWord, encrypted_payload: &[u8]) -> Bytes {
    // data_len = payload + 4 (CRC) + 4 (suffix)
    let data_len = encrypted_payload.len() as u32 + 8;

    // Build the portion over which CRC is computed (prefix through end of data).
    let mut buf = BytesMut::with_capacity(MIN_FRAME_LEN + encrypted_payload.len());
    buf.put_slice(&PREFIX);
    buf.put_u32(seq_no);
    buf.put_u32(cmd as u32);
    buf.put_u32(data_len);
    buf.put_slice(encrypted_payload);

    let crc = crc32_of(&buf);
    buf.put_u32(crc);
    buf.put_slice(&SUFFIX);

    buf.freeze()
}

// ─── Parse a received frame ──────────────────────────────────────────────────

/// Attempt to parse one `TuyaFrame` from `buf`.
///
/// Returns `Ok(Some((frame, consumed)))` when a complete frame is available,
/// `Ok(None)` when more data is needed, or an error on malformed data.
pub fn parse_frame(buf: &[u8]) -> Result<Option<(TuyaFrame, usize)>, TuyaError> {
    if buf.len() < MIN_FRAME_LEN {
        return Ok(None);
    }

    // Locate prefix.
    if &buf[..4] != PREFIX {
        return Err(TuyaError::Protocol(format!(
            "bad prefix: {:02X?}",
            &buf[..4]
        )));
    }

    let mut cursor = &buf[4..];
    let seq_no   = cursor.get_u32();
    let cmd_raw  = cursor.get_u32();
    let data_len = cursor.get_u32() as usize; // includes CRC(4) + suffix(4)

    let total_frame = 4 + 4 + 4 + 4 + data_len; // prefix + seq + cmd + len + rest
    if buf.len() < total_frame {
        return Ok(None); // incomplete frame
    }

    // payload = everything between the header and CRC+suffix
    let payload_len = data_len.saturating_sub(8);
    let payload_end = 4 + 4 + 4 + 4 + payload_len;
    let payload     = Bytes::copy_from_slice(&buf[16..payload_end]);

    // Verify CRC over prefix through end of payload.
    let expected_crc = u32::from_be_bytes(buf[payload_end..payload_end + 4].try_into().unwrap());
    let actual_crc   = crc32_of(&buf[..payload_end]);
    if expected_crc != actual_crc {
        return Err(TuyaError::Protocol(format!(
            "CRC mismatch: expected {expected_crc:#010X}, got {actual_crc:#010X}"
        )));
    }

    // Verify suffix.
    if &buf[payload_end + 4..payload_end + 8] != SUFFIX {
        return Err(TuyaError::Protocol("bad suffix".into()));
    }

    let command = CommandWord::try_from(cmd_raw)?;
    let frame   = TuyaFrame { seq_no, command, payload };
    Ok(Some((frame, total_frame)))
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn crc32_of(data: &[u8]) -> u32 {
    let mut h = Crc32Hasher::new();
    h.update(data);
    h.finalize()
}

/// Returns `true` if this v3.3 command requires the 15-byte version prefix.
///
/// Only `Control` (0x07) carries the prefix; `DpQuery`, `Heartbeat`, and
/// session-key negotiation frames are sent as raw ciphertext.
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
