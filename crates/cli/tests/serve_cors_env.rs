//! An invalid `MUSHROOMDB_CORS_ORIGINS` stops `serve` at startup, before the
//! store is created.
use std::process::Command;

#[test]
fn invalid_cors_origins_exit_1_before_the_store_is_created() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("mdb-cors-env-{}-{nanos}", std::process::id()));
    let out = Command::new(env!("CARGO_BIN_EXE_mushroomdb"))
        .arg("serve")
        .arg(&dir)
        .arg("--addr")
        .arg("127.0.0.1:0")
        .arg("--no-ui")
        .env(
            "MUSHROOMDB_CORS_ORIGINS",
            "https://ok.example,not-an-origin",
        )
        .output()
        .expect("run mushroomdb serve");
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("MUSHROOMDB_CORS_ORIGINS") && stderr.contains("not-an-origin"),
        "stderr: {stderr}"
    );
    assert!(!dir.exists(), "store directory must not be created");
}
