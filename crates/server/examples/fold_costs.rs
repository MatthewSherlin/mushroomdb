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
//! makes a reader hold an older version while a commit is published, and
//! each pass reports how many reads they completed per second.
//!
//! Builds against v0.9.2 as well as 0.10, so the same source is the baseline.
//!
//! `EDGES=<k>` adds `k` out-edges per node to the load.
//!
//! `CLOCK=cpu` times each commit by the writer thread's CPU time instead of
//! wall time, which a loaded machine disturbs far less (fsync waits are then
//! not counted, so read the fsync-off rows). `PASSES=<n>` repeats each pass
//! `n` times, for more fold samples.
//!
//! ```text
//! cargo run --release -p mushroomdb-server --example fold_costs -- <nodes> [label]
//! ```

use mushroomdb::{BatchOp, FsyncPolicy, SharedDb, Value};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

fn pct(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() as f64 * p) as usize).min(v.len() - 1)]
}

/// Milliseconds on the clock `CLOCK` names: the calling thread's CPU time
/// for `cpu`, else wall time since the first call.
fn now_ms() -> f64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    #[cfg(unix)]
    if std::env::var("CLOCK").is_ok_and(|v| v == "cpu") {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `ts` is a valid, writable timespec.
        unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
        return ts.tv_sec as f64 * 1e3 + ts.tv_nsec as f64 / 1e6;
    }
    START.get_or_init(Instant::now).elapsed().as_secs_f64() * 1e3
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
    let load_start = Instant::now();
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
    // `EDGES=<k>`: give every node `k` `LINK` out-edges as well, so a store
    // with no snapshot holds a topology overlay the size of the graph.
    let edges: usize = std::env::var("EDGES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    for chunk in (0..n).step_by(10_000) {
        if edges == 0 {
            break;
        }
        let ops = (chunk..(chunk + 10_000).min(n))
            .flat_map(|i| {
                (1..=edges).map(move |d| BatchOp::InsertEdge {
                    edge_type: "LINK".into(),
                    src_key: format!("p{i}"),
                    dst_key: format!("p{}", (i + d) % n),
                })
            })
            .collect();
        db.submit_batch(ops).unwrap();
    }
    let load_ms = load_start.elapsed().as_secs_f64() * 1e3;

    let stop = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(AtomicU64::new(0));
    let reader_threads: Vec<_> = (0..if readers { 8 } else { 0 })
        .map(|r| {
            let (db, stop, reads) = (db.clone(), Arc::clone(&stop), Arc::clone(&reads));
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
                    reads.fetch_add(1, Ordering::Relaxed);
                }
            })
        })
        .collect();

    // More bulk commits, timed (wall clock: the drain thread commits them):
    // what a 10k ingest batch costs the writer. `BULKS=<n>` times `n` of
    // them, which crosses a fold, and prints their median and mean.
    let bulks: usize = std::env::var("BULKS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    let mut bulk_ms = Vec::with_capacity(bulks);
    for b in 0..bulks {
        let from = n + b * 10_000;
        let ops = (from..from + 10_000)
            .map(|i| BatchOp::InsertNode {
                label: "Person".into(),
                key: format!("p{i}"),
                props: vec![("name".into(), Value::Str(format!("person {i}")))],
            })
            .collect();
        let t = Instant::now();
        db.submit_batch(ops).unwrap();
        bulk_ms.push(t.elapsed().as_secs_f64() * 1e3);
    }
    let bulk = bulk_ms[0];
    if bulks > 1 {
        let mean = bulk_ms.iter().sum::<f64>() / bulks as f64;
        println!(
            "{label}\tn={n}\tbulks={bulks}\tbulk_p50={:.3}\tbulk_mean={mean:.3}\tbulk_max={:.3}",
            pct(&mut bulk_ms.clone(), 0.5),
            pct(&mut bulk_ms, 1.0)
        );
    }
    println!("{label}\tn={n}\treaders={readers}\tbulk10k={bulk:.3}\tload_ms={load_ms:.1}");

    // `NAME_LOOP=<secs>` / `PLAIN_LOOP=<secs>`: only commit `name` (or plain
    // `score`) writes for that long, fsync off — a steady target for a
    // sampling profiler.
    for (var, field) in [("NAME_LOOP", "name"), ("PLAIN_LOOP", "score")] {
        let Some(secs) = std::env::var(var).ok().and_then(|v| v.parse::<u64>().ok()) else {
            continue;
        };
        let end = Instant::now() + std::time::Duration::from_secs(secs);
        let mut i = 0usize;
        while Instant::now() < end {
            let value = match field {
                "name" => Value::Str(format!("renamed {i}")),
                _ => Value::Int(i as i64),
            };
            db.write()
                .set_prop(&format!("p{}", (i * 7919) % n), field, value)
                .unwrap();
            i += 1;
        }
        println!("{label}\t{field}_loop_commits={i}");
        stop.store(true, Ordering::Relaxed);
        for t in reader_threads {
            t.join().unwrap();
        }
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
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
        let (reads_before, pass_start) = (reads.load(Ordering::Relaxed), Instant::now());
        let passes: usize = std::env::var("PASSES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1);
        for i in 0..(4 * passes * mushroomdb::FOLD_EVERY_K) {
            let key = format!("p{}", (i * 7919) % n);
            let value = match field {
                "name" => Value::Str(format!("renamed {i}")),
                _ => Value::Int(i as i64),
            };
            let t = now_ms();
            db.write().set_prop(&key, field, value).unwrap();
            let ms = now_ms() - t;
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
        let reads_per_s = (reads.load(Ordering::Relaxed) - reads_before) as f64
            / pass_start.elapsed().as_secs_f64();
        println!(
            "{label}\tn={n}\treaders={readers}\tfield={field}\tfsync={fsync}\tfold={:.4}\tfirst_after_fold={:.4}\tsteady_p50={:.4}\tsteady_p99={:.4}\tfolds={}\treads_per_s={reads_per_s:.0}",
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
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}
