//! SigV4 presigned WebSocket URL for AWS IoT Core MQTT.

use chrono::Utc;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use urlencoding::encode;

type HmacSha256 = Hmac<Sha256>;

pub const IOT_HOST: &str = "a3q27gia9qg3zy-ats.iot.us-east-1.amazonaws.com";
const REGION:   &str = "us-east-1";
const SERVICE:  &str = "iotdevicegateway";

/// Build a presigned WSS URL for connecting to AWS IoT Core MQTT over WebSocket.
///
/// IMPORTANT: The security token is appended *after* the signature per IoT Core
/// requirements — it is not included in the canonical request or signing key.
pub fn presign_mqtt_url(key_id: &str, secret: &str, session_token: &str) -> String {
    let now      = Utc::now();
    let date     = now.format("%Y%m%d").to_string();
    let datetime = now.format("%Y%m%dT%H%M%SZ").to_string();

    let credential = format!("{key_id}/{date}/{REGION}/{SERVICE}/aws4_request");

    // Canonical query string — parameters in alphabetical order, all URL-encoded.
    let qs = format!(
        "X-Amz-Algorithm=AWS4-HMAC-SHA256\
         &X-Amz-Credential={}\
         &X-Amz-Date={}\
         &X-Amz-Expires=86400\
         &X-Amz-SignedHeaders=host",
        encode(&credential),
        encode(&datetime),
    );

    // Canonical request.
    let payload_hash = hex::encode(Sha256::digest(b""));
    let canonical_request = format!(
        "GET\n/mqtt\n{qs}\nhost:{IOT_HOST}\n\nhost\n{payload_hash}"
    );

    // String to sign.
    let cr_hash = hex::encode(Sha256::digest(canonical_request.as_bytes()));
    let sts = format!(
        "AWS4-HMAC-SHA256\n{datetime}\n{date}/{REGION}/{SERVICE}/aws4_request\n{cr_hash}"
    );

    // Signing key derivation.
    let k_date    = hmac_bytes(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k_region  = hmac_bytes(&k_date,    REGION.as_bytes());
    let k_service = hmac_bytes(&k_region,  SERVICE.as_bytes());
    let k_signing = hmac_bytes(&k_service, b"aws4_request");

    let signature = hex::encode(hmac_bytes(&k_signing, sts.as_bytes()));

    // Append security token *after* the signature (IoT Core quirk).
    format!(
        "wss://{IOT_HOST}/mqtt?{qs}&X-Amz-Signature={signature}&X-Amz-Security-Token={}",
        encode(session_token),
    )
}

fn hmac_bytes(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC key");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}
