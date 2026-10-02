//! Twenty concurrent writers against one store a live server holds open.
//!
//! Ported in 0.7 from step 6 of `scripts/acceptance-0.6.sh`, which went with
//! the code-graph job it lived in. That step was the only coverage anywhere of
//! many writers hitting one store while a server holds it, and nothing about
//! it was specific to code graphs: every write lands, none errors, and the
//! store verifies clean afterwards. Its writers were `touch` processes, a
//! subcommand 0.7 removed. Two legs replace them, each against a live
//! `mushroomdb serve`:
//!
//! - **In-process contention.** Twenty HTTP clients of `serve`, released at
//!   once by a barrier. Every write queues inside the one server process, so
//!   this covers its write queue and group commit, and never contends for the
//!   cross-process lock.
//! - **Cross-process contention.** Twenty separate `mushroomdb mcp <dir>`
//!   processes, started together, each sending one `upsert_entity` over stdio
//!   while `serve` holds the store. Each takes the store's cross-process write
//!   lock against the other nineteen and the server: the case the acceptance
//!   step's `touch` processes covered.
//!
//! Plain `std::net` rather than an HTTP client crate: one request per
//! connection, `Connection: close`, and the status line is all that is read.
use core_api::GraphDb;
use serde_json::{json, Value as Js};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc, Barrier};
use std::time::Duration;

/// The number the acceptance step was specified in.
const WRITERS: usize = 20;

/// A scratch store directory, removed when dropped, so a failed assertion
/// leaves nothing behind in the temp dir.
struct TempStore(PathBuf);

impl TempStore {
    fn new(name: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        TempStore(std::env::temp_dir().join(format!(
            "mdb-concurrency-{name}-{}-{nanos}",
            std::process::id()
        )))
    }
}

impl Drop for TempStore {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
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

/// Spawn `mushroomdb serve <dir> --addr 127.0.0.1:0 --no-ui` and wait, at most
/// ten seconds, for the `listening on http://<addr>` line it prints once
/// bound, keeping the address.
///
/// Stdout is read on its own thread and handed over a channel, so a server
/// that hangs before printing fails the test on the deadline instead of
/// blocking a `read`. The thread keeps draining stdout after the line, so the
/// server never blocks on a full pipe.
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
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            // The receiver is gone once the address is known; keep draining.
            let _ = tx.send(line);
        }
    });
    let mut server = Server {
        child,
        addr: String::new(),
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(left) {
            Ok(line) => {
                if let Some(addr) = line.strip_prefix("listening on http://") {
                    server.addr = addr.trim().to_string();
                    return server;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                panic!("mushroomdb serve printed no listening line within 10 s")
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!("mushroomdb serve exited before it started listening")
            }
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

/// Every key readable through the live server.
fn assert_readable_live(addr: &str, keys: &[String]) {
    for key in keys {
        let (status, body) = request(addr, "GET", &format!("/node/{key}"), None);
        assert_eq!(
            status, 200,
            "{key} not readable from the live server: {body}"
        );
    }
}

/// With the server gone: every key survives a reopen, and `snapshot` then
/// `verify` pass. `verify` checks a snapshot against its WAL, so one is taken
/// first; it folds in everything the writers appended under contention.
fn assert_durable_and_verified(dir: &Path, keys: &[String]) {
    {
        let db = GraphDb::open(dir).expect("reopen the store after the server");
        for key in keys {
            assert!(
                db.has_node(key),
                "{key} was acknowledged but did not survive the server"
            );
        }
    }
    for step in ["snapshot", "verify"] {
        let out = Command::new(env!("CARGO_BIN_EXE_mushroomdb"))
            .arg(step)
            .arg(dir)
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

#[test]
fn twenty_concurrent_writers_against_a_live_server_all_land() {
    let store = TempStore::new("in-process");
    let server = serve(&store.0);

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

    let keys: Vec<String> = (0..WRITERS).map(|i| format!("w{i}")).collect();
    assert_readable_live(&server.addr, &keys);
    drop(server);
    assert_durable_and_verified(&store.0, &keys);
}

/// What one `mushroomdb mcp` writer process did.
struct McpOutcome {
    exit_ok: bool,
    /// The `tools/call` response, if one came back.
    response: Option<Js>,
    stderr: String,
}

/// Spawn `mushroomdb mcp <dir>`, send `initialize`, the `initialized`
/// notification and one `upsert_entity` call, then close stdin — the EOF that
/// must make the server exit 0 — and collect its answer.
fn one_mcp_write(dir: &Path, key: &str, n: usize) -> McpOutcome {
    let mut child = Command::new(env!("CARGO_BIN_EXE_mushroomdb"))
        .arg("mcp")
        .arg(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mushroomdb mcp");
    let messages = [
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2024-11-05", "capabilities": {},
            "clientInfo": {"name": "mcp_concurrency", "version": "0"}}}),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
            "name": "upsert_entity",
            "arguments": {"key": key, "label": "Widget", "props": {"n": n}}}}),
    ];
    {
        let mut stdin = child.stdin.take().expect("piped stdin");
        for m in messages {
            writeln!(stdin, "{m}").expect("write to mcp stdin");
        }
    }
    let out = child.wait_with_output().expect("wait for mcp");
    let response = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| serde_json::from_str::<Js>(l).ok())
        .find(|m| m.get("id") == Some(&json!(2)));
    McpOutcome {
        exit_ok: out.status.success(),
        response,
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

#[test]
fn twenty_mcp_processes_write_while_serve_holds_the_store() {
    let store = TempStore::new("cross-process");
    let server = serve(&store.0);

    let barrier = Arc::new(Barrier::new(WRITERS));
    let writers: Vec<_> = (0..WRITERS)
        .map(|i| {
            let barrier = Arc::clone(&barrier);
            let dir = store.0.clone();
            std::thread::spawn(move || {
                barrier.wait();
                one_mcp_write(&dir, &format!("x{i}"), i)
            })
        })
        .collect();
    let outcomes: Vec<McpOutcome> = writers
        .into_iter()
        .map(|h| h.join().expect("writer thread panicked"))
        .collect();

    let failed: Vec<String> = outcomes
        .iter()
        .enumerate()
        .filter(|(_, o)| {
            let tool_ok = o.response.as_ref().is_some_and(|r| {
                r.get("error").is_none() && !r["result"]["isError"].as_bool().unwrap_or(false)
            });
            !(o.exit_ok && tool_ok)
        })
        .map(|(i, o)| {
            format!(
                "x{i}: exit_ok={} response={:?} stderr={}",
                o.exit_ok,
                o.response.as_ref().map(ToString::to_string),
                o.stderr.trim()
            )
        })
        .collect();
    assert!(
        failed.is_empty(),
        "{} of {WRITERS} mcp writer processes failed:\n{}",
        failed.len(),
        failed.join("\n")
    );

    let keys: Vec<String> = (0..WRITERS).map(|i| format!("x{i}")).collect();
    assert_readable_live(&server.addr, &keys);
    drop(server);
    assert_durable_and_verified(&store.0, &keys);
}
