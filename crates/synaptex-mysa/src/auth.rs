//! Cognito SRP authentication + AWS credential exchange for Mysa cloud.

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use chrono::Utc;
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use num_bigint::BigUint;
use rand::{rngs::OsRng, RngCore};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

// ─── Cognito constants ───────────────────────────────────────────────────────

#[allow(dead_code)]
const REGION:           &str = "us-east-1";
#[allow(dead_code)]
const USER_POOL_ID:     &str = "us-east-1_GUFWfhI7g";
const POOL_ID_SHORT:    &str = "GUFWfhI7g";
const CLIENT_ID:        &str = "6cktj934gasnc72f7jo2cmf6rt";
const IDENTITY_POOL_ID: &str = "us-east-1:ebd95d52-9995-45da-b059-56b865a18379";
const COGNITO_ENDPOINT: &str = "https://cognito-idp.us-east-1.amazonaws.com/";
const IDENTITY_ENDPOINT:&str = "https://cognito-identity.us-east-1.amazonaws.com/";
const PROVIDER:         &str = "cognito-idp.us-east-1.amazonaws.com/us-east-1_GUFWfhI7g";

// 3072-bit MODP prime (RFC 3526 group 15), used by Cognito SRP.
const N_HEX: &str = concat!(
    "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD1",
    "29024E088A67CC74020BBEA63B139B22514A08798E3404DD",
    "EF9519B3CD3A431B302B0A6DF25F14374FE1356D6D51C245",
    "E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7ED",
    "EE386BFB5A899FA5AE9F24117C4B1FE649286651ECE45B3D",
    "C2007CB8A163BF0598DA48361C55D39A69163FA8FD24CF5F",
    "83655D23DCA3AD961C62F356208552BB9ED529077096966D",
    "670C354E4ABC9804F1746C08CA18217C32905E462E36CE3B",
    "E39E772C180E86039B2783A2EC07A28FB5C55DF06F4C52C9",
    "DE2BCBF6955817183995497CEA956AE515D2261898FA0510",
    "15728E5A8AAAC42DAD33170D04507A33A85521ABDF1CBA64",
    "ECFB850458DBEF0A8AEA71575D060C7DB3970F85A6E1E4C7",
    "ABF5AE8CDB0933D71E8C94E04A25619DCEE3D2261AD2EE6B",
    "F12FFA06D98A0864D87602733EC86A64521F2B18177B200C",
    "BBE117577A615D6C770988C0BAD946E208E24FA074E5AB31",
    "43DB5BFCE0FD108E4B82D120A93AD2CAFFFFFFFFFFFFFFFF",
);


// ─── Session ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct CognitoSession {
    pub id_token:      String,
    pub access_token:  String,
    pub refresh_token: String,
    /// Unix timestamp (seconds) at which id_token expires.
    pub id_token_exp:  u64,
    /// AWS temporary credentials (from GetCredentialsForIdentity).
    pub aws_key_id:    String,
    pub aws_secret:    String,
    pub aws_session:   String,
    /// Unix timestamp (seconds) at which the AWS credentials expire.
    pub aws_cred_exp:  u64,
    /// Cognito identity ID (from GetId) used as `userId` in MQTT commands.
    pub identity_id:   String,
}

// ─── Public API ──────────────────────────────────────────────────────────────

/// Authenticate with Cognito using SRP and exchange for AWS credentials.
pub async fn authenticate(username: &str, password: &str) -> Result<CognitoSession> {
    let client = reqwest::Client::new();

    // Step 1–4: SRP USER_SRP_AUTH flow.
    let (id_token, access_token, refresh_token) =
        srp_authenticate(&client, username, password).await?;

    let id_token_exp = jwt_exp(&id_token).unwrap_or(0);

    // Step 5: Exchange IdToken for AWS temporary credentials.
    let (identity_id, aws_key_id, aws_secret, aws_session, aws_cred_exp) =
        exchange_for_aws_creds(&client, &id_token).await?;

    Ok(CognitoSession {
        id_token,
        access_token,
        refresh_token,
        id_token_exp,
        aws_key_id,
        aws_secret,
        aws_session,
        aws_cred_exp,
        identity_id,
    })
}

/// Refresh the id/access tokens using the stored refresh_token.
/// Also refreshes AWS credentials.
pub async fn refresh(session: &mut CognitoSession) -> Result<()> {
    let client = reqwest::Client::new();

    let body = serde_json::json!({
        "AuthFlow": "REFRESH_TOKEN_AUTH",
        "ClientId": CLIENT_ID,
        "AuthParameters": {
            "REFRESH_TOKEN": &session.refresh_token,
        }
    });

    let resp: serde_json::Value = cognito_post(&client, "InitiateAuth", &body).await?;
    let result = &resp["AuthenticationResult"];

    let id_token = result["IdToken"].as_str()
        .context("missing IdToken in refresh response")?
        .to_string();
    let access_token = result["AccessToken"].as_str()
        .context("missing AccessToken in refresh response")?
        .to_string();

    // RefreshToken may or may not be rotated; keep the old one if absent.
    if let Some(new_rt) = result["RefreshToken"].as_str() {
        session.refresh_token = new_rt.to_string();
    }

    session.id_token_exp = jwt_exp(&id_token).unwrap_or(0);
    session.id_token     = id_token.clone();
    session.access_token = access_token;

    // Refresh AWS credentials too.
    let (identity_id, aws_key_id, aws_secret, aws_session, aws_cred_exp) =
        exchange_for_aws_creds(&client, &id_token).await?;

    session.identity_id  = identity_id;
    session.aws_key_id   = aws_key_id;
    session.aws_secret   = aws_secret;
    session.aws_session  = aws_session;
    session.aws_cred_exp = aws_cred_exp;

    Ok(())
}

/// Return true if the id_token will expire within the next 60 seconds.
pub fn id_token_needs_refresh(session: &CognitoSession) -> bool {
    let now = now_secs();
    session.id_token_exp < now + 60
}

/// Return true if the AWS credentials will expire within the next 60 seconds.
pub fn aws_creds_need_refresh(session: &CognitoSession) -> bool {
    let now = now_secs();
    session.aws_cred_exp < now + 60
}

// ─── SRP implementation ──────────────────────────────────────────────────────

async fn srp_authenticate(
    client:   &reqwest::Client,
    username: &str,
    password: &str,
) -> Result<(String, String, String)> {
    let n   = BigUint::parse_bytes(N_HEX.as_bytes(), 16).expect("parse N");
    let g   = BigUint::from(2u32);

    // k = SHA256(padHex(N) || padHex(g))
    // Uses Cognito-style padding: prepend 0x00 if high bit set, otherwise minimal bytes.
    let k = {
        let mut h = Sha256::new();
        h.update(&pad_hex_srp(&n));
        h.update(&pad_hex_srp(&g));
        BigUint::from_bytes_be(&h.finalize())
    };

    // Generate ephemeral a (256-bit random) and A = g^a mod N.
    let mut a_bytes = [0u8; 32];
    OsRng.fill_bytes(&mut a_bytes);
    let a     = BigUint::from_bytes_be(&a_bytes);
    let big_a = g.modpow(&a, &n);
    let big_a_hex = hex::encode(big_a.to_bytes_be());

    // InitiateAuth
    let initiate_body = serde_json::json!({
        "AuthFlow":        "USER_SRP_AUTH",
        "ClientId":        CLIENT_ID,
        "AuthParameters":  {
            "USERNAME": username,
            "SRP_A":    big_a_hex,
        }
    });
    let init_resp: serde_json::Value =
        cognito_post(client, "InitiateAuth", &initiate_body).await?;

    if init_resp["ChallengeName"].as_str() != Some("PASSWORD_VERIFIER") {
        bail!("unexpected Cognito challenge: {}", init_resp["ChallengeName"]);
    }

    let params        = &init_resp["ChallengeParameters"];
    let srp_b_hex     = params["SRP_B"].as_str().context("missing SRP_B")?;
    let salt_hex      = params["SALT"].as_str().context("missing SALT")?;
    let secret_block  = params["SECRET_BLOCK"].as_str().context("missing SECRET_BLOCK")?;
    let user_id_srp   = params["USER_ID_FOR_SRP"].as_str().context("missing USER_ID_FOR_SRP")?;

    let b = BigUint::parse_bytes(srp_b_hex.as_bytes(), 16)
        .context("parse SRP_B")?;

    // salt: parse as BigUint then pad (matches pycognito's pad_hex(salt_hex_string))
    let salt_biguint = BigUint::parse_bytes(salt_hex.as_bytes(), 16)
        .context("parse SALT")?;
    let salt_padded = pad_hex_srp(&salt_biguint);

    // u = SHA256(padHex(A) || padHex(B))
    let u = {
        let mut h = Sha256::new();
        h.update(&pad_hex_srp(&big_a));
        h.update(&pad_hex_srp(&b));
        BigUint::from_bytes_be(&h.finalize())
    };

    // x = SHA256(padHex(salt) || SHA256(pool_id_short || user_id_srp || ":" || password))
    let x = {
        let inner = {
            let mut h = Sha256::new();
            h.update(POOL_ID_SHORT.as_bytes());
            h.update(user_id_srp.as_bytes());
            h.update(b":");
            h.update(password.as_bytes());
            h.finalize()
        };
        let mut h = Sha256::new();
        h.update(&salt_padded);
        h.update(&inner);
        BigUint::from_bytes_be(&h.finalize())
    };

    // S = (B - k*g^x mod N)^(a + u*x) mod N
    let g_x   = g.modpow(&x, &n);
    let k_g_x = (&k * &g_x) % &n;
    let b_minus = if b > k_g_x {
        (&b - &k_g_x) % &n
    } else {
        (&n + &b - &k_g_x) % &n
    };
    let exp = &a + &u * &x;
    let s   = b_minus.modpow(&exp, &n);

    // HKDF key = HKDF-SHA256(ikm=padHex(S), salt=padHex(u), info="Caldera Derived Key")[0..16]
    // pycognito: compute_hkdf(pad_hex(S), pad_hex(long_to_hex(u)))
    let s_pad = pad_hex_srp(&s);
    let u_pad = pad_hex_srp(&u);
    let hk = Hkdf::<Sha256>::new(Some(&u_pad), &s_pad);
    let mut hkdf_key = [0u8; 16];
    hk.expand(b"Caldera Derived Key", &mut hkdf_key)
        .map_err(|e| anyhow::anyhow!("HKDF expand: {e}"))?;

    // Timestamp in Cognito's required format: "Www Mmm D HH:MM:SS UTC YYYY"
    // Day is NOT zero/space-padded (pycognito: "{day:d}", JS SDK: getUTCDate() as number).
    let timestamp = {
        let now = Utc::now();
        format!("{}", now.format("%a %b %-d %T UTC %Y"))
    };

    // Signature = HMAC-SHA256(hkdf_key, pool_id_short || user_id_srp || secret_block_bytes || timestamp)
    let secret_block_bytes = base64::engine::general_purpose::STANDARD
        .decode(secret_block)
        .context("decode SECRET_BLOCK")?;

    let signature_b64 = {
        let mut mac = HmacSha256::new_from_slice(&hkdf_key)
            .map_err(|e| anyhow::anyhow!("HMAC key: {e}"))?;
        mac.update(POOL_ID_SHORT.as_bytes());
        mac.update(user_id_srp.as_bytes());
        mac.update(&secret_block_bytes);
        mac.update(timestamp.as_bytes());
        base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
    };

    // RespondToAuthChallenge
    let respond_body = serde_json::json!({
        "ChallengeName": "PASSWORD_VERIFIER",
        "ClientId":      CLIENT_ID,
        "ChallengeResponses": {
            "PASSWORD_CLAIM_SIGNATURE":    signature_b64,
            "PASSWORD_CLAIM_SECRET_BLOCK": secret_block,
            "TIMESTAMP":                   timestamp,
            "USERNAME":                    user_id_srp,
        }
    });
    let respond_resp: serde_json::Value =
        cognito_post(client, "RespondToAuthChallenge", &respond_body).await?;

    let result = &respond_resp["AuthenticationResult"];
    let id_token     = result["IdToken"].as_str()
        .context("missing IdToken")?.to_string();
    let access_token = result["AccessToken"].as_str()
        .context("missing AccessToken")?.to_string();
    let refresh_token = result["RefreshToken"].as_str()
        .context("missing RefreshToken")?.to_string();

    Ok((id_token, access_token, refresh_token))
}

// ─── AWS credential exchange ─────────────────────────────────────────────────

async fn exchange_for_aws_creds(
    client:   &reqwest::Client,
    id_token: &str,
) -> Result<(String, String, String, String, u64)> {
    // GetId
    let get_id_body = serde_json::json!({
        "IdentityPoolId": IDENTITY_POOL_ID,
        "Logins": { PROVIDER: id_token }
    });
    let get_id_resp: serde_json::Value =
        cognito_identity_post(client, "GetId", &get_id_body).await?;
    let identity_id = get_id_resp["IdentityId"].as_str()
        .context("missing IdentityId")?.to_string();

    // GetCredentialsForIdentity
    let get_creds_body = serde_json::json!({
        "IdentityId": &identity_id,
        "Logins": { PROVIDER: id_token }
    });
    let get_creds_resp: serde_json::Value =
        cognito_identity_post(client, "GetCredentialsForIdentity", &get_creds_body).await?;

    let creds      = &get_creds_resp["Credentials"];
    let aws_key_id  = creds["AccessKeyId"].as_str()
        .context("missing AccessKeyId")?.to_string();
    let aws_secret  = creds["SecretKey"].as_str()
        .context("missing SecretKey")?.to_string();
    let aws_session = creds["SessionToken"].as_str()
        .context("missing SessionToken")?.to_string();
    let exp_secs    = creds["Expiration"].as_f64().unwrap_or(0.0) as u64;

    Ok((identity_id, aws_key_id, aws_secret, aws_session, exp_secs))
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

async fn cognito_post(
    client:    &reqwest::Client,
    operation: &str,
    body:      &serde_json::Value,
) -> Result<serde_json::Value> {
    let target = format!("AWSCognitoIdentityProviderService.{operation}");
    let resp = client.post(COGNITO_ENDPOINT)
        .header("X-Amz-Target", &target)
        .header("Content-Type", "application/x-amz-json-1.1")
        .json(body)
        .send()
        .await
        .with_context(|| format!("Cognito {operation} request"))?;

    let status = resp.status();
    let text   = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("Cognito {operation} failed ({status}): {text}");
    }
    let val: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("parse Cognito {operation} response"))?;
    Ok(val)
}

async fn cognito_identity_post(
    client:    &reqwest::Client,
    operation: &str,
    body:      &serde_json::Value,
) -> Result<serde_json::Value> {
    let target = format!("AWSCognitoIdentityService.{operation}");
    let resp = client.post(IDENTITY_ENDPOINT)
        .header("X-Amz-Target", &target)
        .header("Content-Type", "application/x-amz-json-1.1")
        .json(body)
        .send()
        .await
        .with_context(|| format!("Cognito Identity {operation} request"))?;

    let status = resp.status();
    let text   = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("Cognito Identity {operation} failed ({status}): {text}");
    }
    let val: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("parse Cognito Identity {operation} response"))?;
    Ok(val)
}

/// Pad a BigUint using Cognito's `padHex` convention:
/// - Return minimal big-endian bytes (no unnecessary leading zeros)
/// - Prepend a single 0x00 byte if the high bit is set (to prevent signed misinterpretation)
/// This matches amazon-cognito-identity-js's `padHex` function exactly.
fn pad_hex_srp(n: &BigUint) -> Vec<u8> {
    let bytes = n.to_bytes_be();
    if bytes.is_empty() || bytes[0] >= 0x80 {
        let mut out = Vec::with_capacity(bytes.len() + 1);
        out.push(0x00);
        out.extend_from_slice(&bytes);
        out
    } else {
        bytes
    }
}

/// Extract the `exp` claim from a JWT (without verification).
pub fn jwt_exp(token: &str) -> Option<u64> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() < 2 {
        return None;
    }
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(parts[1]).ok()?;
    let json: serde_json::Value = serde_json::from_slice(&payload).ok()?;
    json["exp"].as_u64()
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ─── Tests ────────────────────────────────────────────────────────────────────
//
// Test vectors derived from pycognito (pycognito/aws_srp.py) with fixed inputs.
// Run with: cargo test -p synaptex-mysa

#[cfg(test)]
mod tests {
    use super::*;
    use hkdf::Hkdf;

    // ── pad_hex_srp ──────────────────────────────────────────────────────────

    #[test]
    fn test_pad_hex_srp_small_value() {
        // g = 2: single byte 0x02 < 0x80, no prefix added
        assert_eq!(pad_hex_srp(&BigUint::from(2u32)), vec![0x02]);
    }

    #[test]
    fn test_pad_hex_srp_high_bit_set() {
        // 0x80: high bit set, must prepend 0x00
        assert_eq!(pad_hex_srp(&BigUint::from(0x80u32)), vec![0x00, 0x80]);
    }

    #[test]
    fn test_pad_hex_srp_high_bit_clear() {
        // 0x7f: high bit clear, no prefix
        assert_eq!(pad_hex_srp(&BigUint::from(0x7fu32)), vec![0x7f]);
    }

    #[test]
    fn test_pad_hex_srp_zero() {
        // zero: to_bytes_be() is empty, treated as high-bit-set → [0x00]
        assert_eq!(pad_hex_srp(&BigUint::from(0u32)), vec![0x00]);
    }

    // ── k value ──────────────────────────────────────────────────────────────

    #[test]
    fn test_k_value() {
        // pycognito: hex_hash("00" + N_HEX + "0" + "2")
        // = SHA256(0x00 || N_bytes || 0x02)
        let n = BigUint::parse_bytes(N_HEX.as_bytes(), 16).unwrap();
        let g = BigUint::from(2u32);
        let mut h = Sha256::new();
        h.update(&pad_hex_srp(&n));
        h.update(&pad_hex_srp(&g));
        let k = BigUint::from_bytes_be(&h.finalize());
        assert_eq!(
            hex::encode(k.to_bytes_be()),
            "538282c4354742d7cbbde2359fcf67f9f5b3a6b08791e5011b43b8a5b66d9ee6"
        );
    }

    // ── x computation ────────────────────────────────────────────────────────

    fn compute_x_test(salt_hex: &str, pool: &str, user: &str, pw: &str) -> BigUint {
        let salt_big = BigUint::parse_bytes(salt_hex.as_bytes(), 16).unwrap();
        let salt_pad = pad_hex_srp(&salt_big);
        let inner = {
            let mut h = Sha256::new();
            h.update(pool.as_bytes());
            h.update(user.as_bytes());
            h.update(b":");
            h.update(pw.as_bytes());
            h.finalize()
        };
        let mut h = Sha256::new();
        h.update(&salt_pad);
        h.update(&inner);
        BigUint::from_bytes_be(&h.finalize())
    }

    #[test]
    fn test_x_salt_low_nibble() {
        // salt starts with '1' (< 8): pad_hex doesn't change it; pycognito and raw agree
        let x = compute_x_test(
            "1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d",
            "GUFWfhI7g", "testuser", "testpass",
        );
        assert_eq!(
            hex::encode(x.to_bytes_be()),
            "6036553127cee8e492033f0d511046ccc355993fe3d6a60a586a275b2460d871"
        );
    }

    #[test]
    fn test_x_salt_high_nibble() {
        // salt starts with 'a' (>= 8): pad_hex prepends 0x00; raw decode gives wrong result
        let x = compute_x_test(
            "aabb3c4d5e6f7a8b9c0d1e2f3a4b5c6d",
            "GUFWfhI7g", "testuser", "testpass",
        );
        // Expected from pycognito with pad_hex applied to the high-nibble salt.
        // Note: pycognito's long_to_hex() drops leading zeros; we zero-pad to 32 bytes.
        assert_eq!(
            hex::encode(x.to_bytes_be()),
            "0b6a7a137d379f753d6b2b74a67d1cbc9784ca6d1339d0efddcebe7ee91ddec9"
        );
    }

    // ── HKDF key ─────────────────────────────────────────────────────────────

    #[test]
    fn test_hkdf_padded_u() {
        // small_a=1, small_b=4 → u first byte 0x9c (>= 0x80): pad_hex adds 0x00 prefix
        // Verifies that using pad_hex_srp(u) as HKDF salt produces the correct key.
        let n  = BigUint::parse_bytes(N_HEX.as_bytes(), 16).unwrap();
        let g  = BigUint::from(2u32);
        let big_a   = g.modpow(&BigUint::from(1u32), &n);  // g^1 mod N = 2
        let server_b = g.modpow(&BigUint::from(4u32), &n); // g^4 mod N

        let u = {
            let mut h = Sha256::new();
            h.update(&pad_hex_srp(&big_a));
            h.update(&pad_hex_srp(&server_b));
            BigUint::from_bytes_be(&h.finalize())
        };
        // Verify u first byte is 0x9c (>= 0x80) so padding actually matters here
        let u_bytes = u.to_bytes_be();
        assert_eq!(u_bytes[0], 0x9c, "u first byte should be 0x9c for this vector");

        // Use S = 0xdeadbeef for a simple test of the HKDF padding path
        let s = BigUint::from(0xdeadbeefu64);
        let s_pad = pad_hex_srp(&s);
        let u_pad = pad_hex_srp(&u);
        assert_eq!(u_pad.len(), u_bytes.len() + 1, "u_pad should be 1 byte longer (0x00 prefix)");

        let hk = Hkdf::<Sha256>::new(Some(&u_pad), &s_pad);
        let mut key = [0u8; 16];
        hk.expand(b"Caldera Derived Key", &mut key).unwrap();

        // Wrong result using raw u_hash (what the old code produced)
        let hk_wrong = Hkdf::<Sha256>::new(Some(&u_bytes), &s_pad);
        let mut key_wrong = [0u8; 16];
        hk_wrong.expand(b"Caldera Derived Key", &mut key_wrong).unwrap();

        assert_ne!(key, key_wrong, "padded and raw u must give different keys here");
    }

    // ── Full SRP flow ─────────────────────────────────────────────────────────

    #[test]
    fn test_full_srp_flow_hkdf_and_signature() {
        // Fixed inputs: small_a=1, small_b=4, salt_lo, pool/user/pass
        // All expected values derived from pycognito.
        let n  = BigUint::parse_bytes(N_HEX.as_bytes(), 16).unwrap();
        let g  = BigUint::from(2u32);
        let k  = {
            let mut h = Sha256::new();
            h.update(&pad_hex_srp(&n));
            h.update(&pad_hex_srp(&g));
            BigUint::from_bytes_be(&h.finalize())
        };

        let small_a  = BigUint::from(1u32);
        let small_b  = BigUint::from(4u32);
        let big_a    = g.modpow(&small_a, &n);
        let server_b = g.modpow(&small_b, &n);

        let u = {
            let mut h = Sha256::new();
            h.update(&pad_hex_srp(&big_a));
            h.update(&pad_hex_srp(&server_b));
            BigUint::from_bytes_be(&h.finalize())
        };

        let salt_hex = "1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d"; // low first nibble
        let salt_pad = pad_hex_srp(&BigUint::parse_bytes(salt_hex.as_bytes(), 16).unwrap());

        let pool = "GUFWfhI7g";
        let user = "testuser";
        let pw   = "testpass";

        let x = {
            let inner = {
                let mut h = Sha256::new();
                h.update(pool.as_bytes());
                h.update(user.as_bytes());
                h.update(b":");
                h.update(pw.as_bytes());
                h.finalize()
            };
            let mut h = Sha256::new();
            h.update(&salt_pad);
            h.update(&inner);
            BigUint::from_bytes_be(&h.finalize())
        };

        let g_x   = g.modpow(&x, &n);
        let k_g_x = (&k * &g_x) % &n;
        let b_minus = if server_b > k_g_x {
            (&server_b - &k_g_x) % &n
        } else {
            (&n + &server_b - &k_g_x) % &n
        };
        let exp = &small_a + &u * &x;
        let s   = b_minus.modpow(&exp, &n);

        let s_pad = pad_hex_srp(&s);
        let u_pad = pad_hex_srp(&u);
        let hk = Hkdf::<Sha256>::new(Some(&u_pad), &s_pad);
        let mut hkdf_key = [0u8; 16];
        hk.expand(b"Caldera Derived Key", &mut hkdf_key).unwrap();

        assert_eq!(hex::encode(hkdf_key), "26621253c85f52c1a65a8ed884b1f5f7");

        // Signature: HMAC-SHA256(hkdf_key, pool || user_id || secret_block || timestamp)
        let secret_block = base64::engine::general_purpose::STANDARD
            .decode("ZmFrZXNlY3JldGJsb2NrMTIzNDU2Nzg5MGFiY2RlZiEh")
            .unwrap();
        let timestamp = "Thu Apr 17 12:00:00 UTC 2026";
        let mut mac = HmacSha256::new_from_slice(&hkdf_key).unwrap();
        mac.update(pool.as_bytes());
        mac.update(user.as_bytes());
        mac.update(&secret_block);
        mac.update(timestamp.as_bytes());
        let sig = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());

        assert_eq!(sig, "vDEuj1EDQPQJhSDzSgDqP6Cc7fKuGYOyHgTBrMkcoIs=");
    }

    // ── Timestamp format ─────────────────────────────────────────────────────

    fn cognito_timestamp(dt: chrono::DateTime<chrono::Utc>) -> String {
        format!("{}", dt.format("%a %b %-d %T UTC %Y"))
    }

    #[test]
    fn test_cognito_timestamp_single_digit_day() {
        use chrono::TimeZone;
        // Day 1: must be "1" not " 1" (no space padding)
        let dt = chrono::Utc.with_ymd_and_hms(2022, 1, 1, 0, 0, 0).unwrap();
        assert_eq!(cognito_timestamp(dt), "Sat Jan 1 00:00:00 UTC 2022");
        let dt = chrono::Utc.with_ymd_and_hms(2022, 1, 7, 9, 5, 3).unwrap();
        assert_eq!(cognito_timestamp(dt), "Fri Jan 7 09:05:03 UTC 2022");
    }

    #[test]
    fn test_cognito_timestamp_double_digit_day() {
        use chrono::TimeZone;
        let dt = chrono::Utc.with_ymd_and_hms(2022, 1, 10, 12, 0, 0).unwrap();
        assert_eq!(cognito_timestamp(dt), "Mon Jan 10 12:00:00 UTC 2022");
        let dt = chrono::Utc.with_ymd_and_hms(2022, 12, 31, 23, 59, 59).unwrap();
        assert_eq!(cognito_timestamp(dt), "Sat Dec 31 23:59:59 UTC 2022");
    }
}
