use std::path::Path;

use anyhow::Result;
use rcgen::generate_simple_self_signed;
use tracing::info;

/// Load a TLS certificate and private key from `cert_path` / `key_path`.
/// If either file is missing, a new self-signed certificate is generated and
/// written to both paths.
///
/// Returns `(cert_pem, key_pem)` as raw bytes suitable for
/// `tonic::transport::Identity::from_pem`.
pub fn load_or_generate(cert_path: &Path, key_path: &Path) -> Result<(Vec<u8>, Vec<u8>)> {
    if cert_path.exists() && key_path.exists() {
        let cert = std::fs::read(cert_path)?;
        let key  = std::fs::read(key_path)?;
        return Ok((cert, key));
    }

    info!("no TLS certificate found — generating self-signed certificate");

    // Subject Alternative Name: used by clients to verify the server identity.
    // "synaptex-router" is the domain name core will use in ClientTlsConfig.
    let cert = generate_simple_self_signed(vec!["synaptex-router".to_string()])?;
    let cert_pem = cert.serialize_pem()?;
    let key_pem  = cert.serialize_private_key_pem();

    std::fs::write(cert_path, &cert_pem)?;
    std::fs::write(key_path, &key_pem)?;

    info!(
        cert = %cert_path.display(),
        key  = %key_path.display(),
        "TLS certificate written — copy {} to synaptex-core as the router CA",
        cert_path.display(),
    );

    Ok((cert_pem.into_bytes(), key_pem.into_bytes()))
}
