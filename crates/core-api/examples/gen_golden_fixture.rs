//! Regenerate the golden snapshot fixture for the *current* VERSION.
//!
//! ```text
//! cargo run -p mushroomdb --example gen_golden_fixture -- crates/core-api/tests/fixtures/golden_v7.bin
//! ```
//!
//! Builds the same tiny graph the `golden_v5_pin` / `golden_v6_pin` /
//! `golden_v7_pin` tests assert (2 nodes, 1 edge, prop `a.v=42`), snapshots
//! it, and copies the snapshot file to the given path. Run this ONLY when
//! introducing a new snapshot VERSION — never to "fix" a failing pin test,
//! which exists precisely to catch unintended byte-format drift.

use core_api::{GraphDb, Value};

fn main() {
    let out = std::env::args()
        .nth(1)
        .expect("usage: gen_golden_fixture <output-path>");
    let dir = std::env::temp_dir().join(format!("gen-golden-fixture-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let mut db = GraphDb::open(&dir).expect("open");
        db.insert_node("N", "a", vec![("v".into(), Value::Int(42))])
            .expect("insert a");
        db.insert_node("N", "b", vec![]).expect("insert b");
        db.insert_edge("E", "a", "b").expect("insert edge");
        db.snapshot().expect("snapshot");
    }
    std::fs::copy(dir.join("snapshot.bin"), &out).expect("copy fixture");
    let _ = std::fs::remove_dir_all(&dir);
    println!("wrote {out}");
}
