/// DST (Derived-Set Test) oracle sweep for top-k per-source rule semantics.
///
/// Invariant (per brief): at every quiescent point, the engine's derived set
/// for each source node under a top-k rule must equal a scratch recompute:
///
///   engine out-neighbors(src, etype) == oracle.top_k_dsts_for_src(rule, k, src)
///
/// Operations: random insert-node / set-prop mutations on two top-k rules
/// (k=1, k=3) spanning FieldEqual (unscored, key-ASC tiebreak) and
/// NumericWithin (scored, float tiebreak).  Checked after every op.
use core_api::{Direction, GraphDb, Predicate, RuleDef, Value};
use proptest::prelude::*;
use sim_harness::{Oracle, SimFs, APPROX_RECALL_FLOOR_QUIESCED};
use std::collections::BTreeSet;

// ---------------------------------------------------------------------------
// Rule templates (unique edge types → no etype collision in DST check)
// ---------------------------------------------------------------------------

fn rule_fe_k1() -> RuleDef {
    RuleDef {
        name: "topk1_fe".into(),
        src_label: "P".into(),
        dst_label: "P".into(),
        predicate: Predicate::FieldEqual { field: "f".into() },
        edge_type: "FE_K1".into(),
        weight_prop: None,
        max_edges: Some(1),
        approximate: false,
    }
}

fn rule_nw_k3() -> RuleDef {
    RuleDef {
        name: "topk3_nw".into(),
        src_label: "P".into(),
        dst_label: "P".into(),
        predicate: Predicate::NumericWithin {
            field: "year".into(),
            tolerance: 10.0,
        },
        edge_type: "NW_K3".into(),
        weight_prop: Some("score".into()),
        max_edges: Some(3),
        approximate: false,
    }
}

// ---------------------------------------------------------------------------
// Op enum: deliberately small so most random ops do something
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Op {
    InsertNode(u8),   // key = "k{n}", label always "P"
    SetF(u8, u8),     // key k{n}, write f = "k{m}" (FE tiebreak material)
    SetYear(u8, u8),  // key k{n}, write year = (m % 20) as f64
    DeleteNode(u8),   // key = "k{n}"
}

fn year_of(m: u8) -> Value {
    Value::Float((m % 20) as f64)
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        5 => any::<u8>().prop_map(Op::InsertNode),
        3 => (any::<u8>(), any::<u8>()).prop_map(|(k, m)| Op::SetF(k, m)),
        3 => (any::<u8>(), any::<u8>()).prop_map(|(k, m)| Op::SetYear(k, m)),
        1 => any::<u8>().prop_map(Op::DeleteNode),
    ]
}

// ---------------------------------------------------------------------------
// DST check
// ---------------------------------------------------------------------------

/// Collect the engine's out-neighbors for (src, etype) as a BTreeSet of keys.
fn engine_out(db: &GraphDb<SimFs>, src: &str, etype: &str) -> BTreeSet<String> {
    db.neighbors(src, etype, Direction::Out)
        .unwrap_or_default()
        .into_iter()
        .collect()
}

/// Assert that for every live node the engine's per-source derived set
/// matches the oracle's brute-force top-k, for all registered top-k rules.
fn assert_dst_invariant(
    db: &GraphDb<SimFs>,
    oracle: &Oracle,
    rules: &[RuleDef],
    n_max: u8,
    hint: &str,
) -> Result<(), TestCaseError> {
    for rule in rules {
        let k = rule.max_edges.expect("DST only for top-k rules");
        for n in 0..=n_max {
            let src = format!("k{n}");
            if !oracle.has_node(&src) {
                // Engine should have no derived edges for this key as src.
                let eng = engine_out(db, &src, &rule.edge_type);
                prop_assert!(
                    eng.is_empty(),
                    "rule={} src={src} k={k}: engine has derived edges for non-existent node; \
                     hint={hint}; got {eng:?}",
                    rule.name
                );
                continue;
            }
            let oracle_set = oracle.top_k_dsts_for_src(rule, k, &src);
            let engine_set = engine_out(db, &src, &rule.edge_type);
            prop_assert_eq!(
                &oracle_set,
                &engine_set,
                "rule={} src={} k={} hint={}: per-source derived set mismatch",
                rule.name,
                src,
                k,
                hint
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Proptest DST sweep
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Random-op DST sweep: k ∈ {1, 3}, unscored and scored predicates.
    /// After every op the engine's per-source derived set is compared to
    /// the oracle's scratch recompute at that quiescent point.
    #[test]
    fn topk_dst_sweep(ops in proptest::collection::vec(op_strategy(), 1..60)) {
        let mut db = GraphDb::open_with(SimFs::new()).unwrap();
        let mut oracle = Oracle::new();

        // Register both top-k rules upfront (create on empty graph — no backfill
        // needed; incremental fires carry everything).
        let rules = vec![rule_fe_k1(), rule_nw_k3()];
        for rule in &rules {
            db.create_rule(rule.clone()).unwrap();
            oracle.create_rule(rule.clone());
        }

        // Highest node index seen (used in DST check to bound the sweep).
        let mut n_max: u8 = 0;

        for op in &ops {
            match op {
                Op::InsertNode(n) => {
                    let key = format!("k{n}");
                    let props = vec![
                        ("f".to_string(), Value::Str(format!("k{n}"))),
                        ("year".to_string(), year_of(*n)),
                    ];
                    // Both may return false (duplicate) — that's fine.
                    let _ = db.insert_node("P", &key, props.clone());
                    let _ = oracle.insert_node("P", &key, &props);
                    n_max = n_max.max(*n);
                }
                Op::SetF(k, m) => {
                    let key = format!("k{k}");
                    let val = Value::Str(format!("k{m}"));
                    let _ = db.set_prop(&key, "f", val.clone());
                    oracle.set_prop(&key, "f", val);
                }
                Op::SetYear(k, m) => {
                    let key = format!("k{k}");
                    let val = year_of(*m);
                    let _ = db.set_prop(&key, "year", val.clone());
                    oracle.set_prop(&key, "year", val);
                }
                Op::DeleteNode(n) => {
                    let key = format!("k{n}");
                    let _ = db.delete_node(&key);
                    oracle.delete_node(&key);
                }
            }

            // DST check after every op (engine is always quiescent — no async).
            assert_dst_invariant(&db, &oracle, &rules, n_max, &format!("after op {op:?}"))?;
        }
    }
}

// ---------------------------------------------------------------------------
// Deterministic regression: insert-evict + retract-backfill in sequence
// ---------------------------------------------------------------------------

/// k=1 FieldEqual: insert 3 nodes with the same "f" field, verify each src
/// keeps only the smallest-key dst.  Then change a dst's field breaking the
/// match, verify backfill happens.
#[test]
fn topk_dst_insert_evict_and_backfill_sequence() {
    let mut db = GraphDb::open_with(SimFs::new()).unwrap();
    let mut oracle = Oracle::new();
    let rule = rule_fe_k1();
    db.create_rule(rule.clone()).unwrap();
    oracle.create_rule(rule.clone());

    // Insert 3 nodes all with f="same".
    for key in ["a", "b", "c"] {
        let props = vec![("f".to_string(), Value::Str("same".into()))];
        db.insert_node("P", key, props.clone()).unwrap();
        oracle.insert_node("P", key, &props);
    }

    // At quiescent: each src's k=1 dst should be the smallest key that isn't self.
    // a→b (b < c), b→a (a < c), c→a (a < b).
    let et = "FE_K1";
    fn out(db: &GraphDb<SimFs>, key: &str, et: &str) -> Vec<String> {
        db.neighbors(key, et, Direction::Out).unwrap_or_default()
    }
    assert_eq!(out(&db, "a", et), vec!["b"], "a→b");
    assert_eq!(out(&db, "b", et), vec!["a"], "b→a");
    assert_eq!(out(&db, "c", et), vec!["a"], "c→a");

    for key in ["a", "b", "c"] {
        let oracle_top = oracle.top_k_dsts_for_src(&rule, 1, key);
        let engine_top: BTreeSet<String> = out(&db, key, et).into_iter().collect();
        assert_eq!(oracle_top, engine_top, "mismatch after inserts, key={key}");
    }

    // Change "b" so it no longer matches "same" → b retracted from everyone else's
    // top-k; backfill brings in "c" where relevant.
    db.set_prop("b", "f", Value::Str("other".into())).unwrap();
    oracle.set_prop("b", "f", Value::Str("other".into()));

    // a→c (b no longer matches; c is next), b→∅ (no match), c→a (still matches)
    assert_eq!(out(&db, "a", et), vec!["c"], "a→c after b retracted");
    assert!(out(&db, "b", et).is_empty(), "b→∅ (different field)");
    assert_eq!(out(&db, "c", et), vec!["a"], "c→a after b retracted");

    for key in ["a", "b", "c"] {
        let oracle_top = oracle.top_k_dsts_for_src(&rule, 1, key);
        let engine_top: BTreeSet<String> = out(&db, key, et).into_iter().collect();
        assert_eq!(oracle_top, engine_top, "mismatch after retract, key={key}");
    }
}

/// k=3 NumericWithin: insert nodes at different years, verify top-3 ordered
/// by score (1 - |Δ|/10) and key tiebreak.
#[test]
fn topk_dst_numeric_k3_score_order() {
    let mut db = GraphDb::open_with(SimFs::new()).unwrap();
    let mut oracle = Oracle::new();
    let rule = rule_nw_k3();
    db.create_rule(rule.clone()).unwrap();
    oracle.create_rule(rule.clone());

    // src s0 at year=0.0; dsts at years 1, 2, 3, 9 (all within tolerance 10).
    // Scores: d1→0.9, d2→0.8, d3→0.7, d9→0.1.
    // Top-3 for s0: d1, d2, d3 (highest scores).
    let nodes = [
        ("s0", 0.0f64),
        ("d1", 1.0),
        ("d2", 2.0),
        ("d3", 3.0),
        ("d9", 9.0),
    ];
    for (key, year) in nodes {
        let props = vec![("year".to_string(), Value::Float(year))];
        db.insert_node("P", key, props.clone()).unwrap();
        oracle.insert_node("P", key, &props);
    }

    let engine_top: BTreeSet<String> = db
        .neighbors("s0", "NW_K3", Direction::Out)
        .unwrap_or_default()
        .into_iter()
        .collect();
    let oracle_top = oracle.top_k_dsts_for_src(&rule, 3, "s0");

    assert_eq!(oracle_top, engine_top);
    assert!(engine_top.contains("d1"), "d1 should be in top-3");
    assert!(engine_top.contains("d2"), "d2 should be in top-3");
    assert!(engine_top.contains("d3"), "d3 should be in top-3");
    assert!(!engine_top.contains("d9"), "d9 should be evicted (4th best)");
}

// ---------------------------------------------------------------------------
// I1: explain() on a predicate-matching but top-k-evicted pair
// ---------------------------------------------------------------------------

/// A pair that satisfies the predicate but is outside the top-k window must:
/// - not appear as an out-neighbor,
/// - return an empty `explain()` result (no derived edge, not hidden),
/// - have no provenance entry.
#[test]
fn topk_evicted_pair_has_no_explain_entry() {
    let mut db = GraphDb::open_with(SimFs::new()).unwrap();

    // k=1 NumericWithin tolerance=10; year=0 src; 3 candidate dsts at years 1, 2, 9.
    // Top-1: d1 (score=0.9, highest). d2 and d9 satisfy predicate but are evicted.
    let rule = RuleDef {
        name: "nw1".into(),
        src_label: "P".into(),
        dst_label: "P".into(),
        predicate: Predicate::NumericWithin {
            field: "year".into(),
            tolerance: 10.0,
        },
        edge_type: "NW1".into(),
        weight_prop: Some("score".into()),
        max_edges: Some(1),
        approximate: false,
    };
    db.create_rule(rule).unwrap();

    let nodes = [("s0", 0.0f64), ("d1", 1.0), ("d2", 2.0), ("d9", 9.0)];
    for (key, year) in nodes {
        db.insert_node("P", key, vec![("year".into(), Value::Float(year))])
            .unwrap();
    }

    // d1 is in top-1 for s0.
    let top1: Vec<String> = db.neighbors("s0", "NW1", Direction::Out).unwrap_or_default();
    assert_eq!(top1, vec!["d1"], "s0's top-1 must be d1 (score=0.9)");

    // d2 satisfies the predicate (|0-2|=2 < 10) but is evicted.
    let s0_d2_edges = db.explain("s0", "d2").expect("explain must not error");
    assert!(
        s0_d2_edges.is_empty(),
        "explain(s0, d2): predicate-matching but evicted pair must have no derived edge; \
         got {s0_d2_edges:?}"
    );

    // d9 satisfies the predicate (|0-9|=9 < 10) but is evicted.
    let s0_d9_edges = db.explain("s0", "d9").expect("explain must not error");
    assert!(
        s0_d9_edges.is_empty(),
        "explain(s0, d9): predicate-matching but evicted pair must have no derived edge; \
         got {s0_d9_edges:?}"
    );
}

// ---------------------------------------------------------------------------
// C1: IVF / VectorSimilar approximate top-k recall floor
// ---------------------------------------------------------------------------

/// Approximate (IVF) VectorSimilar rule with max_edges=Some(k):
/// for each source node, the engine's top-k out-neighbors must overlap the
/// exact-scan top-k by at least APPROX_RECALL_FLOOR_QUIESCED (0.90).
///
/// Recall is computed per source (|engine_topk ∩ exact_topk| / |exact_topk|)
/// and the minimum across all sources must meet the floor.
#[test]
fn topk_approx_recall_floor() {
    let mut db = GraphDb::open_with(SimFs::new()).unwrap();

    // 20 2-D unit vectors in 4 clusters (5 per cluster).
    // Within each cluster, cosine similarity ≥ 0.98 (very tight cluster).
    // Across clusters, similarity < 0.5.  min_sim=0.9 → intra-cluster only.
    let vecs: &[(&str, f64, f64)] = &[
        // cluster A: near [1, 0]
        ("a0", 1.0, 0.0),
        ("a1", 0.999, 0.045),
        ("a2", 0.998, 0.063),
        ("a3", 0.997, 0.077),
        ("a4", 0.995, 0.100),
        // cluster B: near [0, 1]
        ("b0", 0.0, 1.0),
        ("b1", 0.045, 0.999),
        ("b2", 0.063, 0.998),
        ("b3", 0.077, 0.997),
        ("b4", 0.100, 0.995),
        // cluster C: near [-1, 0]
        ("c0", -1.0, 0.0),
        ("c1", -0.999, 0.045),
        ("c2", -0.998, 0.063),
        ("c3", -0.997, 0.077),
        ("c4", -0.995, 0.100),
        // cluster D: near [0, -1]
        ("d0", 0.0, -1.0),
        ("d1", 0.045, -0.999),
        ("d2", 0.063, -0.998),
        ("d3", 0.077, -0.997),
        ("d4", 0.100, -0.995),
    ];
    let min_sim = 0.9_f64;
    let k: u64 = 3; // each cluster has 5 nodes → 4 candidates per src → top-3 is non-trivial

    db.create_rule(RuleDef {
        name: "approx_topk".into(),
        src_label: "V".into(),
        dst_label: "V".into(),
        predicate: Predicate::VectorSimilar {
            field: "emb".into(),
            min: min_sim,
        },
        edge_type: "ATOPK".into(),
        weight_prop: None,
        max_edges: Some(k),
        approximate: true,
    })
    .unwrap();

    // Insert all nodes (normalized).
    let mut normalized: Vec<(&str, f64, f64)> = Vec::new();
    for &(key, x, y) in vecs {
        let norm = (x * x + y * y).sqrt();
        let (nx, ny) = (x / norm, y / norm);
        let val = Value::List(vec![Value::Float(nx), Value::Float(ny)]);
        db.insert_node("V", key, vec![("emb".into(), val)]).unwrap();
        normalized.push((key, nx, ny));
    }

    // Compute exact top-k for each source: all qualifying dsts sorted by sim DESC, key ASC.
    let mut min_recall = 1.0_f64;
    let mut any_src_with_exact = false;

    for &(src_key, sx, sy) in &normalized {
        // Exact candidates: all dsts where cosine_sim ≥ min_sim and key ≠ src.
        let mut exact_candidates: Vec<(String, f64)> = normalized
            .iter()
            .filter(|&&(dkey, _, _)| dkey != src_key)
            .filter_map(|&(dkey, dx, dy)| {
                let sim = sx * dx + sy * dy; // already normalized
                if sim >= min_sim { Some((dkey.to_string(), sim)) } else { None }
            })
            .collect();

        if exact_candidates.is_empty() {
            continue;
        }
        any_src_with_exact = true;

        // Sort by sim DESC, key ASC, take k.
        exact_candidates.sort_by(|(ka, sa), (kb, sb)| {
            sb.total_cmp(sa).then_with(|| ka.cmp(kb))
        });
        exact_candidates.truncate(k as usize);
        let exact_topk: BTreeSet<String> =
            exact_candidates.into_iter().map(|(k, _)| k).collect();

        let engine_topk: BTreeSet<String> = db
            .neighbors(src_key, "ATOPK", Direction::Out)
            .unwrap_or_default()
            .into_iter()
            .collect();

        let intersection = engine_topk.intersection(&exact_topk).count();
        let recall = intersection as f64 / exact_topk.len() as f64;
        if recall < min_recall {
            min_recall = recall;
        }
    }

    assert!(any_src_with_exact, "test setup error: no source had exact candidates");
    assert!(
        min_recall >= APPROX_RECALL_FLOOR_QUIESCED,
        "IVF top-k recall {:.3} < floor {:.3} (k={k}, min_sim={min_sim})",
        min_recall,
        APPROX_RECALL_FLOOR_QUIESCED,
    );
}
