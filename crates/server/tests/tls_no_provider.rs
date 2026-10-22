//! Regression test for ledger row 141: with two rustls crypto providers
//! compiled in (aws-lc-rs through axum-server, ring through the backup client
//! and, here, the dev-dependencies) and none installed, building the server's
//! TLS config panicked. This file is its own test binary, so nothing installs
//! a provider first; the shipped binary installs none either.
#![cfg(feature = "tls")]

use mushroomdb::SharedDb;
use std::collections::HashMap;

#[tokio::test]
async fn a_tls_server_starts_with_no_crypto_provider_installed() {
    // The precondition the panic needed: no process-level provider.
    assert!(
        rustls::crypto::CryptoProvider::get_default().is_none(),
        "this test binary must not install a provider"
    );
    let dir = tempfile::tempdir().unwrap();
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let cert_path = dir.path().join("cert.pem");
    let key_path = dir.path().join("key.pem");
    std::fs::write(&cert_path, cert.cert.pem()).unwrap();
    std::fs::write(&key_path, cert.key_pair.serialize_pem()).unwrap();

    assert!(server::load_server_config(&cert_path, &key_path).is_ok());

    let db = SharedDb::open(&dir.path().join("store")).unwrap();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let handle = tokio::spawn(server::serve_tls(
        db,
        "127.0.0.1:0".parse().unwrap(),
        ready_tx,
        cert_path,
        key_path,
        None,
        HashMap::new(),
    ));
    let addr = ready_rx.await.expect("the TLS server binds");
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();
    let res = client
        .get(format!("https://{addr}/health"))
        .send()
        .await
        .expect("a TLS handshake and a reply");
    assert!(res.status().is_success());
    // The helper chose a provider without installing one for the process.
    assert!(rustls::crypto::CryptoProvider::get_default().is_none());
    handle.abort();
}
