//! CI benchmark: deterministic synthetic store, five wall-time metrics, JSON output.
//!
//! Generates 10 000 Item nodes with deterministic properties (no external RNG
//! dependency — properties derived arithmetically from the node index), registers
//! two rules, writes and reopens a snapshot, and runs a fixed two-hop Cypher
//! query 50 times to capture a p50 latency.
//!
//! ```text
//! cargo run --release -p mushroomdb --example ci_bench
//! ```
//!
//! Prints a single JSON object to stdout:
//!
//! ```json
//! {
//!   "ingest_wall_s": ...,
//!   "rule_backfill_wall_s": ...,
//!   "snapshot_write_s": ...,
//!   "snapshot_open_s": ...,
//!   "query_p50_ms": ...
//! }
//! ```
//!
//! This is the canonical artifact consumed by `benchmarks/ci/run.sh` and
//! `benchmarks/ci/compare.py`.  Numbers are local-machine samples, not baselines;
//! see `benchmarks/README.md` for how baselines are captured on CI runners.

use core_api::{default_max_edges, GraphDb, IngestOptions, Predicate, RuleDef};
use std::collections::BTreeMap;
use std::time::Instant;

const N_NODES: usize = 10_000;
const N_QUERY_RUNS: usize = 50;
// Two-hop Cypher query fixed to a node that always exists.
const QUERY: &str =
    "MATCH (a:Item)-[:NEAR]->(b:Item)-[:NEAR]->(c:Item) WHERE a.id = 'item-0' RETURN c.id LIMIT 5";
// Scale for the FieldEqual streaming-backfill probe (src count = dst count).
const FE_SCALE: usize = 5_000;

fn main() {
    let db_path = std::env::temp_dir().join(format!("mushroomdb_ci_bench_{}", std::process::id()));
    std::fs::create_dir_all(&db_path).expect("create bench dir");
    // Ensure cleanup even on panic (best-effort; not critical for CI).
    let _guard = DirCleanup(db_path.clone());

    // ── 1. Ingest ────────────────────────────────────────────────────────────
    // Open first (untimed — empty store, negligible) so the timer measures
    // only the ingest call itself, matching the "ingest wall" spec metric.
    let json = build_nodes_json(N_NODES);
    let mut db = GraphDb::open(&db_path).expect("open");
    let t_ingest = Instant::now();
    db.ingest_json("Item", &json, &IngestOptions::default())
        .expect("ingest");
    let ingest_wall_s = t_ingest.elapsed().as_secs_f64();

    // ── 2. Rule backfill ─────────────────────────────────────────────────────
    // create_rule performs an inline backfill synchronously; timing both
    // rules together gives the combined backfill wall time.
    let score_near = Predicate::NumericWithin {
        field: "score".into(),
        tolerance: 10.0,
    };
    let tag_match = Predicate::Overlap {
        field: "tags".into(),
        min: 0.5,
    };
    let t_backfill = Instant::now();
    db.create_rule(RuleDef {
        name: "score_near".into(),
        src_label: "Item".into(),
        dst_label: "Item".into(),
        predicate: score_near.clone(),
        edge_type: "NEAR".into(),
        weight_prop: Some("score".into()),
        max_edges: Some(default_max_edges(&score_near)),
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
    })
    .expect("create score_near rule");
    db.create_rule(RuleDef {
        name: "tag_match".into(),
        src_label: "Item".into(),
        dst_label: "Item".into(),
        predicate: tag_match.clone(),
        edge_type: "TAG_MATCH".into(),
        weight_prop: Some("score".into()),
        max_edges: Some(default_max_edges(&tag_match)),
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
    })
    .expect("create tag_match rule");
    let rule_backfill_wall_s = t_backfill.elapsed().as_secs_f64();

    // ── 3. Snapshot write ─────────────────────────────────────────────────────
    let t_snap_write = Instant::now();
    db.snapshot().expect("snapshot");
    let snapshot_write_s = t_snap_write.elapsed().as_secs_f64();

    drop(db);

    // ── 4. Snapshot open ──────────────────────────────────────────────────────
    let t_snap_open = Instant::now();
    let db2 = GraphDb::open(&db_path).expect("reopen");
    let snapshot_open_s = t_snap_open.elapsed().as_secs_f64();

    // ── 5. Two-hop query p50 (N = 50) ────────────────────────────────────────
    let params = BTreeMap::new();
    let mut timings_ms: Vec<f64> = (0..N_QUERY_RUNS)
        .map(|_| {
            let t = Instant::now();
            db2.query(QUERY, &params).expect("query");
            t.elapsed().as_secs_f64() * 1000.0
        })
        .collect();
    timings_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    // True median for even N: average the two middle values.
    let query_p50_ms = (timings_ms[N_QUERY_RUNS / 2 - 1] + timings_ms[N_QUERY_RUNS / 2]) / 2.0;

    // ── 6. FieldEqual 5k × 5k streaming-backfill ─────────────────────────────
    // FE_SCALE Src + FE_SCALE Dst nodes all share group="g0"; max_edges=Some(5)
    // caps each source at 5 edges and exercises the streaming per-source budget
    // path (`apply_streaming_create_top_k`) rather than cross-product materialise.
    let fe_db_path =
        std::env::temp_dir().join(format!("mushroomdb_ci_bench_fe_{}", std::process::id()));
    std::fs::create_dir_all(&fe_db_path).expect("create fe bench dir");
    let _guard_fe = DirCleanup(fe_db_path.clone());
    let src_json = build_field_equal_json(FE_SCALE, "src");
    let dst_json = build_field_equal_json(FE_SCALE, "dst");
    let mut fe_db = GraphDb::open(&fe_db_path).expect("open fe");
    fe_db
        .ingest_json("Src", &src_json, &IngestOptions::default())
        .expect("ingest src");
    fe_db
        .ingest_json("Dst", &dst_json, &IngestOptions::default())
        .expect("ingest dst");
    let t_fe = Instant::now();
    fe_db
        .create_rule(RuleDef {
            name: "shared_group".into(),
            src_label: "Src".into(),
            dst_label: "Dst".into(),
            predicate: Predicate::FieldEqual {
                field: "group".into(),
            },
            edge_type: "GROUPED".into(),
            weight_prop: None,
            max_edges: Some(5),
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
        })
        .expect("create field_equal rule");
    let backfill_field_equal_5k_wall_s = t_fe.elapsed().as_secs_f64();

    // ── Output ────────────────────────────────────────────────────────────────
    println!(
        "{{\n  \"ingest_wall_s\": {ingest_wall_s:.6},\n  \"rule_backfill_wall_s\": {rule_backfill_wall_s:.6},\n  \"snapshot_write_s\": {snapshot_write_s:.6},\n  \"snapshot_open_s\": {snapshot_open_s:.6},\n  \"query_p50_ms\": {query_p50_ms:.6},\n  \"backfill_field_equal_5k_wall_s\": {backfill_field_equal_5k_wall_s:.6}\n}}"
    );
}

struct DirCleanup(std::path::PathBuf);
impl Drop for DirCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Build a JSON array of N Item objects with deterministic, seeded properties.
///
/// Each node has:
/// - `id`:    `"item-{i}"` — used as the ingest key field.
/// - `score`: `i % 100` — integer, drives the NumericWithin rule.
/// - `tags`:  `["g{i % 20}"]` — single-element list, drives the Overlap rule.
///
/// Properties are derived purely from the index; no RNG dependency.
fn build_nodes_json(n: usize) -> String {
    let mut out = String::with_capacity(n * 64);
    out.push('[');
    for i in 0..n {
        if i > 0 {
            out.push(',');
        }
        let score = i % 100;
        let tag = i % 20;
        out.push_str(&format!(
            r#"{{"id":"item-{i}","score":{score},"tags":["g{tag}"]}}"#
        ));
    }
    out.push(']');
    out
}

/// Build a JSON array of N nodes for the FieldEqual 5k×5k probe.
///
/// Each node has:
/// - `id`:    `"{prefix}-{i}"` — used as the ingest key field.
/// - `group`: `"g0"` — all nodes share the same value, creating a full
///   cross-product candidate set that the streaming budget path must cap.
fn build_field_equal_json(n: usize, prefix: &str) -> String {
    let mut out = String::with_capacity(n * 48);
    out.push('[');
    for i in 0..n {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!(r#"{{"id":"{prefix}-{i}","group":"g0"}}"#));
    }
    out.push(']');
    out
}
