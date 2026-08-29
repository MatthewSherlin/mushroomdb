//! Tests for mask-aware ANN search (`find_similar_vector_masked`).
//!
//! Coverage:
//!  1. Brute-force path (no rule) — hidden nodes excluded.
//!  2. HNSW path (approximate rule) — hidden nodes excluded.
//!  3. Pre-truncation masking — top-k masked nodes do not leak; caller still
//!     receives up to k visible hits.
//!  4. Labeled (Some("Label")) and any-label (None) variants.
//!  5. Adversarial: a hidden node's key/score never appears in output.

use core_api::{GraphDb, NodeMask, Predicate, RuleDef, Value};

fn tmp(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "graphdb-masked-ann-{}-{}-{}",
        name,
        std::process::id(),
        nanos
    ))
}

fn emb(xs: &[f64]) -> Value {
    Value::List(xs.iter().map(|&x| Value::Float(x)).collect())
}

fn approx_rule(src: &str, dst: &str, field: &str, edge: &str) -> RuleDef {
    RuleDef {
        name: format!("ann_{edge}"),
        src_label: src.into(),
        dst_label: dst.into(),
        predicate: Predicate::VectorSimilar {
            field: field.into(),
            min: 0.5,
        },
        edge_type: edge.into(),
        weight_prop: None,
        max_edges: None,
        approximate: true,
        via_label: None,
        via_edge: None,
        via_dir: None,
    }
}

// ---------------------------------------------------------------------------
// 1. Brute-force path (no HNSW rule) — hidden node excluded
// ---------------------------------------------------------------------------

#[test]
fn brute_force_hides_masked_node_labeled() {
    let dir = tmp("bf-labeled");
    let mut db = GraphDb::open(&dir).unwrap();

    // a ≈ [1,0], perfectly aligned with query.
    db.insert_node("Item", "a", vec![("emb".into(), emb(&[1.0, 0.0]))])
        .unwrap();
    // b ≈ [0.9,0.1], close but slightly off.
    db.insert_node("Item", "b", vec![("emb".into(), emb(&[0.9, 0.1]))])
        .unwrap();
    // hidden: [1,0] — would score 1.0 but is masked out.
    db.insert_node("Item", "hidden", vec![("emb".into(), emb(&[1.0, 0.0]))])
        .unwrap();

    // Mask: only a and b are visible.
    let mask = NodeMask::from_keys(&db, ["a", "b"]);

    let hits = db.find_similar_vector_masked("emb", Some("Item"), &[1.0_f64, 0.0], 10, 0.0, &mask);

    let keys: Vec<&str> = hits.iter().map(|(k, _)| k.as_str()).collect();
    assert!(keys.contains(&"a"), "a must be visible");
    assert!(keys.contains(&"b"), "b must be visible");
    assert!(
        !keys.contains(&"hidden"),
        "hidden node must never appear in masked results"
    );
}

#[test]
fn brute_force_hides_masked_node_any_label() {
    let dir = tmp("bf-any-label");
    let mut db = GraphDb::open(&dir).unwrap();

    db.insert_node("X", "visible", vec![("emb".into(), emb(&[1.0, 0.0]))])
        .unwrap();
    db.insert_node("Y", "hidden", vec![("emb".into(), emb(&[1.0, 0.0]))])
        .unwrap();

    let mask = NodeMask::from_keys(&db, ["visible"]);

    // label=None spans all labels.
    let hits = db.find_similar_vector_masked("emb", None, &[1.0_f64, 0.0], 10, 0.0, &mask);

    let keys: Vec<&str> = hits.iter().map(|(k, _)| k.as_str()).collect();
    assert!(keys.contains(&"visible"));
    assert!(!keys.contains(&"hidden"));
}

// ---------------------------------------------------------------------------
// 2. HNSW fast path — hidden node excluded after index lookup
// ---------------------------------------------------------------------------

#[test]
fn hnsw_hides_masked_node() {
    let dir = tmp("hnsw-mask");
    let mut db = GraphDb::open(&dir).unwrap();

    // Insert enough nodes to populate the HNSW index (needs at least 2).
    for i in 0..6u32 {
        let x = i as f64 * 0.15;
        db.insert_node(
            "V",
            &format!("v{i}"),
            vec![("emb".into(), emb(&[x, 1.0 - x]))],
        )
        .unwrap();
    }
    // v0 ≈ [0,1], v5 ≈ [0.75,0.25] — query [1,0] ranks v5 higher.
    // "secret" overlaps exactly with the query direction — would be top result.
    db.insert_node("V", "secret", vec![("emb".into(), emb(&[1.0, 0.0]))])
        .unwrap();

    db.create_rule(approx_rule("V", "V", "emb", "SIM")).unwrap();
    assert!(db.has_vector_rule("emb"), "HNSW rule must be registered");

    // Visible set excludes secret.
    let visible: Vec<String> = (0..6).map(|i| format!("v{i}")).collect();
    let mask = NodeMask::from_keys(&db, visible.iter().map(String::as_str));

    let hits = db.find_similar_vector_masked("emb", Some("V"), &[1.0_f64, 0.0], 10, 0.0, &mask);

    let keys: Vec<&str> = hits.iter().map(|(k, _)| k.as_str()).collect();
    assert!(
        !keys.contains(&"secret"),
        "secret must never appear via HNSW masked path"
    );
    assert!(!keys.is_empty(), "visible nodes must still be returned");
}

// ---------------------------------------------------------------------------
// 3. Pre-truncation: top-k masked → still receives up to k visible hits
// ---------------------------------------------------------------------------

#[test]
fn pre_truncation_mask_applied_before_k() {
    let dir = tmp("pre-trunc");
    let mut db = GraphDb::open(&dir).unwrap();

    // 6 nodes: hidden0=[1,0] and hidden1=[0.99,0.01] are closest, then v0..v3.
    db.insert_node("N", "hidden0", vec![("emb".into(), emb(&[1.0, 0.0]))])
        .unwrap();
    db.insert_node("N", "hidden1", vec![("emb".into(), emb(&[0.99, 0.01]))])
        .unwrap();
    for i in 0..4u32 {
        let x = 0.5 + i as f64 * 0.05;
        db.insert_node(
            "N",
            &format!("v{i}"),
            vec![("emb".into(), emb(&[x, 1.0 - x]))],
        )
        .unwrap();
    }

    // Mask hides the two top-scoring nodes.
    let visible: Vec<&str> = vec!["v0", "v1", "v2", "v3"];
    let mask = NodeMask::from_keys(&db, visible.iter().copied());

    // Request k=4 — should return all 4 visible nodes, not 0 or 2.
    let hits = db.find_similar_vector_masked("emb", Some("N"), &[1.0_f64, 0.0], 4, 0.0, &mask);

    let keys: Vec<&str> = hits.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        hits.len(),
        4,
        "must return k=4 visible hits, not be cut short by hidden top-2"
    );
    assert!(!keys.contains(&"hidden0"));
    assert!(!keys.contains(&"hidden1"));
}

// ---------------------------------------------------------------------------
// 4. Labeled vs any-label both covered (HNSW path)
// ---------------------------------------------------------------------------

#[test]
fn hnsw_labeled_and_any_label_both_respect_mask() {
    let dir = tmp("hnsw-label-any");
    let mut db = GraphDb::open(&dir).unwrap();

    // Two labels, each with a visible and a hidden node.
    db.insert_node("A", "a_vis", vec![("emb".into(), emb(&[1.0, 0.0]))])
        .unwrap();
    db.insert_node("A", "a_hid", vec![("emb".into(), emb(&[1.0, 0.0]))])
        .unwrap();
    db.insert_node("B", "b_vis", vec![("emb".into(), emb(&[0.9, 0.1]))])
        .unwrap();
    db.insert_node("B", "b_hid", vec![("emb".into(), emb(&[0.9, 0.1]))])
        .unwrap();

    db.create_rule(approx_rule("A", "A", "emb", "SIMA"))
        .unwrap();
    db.create_rule(approx_rule("B", "B", "emb", "SIMB"))
        .unwrap();

    let mask = NodeMask::from_keys(&db, ["a_vis", "b_vis"]);

    // Labeled search — only A nodes.
    let hits_a = db.find_similar_vector_masked("emb", Some("A"), &[1.0_f64, 0.0], 10, 0.0, &mask);
    let keys_a: Vec<&str> = hits_a.iter().map(|(k, _)| k.as_str()).collect();
    assert!(keys_a.contains(&"a_vis"));
    assert!(!keys_a.contains(&"a_hid"));
    assert!(!keys_a.contains(&"b_vis"), "label filter must exclude B");

    // Any-label search (None).
    let hits_any = db.find_similar_vector_masked("emb", None, &[1.0_f64, 0.0], 10, 0.0, &mask);
    let keys_any: Vec<&str> = hits_any.iter().map(|(k, _)| k.as_str()).collect();
    assert!(keys_any.contains(&"a_vis"));
    assert!(keys_any.contains(&"b_vis"));
    assert!(!keys_any.contains(&"a_hid"));
    assert!(!keys_any.contains(&"b_hid"));
}

// ---------------------------------------------------------------------------
// 5. Adversarial: no score or key from a hidden node leaks
// ---------------------------------------------------------------------------

#[test]
fn adversarial_hidden_node_never_leaks() {
    let dir = tmp("adversarial");
    let mut db = GraphDb::open(&dir).unwrap();

    // "poison" node is the most similar to the query — must never appear.
    db.insert_node("T", "poison", vec![("emb".into(), emb(&[1.0, 0.0]))])
        .unwrap();
    for i in 0..5u32 {
        let x = 0.5 + i as f64 * 0.05;
        db.insert_node(
            "T",
            &format!("ok{i}"),
            vec![("emb".into(), emb(&[x, 1.0 - x]))],
        )
        .unwrap();
    }

    let visible: Vec<String> = (0..5u32).map(|i| format!("ok{i}")).collect();
    let mask = NodeMask::from_keys(&db, visible.iter().map(String::as_str));

    // Brute-force path.
    let hits = db.find_similar_vector_masked("emb", Some("T"), &[1.0_f64, 0.0], 10, 0.0, &mask);

    for (key, _score) in &hits {
        assert_ne!(key, "poison", "poison key must never appear");
    }
    // The poison score (1.0) must not be present — all visible scores < 1.0.
    let max_score = hits
        .iter()
        .map(|(_, s)| *s)
        .fold(f64::NEG_INFINITY, f64::max);
    assert!(
        max_score < 1.0 - 1e-9,
        "no result should have the poison node's score of 1.0; got {max_score}"
    );
}

// ---------------------------------------------------------------------------
// 6. Adversarial: HNSW path — hidden top-scorer never leaks key or score
// ---------------------------------------------------------------------------

#[test]
fn adversarial_hnsw_hidden_node_never_leaks() {
    let dir = tmp("adversarial-hnsw");
    let mut db = GraphDb::open(&dir).unwrap();

    // "poison" has the perfect query alignment; it must never appear after masking.
    db.insert_node("V", "poison", vec![("emb".into(), emb(&[1.0, 0.0]))])
        .unwrap();
    for i in 0..6u32 {
        let x = 0.5 + i as f64 * 0.05;
        db.insert_node(
            "V",
            &format!("ok{i}"),
            vec![("emb".into(), emb(&[x, 1.0 - x]))],
        )
        .unwrap();
    }

    // Register an approximate rule so the HNSW index is populated.
    db.create_rule(approx_rule("V", "V", "emb", "SIM")).unwrap();
    assert!(db.has_vector_rule("emb"), "HNSW rule must be present");

    let visible: Vec<String> = (0..6u32).map(|i| format!("ok{i}")).collect();
    let mask = NodeMask::from_keys(&db, visible.iter().map(String::as_str));

    let hits = db.find_similar_vector_masked("emb", Some("V"), &[1.0_f64, 0.0], 10, 0.0, &mask);

    for (key, _score) in &hits {
        assert_ne!(key, "poison", "poison key must never appear via HNSW path");
    }
    let max_score = hits
        .iter()
        .map(|(_, s)| *s)
        .fold(f64::NEG_INFINITY, f64::max);
    // All visible nodes have x ∈ [0.5, 0.75], so their cosine with [1,0] < 1.0.
    assert!(
        max_score < 1.0 - 1e-9,
        "poison score of 1.0 must not appear in HNSW-masked results; got {max_score}"
    );
    assert!(!hits.is_empty(), "visible nodes must still be returned");
}
