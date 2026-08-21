/// Integration tests for `GraphDb::suggest_rules_seeded`.
///
/// Fixture: "Product" (3 nodes) and "Supplier" (2 nodes).
///
/// Per-detector true-positive + near-miss:
///   (a) KeyMatch:      supplier_id matches Supplier keys  |  fake_id matches nothing
///   (b) Overlap:       tags list shares tokens            |  no_tags lists are disjoint
///   (c) FieldEqual:    category (low card, shared values) |  model (only in Product)
///   (d) NumericWithin: score ranges overlap               |  weight ranges don't overlap
///   (e) VectorSimilar: embedding dim-4 matches            |  emb_short dim mismatch (2 vs 3)
use core_api::{GraphDb, Predicate, RuleDef, RuleSuggestion, SuggestConfig, SuggestReport, Value, SUGGEST_DEFAULT_SEED};
use std::path::PathBuf;
use std::time::Instant;

fn tmp(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let d = std::env::temp_dir().join(format!(
        "graphdb-suggest-{}-{}-{}",
        name,
        std::process::id(),
        nanos
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn str(s: &str) -> Value {
    Value::Str(s.into())
}

fn float_list(v: &[f64]) -> Value {
    Value::List(v.iter().map(|&x| Value::Float(x)).collect())
}

fn str_list(v: &[&str]) -> Value {
    Value::List(v.iter().map(|&s| Value::Str(s.into())).collect())
}

/// Build the fixture database. Returns the directory.
fn build_fixture(name: &str) -> PathBuf {
    let dir = tmp(name);
    let mut db = GraphDb::open(&dir).unwrap();

    // --- Supplier nodes ---
    db.insert_node("Supplier", "s1", vec![
        ("category".into(),   str("tech")),
        ("tags".into(),       str_list(&["rust", "graph"])),
        ("score".into(),      Value::Int(101)),
        ("weight".into(),     Value::Float(100.0)),
        ("embedding".into(),  float_list(&[1.0, 0.0, 0.0, 0.0])),
        ("emb_short".into(),  float_list(&[1.0, 0.0, 0.0])), // dim 3
        ("no_tags".into(),    str_list(&["w1"])),
    ]).unwrap();
    db.insert_node("Supplier", "s2", vec![
        ("category".into(),   str("other")),
        ("tags".into(),       str_list(&["db", "sql"])),
        ("score".into(),      Value::Int(103)),
        ("weight".into(),     Value::Float(200.0)),
        ("embedding".into(),  float_list(&[0.0, 0.0, 1.0, 0.0])),
        ("emb_short".into(),  float_list(&[0.0, 1.0, 0.0])), // dim 3
        ("no_tags".into(),    str_list(&["w2"])),
    ]).unwrap();

    // --- Product nodes ---
    db.insert_node("Product", "p1", vec![
        ("supplier_id".into(), str("s1")),      // KeyMatch true positive
        ("fake_id".into(),     str("nope-1")), // KeyMatch near-miss: no match
        ("category".into(),    str("tech")),    // FieldEqual true positive
        ("model".into(),       str("alpha")),   // FieldEqual near-miss: not in Supplier
        ("tags".into(),        str_list(&["rust", "graph"])), // Overlap true positive
        ("no_tags".into(),     str_list(&["z1"])), // Overlap near-miss
        ("score".into(),       Value::Int(100)), // NumericWithin true positive
        ("weight".into(),      Value::Float(1.0)), // NumericWithin near-miss
        ("embedding".into(),   float_list(&[1.0, 0.0, 0.0, 0.0])), // VectorSimilar true positive
        ("emb_short".into(),   float_list(&[1.0, 0.0])), // VectorSimilar near-miss: dim 2
    ]).unwrap();
    db.insert_node("Product", "p2", vec![
        ("supplier_id".into(), str("s2")),
        ("fake_id".into(),     str("nope-2")),
        ("category".into(),    str("tech")),
        ("model".into(),       str("beta")),
        ("tags".into(),        str_list(&["rust", "db"])),
        ("no_tags".into(),     str_list(&["z2"])),
        ("score".into(),       Value::Int(102)),
        ("weight".into(),      Value::Float(2.0)),
        ("embedding".into(),   float_list(&[1.0, 0.0, 0.0, 0.0])),
        ("emb_short".into(),   float_list(&[0.0, 1.0])),
    ]).unwrap();
    db.insert_node("Product", "p3", vec![
        ("supplier_id".into(), str("s1")),
        ("fake_id".into(),     str("nope-3")),
        ("category".into(),    str("other")),
        ("model".into(),       str("gamma")),
        ("tags".into(),        str_list(&["graph", "sql"])),
        ("no_tags".into(),     str_list(&["z3"])),
        ("score".into(),       Value::Int(101)),
        ("weight".into(),      Value::Float(1.5)),
        ("embedding".into(),   float_list(&[0.0, 0.0, 1.0, 0.0])),
        // p3 has no emb_short — reduces emb_short count in Product to 2/3
    ]).unwrap();

    dir
}

// ---------------------------------------------------------------------------
// Helper: search suggestions by predicate kind + field
// ---------------------------------------------------------------------------

fn has_km(suggestions: &[RuleSuggestion], src: &str, dst: &str, field: &str) -> bool {
    suggestions.iter().any(|s| {
        s.def.src_label == src
            && s.def.dst_label == dst
            && matches!(&s.def.predicate, Predicate::KeyMatch { field: f } if f == field)
    })
}

fn has_fe(suggestions: &[RuleSuggestion], src: &str, dst: &str, field: &str) -> bool {
    suggestions.iter().any(|s| {
        s.def.src_label == src
            && s.def.dst_label == dst
            && matches!(&s.def.predicate, Predicate::FieldEqual { field: f } if f == field)
    })
}

fn has_ov(suggestions: &[RuleSuggestion], src: &str, dst: &str, field: &str) -> bool {
    suggestions.iter().any(|s| {
        s.def.src_label == src
            && s.def.dst_label == dst
            && matches!(&s.def.predicate, Predicate::Overlap { field: f, .. } if f == field)
    })
}

fn has_nw(suggestions: &[RuleSuggestion], src: &str, dst: &str, field: &str) -> bool {
    suggestions.iter().any(|s| {
        s.def.src_label == src
            && s.def.dst_label == dst
            && matches!(&s.def.predicate, Predicate::NumericWithin { field: f, .. } if f == field)
    })
}

fn has_vs(suggestions: &[RuleSuggestion], src: &str, dst: &str, field: &str) -> bool {
    suggestions.iter().any(|s| {
        s.def.src_label == src
            && s.def.dst_label == dst
            && matches!(&s.def.predicate, Predicate::VectorSimilar { field: f, .. } if f == field)
    })
}

fn find_km<'a>(suggestions: &'a [RuleSuggestion], src: &str, dst: &str, field: &str) -> &'a RuleSuggestion {
    suggestions.iter().find(|s| {
        s.def.src_label == src
            && s.def.dst_label == dst
            && matches!(&s.def.predicate, Predicate::KeyMatch { field: f } if f == field)
    }).expect("KeyMatch suggestion not found")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn empty_db_returns_no_suggestions() {
    let dir = tmp("suggest-empty");
    let db = GraphDb::open(&dir).unwrap();
    let suggestions = db.suggest_rules();
    assert!(suggestions.is_empty(), "expected empty, got: {suggestions:?}");
}

#[test]
fn empty_db_no_panic() {
    // Explicitly check no panic even with an empty directory.
    let dir = tmp("suggest-empty-panic");
    let db = GraphDb::open(&dir).unwrap();
    let _ = db.suggest_rules();
}

#[test]
fn keymatches_true_positive_and_near_miss() {
    let dir = build_fixture("km");
    let db = GraphDb::open(&dir).unwrap();
    let suggestions = db.suggest_rules_seeded(SUGGEST_DEFAULT_SEED);

    // True positive: supplier_id → Supplier keys (s1, s2).
    assert!(
        has_km(&suggestions, "Product", "Supplier", "supplier_id"),
        "expected KeyMatch(supplier_id) suggestion. Got: {suggestions:#?}"
    );

    // Near-miss: fake_id values don't match any Supplier key.
    assert!(
        !has_km(&suggestions, "Product", "Supplier", "fake_id"),
        "fake_id should NOT be suggested as KeyMatch"
    );
}

#[test]
fn keymatch_est_edges_exact_when_fully_sampled() {
    // With 3 Product nodes and max_sample_sources=200, all 3 are sampled.
    // True count: p1→s1, p2→s2, p3→s1 = 3 edges.
    // Formula: hit_rate = 3/(3×2) = 0.5; est = 0.5×3×2 = 3.0 → exact.
    let dir = build_fixture("km-est");
    let db = GraphDb::open(&dir).unwrap();
    let suggestions = db.suggest_rules_seeded(SUGGEST_DEFAULT_SEED);
    let s = find_km(&suggestions, "Product", "Supplier", "supplier_id");
    assert_eq!(
        s.est_edges, 3,
        "all 3 sources sampled → est_edges must exactly equal true count 3, got {}",
        s.est_edges
    );
}

#[test]
fn keymatch_rationale_non_empty() {
    let dir = build_fixture("km-rationale");
    let db = GraphDb::open(&dir).unwrap();
    let suggestions = db.suggest_rules_seeded(SUGGEST_DEFAULT_SEED);
    for s in &suggestions {
        assert!(
            !s.rationale.is_empty(),
            "suggestion {:?} has empty rationale",
            s.def.name
        );
    }
}

#[test]
fn overlap_true_positive_and_near_miss() {
    let dir = build_fixture("ov");
    let db = GraphDb::open(&dir).unwrap();
    let suggestions = db.suggest_rules_seeded(SUGGEST_DEFAULT_SEED);

    // True positive: tags have Jaccard > 0 across labels.
    assert!(
        has_ov(&suggestions, "Product", "Supplier", "tags"),
        "expected Overlap(tags) suggestion. Got: {suggestions:#?}"
    );

    // Near-miss: no_tags lists are fully disjoint (Jaccard p50 = 0).
    assert!(
        !has_ov(&suggestions, "Product", "Supplier", "no_tags"),
        "no_tags should NOT be suggested as Overlap"
    );
}

#[test]
fn fieldequal_true_positive_and_near_miss() {
    let dir = build_fixture("fe");
    let db = GraphDb::open(&dir).unwrap();
    let suggestions = db.suggest_rules_seeded(SUGGEST_DEFAULT_SEED);

    // True positive: category is low-cardinality and shared across labels.
    assert!(
        has_fe(&suggestions, "Product", "Supplier", "category"),
        "expected FieldEqual(category) suggestion. Got: {suggestions:#?}"
    );

    // Near-miss: model is only present in Product, not in Supplier.
    // So there is no dst profile for model in Supplier → not suggested cross-label.
    assert!(
        !has_fe(&suggestions, "Product", "Supplier", "model"),
        "model should NOT be suggested as FieldEqual for Product→Supplier (Supplier has no model)"
    );
}

#[test]
fn numeric_within_true_positive_and_near_miss() {
    let dir = build_fixture("nw");
    let db = GraphDb::open(&dir).unwrap();
    let suggestions = db.suggest_rules_seeded(SUGGEST_DEFAULT_SEED);

    // True positive: score ranges [100,102] and [101,103] overlap.
    assert!(
        has_nw(&suggestions, "Product", "Supplier", "score"),
        "expected NumericWithin(score) suggestion. Got: {suggestions:#?}"
    );

    // Near-miss: weight ranges [1,2] vs [100,200] don't overlap.
    assert!(
        !has_nw(&suggestions, "Product", "Supplier", "weight"),
        "weight should NOT be suggested as NumericWithin (ranges don't overlap)"
    );
}

#[test]
fn vector_similar_true_positive_and_near_miss() {
    let dir = build_fixture("vs");
    let db = GraphDb::open(&dir).unwrap();
    let suggestions = db.suggest_rules_seeded(SUGGEST_DEFAULT_SEED);

    // True positive: embedding dim=4 in both Product and Supplier.
    assert!(
        has_vs(&suggestions, "Product", "Supplier", "embedding"),
        "expected VectorSimilar(embedding) suggestion. Got: {suggestions:#?}"
    );

    // Near-miss: emb_short is dim=2 in Product but dim=3 in Supplier → dim mismatch.
    assert!(
        !has_vs(&suggestions, "Product", "Supplier", "emb_short"),
        "emb_short should NOT be suggested (dim mismatch 2 vs 3)"
    );
}

#[test]
fn already_ruled_pair_not_re_suggested() {
    let dir = build_fixture("dedup");
    let mut db = GraphDb::open(&dir).unwrap();

    // Create a FieldEqual rule for category before calling suggest.
    db.create_rule(RuleDef {
        name: "existing_category_rule".into(),
        src_label: "Product".into(),
        dst_label: "Supplier".into(),
        predicate: Predicate::FieldEqual {
            field: "category".into(),
        },
        edge_type: "SAME_CATEGORY".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
    })
    .unwrap();

    let suggestions = db.suggest_rules_seeded(SUGGEST_DEFAULT_SEED);

    // The already-ruled pair must not appear.
    assert!(
        !has_fe(&suggestions, "Product", "Supplier", "category"),
        "Product→Supplier FieldEqual(category) should NOT be re-suggested (rule exists)"
    );
}

#[test]
fn determinism_same_seed_identical_output() {
    let dir = build_fixture("det");
    let db = GraphDb::open(&dir).unwrap();

    let r1 = db.suggest_rules_seeded(42);
    let r2 = db.suggest_rules_seeded(42);

    assert_eq!(
        r1.len(),
        r2.len(),
        "same seed should return same number of suggestions"
    );
    for (a, b) in r1.iter().zip(r2.iter()) {
        assert_eq!(a.def.name, b.def.name, "def.name must be identical");
        assert_eq!(a.est_edges, b.est_edges, "est_edges must be identical");
        assert_eq!(a.rationale, b.rationale, "rationale must be identical (same seed)");
        assert_eq!(
            a.examples.len(),
            b.examples.len(),
            "example count must be identical"
        );
        for ((src1, dst1, score1), (src2, dst2, score2)) in
            a.examples.iter().zip(b.examples.iter())
        {
            assert_eq!(src1, src2, "example src must be identical");
            assert_eq!(dst1, dst2, "example dst must be identical");
            assert!(
                (score1 - score2).abs() < 1e-12,
                "example score must be identical: {score1} vs {score2}"
            );
        }
    }
}

#[test]
fn different_seeds_may_differ() {
    // Different seeds should produce the same deterministic structure
    // (same candidates, possibly different sample ordering) but at least
    // not crash. Both must return non-empty results.
    let dir = build_fixture("diff-seed");
    let db = GraphDb::open(&dir).unwrap();
    let r1 = db.suggest_rules_seeded(1);
    let r2 = db.suggest_rules_seeded(999_999);
    // Both runs must produce the same candidates (the detector logic is deterministic
    // per data; only the preview sample may differ slightly).
    assert_eq!(r1.len(), r2.len(), "same data, different seeds: candidate count must match");
}

#[test]
fn time_budget_per_candidate_does_not_hang() {
    // per-candidate budget=1 ms — must return quickly, not panic.
    let dir = build_fixture("budget-pc");
    let db = GraphDb::open(&dir).unwrap();
    let config = SuggestConfig {
        budget_ms: 1,
        ..SuggestConfig::default()
    };
    let start = Instant::now();
    let _ = db.suggest_rules_with_config(&config, SUGGEST_DEFAULT_SEED);
    assert!(start.elapsed().as_secs() < 30, "suggest hung");
}

#[test]
fn global_budget_fires_and_sets_truncated() {
    // Non-vacuous: the fixture produces multiple suggestions with the full budget.
    // With global_budget_ms=0 the deadline fires before the first preview, so we get
    // truncated=true and fewer suggestions than the full run.
    //
    // Removing the `if Instant::now() >= global_deadline { truncated = true; break 'detect; }`
    // lines from suggest.rs would leave truncated=false and suggestions.len() equal to the full
    // run's count — both assertions below would then fail.
    let dir = build_fixture("budget-global");
    let db = GraphDb::open(&dir).unwrap();

    // Full run must produce at least one suggestion from this fixture.
    let full: SuggestReport =
        db.suggest_rules_with_config(&SuggestConfig::default(), SUGGEST_DEFAULT_SEED);
    assert!(
        !full.suggestions.is_empty(),
        "fixture must produce suggestions with full budget"
    );
    assert!(!full.truncated, "full budget must not set truncated");

    // Zero global budget must truncate immediately.
    let config_zero = SuggestConfig {
        global_budget_ms: 0,
        ..SuggestConfig::default()
    };
    let partial: SuggestReport =
        db.suggest_rules_with_config(&config_zero, SUGGEST_DEFAULT_SEED);
    assert!(
        partial.truncated,
        "global_budget_ms=0 must set truncated=true; got truncated={}",
        partial.truncated
    );
    assert!(
        partial.suggestions.len() < full.suggestions.len(),
        "partial run must have fewer suggestions than full run: {} vs {}",
        partial.suggestions.len(),
        full.suggestions.len()
    );
}

#[test]
fn time_budget_zero_no_panic() {
    let dir = build_fixture("budget-zero");
    let db = GraphDb::open(&dir).unwrap();
    let config = SuggestConfig {
        budget_ms: 0,
        global_budget_ms: 0,
        ..SuggestConfig::default()
    };
    // Must not panic; results may be empty.
    let _ = db.suggest_rules_with_config(&config, SUGGEST_DEFAULT_SEED);
}

#[test]
fn suggestions_sorted_by_est_edges_desc() {
    let dir = build_fixture("sorted");
    let db = GraphDb::open(&dir).unwrap();
    let suggestions = db.suggest_rules_seeded(SUGGEST_DEFAULT_SEED);
    for w in suggestions.windows(2) {
        assert!(
            w[0].est_edges >= w[1].est_edges,
            "suggestions not sorted: {:?} before {:?}",
            w[0].def.name,
            w[1].def.name
        );
    }
}
