//! Twenty concurrent writers against one live server.
//!
//! Ported in 0.7 from step 6 of `scripts/acceptance-0.6.sh`, which went with
//! the code-graph job it lived in. That step was the only coverage anywhere of
//! many writers hitting one store a live server holds open, and nothing about
//! it was specific to code graphs: it asked that every write lands, none
//! errors, and the store verifies clean afterwards.
//!
//! The script's writers were `touch` processes, a subcommand 0.7 removed. Here
//! they are HTTP clients of `mushroomdb serve`, all released at once by a
//! barrier, so the server's own write path takes the contention. Plain
//! `std::net` rather than an HTTP client crate: one request per connection,
//! `Connection: close`, and the status line is all that is read.
use core_api::GraphDb;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

/// The number the acceptance step was specified in.
const WRITERS: usize = 20;

fn tmp(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "mdb-concurrency-{name}-{}-{nanos}",
        std::process::id()
    ))
}

/// A `mushroomdb serve` child, killed when dropped so a failed assertion
/// never leaves a server running.
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

/// Spawn `mushroomdb serve <dir> --addr 127.0.0.1:0 --no-ui` and wait for the
/// `listening on http://<addr>` line it prints once bound — the same
/// synchronisation `serve_memory_defaults.rs` uses — keeping the address.
fn serve(dir: &Path) -> Server {
    let mut child = Command::new(env!("CARGO_BIN_EXE_mushroomdb"))
        .arg("serve")
        .arg(dir)
        .arg("--addr")
        .arg("127.0.0.1:0")
        .arg("--no-ui")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mushroomdb serve");
    let stdout = child.stdout.take().expect("piped stdout");
    let mut server = Server {
        child,
        addr: String::new(),
    };
    let mut lines = BufReader::new(stdout).lines();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            Instant::now() < deadline,
            "mushroomdb serve never printed its listening line"
        );
        match lines.next() {
            Some(Ok(line)) => {
                if let Some(addr) = line.strip_prefix("listening on http://") {
                    server.addr = addr.trim().to_string();
                    return server;
                }
            }
            Some(Err(e)) => panic!("reading serve stdout: {e}"),
            None => panic!("mushroomdb serve exited before it started listening"),
        }
    }
}

/// One HTTP/1.1 request on its own connection; returns the status code and
/// the body.
fn request(addr: &str, method: &str, path: &str, body: Option<&str>) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).expect("connect to serve");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .expect("set read timeout");
    let body = body.unwrap_or("");
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).expect("send request");
    let mut raw = String::new();
    stream.read_to_string(&mut raw).expect("read response");
    let status = raw
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no status line in {raw:?}"));
    let body = raw
        .split_once("\r\n\r\n")
        .map_or(String::new(), |(_, b)| b.to_string());
    (status, body)
}

#[test]
fn twenty_concurrent_writers_against_a_live_server_all_land() {
    let dir = tmp("store");
    let server = serve(&dir);

    let barrier = Arc::new(Barrier::new(WRITERS));
    let writers: Vec<_> = (0..WRITERS)
        .map(|i| {
            let barrier = Arc::clone(&barrier);
            let addr = server.addr.clone();
            std::thread::spawn(move || {
                let body = format!(r#"{{"label":"Widget","key":"w{i}","props":{{"n":{i}}}}}"#);
                barrier.wait();
                request(&addr, "POST", "/nodes", Some(&body))
            })
        })
        .collect();
    let failed: Vec<(usize, u16, String)> = writers
        .into_iter()
        .enumerate()
        .map(|(i, h)| {
            let (status, body) = h.join().expect("writer thread panicked");
            (i, status, body)
        })
        .filter(|(_, status, _)| !(200..300).contains(status))
        .collect();
    assert!(
        failed.is_empty(),
        "{} of {WRITERS} concurrent writes failed: {failed:?}",
        failed.len()
    );

    // Every write is visible through the live server that took it.
    for i in 0..WRITERS {
        let (status, body) = request(&server.addr, "GET", &format!("/node/w{i}"), None);
        assert_eq!(
            status, 200,
            "w{i} not readable from the live server: {body}"
        );
    }

    // And durable: the server goes away, the store reopens with every node,
    // and `verify` passes on it.
    drop(server);
    {
        let db = GraphDb::open(&dir).expect("reopen the store after the server");
        for i in 0..WRITERS {
            assert!(
                db.has_node(&format!("w{i}")),
                "w{i} was acknowledged but did not survive the server"
            );
        }
    }
    // `verify` checks a snapshot against its WAL, so one is taken first: it
    // folds in everything the writers appended under contention.
    for step in ["snapshot", "verify"] {
        let out = Command::new(env!("CARGO_BIN_EXE_mushroomdb"))
            .arg(step)
            .arg(&dir)
            .output()
            .unwrap_or_else(|e| panic!("run mushroomdb {step}: {e}"));
        assert!(
            out.status.success(),
            "{step} failed after the concurrent writes:\n{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
