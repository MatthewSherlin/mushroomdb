/// Integration tests for `Predicate::Any` — OR composition.
///
/// TDD order per brief:
///   1. Two-branch Any (Overlap + NumericWithin) derives correct edges.
///   2. Nested All-of-Any composition.
///   3. Score = max pin: Any score is the maximum over satisfied branches.
///   4. Retraction when the only satisfied branch breaks.
///   5. Any with max_edges (top-k): branch-score changes cause evict/backfill.
///   6. Snapshot V4 round-trip: RuleDef with Any survives snapshot + WAL replay.
///   7. Bincode backward-compat: old (pre-Any) records still decode.
use core_api::{Direction, GraphDb, Predicate, RuleDef, Value};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir()
        .join(format!("graphdb-any-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn mk_tags(items: &[&str]) -> Value {
    Value::List(items.iter().map(|s| Value::Str((*s).into())).collect())
}

// ---------------------------------------------------------------------------
// 1. Two-branch Any: Overlap OR NumericWithin
// ---------------------------------------------------------------------------

/// Any([Overlap(tags, 0.3), NumericWithin(year, 5)]) derives an edge when
/// either branch fires — even when the other does not.
#[test]
fn any_two_branch_overlap_or_numeric_derives_edges() {
    let dir = tmp("two-branch");
    let mut db = GraphDb::open(&dir).unwrap();

    // Rule: src/dst label "N", Any(Overlap OR NumericWithin).
    db.create_rule(RuleDef {
        name: "any_test".into(),
        src_label: "N".into(),
        dst_label: "N".into(),
        predicate: Predicate::Any(vec![
            Predicate::Overlap {
                field: "tags".into(),
                min: 0.3,
            },
            Predicate::NumericWithin {
                field: "year".into(),
                tolerance: 5.0,
            },
        ]),
        edge_type: "ANY".into(),
        weight_prop: Some("score".into()),
        max_edges: None,
        approximate: false,
    })
    .unwrap();

    // a: tags=["x","y"], year=2000
    // b: tags=["y","z"], year=2010  — shares tag "y" (jaccard 1/3 ≥ 0.3 → Overlap fires)
    // c: tags=["p","q"], year=2003  — no tag overlap; year diff=3 ≤ 5 → NumericWithin fires
    // d: tags=["p","q"], year=2050  — no tag overlap; year diff=50 > 5 → neither fires
    db.insert_node("N", "a", vec![
        ("tags".into(), mk_tags(&["x", "y"])),
        ("year".into(), Value::Int(2000)),
    ]).unwrap();
    db.insert_node("N", "b", vec![
        ("tags".into(), mk_tags(&["y", "z"])),
        ("year".into(), Value::Int(2010)),
    ]).unwrap();
    db.insert_node("N", "c", vec![
        ("tags".into(), mk_tags(&["p", "q"])),
        ("year".into(), Value::Int(2003)),
    ]).unwrap();
    db.insert_node("N", "d", vec![
        ("tags".into(), mk_tags(&["p", "q"])),
        ("year".into(), Value::Int(2050)),
    ]).unwrap();

    let a_out: Vec<String> = db.neighbors("a", "ANY", Direction::Out).unwrap_or_default();

    // a→b: Overlap fires (jaccard 1/3 ≥ 0.3); year diff=10 > 5 so numeric doesn't.
    assert!(
        a_out.contains(&"b".to_string()),
        "a→b must exist (Overlap branch fires); got {a_out:?}"
    );

    // a→c: tags disjoint; year diff=3 ≤ 5 → NumericWithin fires.
    assert!(
        a_out.contains(&"c".to_string()),
        "a→c must exist (NumericWithin branch fires); got {a_out:?}"
    );

    // a→d: neither branch fires.
    assert!(
        !a_out.contains(&"d".to_string()),
        "a→d must not exist (no branch fires); got {a_out:?}"
    );
}

// ---------------------------------------------------------------------------
// 2. Nested All(FieldEqual, Any(Overlap, NumericWithin))
// ---------------------------------------------------------------------------

#[test]
fn any_nested_in_all_derives_edges() {
    let dir = tmp("nested");
    let mut db = GraphDb::open(&dir).unwrap();

    // Rule: All(FieldEqual(ind), Any(Overlap(tags,0.3), NumericWithin(year,5)))
    db.create_rule(RuleDef {
        name: "nested".into(),
        src_label: "N".into(),
        dst_label: "N".into(),
        predicate: Predicate::All(vec![
            Predicate::FieldEqual {
                field: "ind".into(),
            },
            Predicate::Any(vec![
                Predicate::Overlap {
                    field: "tags".into(),
                    min: 0.3,
                },
                Predicate::NumericWithin {
                    field: "year".into(),
                    tolerance: 5.0,
                },
            ]),
        ]),
        edge_type: "NESTED".into(),
        weight_prop: Some("score".into()),
        max_edges: None,
        approximate: false,
    })
    .unwrap();

    // a and b: same ind, share tag → FieldEqual fires + Overlap fires.
    // a and c: same ind, year diff=3 ≤ 5 → FieldEqual fires + NumericWithin fires.
    // a and d: different ind → FieldEqual fails → no edge (regardless of Any).
    db.insert_node("N", "a", vec![
        ("ind".into(), Value::Str("arch".into())),
        ("tags".into(), mk_tags(&["x", "y"])),
        ("year".into(), Value::Int(2000)),
    ]).unwrap();
    db.insert_node("N", "b", vec![
        ("ind".into(), Value::Str("arch".into())),
        ("tags".into(), mk_tags(&["y", "z"])),
        ("year".into(), Value::Int(2020)),
    ]).unwrap();
    db.insert_node("N", "c", vec![
        ("ind".into(), Value::Str("arch".into())),
        ("tags".into(), mk_tags(&["p", "q"])),
        ("year".into(), Value::Int(2003)),
    ]).unwrap();
    db.insert_node("N", "d", vec![
        ("ind".into(), Value::Str("law".into())),
        ("tags".into(), mk_tags(&["y", "z"])),
        ("year".into(), Value::Int(2001)),
    ]).unwrap();

    let a_out: Vec<String> = db.neighbors("a", "NESTED", Direction::Out).unwrap_or_default();
    assert!(
        a_out.contains(&"b".to_string()),
        "a→b: same ind + tag overlap → must exist; got {a_out:?}"
    );
    assert!(
        a_out.contains(&"c".to_string()),
        "a→c: same ind + year proximity → must exist; got {a_out:?}"
    );
    assert!(
        !a_out.contains(&"d".to_string()),
        "a→d: different ind → must not exist; got {a_out:?}"
    );
}

// ---------------------------------------------------------------------------
// 3. Score = max pin
// ---------------------------------------------------------------------------

/// When both branches satisfy, Any returns the higher score.
#[test]
fn any_score_is_max_over_satisfied_branches() {
    let dir = tmp("maxscore");
    let mut db = GraphDb::open(&dir).unwrap();

    // Rule: Any(FieldEqual(ind) → score 1.0, NumericWithin(year, 3) → variable).
    db.create_rule(RuleDef {
        name: "maxscore".into(),
        src_label: "N".into(),
        dst_label: "N".into(),
        predicate: Predicate::Any(vec![
            Predicate::FieldEqual {
                field: "ind".into(),
            },
            Predicate::NumericWithin {
                field: "year".into(),
                tolerance: 3.0,
            },
        ]),
        edge_type: "MS".into(),
        weight_prop: Some("score".into()),
        max_edges: None,
        approximate: false,
    })
    .unwrap();

    // a & b: ind match (score 1.0) AND year diff=1, tol=3 (score 2/3).
    // Any → max(1.0, 2/3) = 1.0.
    db.insert_node("N", "a", vec![
        ("ind".into(), Value::Str("arch".into())),
        ("year".into(), Value::Int(2000)),
    ]).unwrap();
    db.insert_node("N", "b", vec![
        ("ind".into(), Value::Str("arch".into())),
        ("year".into(), Value::Int(2001)),
    ]).unwrap();

    let explain = db.explain("a", "b").unwrap();
    let entry = explain
        .iter()
        .find(|e| e.rule == "maxscore" && e.src_key == "a" && e.dst_key == "b")
        .expect("a→b must have an explain entry for 'maxscore'");
    let w = entry.weight.expect("weight must be present (weight_prop set)");
    assert!(
        (w - 1.0).abs() < 1e-9,
        "Any score must be max(1.0, 2/3) = 1.0; got {w}"
    );

    // a & c: only NumericWithin fires (ind differs), year diff=2 → score 1/3.
    db.insert_node("N", "c", vec![
        ("ind".into(), Value::Str("law".into())),
        ("year".into(), Value::Int(2002)),
    ]).unwrap();
    let explain_c = db.explain("a", "c").unwrap();
    let entry_c = explain_c
        .iter()
        .find(|e| e.rule == "maxscore" && e.src_key == "a" && e.dst_key == "c")
        .expect("a→c must have an explain entry");
    let wc = entry_c.weight.expect("weight present");
    assert!(
        (wc - 1.0 / 3.0).abs() < 1e-9,
        "Any score (only numeric branch fires, year diff=2, tol=3) must be 1/3; got {wc}"
    );
}

// ---------------------------------------------------------------------------
// 4. Retraction: sole satisfied branch breaks → edge retracted.
// ---------------------------------------------------------------------------

#[test]
fn any_retraction_when_sole_branch_breaks() {
    let dir = tmp("retract");
    let mut db = GraphDb::open(&dir).unwrap();

    // Rule: Any(FieldEqual(ind), NumericWithin(year, 2))
    db.create_rule(RuleDef {
        name: "ret".into(),
        src_label: "N".into(),
        dst_label: "N".into(),
        predicate: Predicate::Any(vec![
            Predicate::FieldEqual {
                field: "ind".into(),
            },
            Predicate::NumericWithin {
                field: "year".into(),
                tolerance: 2.0,
            },
        ]),
        edge_type: "RET".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
    })
    .unwrap();

    // a: ind="arch", year=2000; b: ind="law", year=2001 (year diff=1 ≤ 2).
    // Only NumericWithin branch fires for a→b initially.
    db.insert_node("N", "a", vec![
        ("ind".into(), Value::Str("arch".into())),
        ("year".into(), Value::Int(2000)),
    ]).unwrap();
    db.insert_node("N", "b", vec![
        ("ind".into(), Value::Str("law".into())),
        ("year".into(), Value::Int(2001)),
    ]).unwrap();

    let a_out = db.neighbors("a", "RET", Direction::Out).unwrap_or_default();
    assert!(
        a_out.contains(&"b".to_string()),
        "a→b must exist initially (NumericWithin branch fires); got {a_out:?}"
    );

    // Change b's year so diff=5 > 2 — NumericWithin no longer fires.
    // FieldEqual still won't fire (ind still differs). Edge must be retracted.
    db.set_prop("b", "year", Value::Int(2005)).unwrap();

    let a_out2 = db.neighbors("a", "RET", Direction::Out).unwrap_or_default();
    assert!(
        !a_out2.contains(&"b".to_string()),
        "a→b must be retracted after year change breaks the sole matching branch; got {a_out2:?}"
    );

    // Change b's ind to match "arch" → FieldEqual branch now fires; edge re-derives.
    db.set_prop("b", "ind", Value::Str("arch".into())).unwrap();

    let a_out3 = db.neighbors("a", "RET", Direction::Out).unwrap_or_default();
    assert!(
        a_out3.contains(&"b".to_string()),
        "a→b must re-derive when FieldEqual branch fires; got {a_out3:?}"
    );
}

// ---------------------------------------------------------------------------
// 5. Any with max_edges (top-k): branch-score change causes evict/backfill.
// ---------------------------------------------------------------------------

/// Verifies that when max_edges=Some(1) and a property change alters which
/// branch of Any fires (and thus the score), the per-source top-1 is
/// correctly re-evaluated: the lower-scoring dst is evicted and the higher-
/// scoring one backfills.
#[test]
fn any_with_max_edges_score_change_causes_evict_backfill() {
    let dir = tmp("topk");
    let mut db = GraphDb::open(&dir).unwrap();

    // Rule: Any(FieldEqual(ind) → score 1.0, NumericWithin(year, 10) → variable),
    // max_edges=Some(1) → top-1 per source.
    db.create_rule(RuleDef {
        name: "topk_any".into(),
        src_label: "N".into(),
        dst_label: "N".into(),
        predicate: Predicate::Any(vec![
            Predicate::FieldEqual {
                field: "ind".into(),
            },
            Predicate::NumericWithin {
                field: "year".into(),
                tolerance: 10.0,
            },
        ]),
        edge_type: "TK".into(),
        weight_prop: Some("score".into()),
        max_edges: Some(1),
        approximate: false,
    })
    .unwrap();

    // src: ind="arch", year=2000.
    // d_low: ind="law", year=2009 → only NumericWithin fires, score=1-9/10=0.1.
    // d_high: ind="arch", year=2020 → FieldEqual fires (score 1.0); numeric year diff=20>10 no.
    //
    // Insert d_low first so it claims top-1 provisionally.
    // Insert d_high second → score 1.0 > 0.1 → d_high evicts d_low.
    db.insert_node("N", "src", vec![
        ("ind".into(), Value::Str("arch".into())),
        ("year".into(), Value::Int(2000)),
    ]).unwrap();
    db.insert_node("N", "d_low", vec![
        ("ind".into(), Value::Str("law".into())),
        ("year".into(), Value::Int(2009)),
    ]).unwrap();
    db.insert_node("N", "d_high", vec![
        ("ind".into(), Value::Str("arch".into())),
        ("year".into(), Value::Int(2020)),
    ]).unwrap();

    let top1: Vec<String> = db
        .neighbors("src", "TK", Direction::Out)
        .unwrap_or_default();
    assert_eq!(
        top1,
        vec!["d_high"],
        "top-1 must be d_high (score 1.0 > 0.1); got {top1:?}"
    );

    // Change d_high's ind so FieldEqual no longer fires and year diff=20>10 so
    // NumericWithin also doesn't fire. d_high drops out entirely.
    // d_low (year diff=9 ≤ 10) must backfill as the new top-1.
    db.set_prop("d_high", "ind", Value::Str("law".into())).unwrap();

    let top1_after: Vec<String> = db
        .neighbors("src", "TK", Direction::Out)
        .unwrap_or_default();
    assert_eq!(
        top1_after,
        vec!["d_low"],
        "d_low must backfill after d_high loses its only matching branch; got {top1_after:?}"
    );

    // Restore d_high's ind → d_high evicts d_low again.
    db.set_prop("d_high", "ind", Value::Str("arch".into())).unwrap();
    let top1_restored: Vec<String> = db
        .neighbors("src", "TK", Direction::Out)
        .unwrap_or_default();
    assert_eq!(
        top1_restored,
        vec!["d_high"],
        "d_high must reclaim top-1 after ind restored; got {top1_restored:?}"
    );
}

// ---------------------------------------------------------------------------
// 6. Snapshot V4 round-trip.
// ---------------------------------------------------------------------------

#[test]
fn any_snapshot_v4_roundtrip() {
    let dir = tmp("snap");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.create_rule(RuleDef {
            name: "any_snap".into(),
            src_label: "N".into(),
            dst_label: "N".into(),
            predicate: Predicate::Any(vec![
                Predicate::FieldEqual {
                    field: "ind".into(),
                },
                Predicate::NumericWithin {
                    field: "year".into(),
                    tolerance: 3.0,
                },
            ]),
            edge_type: "SNAP".into(),
            weight_prop: Some("score".into()),
            max_edges: None,
            approximate: false,
        })
        .unwrap();

        db.insert_node("N", "a", vec![
            ("ind".into(), Value::Str("arch".into())),
            ("year".into(), Value::Int(2000)),
        ]).unwrap();
        db.insert_node("N", "b", vec![
            ("ind".into(), Value::Str("arch".into())),
            ("year".into(), Value::Int(2001)),
        ]).unwrap();

        // Take snapshot while derived edges (a→b, b→a) are live.
        db.snapshot().unwrap();

        // WAL-tail write after snapshot.
        db.insert_node("N", "c", vec![
            ("ind".into(), Value::Str("law".into())),
            ("year".into(), Value::Int(2002)),
        ]).unwrap();
    }

    // Reopen: snapshot + WAL tail must restore the rule and all derived edges.
    let db = GraphDb::open(&dir).unwrap();

    assert_eq!(db.rules().len(), 1, "rule must survive snapshot+WAL replay");
    assert_eq!(db.rules()[0].name, "any_snap");

    // a→b from snapshot (FieldEqual branch fires, both ind="arch").
    let a_out = db.neighbors("a", "SNAP", Direction::Out).unwrap_or_default();
    assert!(
        a_out.contains(&"b".to_string()),
        "a→b must survive snapshot round-trip; got {a_out:?}"
    );

    // a→c from WAL tail (NumericWithin branch fires, year diff=2 ≤ 3).
    assert!(
        a_out.contains(&"c".to_string()),
        "a→c must be derived after WAL replay; got {a_out:?}"
    );
}

// ---------------------------------------------------------------------------
// 7. Bincode backward-compat: old (pre-Any) records still decode.
// ---------------------------------------------------------------------------

#[test]
fn any_bincode_roundtrip_and_old_records_still_decode() {
    // Any must survive bincode encode→decode (the V4 snapshot path uses bincode).
    let rule = RuleDef {
        name: "bc".into(),
        src_label: "A".into(),
        dst_label: "B".into(),
        predicate: Predicate::Any(vec![
            Predicate::FieldEqual {
                field: "f".into(),
            },
            Predicate::Overlap {
                field: "tags".into(),
                min: 0.5,
            },
        ]),
        edge_type: "E".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
    };
    let bytes = bincode::serialize(&rule).unwrap();
    let decoded: RuleDef = bincode::deserialize(&bytes).unwrap();
    assert_eq!(rule, decoded, "Any RuleDef must round-trip via bincode");

    // Pre-Any records (old variants 0–6) must still decode correctly.
    let old = RuleDef {
        name: "r".into(),
        src_label: "A".into(),
        dst_label: "B".into(),
        predicate: Predicate::VectorSimilar {
            field: "emb".into(),
            min: 0.9,
        },
        edge_type: "E".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
    };
    let old_bytes = bincode::serialize(&old).unwrap();
    let old_decoded: RuleDef = bincode::deserialize(&old_bytes).unwrap();
    assert_eq!(old, old_decoded, "pre-Any VectorSimilar record must still decode");
}
