/// AES-128-ECB encryption / decryption with PKCS7 padding.
///
/// Tuya v3.3 local protocol uses AES-128-ECB with PKCS7 padding for payload
/// encryption.  The `local_key` (first 16 bytes) is the AES key.
use aes::{
    cipher::{generic_array::GenericArray, BlockDecrypt, BlockEncrypt, KeyInit},
    Aes128,
};

use crate::error::TuyaError;

// ─── PKCS7 padding ───────────────────────────────────────────────────────────

fn pkcs7_pad(data: &[u8], block_size: usize) -> Vec<u8> {
    let pad_len = block_size - (data.len() % block_size);
    let mut padded = data.to_vec();
    padded.extend(std::iter::repeat(pad_len as u8).take(pad_len));
    padded
}

fn pkcs7_unpad(data: &[u8]) -> Result<&[u8], TuyaError> {
    if data.is_empty() {
        return Err(TuyaError::Cipher("empty ciphertext".into()));
    }
    let pad_len = *data.last().unwrap() as usize;
    if pad_len == 0 || pad_len > 16 || pad_len > data.len() {
        return Err(TuyaError::Cipher(format!("invalid PKCS7 pad byte: {pad_len}")));
    }
    // Verify all padding bytes are consistent.
    if data[data.len() - pad_len..].iter().any(|&b| b as usize != pad_len) {
        return Err(TuyaError::Cipher("PKCS7 padding bytes inconsistent".into()));
    }
    Ok(&data[..data.len() - pad_len])
}

// ─── ECB mode ────────────────────────────────────────────────────────────────

/// Encrypt `plaintext` with AES-128-ECB + PKCS7 padding.
pub fn encrypt(key: &[u8; 16], plaintext: &[u8]) -> Vec<u8> {
    let cipher  = Aes128::new(GenericArray::from_slice(key));
    let padded  = pkcs7_pad(plaintext, 16);
    let mut out = padded;

    for block in out.chunks_mut(16) {
        let arr = GenericArray::from_mut_slice(block);
        cipher.encrypt_block(arr);
    }
    out
}

/// Decrypt `ciphertext` with AES-128-ECB and strip PKCS7 padding.
pub fn decrypt(key: &[u8; 16], ciphertext: &[u8]) -> Result<Vec<u8>, TuyaError> {
    if ciphertext.len() % 16 != 0 {
        return Err(TuyaError::Cipher(format!(
            "ciphertext length {} is not a multiple of 16",
            ciphertext.len()
        )));
    }

    let cipher  = Aes128::new(GenericArray::from_slice(key));
    let mut out = ciphertext.to_vec();

    for block in out.chunks_mut(16) {
        let arr = GenericArray::from_mut_slice(block);
        cipher.decrypt_block(arr);
    }

    pkcs7_unpad(&out).map(|s| s.to_vec())
}

// ─── Raw ECB (no PKCS7, for v3.4 session-key negotiation) ────────────────────

/// Encrypt block-aligned data with AES-128-ECB, **without** PKCS7 padding.
/// `data` must be non-empty and a multiple of 16 bytes.
pub fn ecb_encrypt_raw(key: &[u8; 16], data: &[u8]) -> Result<Vec<u8>, TuyaError> {
    if data.is_empty() || data.len() % 16 != 0 {
        return Err(TuyaError::Cipher(format!(
            "ecb_encrypt_raw: length {} is not a non-zero multiple of 16",
            data.len()
        )));
    }
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let mut out = data.to_vec();
    for block in out.chunks_mut(16) {
        cipher.encrypt_block(GenericArray::from_mut_slice(block));
    }
    Ok(out)
}

/// Decrypt block-aligned data with AES-128-ECB, **without** PKCS7 unpadding.
/// `data` must be non-empty and a multiple of 16 bytes.
pub fn ecb_decrypt_raw(key: &[u8; 16], data: &[u8]) -> Result<Vec<u8>, TuyaError> {
    if data.is_empty() || data.len() % 16 != 0 {
        return Err(TuyaError::Cipher(format!(
            "ecb_decrypt_raw: length {} is not a non-zero multiple of 16",
            data.len()
        )));
    }
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let mut out = data.to_vec();
    for block in out.chunks_mut(16) {
        cipher.decrypt_block(GenericArray::from_mut_slice(block));
    }
    Ok(out)
}

// ─── CBC mode (v3.4 command/status payloads) ─────────────────────────────────

/// Encrypt `plaintext` with AES-128-CBC + PKCS7 padding.
pub fn cbc_encrypt(key: &[u8; 16], iv: &[u8; 16], plaintext: &[u8]) -> Vec<u8> {
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let mut out = pkcs7_pad(plaintext, 16);
    let mut prev = *iv;
    for block in out.chunks_mut(16) {
        for (b, p) in block.iter_mut().zip(prev.iter()) {
            *b ^= p;
        }
        let arr = GenericArray::from_mut_slice(block);
        cipher.encrypt_block(arr);
        prev.copy_from_slice(block);
    }
    out
}

/// Decrypt `ciphertext` with AES-128-CBC and strip PKCS7 padding.
pub fn cbc_decrypt(key: &[u8; 16], iv: &[u8; 16], ciphertext: &[u8]) -> Result<Vec<u8>, TuyaError> {
    if ciphertext.is_empty() || ciphertext.len() % 16 != 0 {
        return Err(TuyaError::Cipher(format!(
            "cbc_decrypt: length {} is not a non-zero multiple of 16",
            ciphertext.len()
        )));
    }
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let mut out = ciphertext.to_vec();
    let mut prev = *iv;
    for block in out.chunks_mut(16) {
        // Save ciphertext block before decrypting in place.
        let mut ct = [0u8; 16];
        ct.copy_from_slice(block);
        cipher.decrypt_block(GenericArray::from_mut_slice(block));
        for (b, p) in block.iter_mut().zip(prev.iter()) {
            *b ^= p;
        }
        prev = ct;
    }
    pkcs7_unpad(&out).map(|s| s.to_vec())
}

// ─── HMAC-SHA256 ─────────────────────────────────────────────────────────────

/// Compute HMAC-SHA256(key, data) and return the 32-byte digest.
pub fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    // Use explicit trait path to disambiguate from `KeyInit::new_from_slice`.
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key)
        .expect("HMAC accepts keys of any length");
    mac.update(data);
    let result = mac.finalize().into_bytes();
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&result);
    arr
}

// ─── AES-128-GCM ─────────────────────────────────────────────────────────────

/// AES-128-GCM encrypt.  Returns `IV(12) ++ ciphertext ++ tag(16)`.
pub fn gcm_encrypt(key: &[u8; 16], iv: &[u8; 12], aad: &[u8], plaintext: &[u8]) -> Vec<u8> {
    use aes_gcm::{
        aead::{Aead, KeyInit, Payload},
        Aes128Gcm, Nonce,
    };
    let cipher    = Aes128Gcm::new_from_slice(key).expect("16-byte key");
    let nonce     = Nonce::from_slice(iv);
    let payload   = Payload { msg: plaintext, aad };
    // Output: ciphertext || tag(16)
    let ct_tag    = cipher.encrypt(nonce, payload).expect("GCM encrypt should not fail");
    let mut out   = Vec::with_capacity(12 + ct_tag.len());
    out.extend_from_slice(iv);
    out.extend_from_slice(&ct_tag);
    out
}

/// AES-128-GCM decrypt.  `data` = raw ciphertext (without tag); `tag` = 16-byte auth tag.
/// Returns the plaintext or a `TuyaError::Cipher` if authentication fails.
pub fn gcm_decrypt(
    key:  &[u8; 16],
    iv:   &[u8; 12],
    aad:  &[u8],
    data: &[u8],
    tag:  &[u8; 16],
) -> Result<Vec<u8>, TuyaError> {
    use aes_gcm::{
        aead::{Aead, KeyInit, Payload},
        Aes128Gcm, Nonce,
    };
    let cipher = Aes128Gcm::new_from_slice(key).expect("16-byte key");
    let nonce  = Nonce::from_slice(iv);
    // aes-gcm expects ciphertext || tag in the msg field
    let mut ct_tag = data.to_vec();
    ct_tag.extend_from_slice(tag);
    let payload = Payload { msg: &ct_tag, aad };
    cipher
        .decrypt(nonce, payload)
        .map_err(|_| TuyaError::Cipher("GCM authentication tag mismatch".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let key       = b"0123456789abcdef";
        let plaintext = b"hello tuya world";
        let ct        = encrypt(key, plaintext);
        let pt        = decrypt(key, &ct).unwrap();
        assert_eq!(pt, plaintext);
    }

    #[test]
    fn round_trip_unaligned() {
        let key       = b"0123456789abcdef";
        let plaintext = b"short";
        let ct        = encrypt(key, plaintext);
        let pt        = decrypt(key, &ct).unwrap();
        assert_eq!(pt, plaintext);
    }

    #[test]
    fn gcm_round_trip() {
        let key  = b"0123456789abcdef";
        let iv   = b"unique_iv___";
        let aad  = b"header";
        let pt   = b"payload data";
        // encrypt → IV(12) ++ ciphertext ++ tag(16)
        let out  = gcm_encrypt(key, iv, aad, pt);
        assert_eq!(&out[..12], iv);
        let ciphertext = &out[12..out.len() - 16];
        let tag: &[u8; 16] = out[out.len() - 16..].try_into().unwrap();
        let decrypted = gcm_decrypt(key, iv, aad, ciphertext, tag).unwrap();
        assert_eq!(decrypted, pt);
    }

    #[test]
    fn gcm_bad_tag_rejected() {
        let key       = b"0123456789abcdef";
        let iv        = b"unique_iv___";
        let out       = gcm_encrypt(key, iv, b"", b"hello");
        let ciphertext = &out[12..out.len() - 16];
        let mut bad_tag = [0u8; 16];
        bad_tag.copy_from_slice(&out[out.len() - 16..]);
        bad_tag[0] ^= 0xFF; // flip a bit
        assert!(gcm_decrypt(key, iv, b"", ciphertext, &bad_tag).is_err());
    }
}
