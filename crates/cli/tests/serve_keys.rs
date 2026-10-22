//! `mushroomdb serve` reads `keys.json` from the store directory (HO-04): the
//! tiers apply over real HTTP, SIGHUP re-reads the file, and a file that does
//! not parse stops the start.
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn tmp(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("mdb-keys-{name}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct Server {
    child: Child,
    addr: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start(dir: &Path) -> Server {
    let child = Command::new(env!("CARGO_BIN_EXE_mushroomdb"))
        .arg("serve")
        .arg(dir)
        .arg("--addr")
        .arg("127.0.0.1:0")
        .arg("--no-ui")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mushroomdb serve");
    // Owned by `Server` from here, so every exit below kills and reaps it.
    let mut server = Server {
        child,
        addr: String::new(),
    };
    let stdout = server.child.stdout.take().expect("piped stdout");
    let mut lines = BufReader::new(stdout).lines();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        assert!(Instant::now() < deadline, "serve never started listening");
        match lines.next() {
            Some(Ok(line)) => {
                if let Some(rest) = line.strip_prefix("listening on http://") {
                    server.addr = rest.split_whitespace().next().unwrap().to_string();
                    return server;
                }
            }
            _ => panic!("serve exited before it started listening"),
        }
    }
}

/// Status code of `GET path` with `Authorization: Bearer token`.
fn status(addr: &str, path: &str, token: &str) -> u16 {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut reply = String::new();
    stream.read_to_string(&mut reply).unwrap();
    reply
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("no status line in {reply:?}"))
}

fn wait_for(addr: &str, path: &str, token: &str, want: u16) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if status(addr, path, token) == want {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

const KEYS: &str = r#"{"keys":[
    {"token":"adm-key","tier":"admin"},
    {"token":"wr-key","tier":"write"}]}"#;

#[test]
fn serve_applies_the_key_tiers_and_reloads_them_on_sighup() {
    let dir = tmp("tiers");
    std::fs::write(dir.join("keys.json"), KEYS).unwrap();
    let server = start(&dir);

    assert_eq!(status(&server.addr, "/stats", "adm-key"), 200);
    assert_eq!(status(&server.addr, "/stats", "wr-key"), 403);
    assert_eq!(status(&server.addr, "/stats", "unknown"), 401);

    std::fs::write(
        dir.join("keys.json"),
        r#"{"keys":[{"token":"new-key","tier":"admin"}]}"#,
    )
    .unwrap();
    assert_eq!(
        status(&server.addr, "/stats", "new-key"),
        401,
        "not read until SIGHUP"
    );
    let signalled = Command::new("kill")
        .arg("-HUP")
        .arg(server.child.id().to_string())
        .status()
        .unwrap();
    assert!(signalled.success());
    assert!(
        wait_for(&server.addr, "/stats", "new-key", 200),
        "the new key never started working"
    );
    assert_eq!(status(&server.addr, "/stats", "adm-key"), 401);
    assert_eq!(status(&server.addr, "/stats", "wr-key"), 401);
}

#[test]
fn an_invalid_keys_file_stops_serve_before_the_store_is_opened() {
    let dir = tmp("invalid");
    std::fs::write(
        dir.join("keys.json"),
        r#"{"keys":[{"token":"SECRET-TOKEN-1","tier":"superuser"}]}"#,
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_mushroomdb"))
        .arg("serve")
        .arg(&dir)
        .arg("--addr")
        .arg("127.0.0.1:0")
        .arg("--no-ui")
        .output()
        .expect("run mushroomdb serve");
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("keys.json"), "stderr: {stderr}");
    assert!(!stderr.contains("SECRET-TOKEN-1"), "a token was echoed");
    assert!(
        !dir.join("wal.bin").exists(),
        "the store must not have been opened"
    );
}
