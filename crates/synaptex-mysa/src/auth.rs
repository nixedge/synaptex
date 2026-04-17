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
    let salt_bytes = hex::decode(salt_hex).context("decode SALT hex")?;

    // u = SHA256(padHex(A) || padHex(B))
    let (u_hash, u) = {
        let mut h = Sha256::new();
        h.update(&pad_hex_srp(&big_a));
        h.update(&pad_hex_srp(&b));
        let hash = h.finalize();
        let u = BigUint::from_bytes_be(&hash);
        (hash, u)
    };

    // x = SHA256(salt || SHA256(pool_id_short || user_id_srp || ":" || password))
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
        h.update(&salt_bytes);
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

    // HKDF key = HKDF-SHA256(ikm=padHex(S), salt=u_hash, info="Caldera Derived Key")[0..16]
    let s_pad = pad_hex_srp(&s);
    let hk = Hkdf::<Sha256>::new(Some(&u_hash), &s_pad);
    let mut hkdf_key = [0u8; 16];
    hk.expand(b"Caldera Derived Key", &mut hkdf_key)
        .map_err(|e| anyhow::anyhow!("HKDF expand: {e}"))?;

    // Timestamp in Cognito's required format: "Www Mmm  D HH:MM:SS UTC YYYY"
    let timestamp = {
        let now = Utc::now();
        format!("{}", now.format("%a %b %e %T UTC %Y"))
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
