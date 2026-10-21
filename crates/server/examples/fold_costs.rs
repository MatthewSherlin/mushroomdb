//! Writer-side commit costs — the TX-5 harness for fix rounds 3 and 4.
//!
//! On a `memory_defaults()` store of N `Person` nodes (`name` is full-text and
//! equality indexed), times commits through `db.write()` and prints medians
//! (and p99 for the single-node passes):
//!
//! - `bulk10k`: one 10k-node `submit_batch` after the load;
//! - per pass of 4 × `FOLD_EVERY_K` single-property commits, classified as
//!   `fold` (the commit after which the reader's tail is empty), `first`
//!   (the first commit after a fold: ledger row 126) and `steady`.
//!
//! Passes: `name` and `score` (plain) with Relaxed fsync, then `score` and
//! `name` with Strict fsync. With `READERS=1`, 8 threads read the store
//! (capture, node lookup, indexed query) for the whole run, which is what
//! makes a reader hold an older version while a commit is published.
//!
//! Builds against v0.9.2 as well as 0.10, so the same source is the baseline.
//!
//! ```text
//! cargo run --release -p mushroomdb-server --example fold_costs -- <nodes> [label]
//! ```

use mushroomdb::{BatchOp, FsyncPolicy, SharedDb, Value};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

fn pct(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() as f64 * p) as usize).min(v.len() - 1)]
}

fn main() {
    let n: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(100_000);
    let label = std::env::args().nth(2).unwrap_or_default();
    let readers = std::env::var("READERS").is_ok_and(|v| v == "1");
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

    let stop = Arc::new(AtomicBool::new(false));
    let reader_threads: Vec<_> = (0..if readers { 8 } else { 0 })
        .map(|r| {
            let (db, stop) = (db.clone(), Arc::clone(&stop));
            std::thread::spawn(move || {
                let params = BTreeMap::new();
                let mut i = r;
                while !stop.load(Ordering::Relaxed) {
                    let snap = db.reader();
                    let _ = snap.node_info(&format!("p{}", (i * 7919) % n));
                    let _ = snap.query(
                        &format!("MATCH (n:Person {{name: 'person {}'}}) RETURN n.key", i % n),
                        &params,
                    );
                    i += 8;
                }
            })
        })
        .collect();

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
    println!("{label}\tn={n}\treaders={readers}\tbulk10k={bulk:.3}");

    // `NAME_LOOP=<secs>`: only commit `name` writes for that long — a steady
    // target for a sampling profiler.
    if let Some(secs) = std::env::var("NAME_LOOP")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        let end = Instant::now() + std::time::Duration::from_secs(secs);
        let mut i = 0usize;
        while Instant::now() < end {
            db.write()
                .set_prop(
                    &format!("p{}", (i * 7919) % n),
                    "name",
                    Value::Str(format!("renamed {i}")),
                )
                .unwrap();
            i += 1;
        }
        println!("{label}\tname_loop_commits={i}");
        stop.store(true, Ordering::Relaxed);
        return;
    }

    for (field, policy) in [
        ("name", FsyncPolicy::Relaxed),
        ("score", FsyncPolicy::Relaxed),
        ("score", FsyncPolicy::Strict),
        ("name", FsyncPolicy::Strict),
    ] {
        db.write().set_fsync_policy(policy);
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
        let fsync = matches!(policy, FsyncPolicy::Strict);
        println!(
            "{label}\tn={n}\treaders={readers}\tfield={field}\tfsync={fsync}\tfold={:.4}\tfirst_after_fold={:.4}\tsteady_p50={:.4}\tsteady_p99={:.4}\tfolds={}",
            pct(&mut fold, 0.5),
            pct(&mut first, 0.5),
            pct(&mut steady, 0.5),
            pct(&mut steady, 0.99),
            fold.len()
        );
    }
    stop.store(true, Ordering::Relaxed);
    for t in reader_threads {
        t.join().unwrap();
    }
}
