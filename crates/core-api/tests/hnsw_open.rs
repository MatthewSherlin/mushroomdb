//! The open path must reuse the HNSW graph the snapshot persisted instead of
//! rebuilding it.
//!
//! Both open paths — `consume_retained_state_eager` (WAL present) and
//! `ensure_indexes_populated` (clean open, first mutation) — used to run a full
//! node scan into a freshly initialized HNSW graph and then immediately replace
//! that graph with the persisted blob. The build is superlinear in the number
//! of embeddings and its result was discarded every time.

use core_api::{Direction, GraphDb, Predicate, RuleDef, Value};
use core_storage::fs::RealFs;

type Db = GraphDb<RealFs>;

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("graphdb-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn emb(xs: &[f64]) -> Value {
    Value::List(xs.iter().copied().map(Value::Float).collect())
}

fn sim_rule() -> RuleDef {
    RuleDef {
        name: "sim".into(),
        src_label: "Doc".into(),
        dst_label: "Doc".into(),
        predicate: Predicate::VectorSimilar {
            field: "emb".into(),
            min: 0.9,
        },
        edge_type: "SIM".into(),
        weight_prop: None,
        max_edges: None,
        approximate: true,
        via_label: None,
        via_edge: None,
        via_dir: None,
    }
}

/// Eight 2-D unit vectors in four well-separated pairs, so the approximate
/// rule's edge set is deterministic and cheap to compare.
const DOCS: &[(&str, [f64; 2])] = &[
    ("d0", [1.0, 0.0]),
    ("d1", [0.98, 0.2]),
    ("d2", [0.0, 1.0]),
    ("d3", [-0.2, 0.98]),
    ("d4", [-1.0, 0.0]),
    ("d5", [-0.98, 0.2]),
    ("d6", [0.0, -1.0]),
    ("d7", [0.2, -0.98]),
];

fn seed(dir: &std::path::Path) -> Db {
    let mut db = GraphDb::open(dir).unwrap();
    for (k, v) in DOCS {
        db.insert_node("Doc", k, vec![("emb".into(), emb(v))])
            .unwrap();
    }
    db.create_rule(sim_rule()).unwrap();
    db
}

fn edge_map(db: &Db) -> Vec<(String, Vec<String>)> {
    DOCS.iter()
        .map(|(k, _)| {
            (
                k.to_string(),
                db.neighbors(k, "SIM", Direction::Out).unwrap_or_default(),
            )
        })
        .collect()
}

/// Clean open (snapshot truncates the WAL): the first mutation populates the
/// indexes through `ensure_indexes_populated`, which must load the persisted
/// graphs rather than build new ones.
#[test]
fn clean_open_reuses_persisted_hnsw() {
    let dir = tmp("hnsw-open-clean");
    {
        let mut db = seed(&dir);
        db.snapshot().unwrap();
    }

    let mut db = GraphDb::open(&dir).unwrap();
    // A mutation trips the lazy-init guard, the clean-open path into the
    // reindex. Nothing about this node touches the vector rule's dst side.
    db.insert_node("Other", "o", vec![("v".into(), Value::Int(1))])
        .unwrap();
    assert_eq!(
        db.hnsw_build_count(),
        0,
        "clean open rebuilt an HNSW graph the snapshot already holds"
    );

    // The loaded graph still answers.
    let hits = db.find_similar_vector("emb", Some("Doc"), &[1.0, 0.0], 2, 0.0);
    assert!(
        hits.iter().any(|(k, _)| k == "d0"),
        "persisted HNSW must still find d0; got {hits:?}"
    );
}

/// WAL-present open: `consume_retained_state_eager` runs before replay and
/// must load the persisted graphs rather than build new ones.
#[test]
fn wal_present_open_reuses_persisted_hnsw() {
    let dir = tmp("hnsw-open-wal");
    {
        let mut db = seed(&dir);
        db.snapshot().unwrap();
        // WAL tail: replayed on the next open.
        db.insert_node("Doc", "tail", vec![("emb".into(), emb(&[0.99, 0.1]))])
            .unwrap();
    }

    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(
        db.hnsw_build_count(),
        0,
        "WAL-present open rebuilt an HNSW graph the snapshot already holds"
    );
}

/// Embeddings written after the last snapshot arrive through WAL replay, which
/// runs `on_node_changed` against the already-loaded index. They must be
/// searchable on the next open without any rebuild.
#[test]
fn wal_replayed_embeddings_are_searchable_after_open() {
    let dir = tmp("hnsw-open-wal-search");
    {
        let mut db = seed(&dir);
        db.snapshot().unwrap();
        db.insert_node("Doc", "late", vec![("emb".into(), emb(&[0.995, 0.1]))])
            .unwrap();
    }

    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(db.hnsw_build_count(), 0, "open rebuilt a persisted graph");

    let hits = db.find_similar_vector("emb", Some("Doc"), &[1.0, 0.0], 3, 0.5);
    let keys: Vec<&str> = hits.iter().map(|(k, _)| k.as_str()).collect();
    assert!(
        keys.contains(&"late"),
        "embedding written after the snapshot must be searchable after reopen; got {keys:?}"
    );
    assert!(
        keys.contains(&"d0"),
        "pre-snapshot embedding must still be searchable; got {keys:?}"
    );
}

/// Open-time bench on a synthetic 2,000-embedding store.  Ignored by default
/// and additionally gated on `MUSHROOM_BENCH_HNSW_OPEN=1`, because the build it
/// times is exactly the superlinear cost this change removes.
///
/// Run with:
/// `MUSHROOM_BENCH_HNSW_OPEN=1 cargo test -p mushroomdb --test hnsw_open -- --ignored --nocapture`
#[test]
#[ignore = "timing bench: set MUSHROOM_BENCH_HNSW_OPEN=1"]
fn bench_open_2k_embeddings() {
    if std::env::var("MUSHROOM_BENCH_HNSW_OPEN").is_err() {
        return;
    }
    const N: usize = 2_000;
    const D: usize = 64;

    // Deterministic pseudo-random unit vectors (xorshift64*), so the bench
    // measures the same graph on every run.
    let mut state: u64 = 0x2545_F491_4F6C_DD1D;
    let mut next = move || {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        (state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64 - 0.5
    };

    let dir = tmp("hnsw-open-bench");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        for i in 0..N {
            let xs: Vec<f64> = (0..D).map(|_| next()).collect();
            let norm = xs.iter().map(|x| x * x).sum::<f64>().sqrt();
            let unit: Vec<f64> = xs.iter().map(|x| x / norm).collect();
            db.insert_node("Doc", &format!("n{i}"), vec![("emb".into(), emb(&unit))])
                .unwrap();
        }
        let t = std::time::Instant::now();
        db.create_rule(sim_rule()).unwrap();
        eprintln!("create_rule (full build, {N}x{D}): {:?}", t.elapsed());
        db.snapshot().unwrap();
        // WAL tail so the reopen takes the eager (consume_retained_state_eager)
        // path rather than deferring to the first mutation.
        db.insert_node("Doc", "tail", vec![("emb".into(), emb(&vec![0.0; D]))])
            .unwrap();
    }

    let t = std::time::Instant::now();
    let db = GraphDb::open(&dir).unwrap();
    let open = t.elapsed();
    eprintln!(
        "open (WAL-present, {N} embeddings): {open:?}, hnsw builds = {}",
        db.hnsw_build_count()
    );
}

/// Correctness invariant: skipping the build must not change what the rule
/// derives or what an ANN query returns.
#[test]
fn query_results_identical_across_snapshot_and_reopen() {
    let dir = tmp("hnsw-open-equiv");
    let edges_before;
    let hits_before;
    {
        let mut db = seed(&dir);
        edges_before = edge_map(&db);
        hits_before = db.find_similar_vector("emb", Some("Doc"), &[0.9, 0.3], 4, 0.0);
        db.snapshot().unwrap();
    }

    let mut db = GraphDb::open(&dir).unwrap();
    let hits_after = db.find_similar_vector("emb", Some("Doc"), &[0.9, 0.3], 4, 0.0);
    let keys_before: Vec<&str> = hits_before.iter().map(|(k, _)| k.as_str()).collect();
    let keys_after: Vec<&str> = hits_after.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        keys_before, keys_after,
        "top-k candidates must be identical across snapshot + reopen"
    );
    // Scores are recomputed from the stored vectors on each path, so they may
    // differ by a ULP from summation order; they must not differ meaningfully.
    for ((k, a), (_, b)) in hits_before.iter().zip(hits_after.iter()) {
        assert!(
            (a - b).abs() < 1e-12,
            "score for {k} changed across reopen: {a} vs {b}"
        );
    }

    // Force the mutation path to materialize the indexes, then compare the
    // derived edge set the rule owns.
    db.insert_node("Other", "o", vec![("v".into(), Value::Int(1))])
        .unwrap();
    assert_eq!(db.hnsw_build_count(), 0, "reopen rebuilt a persisted graph");
    assert_eq!(
        edges_before,
        edge_map(&db),
        "derived edge set must be identical across snapshot + reopen"
    );
}
