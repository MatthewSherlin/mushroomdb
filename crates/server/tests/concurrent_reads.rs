//! TX-5: HTTP reads do not wait on the writer.
//!
//! The pins are about ordering, not wall-clock percentiles:
//!
//! - every lock-free read route answers while another thread holds the write
//!   lock (a `db.write()` holder: a Cypher write, `/ingest`, `create_rule`),
//!   which before TX-5 blocked each of them until the lock was released;
//! - a write that has returned is seen by the next read, both when the read
//!   lock is free and when another writer holds it (the published snapshot);
//! - a multi-step read (mask resolution, then a BFS) never mixes two versions
//!   of a graph a writer is changing underneath it.

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use mushroomdb::{BatchOp, Predicate, RuleDef, SharedDb, Value};
use serde_json::{json, Value as Json};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

fn tmp(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!(
        "concurrent-reads-{name}-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn get(uri: &str) -> Request<Body> {
    Request::get(uri).body(Body::empty()).unwrap()
}

fn post(uri: &str, body: Json) -> Request<Body> {
    Request::post(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn put(uri: &str, body: Json) -> Request<Body> {
    Request::put(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn send(app: &Router, req: Request<Body>) -> (StatusCode, Json) {
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or(Json::Null);
    (status, body)
}

fn seed(db: &SharedDb, n: usize) {
    let mut ops: Vec<BatchOp> = (0..n)
        .map(|i| BatchOp::InsertNode {
            label: "Person".into(),
            key: format!("p{i}"),
            props: vec![("name".into(), Value::Str(format!("person {i}")))],
        })
        .collect();
    ops.extend((0..n).map(|i| BatchOp::InsertEdge {
        edge_type: "KNOWS".into(),
        src_key: format!("p{i}"),
        dst_key: format!("p{}", (i + 1) % n),
    }));
    db.submit_batch(ops).unwrap();
}

/// Another thread holding `db`'s write lock, the way a long `db.write()`
/// holder does, until [`Held::release`].
struct Held {
    held: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<()>,
}

impl Held {
    /// Returns once the other thread holds the lock.
    fn take(db: &SharedDb) -> Self {
        let held = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let thread = {
            let (db, held, release) = (db.clone(), Arc::clone(&held), Arc::clone(&release));
            std::thread::spawn(move || {
                let guard = db.write();
                held.store(true, Ordering::SeqCst);
                // Released by the test, or after 3 s so a read that blocks
                // (the pre-TX-5 behaviour) fails the test instead of hanging it.
                let deadline = std::time::Instant::now() + Duration::from_secs(3);
                while !release.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(2));
                }
                held.store(false, Ordering::SeqCst);
                drop(guard);
            })
        };
        while !held.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(1));
        }
        Self {
            held,
            release,
            thread,
        }
    }

    fn still_held(&self) -> bool {
        self.held.load(Ordering::SeqCst)
    }

    fn release(self) {
        self.release.store(true, Ordering::SeqCst);
        self.thread.join().unwrap();
    }
}

/// Every route TX-5 moved onto the reader answers while a writer holds the
/// lock. Before TX-5 each of these took `db.read()` and waited for the release.
#[tokio::test(flavor = "current_thread")]
async fn every_lock_free_route_answers_while_a_writer_holds_the_lock() {
    let db = SharedDb::open(&tmp("held")).unwrap();
    seed(&db, 50);
    let app = server::router(db.clone());
    let writer = Held::take(&db);

    let reads = [
        post(
            "/query?format=json",
            json!({"cypher": "MATCH (n:Person) RETURN count(n) AS c"}),
        ),
        post(
            "/query?format=json",
            json!({"cypher": "MATCH (n:Person) RETURN n.key AS k", "mask": ["p1", "p2"]}),
        ),
        get("/node/p1"),
        get("/node/p1/edges"),
        get("/node/p1/neighborhood?depth=2"),
        get("/node/p1?mask=p1,p2&stub_hidden=true"),
        get("/health"),
        get("/metrics"),
        post("/algo/degree", json!({})),
        post("/algo/wcc", json!({})),
        post("/algo/pagerank", json!({})),
    ];
    for req in reads {
        let uri = req.uri().to_string();
        let (status, body) = send(&app, req).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        assert!(
            writer.still_held(),
            "{uri} answered only after the writer released the lock"
        );
    }
    writer.release();
}

/// A write that returned is read by the next request — and still is when
/// another writer then takes the lock, because the released state was
/// published before the lock was let go.
#[tokio::test(flavor = "current_thread")]
async fn a_returned_write_is_read_even_while_another_writer_holds_the_lock() {
    let db = SharedDb::open(&tmp("ryw-held")).unwrap();
    seed(&db, 5);
    let app = server::router(db.clone());

    let (status, body) = send(
        &app,
        post(
            "/nodes",
            json!({"label": "Person", "key": "fresh", "props": {"name": "new"}}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let writer = Held::take(&db);
    let (status, body) = send(&app, get("/node/fresh")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["props"]["name"], json!("new"));
    let (status, body) = send(
        &app,
        post(
            "/query?format=json",
            json!({"cypher": "MATCH (n:Person {name: 'new'}) RETURN n.key AS k"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["rows"], json!([["fresh"]]));
    assert!(writer.still_held(), "the reads waited for the writer");
    writer.release();
}

/// Read-your-write on each moved route, for each kind of write: a Cypher write
/// through `/query`, an `/ingest`, and a property write that moves a weighted
/// rule's score — a value the rule engine writes with no record of its own.
#[tokio::test(flavor = "current_thread")]
async fn each_kind_of_write_is_read_by_the_next_request() {
    let db = SharedDb::open(&tmp("ryw")).unwrap();
    db.write().insert_node("Org", "acme", vec![]).unwrap();
    db.write()
        .create_rule(RuleDef {
            name: "fit".into(),
            src_label: "Person".into(),
            dst_label: "Org".into(),
            predicate: Predicate::Overlap {
                field: "skills".into(),
                min: 0.1,
            },
            edge_type: "FIT".into(),
            weight_prop: Some("score".into()),
            max_edges: None,
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
            namespace: None,
        })
        .unwrap();
    let app = server::router(db.clone());

    // Cypher write, then a Cypher read.
    let (status, body) = send(
        &app,
        post(
            "/query?format=json",
            json!({"cypher": "CREATE (n:Person {id: 'cy', name: 'cypher'})"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, body) = send(
        &app,
        post(
            "/query?format=json",
            json!({"cypher": "MATCH (n:Person {name: 'cypher'}) RETURN n.key AS k"}),
        ),
    )
    .await;
    assert_eq!(body["rows"], json!([["cy"]]));

    // Ingest, then the neighborhood its auto-FK edge reaches.
    let (status, body) = send(
        &app,
        post(
            "/ingest",
            json!({"label": "Person", "rows": [{"id": "ing", "org_id": "acme"}], "options": {}}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, body) = send(&app, get("/node/acme/neighborhood?depth=1")).await;
    let keys: Vec<&Json> = body["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| &r[0])
        .collect();
    assert!(
        keys.contains(&&json!("ing")),
        "ingested node missing: {body}"
    );

    // Score moves: each skills write re-scores the edge in place, and the
    // next read must return the score the live handle now holds.
    db.write()
        .set_prop(
            "acme",
            "skills",
            Value::List(vec![Value::Str("a".into()), Value::Str("b".into())]),
        )
        .unwrap();
    let cypher = "MATCH (p:Person {key: 'cy'})-[r:FIT]->(o) RETURN r.score AS s";
    let mut seen = Vec::new();
    for skills in [json!(["a"]), json!(["a", "b"])] {
        let (status, body) =
            send(&app, put("/node/cy/prop/skills", json!({"value": skills}))).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (_, body) = send(
            &app,
            post("/query?format=json", json!({ "cypher": cypher })),
        )
        .await;
        let live = db.read().query(cypher, &Default::default()).unwrap();
        let live_score = match live.get(0, "s") {
            Some(Value::Float(f)) => *f,
            other => panic!("live score {other:?}"),
        };
        let got = body["rows"][0][0].as_f64();
        assert_eq!(got, Some(live_score), "read {body}, live {live_score}");
        seen.push(live_score);
    }
    assert_ne!(seen[0], seen[1], "the second write must move the score");
}

/// A multi-step read never mixes two versions. Every commit moves the hub's
/// edges as one batch, so the hub always has exactly `SPOKES` neighbours and
/// each carries the generation stamp the hub carries. A read that saw half a
/// commit would count wrong or see two stamps.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_multi_step_read_never_sees_half_a_commit() {
    const SPOKES: usize = 8;
    const POOL: usize = 40;
    let db = SharedDb::open(&tmp("torn")).unwrap();
    let mut ops = vec![BatchOp::InsertNode {
        label: "Hub".into(),
        key: "hub".into(),
        props: vec![("gen".into(), Value::Int(0))],
    }];
    ops.extend((0..POOL).map(|i| BatchOp::InsertNode {
        label: "Spoke".into(),
        key: format!("s{i}"),
        props: vec![("gen".into(), Value::Int(0))],
    }));
    ops.extend((0..SPOKES).map(|i| BatchOp::InsertEdge {
        edge_type: "R".into(),
        src_key: "hub".into(),
        dst_key: format!("s{i}"),
    }));
    db.submit_batch(ops).unwrap();
    let app = server::router(db.clone());

    let stop = Arc::new(AtomicBool::new(false));
    let generation = Arc::new(AtomicU64::new(0));
    let writer = {
        let (db, stop, generation) = (db.clone(), Arc::clone(&stop), Arc::clone(&generation));
        std::thread::spawn(move || {
            let mut g = 0usize;
            while !stop.load(Ordering::Relaxed) && g < 300 {
                let (old, new) = (g % POOL, (g + SPOKES) % POOL);
                let stamp = Value::Int(g as i64 + 1);
                let mut batch = vec![
                    BatchOp::DeleteEdge {
                        edge_type: "R".into(),
                        src_key: "hub".into(),
                        dst_key: format!("s{old}"),
                    },
                    BatchOp::InsertEdge {
                        edge_type: "R".into(),
                        src_key: "hub".into(),
                        dst_key: format!("s{new}"),
                    },
                    BatchOp::SetProp {
                        key: "hub".into(),
                        field: "gen".into(),
                        value: stamp.clone(),
                    },
                ];
                batch.extend((0..POOL).map(|i| BatchOp::SetProp {
                    key: format!("s{i}"),
                    field: "gen".into(),
                    value: stamp.clone(),
                }));
                db.submit_batch(batch).unwrap();
                generation.store(g as u64 + 1, Ordering::Relaxed);
                g += 1;
            }
        })
    };

    let all_spokes: Vec<String> = (0..POOL).map(|i| format!("s{i}")).collect();
    let mask = format!("hub,{}", all_spokes.join(","));
    let mut reads = 0;
    while !writer.is_finished() {
        for uri in [
            "/node/hub/neighborhood?depth=1&dir=out".to_string(),
            format!("/node/hub/neighborhood?depth=1&dir=out&mask={mask}"),
            "/node/hub/edges".to_string(),
        ] {
            let (status, body) = send(&app, get(&uri)).await;
            assert_eq!(status, StatusCode::OK, "{uri}: {body}");
            let n = body
                .get("rows")
                .or(body.get("edges"))
                .and_then(Json::as_array)
                .map(Vec::len)
                .unwrap_or_else(|| body.as_array().map_or(0, Vec::len));
            assert_eq!(n, SPOKES, "{uri} saw half a commit: {body}");
            reads += 1;
        }
        // One query reads the hub's stamp and every spoke's: one version.
        let (status, body) = send(
            &app,
            post(
                "/query?format=json",
                json!({"cypher": "MATCH (h:Hub)-[:R]->(s) RETURN h.gen AS hg, s.gen AS sg"}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let rows = body["rows"].as_array().unwrap();
        assert_eq!(rows.len(), SPOKES, "{body}");
        for r in rows {
            assert_eq!(r[0], r[1], "hub and spoke from different commits: {body}");
        }
    }
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();
    assert!(reads > 30, "only {reads} reads raced the writer");
    assert!(generation.load(Ordering::Relaxed) > 0);
}
