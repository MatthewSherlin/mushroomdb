//! HTTP read latency under a concurrent writer — the TX-5 timing harness.
//!
//! Builds a 10k-node store in each configuration, starts a writer, and has 16
//! concurrent clients issue each read route through the router. Prints one
//! tab-separated line per (store, writer, route) with p50/p99/max in ms.
//!
//! Run the same source against two builds, interleaved, and take medians:
//!
//! ```text
//! cargo run --release -p mushroomdb-server --example read_latency -- <label>
//! ```
//!
//! Stores: `overlay` (never snapshotted), `snapshotted` (mmap base), `memory`
//! (`memory_defaults()`: full-text and an equality index on `name`, what
//! `mushroomdb serve` creates). Writers, 50 commits/s: `off`; `batch` (group
//! commit `submit_batch`, a plain field); `batch-name` (group commit, the
//! full-text and indexed `name`); `locked` (`db.write().set_prop`, which holds
//! the write lock through its fsync).

use axum::body::{to_bytes, Body};
use axum::http::Request;
use mushroomdb::{BatchOp, SharedDb, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tower::ServiceExt;

const NODES: usize = 10_000;
const READERS: usize = 16;
const PER_READER: usize = 100;
const ROUTES: &[&str] = &[
    "neighborhood",
    "node",
    "edges",
    "query_scan",
    "query_indexed",
    "query_mask",
    "health",
];

fn build(store: &str) -> SharedDb {
    let dir = std::env::temp_dir().join(format!("read-latency-{}-{store}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = SharedDb::open(&dir).unwrap();
    if store == "memory" {
        db.write()
            .apply_schema(&mushroomdb::memory_schema::memory_defaults())
            .unwrap();
    }
    let nodes = (0..NODES)
        .map(|i| BatchOp::InsertNode {
            label: "Person".into(),
            key: format!("p{i}"),
            props: vec![
                ("name".into(), Value::Str(format!("person {i}"))),
                (
                    "bio".into(),
                    Value::Str(format!("person number {i} likes things")),
                ),
            ],
        })
        .collect();
    db.submit_batch(nodes).unwrap();
    let edges = (0..NODES)
        .flat_map(|i| {
            [1, 7].map(|step| BatchOp::InsertEdge {
                edge_type: "KNOWS".into(),
                src_key: format!("p{i}"),
                dst_key: format!("p{}", (i + step) % NODES),
            })
        })
        .collect();
    db.submit_batch(edges).unwrap();
    if store == "snapshotted" {
        db.write().snapshot().unwrap();
    }
    db
}

fn request(route: &str, k: usize) -> Request<Body> {
    let json = |v: serde_json::Value| {
        Request::post("/query?format=json")
            .header("content-type", "application/json")
            .body(Body::from(v.to_string()))
            .unwrap()
    };
    match route {
        "neighborhood" => Request::get(format!("/node/p{k}/neighborhood?depth=1"))
            .body(Body::empty())
            .unwrap(),
        "node" => Request::get(format!("/node/p{k}"))
            .body(Body::empty())
            .unwrap(),
        "edges" => Request::get(format!("/node/p{k}/edges"))
            .body(Body::empty())
            .unwrap(),
        "query_scan" => json(serde_json::json!({
            "cypher": format!("MATCH (n:Person)-[:KNOWS]->(m) WHERE n.bio = 'person number {k} likes things' RETURN m.name")
        })),
        "query_indexed" => json(serde_json::json!({
            "cypher": format!("MATCH (n:Person {{name: 'person {k}'}})-[:KNOWS]->(m) RETURN m.name")
        })),
        "query_mask" => json(serde_json::json!({
            "cypher": "MATCH (n:Person) RETURN n.name",
            "mask": [format!("p{k}"), format!("p{}", (k + 1) % NODES)]
        })),
        "health" => Request::get("/health").body(Body::empty()).unwrap(),
        _ => unreachable!(),
    }
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() as f64 * p) as usize).min(v.len() - 1)]
}

async fn run(app: axum::Router, route: &'static str) -> Vec<f64> {
    let mut handles = Vec::new();
    for r in 0..READERS {
        let app = app.clone();
        handles.push(tokio::spawn(async move {
            // A scan is ~10 ms a request; fewer of them keep a run short.
            let n = if route == "query_scan" {
                PER_READER / 5
            } else {
                PER_READER
            };
            let mut lat = Vec::with_capacity(n);
            for i in 0..n {
                let req = request(route, (r * 613 + i * 37) % NODES);
                let t = Instant::now();
                let res = app.clone().oneshot(req).await.unwrap();
                let status = res.status();
                let _ = to_bytes(res.into_body(), usize::MAX).await.unwrap();
                lat.push(t.elapsed().as_secs_f64() * 1e3);
                assert!(status.is_success(), "{route} {status}");
            }
            lat
        }));
    }
    let mut all = Vec::new();
    for h in handles {
        all.extend(h.await.unwrap());
    }
    all
}

fn start_writer(
    db: &SharedDb,
    mode: &'static str,
) -> (Arc<AtomicBool>, Option<std::thread::JoinHandle<()>>) {
    let stop = Arc::new(AtomicBool::new(false));
    if mode == "off" {
        return (stop, None);
    }
    let (db, flag) = (db.clone(), Arc::clone(&stop));
    let handle = std::thread::spawn(move || {
        let mut i = 0usize;
        while !flag.load(Ordering::Relaxed) {
            let next = Instant::now() + Duration::from_millis(20);
            let key = format!("p{}", (i * 131) % NODES);
            match mode {
                "batch" => {
                    db.submit_batch(vec![BatchOp::SetProp {
                        key,
                        field: "score".into(),
                        value: Value::Int(i as i64),
                    }])
                    .unwrap();
                }
                "batch-name" => {
                    db.submit_batch(vec![BatchOp::SetProp {
                        key,
                        field: "name".into(),
                        value: Value::Str(format!("renamed {i}")),
                    }])
                    .unwrap();
                }
                _ => {
                    db.write()
                        .set_prop(&key, "score", Value::Int(i as i64))
                        .unwrap();
                }
            }
            i += 1;
            std::thread::sleep(next.saturating_duration_since(Instant::now()));
        }
    });
    (stop, Some(handle))
}

fn main() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(8)
        .enable_all()
        .build()
        .unwrap();
    let label = std::env::args().nth(1).unwrap_or_default();
    for store in ["overlay", "snapshotted", "memory"] {
        let db = build(store);
        let app = server::router(db.clone());
        for writer in ["off", "batch", "batch-name", "locked"] {
            if writer == "batch-name" && store != "memory" {
                continue;
            }
            let (stop, handle) = start_writer(&db, writer);
            std::thread::sleep(Duration::from_millis(100));
            for route in ROUTES {
                let mut lat = rt.block_on(run(app.clone(), route));
                println!(
                    "{label}\t{store}\t{writer}\t{route}\t{:.3}\t{:.3}\t{:.3}",
                    pct(&mut lat, 0.50),
                    pct(&mut lat, 0.99),
                    pct(&mut lat, 1.0)
                );
            }
            stop.store(true, Ordering::Relaxed);
            if let Some(h) = handle {
                h.join().unwrap();
            }
        }
    }
}
