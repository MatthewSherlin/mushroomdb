#![cfg(feature = "tls")]

use core_api::SharedDb;
use std::collections::HashMap;

fn tmp_tls(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("graphdb-tls-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

#[tokio::test]
async fn tls_serves_https_and_rejects_plain_http() {
    // Both ring and aws-lc-rs may be compiled in; pick ring explicitly.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let dir = tempfile::tempdir().unwrap();
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let cert_path = dir.path().join("cert.pem");
    let key_path = dir.path().join("key.pem");
    std::fs::write(&cert_path, cert.cert.pem()).unwrap();
    std::fs::write(&key_path, cert.key_pair.serialize_pem()).unwrap();

    let db = SharedDb::open(&tmp_tls("serve")).unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let handle = tokio::spawn(server::serve_tls(
        db,
        "127.0.0.1:0".parse().unwrap(),
        tx,
        cert_path,
        key_path,
        None,
        HashMap::new(),
    ));
    let addr = rx.await.unwrap();

    // HTTPS works (self-signed → accept invalid certs; we test transport, not PKI)
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();
    let res = client
        .get(format!("https://{addr}/health"))
        .send()
        .await
        .unwrap();
    assert!(res.status().is_success());

    // Plain HTTP against the TLS port fails at the protocol level
    let plain = reqwest::get(format!("http://{addr}/health")).await;
    assert!(plain.is_err() || !plain.unwrap().status().is_success());

    // /health is unauthenticated and returns JSON — no Set-Cookie header.
    assert!(res.headers().get("set-cookie").is_none(), "health must not set cookies");

    handle.abort();
}
