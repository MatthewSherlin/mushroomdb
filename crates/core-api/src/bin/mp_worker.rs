//! Helper process for `tests/multiprocess.rs`.
//!
//! Built only with `--features mp-test`; the test suite locates it through
//! `env!("CARGO_BIN_EXE_mp_worker")`.  Every subcommand opens the store the
//! way a real out-of-process writer would, so the tests exercise the actual
//! cross-process locking and WAL-tailing paths rather than a simulation.
//!
//! ```text
//! mp_worker <dir> write <prefix> <n>   append n nodes named "<prefix>-<i>"
//! mp_worker <dir> read                 print the live node count
//! mp_worker <dir> ro-read              read-only open; print the live node count
//! mp_worker <dir> busy <wait_ms>       try to take the write lock; exit 3 on Busy
//! mp_worker <dir> snapshot             take a snapshot (plain handle-lifetime lock)
//! mp_worker <dir> snapshot-shared      snapshot through a SharedDb write guard
//! ```
//!
//! Exit codes: `0` success, `3` [`GraphError::Busy`], `1` any other failure.

use core_api::{BatchOp, GraphDb, OpenOptions, SharedDb};
use core_storage::GraphError;
use std::path::Path;
use std::time::Duration;

/// Submissions per group-commit batch.  Small enough that two concurrent
/// writers interleave many times, large enough that the test does not pay one
/// fsync per node.
const CHUNK: usize = 50;

/// How many times a chunk retries after `Busy` before giving up.
///
/// Contention with another writer is expected here; only a peer that holds the
/// lock through every retry is a real failure.
const WRITE_RETRIES: usize = 8;

/// The insert ops for nodes `lo..hi` of this worker's prefix.
fn chunk_ops(prefix: &str, lo: usize, hi: usize) -> Vec<BatchOp> {
    (lo..hi)
        .map(|k| BatchOp::InsertNode {
            label: "Person".into(),
            key: format!("{prefix}-{k}"),
            props: vec![("team".into(), core_api::Value::Str(prefix.into()))],
        })
        .collect()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: mp_worker <dir> <command> [args...]");
        std::process::exit(2);
    }
    let dir = Path::new(&args[1]);
    let cmd = args[2].as_str();

    let code = match cmd {
        "write" => {
            let prefix = args.get(3).map(String::as_str).unwrap_or("n");
            let n: usize = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(0);
            run_write(dir, prefix, n)
        }
        "read" => run_read(dir),
        "ro-read" => run_ro_read(dir),
        "busy" => {
            let ms: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(500);
            run_busy(dir, Duration::from_millis(ms))
        }
        "snapshot" => run_snapshot(dir),
        "snapshot-shared" => run_snapshot_shared(dir),
        other => {
            eprintln!("unknown command: {other}");
            std::process::exit(2);
        }
    };
    std::process::exit(code);
}

/// Map a failure to this process's exit code: `Busy` is distinguishable so the
/// parent test can assert on the contention outcome specifically.
fn exit_code(e: &GraphError) -> i32 {
    match e {
        GraphError::Busy { .. } => 3,
        _ => 1,
    }
}

fn run_write(dir: &Path, prefix: &str, n: usize) -> i32 {
    let db = match SharedDb::open(dir) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("open: {e}");
            return exit_code(&e);
        }
    };
    let mut i = 0usize;
    while i < n {
        let hi = (i + CHUNK).min(n);
        let ops = chunk_ops(prefix, i, hi);
        // `Busy` means a peer held the store lock for the whole wait, which on
        // a loaded machine says nothing about correctness. Retry with a bounded
        // backoff so contention shows up as slowness, not as a failed worker;
        // any other error is real and exits.
        let mut ops = Some(ops);
        let mut attempt = 0;
        loop {
            match db.submit_batch(ops.take().expect("ops present on each attempt")) {
                Ok(_) => break,
                Err(GraphError::Busy { .. }) if attempt < WRITE_RETRIES => {
                    attempt += 1;
                    std::thread::sleep(Duration::from_millis(20 * attempt as u64));
                    ops = Some(chunk_ops(prefix, i, hi));
                }
                Err(e) => {
                    eprintln!("submit_batch: {e}");
                    return exit_code(&e);
                }
            }
        }
        i = hi;
    }
    0
}

fn run_read(dir: &Path) -> i32 {
    match SharedDb::open(dir) {
        Ok(db) => {
            println!("{}", db.read().node_count());
            0
        }
        Err(e) => {
            eprintln!("open: {e}");
            exit_code(&e)
        }
    }
}

fn run_ro_read(dir: &Path) -> i32 {
    let opts = OpenOptions {
        read_only: true,
        ..OpenOptions::default()
    };
    match GraphDb::open_with_options(dir, opts) {
        Ok(db) => {
            println!("{}", db.node_count());
            0
        }
        Err(e) => {
            eprintln!("open: {e}");
            exit_code(&e)
        }
    }
}

fn run_busy(dir: &Path, wait: Duration) -> i32 {
    let db = match SharedDb::open(dir) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("open: {e}");
            return exit_code(&e);
        }
    };
    // Bound the guard's lifetime inside the function body: it borrows `db`, and
    // a tail expression would outlive it.
    let code = match db.write_with_wait(wait) {
        Ok(mut guard) => match guard.insert_node("Person", "busy-probe", vec![]) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("insert: {e}");
                exit_code(&e)
            }
        },
        Err(e) => {
            eprintln!("write_with_wait: {e}");
            exit_code(&e)
        }
    };
    code
}

fn run_snapshot(dir: &Path) -> i32 {
    match GraphDb::open(dir) {
        Ok(mut db) => match db.snapshot() {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("snapshot: {e}");
                exit_code(&e)
            }
        },
        Err(e) => {
            eprintln!("open: {e}");
            exit_code(&e)
        }
    }
}

/// Snapshot the way a server does: through a `SharedDb` write guard, which
/// returns a guard even when the cross-process lock was denied.
///
/// A snapshot replaces `wal.bin`, so running one without the lock destroys a
/// peer's acknowledged commits. This is the path that must refuse.
fn run_snapshot_shared(dir: &Path) -> i32 {
    let db = match SharedDb::open(dir) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("open: {e}");
            return exit_code(&e);
        }
    };
    let code = match db.write().snapshot() {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("snapshot: {e}");
            exit_code(&e)
        }
    };
    code
}
