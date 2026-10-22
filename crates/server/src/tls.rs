//! The TLS configuration both listeners serve (cross-plan contract C4): one
//! certificate chain and key, read from `--tls-cert` / `--tls-key`.
//!
//! The config is protocol-neutral: it advertises no ALPN protocols. The HTTP
//! listener adds `h2` and `http/1.1` to its copy; Bolt negotiates none.
//!
//! The crypto provider is the one the process installed, if any, and otherwise
//! aws-lc-rs. It is chosen here rather than left to rustls because a binary that
//! compiles in both ring and aws-lc-rs (the backup client brings ring) has no
//! default rustls can pick, and building a config would panic.

use std::path::Path;
use std::sync::Arc;

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

/// Reads a PEM certificate chain and a PEM private key and builds the server
/// config. The error names the file and what was wrong with it.
pub fn load_server_config(cert: &Path, key: &Path) -> Result<Arc<rustls::ServerConfig>, String> {
    let cert_pem =
        std::fs::read(cert).map_err(|e| format!("--tls-cert {}: {}", cert.display(), e.kind()))?;
    let key_pem =
        std::fs::read(key).map_err(|e| format!("--tls-key {}: {}", key.display(), e.kind()))?;
    let chain = CertificateDer::pem_slice_iter(&cert_pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("--tls-cert {}: not a PEM certificate: {e}", cert.display()))?;
    if chain.is_empty() {
        return Err(format!(
            "--tls-cert {}: the file holds no PEM certificate",
            cert.display()
        ));
    }
    // Exactly one key: a file with two is an operator mistake, and taking the
    // first would serve whichever happens to come first.
    let mut keys = PrivateKeyDer::pem_slice_iter(&key_pem);
    let private_key = match keys.next() {
        Some(Ok(first)) => first,
        Some(Err(e)) => {
            return Err(format!(
                "--tls-key {}: not a PEM private key: {e}",
                key.display()
            ))
        }
        None => {
            return Err(format!(
                "--tls-key {}: the file holds no PEM private key",
                key.display()
            ))
        }
    };
    if keys.next().is_some() {
        return Err(format!(
            "--tls-key {}: the file holds more than one private key; give it exactly one",
            key.display()
        ));
    }
    let provider = rustls::crypto::CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| Arc::new(rustls::crypto::aws_lc_rs::default_provider()));
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("TLS: {e}"))?
        .with_no_client_auth()
        .with_single_cert(chain, private_key)
        .map_err(|e| format!("--tls-cert/--tls-key: the key does not fit the certificate: {e}"))?;
    Ok(Arc::new(config))
}

/// [`load_server_config`] with the HTTP ALPN protocols, for axum-server.
pub(crate) fn http_rustls_config(
    cert: &Path,
    key: &Path,
) -> std::io::Result<axum_server::tls_rustls::RustlsConfig> {
    let shared = load_server_config(cert, key)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let mut config = (*shared).clone();
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(axum_server::tls_rustls::RustlsConfig::from_config(
        Arc::new(config),
    ))
}
