//! Tests for `GraphDb::search_hybrid` — Reciprocal Rank Fusion over fulltext + vector.
//!
//! Fixture: three nodes in label "Item":
//!   "both"   — matches the text query AND has a vector close to query_vec.
//!   "t_only" — matches text only; its vector is antipodal (cosine -1 < min=0.0, filtered out).
//!   "v_only" — no text match; its vector is exactly aligned with query_vec (cosine 1.0).
//!
//! With k=3 and query_vec = [1.0, 0.0]:
//!
//!   Text ranking (by match_count DESC, key ASC):
//!     rank 1 → "both"   (key "both" < "t_only" when counts tie)
//!     rank 2 → "t_only"
//!
//!   Vector ranking (min=0.0; t_only filtered because dot<0):
//!     rank 1 → "v_only"  (cosine 1.0)
//!     rank 2 → "both"    (cosine ≈ 0.894)
//!
//!   RRF scores (constant = 60):
//!     "both"   → 1/61 + 1/62  (appears in both lists)
//!     "v_only" → 1/61          (vector list only)
//!     "t_only" → 1/62          (text list only)
//!
//!   Final order: both > v_only > t_only

use core_api::{GraphDb, Value};

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("graphdb-hybrid-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn emb(xs: &[f64]) -> Value {
    Value::List(xs.iter().copied().map(Value::Float).collect())
}

/// Build the standard three-node fixture used across hybrid tests.
fn make_db() -> GraphDb<core_storage::fs::RealFs> {
    let dir = tmp("fixture");
    let mut db = GraphDb::open(&dir).unwrap();

    db.enable_fulltext("Item", "body").unwrap();

    // "both" — matches text query "unique" AND vector close to [1,0].
    db.insert_node(
        "Item",
        "both",
        vec![
            ("body".into(), Value::Str("unique".into())),
            ("emb".into(), emb(&[1.0, 0.5])),
        ],
    )
    .unwrap();

    // "t_only" — matches text query "unique", vector is antipodal → dot < 0 → filtered.
    db.insert_node(
        "Item",
        "t_only",
        vec![
            ("body".into(), Value::Str("unique".into())),
            ("emb".into(), emb(&[-1.0, 0.0])),
        ],
    )
    .unwrap();

    // "v_only" — no text match, vector exactly aligned with query → cosine 1.0.
    db.insert_node(
        "Item",
        "v_only",
        vec![
            ("body".into(), Value::Str("other".into())),
            ("emb".into(), emb(&[1.0, 0.0])),
        ],
    )
    .unwrap();

    db
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn hybrid_both_ranks_first_and_exact_scores() {
    let db = make_db();

    let query_vec = vec![1.0_f64, 0.0];
    let results = db.search_hybrid("body", "unique", "emb", &query_vec, Some("Item"), 3);

    assert_eq!(results.len(), 3, "all three nodes must appear");

    // "both" must be first — it contributes from two lists.
    assert_eq!(results[0].0, "both", "both must rank first");

    // Exact RRF scores: RRF_K=60, rank 1-based.
    let expected_both: f64 = 1.0 / 61.0 + 1.0 / 62.0; // text rank 1 + vector rank 2
    let expected_v_only: f64 = 1.0 / 61.0; // vector rank 1 only
    let expected_t_only: f64 = 1.0 / 62.0; // text rank 2 only

    assert!(
        (results[0].1 - expected_both).abs() < 1e-12,
        "both score: expected {expected_both}, got {}",
        results[0].1
    );
    assert_eq!(results[1].0, "v_only", "v_only must rank second");
    assert!(
        (results[1].1 - expected_v_only).abs() < 1e-12,
        "v_only score: expected {expected_v_only}, got {}",
        results[1].1
    );
    assert_eq!(results[2].0, "t_only", "t_only must rank third");
    assert!(
        (results[2].1 - expected_t_only).abs() < 1e-12,
        "t_only score: expected {expected_t_only}, got {}",
        results[2].1
    );

    // Scores must be strictly descending.
    assert!(
        results[0].1 > results[1].1 && results[1].1 > results[2].1,
        "scores must be strictly descending: {:?}",
        results.iter().map(|(k, s)| (k, s)).collect::<Vec<_>>()
    );
}

#[test]
fn hybrid_k_truncates_results() {
    let db = make_db();
    let query_vec = vec![1.0_f64, 0.0];

    let results = db.search_hybrid("body", "unique", "emb", &query_vec, Some("Item"), 2);
    assert_eq!(results.len(), 2, "k=2 must truncate to 2 results");
    assert_eq!(results[0].0, "both");
    assert_eq!(results[1].0, "v_only");
}

#[test]
fn hybrid_tie_broken_by_key_ascending() {
    // Two nodes that each appear in exactly one list at rank 1:
    // both score 1/61 — tie must break by key ascending.
    let dir = tmp("tie");
    let mut db = GraphDb::open(&dir).unwrap();
    db.enable_fulltext("Item", "body").unwrap();

    // "aaa" — text match only (antipodal vector → filtered).
    db.insert_node(
        "Item",
        "aaa",
        vec![
            ("body".into(), Value::Str("termx".into())),
            ("emb".into(), emb(&[-1.0, 0.0])),
        ],
    )
    .unwrap();

    // "zzz" — vector match only (cosine 1.0, no text).
    db.insert_node(
        "Item",
        "zzz",
        vec![
            ("body".into(), Value::Str("other".into())),
            ("emb".into(), emb(&[1.0, 0.0])),
        ],
    )
    .unwrap();

    let query_vec = vec![1.0_f64, 0.0];
    let results = db.search_hybrid("body", "termx", "emb", &query_vec, Some("Item"), 2);
    assert_eq!(results.len(), 2);

    let score_aaa = results.iter().find(|(k, _)| k == "aaa").map(|(_, s)| *s);
    let score_zzz = results.iter().find(|(k, _)| k == "zzz").map(|(_, s)| *s);
    assert!(score_aaa.is_some() && score_zzz.is_some());

    // Both score 1/61; "aaa" < "zzz" alphabetically → aaa is first.
    let expected = 1.0 / 61.0;
    assert!((score_aaa.unwrap() - expected).abs() < 1e-12);
    assert!((score_zzz.unwrap() - expected).abs() < 1e-12);
    assert_eq!(results[0].0, "aaa", "tie broken by key ascending");
}

#[test]
fn hybrid_text_only_when_empty_vec() {
    // When query_vec is empty, the vector leg is skipped → text-only ranking via RRF.
    let db = make_db();
    let results = db.search_hybrid("body", "unique", "emb", &[], Some("Item"), 10);

    // Only text-matching nodes appear: "both" and "t_only".
    // "v_only" has no text match and no vector leg → absent.
    let keys: Vec<&str> = results.iter().map(|(k, _)| k.as_str()).collect();
    assert!(keys.contains(&"both"), "both must appear");
    assert!(keys.contains(&"t_only"), "t_only must appear");
    assert!(!keys.contains(&"v_only"), "v_only has no text match");

    // Text rank 1 = "both" (key < "t_only"), rank 2 = "t_only".
    let expected_both: f64 = 1.0 / 61.0;
    let expected_t_only: f64 = 1.0 / 62.0;
    let s_both = results.iter().find(|(k, _)| k == "both").unwrap().1;
    let s_t_only = results.iter().find(|(k, _)| k == "t_only").unwrap().1;
    assert!(
        (s_both - expected_both).abs() < 1e-12,
        "both text-only score"
    );
    assert!(
        (s_t_only - expected_t_only).abs() < 1e-12,
        "t_only text-only score"
    );
}
