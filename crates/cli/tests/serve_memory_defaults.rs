//! `mushroomdb serve` gets the same memory-schema treatment `mcp` does: a
//! store it creates is searchable from the start (`recall` over the HTTP
//! surface has the same "only Note.text is indexed" defect `mcp` had), and a
//! store it merely reopens is never silently upgraded — declaring full-text
//! on a populated store rebuilds the index at open (227 ms vs. 3.8 ms with
//! none, ledger row 36).
use core_api::GraphDb;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

fn tmp(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "mdb-servedefaults-{name}-{}-{nanos}",
        std::process::id()
    ))
}

/// Spawn `mushroomdb serve <dir> --addr 127.0.0.1:0 --no-ui`, wait for the
/// `listening on http://...` line it prints once bound (proof the schema
/// step — which runs earlier in `run_serve`, before the listener binds — has
/// already happened), then kill it.
fn serve_until_listening(dir: &PathBuf) {
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
    let mut lines = BufReader::new(stdout).lines();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            std::time::Instant::now() < deadline,
            "mushroomdb serve never printed its listening line"
        );
        match lines.next() {
            Some(Ok(line)) if line.starts_with("listening on") => break,
            Some(Ok(_)) => continue,
            Some(Err(e)) => panic!("reading serve stdout: {e}"),
            None => panic!("mushroomdb serve exited before it started listening"),
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn a_store_serve_creates_gets_the_memory_schema() {
    let dir = tmp("new");
    serve_until_listening(&dir);
    let db = GraphDb::open(&dir).expect("reopen the store serve created");
    assert!(
        db.fulltext_pairs().iter().any(|(l, _)| l == "Person"),
        "a store `serve` created must carry the memory schema: {:?}",
        db.fulltext_pairs()
    );
}

#[test]
fn an_existing_store_is_not_upgraded_by_serve() {
    let dir = tmp("existing");
    {
        let mut db = GraphDb::open(&dir).expect("create a store directly, no schema applied");
        db.insert_node("Widget", "w1", vec![]).expect("insert");
    }
    serve_until_listening(&dir);
    let db = GraphDb::open(&dir).expect("reopen the pre-existing store");
    assert!(
        db.fulltext_pairs().is_empty(),
        "serve must never silently upgrade an existing store: {:?}",
        db.fulltext_pairs()
    );
}
