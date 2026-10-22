//! `mushroomdb serve --tx-max` (and `--tx-timeout-secs`, `--tx-max-lifetime-secs`)
//! set the HTTP transaction limits, each winning over its `MUSHROOMDB_TX_*`
//! variable; with no flag the variable still applies (TX-4 × TX-16).
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn tmp(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("mdb-txlim-{name}-{}-{nanos}", std::process::id()))
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

fn start(name: &str, flags: &[&str], env_max: Option<&str>) -> Server {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mushroomdb"));
    cmd.arg("serve")
        .arg(tmp(name))
        .args(["--addr", "127.0.0.1:0", "--no-ui"])
        .args(flags)
        .env_remove("MUSHROOMDB_TX_MAX")
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(v) = env_max {
        cmd.env("MUSHROOMDB_TX_MAX", v);
    }
    let child = cmd.spawn().expect("spawn mushroomdb serve");
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

/// Status code of `POST /tx/begin`.
fn begin(addr: &str) -> u16 {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(
        stream,
        "POST /tx/begin HTTP/1.1\r\nHost: {addr}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
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

/// How many begins succeed before the first 503, up to `cap`.
fn open_until_full(addr: &str, cap: usize) -> usize {
    for n in 0..cap {
        match begin(addr) {
            200 => {}
            503 => return n,
            other => panic!("POST /tx/begin answered {other}"),
        }
    }
    cap
}

#[test]
fn the_tx_max_flag_wins_over_mushroomdb_tx_max() {
    let server = start("flag", &["--tx-max", "2"], Some("5"));
    assert_eq!(open_until_full(&server.addr, 10), 2);
}

#[test]
fn without_the_flag_mushroomdb_tx_max_still_applies() {
    let server = start("env", &[], Some("3"));
    assert_eq!(open_until_full(&server.addr, 10), 3);
}

#[test]
fn a_zero_tx_flag_stops_serve_before_the_store_is_created() {
    let dir = tmp("zero");
    let out = Command::new(env!("CARGO_BIN_EXE_mushroomdb"))
        .arg("serve")
        .arg(&dir)
        .args(["--addr", "127.0.0.1:0", "--no-ui", "--tx-timeout-secs", "0"])
        .output()
        .expect("run mushroomdb serve");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--tx-timeout-secs"), "stderr: {stderr}");
    assert!(!dir.exists(), "store directory must not be created");
}
