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

fn serve(name: &str, format: Option<&str>) -> std::process::Child {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mushroomdb"));
    cmd.arg("serve")
        .arg(tmp(name))
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

fn first_stderr_line(child: &mut std::process::Child) -> String {
    let stderr = child.stderr.take().unwrap();
    let line = BufReader::new(stderr)
        .lines()
        .next()
        .expect("serve wrote nothing to stderr")
        .unwrap();
    let _ = child.kill();
    let _ = child.wait();
    line
}

#[test]
fn json_log_format_env_var() {
    let mut child = serve("json", Some("json"));
    let line = first_stderr_line(&mut child);
    let value: serde_json::Value =
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("not JSON ({e}): {line}"));
    assert_eq!(value["fields"]["message"], "listening", "{line}");
    assert_eq!(value["level"], "INFO", "{line}");
}

#[test]
fn default_log_format_stays_text() {
    let mut child = serve("text", None);
    let line = first_stderr_line(&mut child);
    assert!(serde_json::from_str::<serde_json::Value>(&line).is_err());
    assert!(line.contains("listening"), "{line}");
}

#[test]
fn unknown_log_format_is_a_startup_error() {
    let mut child = serve("bad", Some("yaml"));
    let line = first_stderr_line_no_kill(&mut child);
    assert!(line.contains("MUSHROOMDB_LOG_FORMAT"), "{line}");
    assert_eq!(child.wait().unwrap().code(), Some(2));
}

fn first_stderr_line_no_kill(child: &mut std::process::Child) -> String {
    let stderr = child.stderr.take().unwrap();
    BufReader::new(stderr).lines().next().unwrap().unwrap()
}
