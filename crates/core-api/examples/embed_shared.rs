//! One store, several threads: `SharedDb`.
//! Quoted on docs/site/rust.md. Run: cargo run -p mushroomdb --example embed_shared

use core_api::{BatchOp, SharedDb, Value};
use std::collections::BTreeMap;

fn main() -> core_api::Result<()> {
    let dir = std::env::temp_dir().join(format!("mushroomdb-embed-shared-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    // docs:start
    let db = SharedDb::open(&dir)?;

    // Writers on any thread; concurrent batches share one fsync.
    let writers: Vec<_> = (0..4)
        .map(|n| {
            let db = db.clone();
            std::thread::spawn(move || {
                db.submit_batch(vec![BatchOp::InsertNode {
                    label: "Job".into(),
                    key: format!("job-{n}"),
                    props: vec![("n".into(), Value::Int(n))],
                }])
            })
        })
        .collect();
    for writer in writers {
        writer.join().expect("writer thread")?;
    }

    // Readers take a guard, never wait on each other, and see every commit.
    let rows = db
        .read()
        .query("MATCH (j:Job) RETURN j", &BTreeMap::new())?;
    assert_eq!(rows.len(), 4);
    // docs:end

    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}
