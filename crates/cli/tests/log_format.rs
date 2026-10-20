//! `MUSHROOMDB_LOG_FORMAT` for `mushroomdb serve`: `json` gives one JSON
//! object per stderr line, unset keeps text, and an unknown value is a
//! startup error rather than silently-wrong output.
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn tmp(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("mdb-logfmt-{name}-{}-{nanos}", std::process::id()))
}

fn serve(dir: &PathBuf, format: Option<&str>) -> std::process::Child {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mushroomdb"));
    cmd.arg("serve")
        .arg(dir)
        .args(["--addr", "127.0.0.1:0", "--no-ui"])
        .env_remove("MUSHROOMDB_LOG")
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    match format {
        Some(f) => cmd.env("MUSHROOMDB_LOG_FORMAT", f),
        None => cmd.env_remove("MUSHROOMDB_LOG_FORMAT"),
    };
    cmd.spawn().expect("spawn mushroomdb serve")
}

/// Read the first stderr line, failing (not hanging) after 20 s.
fn first_stderr_line(child: &mut std::process::Child) -> String {
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(BufReader::new(stderr).lines().next());
    });
    let received = rx.recv_timeout(std::time::Duration::from_secs(20));
    let _ = child.kill();
    let _ = child.wait();
    received
        .expect("no stderr line within 20 s")
        .expect("serve wrote nothing to stderr")
        .unwrap()
}

#[test]
fn json_log_format_env_var() {
    let dir = tmp("json");
    let mut child = serve(&dir, Some("json"));
    let line = first_stderr_line(&mut child);
    let _ = std::fs::remove_dir_all(&dir);
    let value: serde_json::Value =
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("not JSON ({e}): {line}"));
    assert_eq!(value["fields"]["message"], "listening", "{line}");
    assert_eq!(value["level"], "INFO", "{line}");
}

#[test]
fn default_log_format_stays_text() {
    let dir = tmp("text");
    let mut child = serve(&dir, None);
    let line = first_stderr_line(&mut child);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(serde_json::from_str::<serde_json::Value>(&line).is_err());
    assert!(line.contains("listening"), "{line}");
}

#[test]
fn unknown_log_format_is_a_startup_error_and_touches_no_store() {
    let dir = tmp("bad");
    let mut child = serve(&dir, Some("yaml"));
    let line = first_stderr_line(&mut child);
    assert!(line.contains("MUSHROOMDB_LOG_FORMAT"), "{line}");
    let created = dir.exists();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        !created,
        "an invalid format must exit before creating the store"
    );
}
