//! TX-5 blocking gates 3 and 4, and the per-phase profile behind them.
//!
//! A memory-schema store (`memory_defaults()` applied before the first write,
//! as `mushroomdb serve` creates one) with 100k `Person` nodes and 200k
//! edges, a writer committing 50 `name` writes a second (`name` is full-text
//! and equality indexed), and 16 HTTP clients:
//!
//! - gate 3: `GET /node/{key}/neighborhood?depth=1`, p99 ≤ 2 ms
//! - gate 4: indexed `POST /query` (`MATCH (n:Person {name: …})`), p99 ≤ 2 ms
//!
//! ```text
//! cargo run --release -p mushroomdb-server --example gates_3_4 -- <gate3|gate4|profile> [label]
//! ```
//!
//! One line per run: `label gate p50 p99 max over_2ms samples`, in ms. The
//! `profile` mode times each phase of a lock-free read in-process, one sample
//! per writer commit: capture (`SharedDb::reader`), layer (`ensure_readable`),
//! index (`queries_use_index`, which builds the snapshot's index layer), and
//! the read itself.

use axum::body::{to_bytes, Body};
use axum::http::Request;
use mushroomdb::{BatchOp, Dir, SharedDb, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tower::ServiceExt;

const NODES: usize = 100_000;
const READERS: usize = 16;
const PER_READER: usize = 100;

/// The store, and its directory for removal once the run is done.
fn build() -> (SharedDb, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "gates-3-4-{}-{}",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let db = SharedDb::open(&dir).unwrap();
    db.write()
        .apply_schema(&mushroomdb::memory_schema::memory_defaults())
        .unwrap();
    for chunk in 0..10 {
        let nodes = (chunk * 10_000..(chunk + 1) * 10_000)
            .map(|i| BatchOp::InsertNode {
                label: "Person".into(),
                key: format!("p{i}"),
                props: vec![("name".into(), Value::Str(format!("person {i}")))],
            })
            .collect();
        db.submit_batch(nodes).unwrap();
    }
    for chunk in 0..10 {
        let edges = (chunk * 10_000..(chunk + 1) * 10_000)
            .flat_map(|i| {
                [1, 7].map(|step| BatchOp::InsertEdge {
                    edge_type: "KNOWS".into(),
                    src_key: format!("p{i}"),
                    dst_key: format!("p{}", (i + step) % NODES),
                })
            })
            .collect();
        db.submit_batch(edges).unwrap();
    }
    (db, dir)
}

fn start_writer(db: &SharedDb) -> (Arc<AtomicBool>, std::thread::JoinHandle<u64>) {
    let stop = Arc::new(AtomicBool::new(false));
    let (db, flag) = (db.clone(), Arc::clone(&stop));
    let handle = std::thread::spawn(move || {
        let mut i = 0u64;
        while !flag.load(Ordering::Relaxed) {
            let next = Instant::now() + Duration::from_millis(20);
            db.submit_batch(vec![BatchOp::SetProp {
                key: format!("p{}", (i as usize * 7919) % NODES),
                field: "name".into(),
                value: Value::Str(format!("renamed {i}")),
            }])
            .unwrap();
            i += 1;
            std::thread::sleep(next.saturating_duration_since(Instant::now()));
        }
        i
    });
    (stop, handle)
}

fn request(gate: &str, k: usize) -> Request<Body> {
    match gate {
        "gate3" => Request::get(format!("/node/p{k}/neighborhood?depth=1"))
            .body(Body::empty())
            .unwrap(),
        _ => Request::post("/query?format=json")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({
                    "cypher": format!("MATCH (n:Person {{name: 'person {k}'}}) RETURN n.key")
                })
                .to_string(),
            ))
            .unwrap(),
    }
}

/// How long the writer runs before measuring: `GATES_WARM_MS`, default 200.
/// Past ~1.3 s (64 commits) the build's bulk batches have folded out of the
/// reader's delta tail.
fn warmup() -> Duration {
    Duration::from_millis(
        std::env::var("GATES_WARM_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(200),
    )
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() as f64 * p) as usize).min(v.len() - 1)]
}

fn run_gate(db: &SharedDb, gate: &'static str, label: &str) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(8)
        .enable_all()
        .build()
        .unwrap();
    let app = server::router(db.clone());
    let (stop, writer) = start_writer(db);
    std::thread::sleep(warmup());
    let mut lat = rt.block_on(async {
        let mut handles = Vec::new();
        for r in 0..READERS {
            let app = app.clone();
            handles.push(tokio::spawn(async move {
                let mut lat = Vec::with_capacity(PER_READER);
                for i in 0..PER_READER {
                    let req = request(gate, (r * 6113 + i * 379) % NODES);
                    let t = Instant::now();
                    let res = app.clone().oneshot(req).await.unwrap();
                    let status = res.status();
                    let _ = to_bytes(res.into_body(), usize::MAX).await.unwrap();
                    lat.push(t.elapsed().as_secs_f64() * 1e3);
                    assert!(status.is_success(), "{gate} {status}");
                    // Clients think between requests, so the run spans commits.
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
                lat
            }));
        }
        let mut all = Vec::new();
        for h in handles {
            all.extend(h.await.unwrap());
        }
        all
    });
    stop.store(true, Ordering::Relaxed);
    let commits = writer.join().unwrap();
    let over = lat.iter().filter(|&&ms| ms > 2.0).count();
    let n = lat.len();
    println!(
        "{label}\t{gate}\t{:.3}\t{:.3}\t{:.3}\t{over}\t{n}\tcommits={commits}",
        pct(&mut lat, 0.50),
        pct(&mut lat, 0.99),
        pct(&mut lat, 1.0)
    );
}

fn profile(db: &SharedDb, label: &str) {
    let (stop, writer) = start_writer(db);
    std::thread::sleep(warmup());
    let mut phases: [Vec<f64>; 7] = Default::default();
    let params = Default::default();
    for i in 0..300usize {
        // One sample a commit: wait for the writer to move the version.
        let v0 = db.reader().version;
        while db.reader().version == v0 {
            std::thread::sleep(Duration::from_micros(200));
        }
        let k = (i * 379) % NODES;
        let t0 = Instant::now();
        let snap = db.reader();
        let t1 = Instant::now();
        snap.ensure_readable().unwrap();
        let t2 = Instant::now();
        let _ = snap.queries_use_index();
        let t3 = Instant::now();
        let _ = snap
            .query(
                &format!("MATCH (n:Person {{name: 'person {k}'}}) RETURN n.key"),
                &params,
            )
            .unwrap();
        let t4 = Instant::now();
        let _ = snap.neighborhood(&format!("p{k}"), 1, None, Dir::Both);
        let t5 = Instant::now();
        let ms = |a: Instant, b: Instant| (b - a).as_secs_f64() * 1e3;
        phases[0].push(ms(t0, t1));
        phases[1].push(ms(t1, t2));
        phases[2].push(ms(t2, t3));
        phases[3].push(ms(t3, t4));
        phases[4].push(ms(t4, t5));
        phases[5].push(snap.deltas.len() as f64);
        phases[6].push(ms(t0, t4));
        if ms(t0, t4) > 2.0 {
            println!(
                "{label}\tslow-sample\t{i}\ttail={}\tcapture={:.3}\tlayer={:.3}\tindex={:.3}\tquery={:.3}",
                snap.deltas.len(),
                ms(t0, t1),
                ms(t1, t2),
                ms(t2, t3),
                ms(t3, t4)
            );
        }
    }
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();
    for (name, v) in [
        "capture",
        "layer",
        "index",
        "query",
        "neighborhood",
        "tail_len",
        "query_total",
    ]
    .iter()
    .zip(phases.iter_mut())
    {
        println!(
            "{label}\tprofile\t{name}\tp50={:.3}\tp99={:.3}\tmax={:.3}",
            pct(v, 0.5),
            pct(v, 0.99),
            pct(v, 1.0)
        );
    }
}

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "gate3".into());
    let label = std::env::args().nth(2).unwrap_or_default();
    let (db, dir) = build();
    match mode.as_str() {
        "profile" => profile(&db, &label),
        "gate3" => run_gate(&db, "gate3", &label),
        _ => run_gate(&db, "gate4", &label),
    }
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}
