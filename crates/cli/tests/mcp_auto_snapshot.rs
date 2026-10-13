//! `mushroomdb mcp` snapshots a store whose WAL has grown.
//!
//! An open replays everything written since the last snapshot. `serve` takes
//! one on a timer and at shutdown, and a git ingest takes one when its WAL
//! passes [`SNAPSHOT_WAL_BYTES`]; a store used only through the MCP server —
//! which is how an assistant's memory is used — was never snapshotted by
//! anything, so every session opened it by replaying its whole history.
//!
//! The MCP server now takes the automatic snapshot when it opens a store
//! whose WAL is past that size, and again when its session ends. At open as
//! well as at the end, because a host is as likely to kill the process as to
//! close its stdin.
//!
//! [`SNAPSHOT_WAL_BYTES`]: cli::ingest_git::SNAPSHOT_WAL_BYTES
use cli::ingest_git::SNAPSHOT_WAL_BYTES;
use core_api::{GraphDb, Value};
use serde_json::{json, Value as Js};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

struct TempStore(PathBuf);

impl TempStore {
    fn new(name: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        TempStore(std::env::temp_dir().join(format!(
            "mdb-mcp-snapshot-{name}-{}-{nanos}",
            std::process::id()
        )))
    }
}

impl Drop for TempStore {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A running `mushroomdb mcp <dir>`, killed when dropped.
struct Session {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl Session {
    fn start(dir: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_mushroomdb"))
            .arg("mcp")
            .arg(dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn mushroomdb mcp");
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Session {
            child,
            stdin,
            stdout,
        }
    }

    /// Send one request and read its one-line answer.
    fn call(&mut self, request: &Js) -> Js {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{request}").unwrap();
        stdin.flush().unwrap();
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        serde_json::from_str(&line).expect("a JSON-RPC answer")
    }

    /// Close stdin, which ends the session, and wait for the process.
    fn end(mut self) {
        drop(self.stdin.take());
        let status = self.child.wait().unwrap();
        assert!(status.success(), "mushroomdb mcp exited with {status}");
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn tools_list() -> Js {
    json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"})
}

fn wal_len(dir: &Path) -> u64 {
    std::fs::metadata(dir.join("wal.bin")).map_or(0, |m| m.len())
}

fn archives(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().ends_with(".archive"))
        .count()
}

/// Write `nodes` nodes of a kibibyte each, in batches.
fn fill(dir: &Path, nodes: usize) {
    let mut db = GraphDb::open(dir).unwrap();
    let text = "x".repeat(1024);
    for chunk in 0..nodes.div_ceil(500) {
        let mut b = db.batch();
        for i in chunk * 500..((chunk + 1) * 500).min(nodes) {
            b.insert_node(
                "Doc",
                &format!("d{i}"),
                vec![("body".into(), Value::Str(text.clone()))],
            );
        }
        b.commit().unwrap();
    }
}

fn node_count(dir: &Path) -> usize {
    GraphDb::open(dir).unwrap().stats().nodes_live
}

#[test]
fn a_store_whose_wal_has_grown_is_snapshotted_when_mcp_opens_it() {
    let store = TempStore::new("open");
    fill(&store.0, 6000);
    assert!(wal_len(&store.0) > SNAPSHOT_WAL_BYTES, "the setup's WAL");
    assert!(!store.0.join("snapshot.bin").exists());

    let mut session = Session::start(&store.0);
    // Answered, so the open is over; the process is still running.
    assert!(session.call(&tools_list())["result"].is_object());
    assert!(
        store.0.join("snapshot.bin").exists(),
        "no snapshot after the open"
    );
    assert!(wal_len(&store.0) < SNAPSHOT_WAL_BYTES);
    // Archived, not dropped: what the store remembers is kept.
    assert_eq!(archives(&store.0), 1);
    session.end();

    assert_eq!(node_count(&store.0), 6000);
    assert_eq!(archives(&store.0), 1, "the end of the session took another");
}

#[test]
fn a_small_store_is_left_as_it_is_by_an_mcp_session() {
    let store = TempStore::new("small");
    fill(&store.0, 20);
    let before = wal_len(&store.0);

    let mut session = Session::start(&store.0);
    assert!(session.call(&tools_list())["result"].is_object());
    session.end();

    assert!(!store.0.join("snapshot.bin").exists());
    assert_eq!(archives(&store.0), 0);
    assert_eq!(wal_len(&store.0), before);
}

#[test]
fn a_session_that_grows_the_wal_snapshots_when_it_ends() {
    let store = TempStore::new("end");
    let mut session = Session::start(&store.0);
    let text = "x".repeat(1024);
    let rows: Vec<Js> = (0..6000)
        .map(|i| json!({"id": format!("d{i}"), "body": text}))
        .collect();
    let answer = session.call(&json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "ingest_json",
            "arguments": {"label": "Doc", "rows_json": Js::Array(rows).to_string()},
        },
    }));
    assert!(answer["result"]["isError"].is_null(), "{answer}");
    assert!(wal_len(&store.0) > SNAPSHOT_WAL_BYTES, "the session's WAL");
    assert!(!store.0.join("snapshot.bin").exists());
    session.end();

    assert!(
        store.0.join("snapshot.bin").exists(),
        "no snapshot after the session"
    );
    assert!(wal_len(&store.0) < SNAPSHOT_WAL_BYTES);
    assert_eq!(archives(&store.0), 1);
    assert_eq!(node_count(&store.0), 6000);
}
