//! A write that returned survives the process dying (PF-06a).
//!
//! Evidence for the claim "a write that returned survives a process abort"
//! on the data-safety page. A real child process (this test binary, re-run
//! with an environment variable) opens a store, performs a write, and calls
//! `std::process::abort()` the moment the write returns, so no destructor,
//! flush, or snapshot runs. The parent then reopens the store and asserts the
//! node and its properties are there.
//!
//! What this does and does not prove: an abort kills the process but not the
//! operating system, so bytes the write handed to the kernel are still
//! delivered to the file. This proves the engine never keeps an acknowledged
//! write in process memory only. It does not prove power-loss durability,
//! which depends on `fsync` reaching the device and is not observable from a
//! process abort. It does not distinguish `FsyncPolicy` variants by sync
//! behaviour; it asserts that every policy keeps the acknowledged write
//! across an abort.
//!
//! Run in CI by the `format-compat` job:
//! `cargo test --release -p mushroomdb --test process_abort_durability -- write_survives_process_abort --ignored --nocapture`

use mushroomdb::{FsyncPolicy, GraphDb, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

const CHILD_ENV: &str = "MUSHROOMDB_ABORT_DURABILITY_CHILD";
const TEST_NAME: &str = "write_survives_process_abort";

#[derive(Clone, Copy, Debug)]
enum WriteShape {
    Single,
    Batch,
}

const POLICIES: [(&str, FsyncPolicy); 3] = [
    ("strict", FsyncPolicy::Strict),
    ("batched", FsyncPolicy::Batched),
    ("relaxed", FsyncPolicy::Relaxed),
];

const SHAPES: [(&str, WriteShape); 2] =
    [("single", WriteShape::Single), ("batch", WriteShape::Batch)];

fn node_key(shape: WriteShape, index: usize) -> String {
    format!("{shape:?}-node-{index}").to_lowercase()
}

fn props(index: usize) -> Vec<(String, Value)> {
    vec![
        ("name".into(), Value::Str(format!("survivor {index}"))),
        ("rank".into(), Value::Int(index as i64)),
    ]
}

/// Child side: write, then abort as soon as the write has returned.
fn write_then_abort(dir: &Path, policy: FsyncPolicy, shape: WriteShape) -> ! {
    let mut db = GraphDb::open(dir).expect("child: open");
    db.set_fsync_policy(policy);
    match shape {
        WriteShape::Single => {
            db.insert_node("SurvivalTest", &node_key(shape, 0), props(0))
                .expect("child: insert_node returned an error");
        }
        WriteShape::Batch => {
            db.write_batch(|b| {
                for index in 0..3 {
                    b.insert_node("SurvivalTest", &node_key(shape, index), props(index));
                }
            })
            .expect("child: write_batch returned an error");
        }
    }
    // Reached only after the write returned Ok. No drop, no flush, no snapshot.
    std::process::abort();
}

fn run_child_and_expect_abort(dir: &Path, policy_name: &str, shape_name: &str) {
    let status = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            TEST_NAME,
            "--ignored",
            "--test-threads=1",
            "--nocapture",
        ])
        .env(
            CHILD_ENV,
            format!("{}|{policy_name}|{shape_name}", dir.display()),
        )
        .status()
        .expect("spawn child");
    assert!(!status.success(), "child must die by abort, got {status}");
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(
            status.signal(),
            Some(6),
            "child must be killed by SIGABRT (the write returned and abort ran), got {status}"
        );
    }
}

fn assert_survived(dir: &Path, shape: WriteShape, label: &str) {
    let db = GraphDb::open(dir).unwrap_or_else(|e| panic!("{label}: reopen failed: {e:?}"));
    let count = match shape {
        WriteShape::Single => 1,
        WriteShape::Batch => 3,
    };
    for index in 0..count {
        let key = node_key(shape, index);
        assert!(db.has_node(&key), "{label}: node {key} lost after abort");
        assert_eq!(
            db.get_prop(&key, "name"),
            Some(Value::Str(format!("survivor {index}"))),
            "{label}: name of {key}"
        );
        assert_eq!(
            db.get_prop(&key, "rank"),
            Some(Value::Int(index as i64)),
            "{label}: rank of {key}"
        );
    }
}

fn fresh_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "mushroomdb-abort-durability-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[test]
#[ignore = "subprocess: a child process aborts after a write; run with --ignored in release (CI: format-compat)"]
fn write_survives_process_abort() {
    if let Ok(spec) = std::env::var(CHILD_ENV) {
        let mut parts = spec.split('|');
        let dir = PathBuf::from(parts.next().unwrap());
        let policy_name = parts.next().unwrap();
        let shape_name = parts.next().unwrap();
        let policy = POLICIES.iter().find(|(n, _)| *n == policy_name).unwrap().1;
        let shape = SHAPES.iter().find(|(n, _)| *n == shape_name).unwrap().1;
        write_then_abort(&dir, policy, shape);
    }

    for (policy_name, _) in POLICIES {
        for (shape_name, shape) in SHAPES {
            let label = format!("{policy_name}/{shape_name}");
            let dir = fresh_dir(&label.replace('/', "-"));
            run_child_and_expect_abort(&dir, policy_name, shape_name);
            assert_survived(&dir, shape, &label);
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
