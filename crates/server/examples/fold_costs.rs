//! Writer-side costs around a fold — the TX-5 fix-round-3 harness.
//!
//! On a `memory_defaults()` store of N `Person` nodes (`name` full-text and
//! equality indexed), times single-property commits through `db.write()`
//! (Relaxed fsync, to measure CPU and lock-hold rather than the disk) and
//! classifies each one:
//!
//! - `fold`: the commit that folded (the reader's tail is empty after it);
//! - `first`: the first commit after a fold, which copies every structure
//!   the fold shares and the write touches (ledger row 126);
//! - `steady`: every other commit.
//!
//! Two passes: writing `name` (indexed: the index is copied too) and writing
//! `score` (not indexed). The difference between their `first` medians is the
//! index copy. Also times one 10k-node bulk commit.
//!
//! ```text
//! cargo run --release -p mushroomdb-server --example fold_costs -- <nodes> [label]
//! ```

use mushroomdb::{BatchOp, FsyncPolicy, SharedDb, Value};
use std::time::Instant;

fn median(v: &mut [f64]) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn main() {
    let n: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(100_000);
    let label = std::env::args().nth(2).unwrap_or_default();
    let dir = std::env::temp_dir().join(format!("fold-costs-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = SharedDb::open(&dir).unwrap();
    db.write()
        .apply_schema(&mushroomdb::memory_schema::memory_defaults())
        .unwrap();
    db.write().set_fsync_policy(FsyncPolicy::Relaxed);
    for chunk in (0..n).step_by(10_000) {
        let ops = (chunk..(chunk + 10_000).min(n))
            .map(|i| BatchOp::InsertNode {
                label: "Person".into(),
                key: format!("p{i}"),
                props: vec![("name".into(), Value::Str(format!("person {i}")))],
            })
            .collect();
        db.submit_batch(ops).unwrap();
    }
    // One more bulk commit, timed: what a 10k ingest batch costs the writer.
    let t = Instant::now();
    let ops = (n..n + 10_000)
        .map(|i| BatchOp::InsertNode {
            label: "Person".into(),
            key: format!("p{i}"),
            props: vec![("name".into(), Value::Str(format!("person {i}")))],
        })
        .collect();
    db.submit_batch(ops).unwrap();
    let bulk = t.elapsed().as_secs_f64() * 1e3;

    for field in ["name", "score"] {
        let (mut fold, mut first, mut steady) = (Vec::new(), Vec::new(), Vec::new());
        let mut after_fold = false;
        for i in 0..(4 * mushroomdb::FOLD_EVERY_K) {
            let key = format!("p{}", (i * 7919) % n);
            let value = match field {
                "name" => Value::Str(format!("renamed {i}")),
                _ => Value::Int(i as i64),
            };
            let t = Instant::now();
            db.write().set_prop(&key, field, value).unwrap();
            let ms = t.elapsed().as_secs_f64() * 1e3;
            let folded = db.read().reader().deltas.is_empty();
            if folded {
                fold.push(ms);
            } else if after_fold {
                first.push(ms);
            } else {
                steady.push(ms);
            }
            after_fold = folded;
        }
        println!(
            "{label}\tn={n}\tfield={field}\tfold={:.3}\tfirst_after_fold={:.3}\tsteady={:.3}\tbulk10k={bulk:.3}\tfolds={}",
            median(&mut fold),
            median(&mut first),
            median(&mut steady),
            fold.len()
        );
    }
}
