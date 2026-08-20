use core_api::{
    AutoFk, Direction, GraphDb, IngestOptions, Predicate, RuleDef, RuleStats, Stats, Value,
};
use core_storage::fs::Fs;
use sim_harness::{Oracle, SimFs, APPROX_RECALL_FLOOR_RECOVERY};
use std::collections::{BTreeMap, BTreeSet};

/// Original workload: 20 nodes + chain edges + one mid-workload snapshot.
fn workload<F: Fs>(db: &mut GraphDb<F>) -> core_api::Result<()> {
    for i in 0..20 {
        db.insert_node("N", &format!("n{i}"), vec![("i".into(), Value::Int(i))])?;
        if i > 0 {
            db.insert_edge("E", &format!("n{}", i - 1), &format!("n{i}"))?;
        }
        if i == 10 {
            db.snapshot()?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Workload with rules
// ---------------------------------------------------------------------------

/// Node keys used by `workload_with_rules`; constant for edge-set sweeps.
const WORKLOAD_KEYS: &[&str] = &[
    "n0", "n1", "n2", "n3", "n4", "n5", "n6", "n7", "n8", "n9", "n10", "n11", "n12", "n13", "x0",
    "y0", "y1", "y2", "y3", "y4", "g0", "g1", "g2", "g3", "g4", "v0", "v1", "v2", "va0", "va1",
    "va2", "va3", "va4", "va5", "va6", "va7",
];

// APPROX_RECALL_FLOOR_RECOVERY imported from sim_harness (canonical location: src/lib.rs).

/// Approximate cosine-similarity recall over the VA nodes for the vec_approx rule.
///
/// 8 VA nodes in 4 pairs spread across quadrants of the 2-D unit circle.
/// With n=8 and IVF_K_MIN=4, k=4 so n > k: IVF is genuinely active (not scan-all fallback).
/// k-means converges to one centroid per pair; P=1 probe finds the correct cluster.
/// Exact pairs (cos ≥ 0.9): va0↔va1, va2↔va3, va4↔va5, va6↔va7 (8 directed edges).
/// All cross-pair cosines are at most 0.2, ensuring no false positives.
fn approx_recall(db: &GraphDb<SimFs>) -> f64 {
    // Pair A (near [1,0]): va0↔va1, cos ≈ 0.98
    // Pair B (near [0,1]): va2↔va3, cos ≈ 0.98
    // Pair C (near [-1,0]): va4↔va5, cos ≈ 0.98
    // Pair D (near [0,-1]): va6↔va7, cos ≈ 0.98
    let va_vecs: &[(&str, [f64; 2])] = &[
        ("va0", [1.0, 0.0]),
        ("va1", [0.98_f64, 0.2_f64]),
        ("va2", [0.0, 1.0]),
        ("va3", [-0.2_f64, (1.0_f64 - 0.04_f64).sqrt()]),
        ("va4", [-1.0, 0.0]),
        ("va5", [-0.98_f64, 0.2_f64]),
        ("va6", [0.0, -1.0]),
        ("va7", [0.2_f64, -0.98_f64]),
    ];
    let min_sim = 0.9_f64;

    let mut exact_count = 0usize;
    let mut hit_count = 0usize;

    for (i, (sk, sv)) in va_vecs.iter().enumerate() {
        if !db.has_node(sk) {
            continue;
        }
        for (j, (dk, dv)) in va_vecs.iter().enumerate() {
            if i == j || !db.has_node(dk) {
                continue;
            }
            let dot = sv[0] * dv[0] + sv[1] * dv[1];
            if dot >= min_sim {
                exact_count += 1;
                let in_approx = db
                    .neighbors(sk, "VAPPROX", Direction::Out)
                    .unwrap_or_default()
                    .contains(&dk.to_string());
                if in_approx {
                    hit_count += 1;
                }
            }
        }
    }

    if exact_count == 0 {
        1.0
    } else {
        hit_count as f64 / exact_count as f64
    }
}

/// Slot count after a complete `workload_with_rules` (n0–n11 + batch n12/n13
/// + ingest x0 + 5Y + 5G + 3V + 8VA; n6 is tombstoned but still a slot).
const WORKLOAD_MAX_SLOTS: usize = 36;

const WORKLOAD_ETYPES: &[&str] = &[
    "E", "KM", "OV", "DUMMY", "ORG", "NW", "NZ", "GEO", "VEC", "VAPPROX",
];

const WORKLOAD_LABELS: &[&str] = &["L0", "L1", "L2", "Y", "G", "V", "VA"];

const WORKLOAD_FIELDS: &[&str] = &[
    "f", "tags", "year", "loc", "emb", "tmp", "org_id", "id", "i",
];

fn tags(items: &[&str]) -> Value {
    Value::List(items.iter().map(|s| Value::Str((*s).into())).collect())
}

fn loc(lat: f64, lon: f64) -> Value {
    Value::List(vec![Value::Float(lat), Value::Float(lon)])
}

fn emb(xs: &[f64]) -> Value {
    Value::List(xs.iter().copied().map(Value::Float).collect())
}

/// Deterministic workload with rules (no randomness, no wall-clock time):
///
///   * 6 L0 nodes (n0-n5): `f` field is a FK to the corresponding L1 node key;
///     `tags` field is a list of tokens.
///   * KM rule created before L1 nodes are inserted (backfill fires on L1 insert).
///   * 6 L1 nodes (n6-n11): `tags` chosen so that (n6,n7) and (n6,n8) exceed
///     Jaccard 0.34 but no other L1 pair does.
///   * OV rule created after L1 nodes.
///   * DUMMY rule created then deleted (exercises delete_rule WAL record + idempotent
///     replay path).
///   * snapshot() called while km and ov are both live.
///   * Three `set_prop` calls that retract and re-create KM edges (exercises the
///     prop-update rule-fire path post-snapshot).
///   * Plan 4 tail: delete_edge, remove_prop, a 3-op batch, ingest+auto-FK,
///     WAL-logged rebuild_rule, then delete_node of n6 (derived OV + KM).
fn workload_with_rules<F: Fs>(db: &mut GraphDb<F>) -> core_api::Result<()> {
    // --- 6 L0 nodes ---
    let l0_tags: &[&[&str]] = &[
        &["alpha", "beta"],
        &["beta", "gamma"],
        &["alpha", "delta"],
        &["epsilon"],
        &["alpha", "beta", "gamma"],
        &["zeta"],
    ];
    for (i, tag_set) in l0_tags.iter().enumerate() {
        db.insert_node(
            "L0",
            &format!("n{i}"),
            vec![
                ("f".into(), Value::Str(format!("n{}", i + 6))),
                ("tags".into(), tags(tag_set)),
            ],
        )?;
    }

    // KM rule: L0 → L1 via "f" field (src.f == dst.key).
    // Created before any L1 nodes exist; backfill fires when L1 nodes are inserted.
    db.create_rule(RuleDef {
        name: "km".into(),
        src_label: "L0".into(),
        dst_label: "L1".into(),
        predicate: Predicate::KeyMatch { field: "f".into() },
        edge_type: "KM".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
    })?;

    // --- 6 L1 nodes ---
    // Jaccard pairs at min=0.34:
    //   n6 ∩ n7 = {beta,gamma}, union={alpha,beta,gamma,delta}: J=0.50 ≥ 0.34 → OV
    //   n6 ∩ n8 = {alpha,beta}, union={alpha,beta,gamma}:       J≈0.67 ≥ 0.34 → OV
    //   n7 ∩ n8 = {beta},       union={alpha,beta,gamma,delta}: J=0.25 < 0.34 → no
    //   n9/n10/n11: all pairs below threshold
    let l1_tags: &[&[&str]] = &[
        &["alpha", "beta", "gamma"], // n6
        &["beta", "gamma", "delta"], // n7
        &["alpha", "beta"],          // n8
        &["xi", "psi"],              // n9
        &["psi", "omega"],           // n10  J(n9,n10)=1/3≈0.33 < 0.34
        &["mu"],                     // n11
    ];
    for (i, tag_set) in l1_tags.iter().enumerate() {
        db.insert_node(
            "L1",
            &format!("n{}", i + 6),
            vec![("tags".into(), tags(tag_set))],
        )?;
    }
    // KM backfill: n0→n6, n1→n7, n2→n8, n3→n9, n4→n10, n5→n11.

    // OV rule: L1 → L1 via "tags" overlap (Jaccard ≥ 0.34).
    // Backfill: n6→n7, n7→n6, n6→n8, n8→n6.
    db.create_rule(RuleDef {
        name: "ov".into(),
        src_label: "L1".into(),
        dst_label: "L1".into(),
        predicate: Predicate::Overlap {
            field: "tags".into(),
            min: 0.34,
        },
        edge_type: "OV".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
    })?;

    // DUMMY rule created and immediately deleted.
    db.create_rule(RuleDef {
        name: "dummy".into(),
        src_label: "L0".into(),
        dst_label: "L0".into(),
        predicate: Predicate::FieldEqual { field: "f".into() },
        edge_type: "DUMMY".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
    })?;
    db.delete_rule("dummy")?;

    // --- Plan 7 predicates (before snapshot so rule_defs persist) ---
    // Numeric: 10.0 / 11.9 same-or-adjacent bucket (tol 2); 12.0 adjacent;
    // y3=-0.0 and y4=+0.0 for the tol=0 signed-zero pair.
    db.insert_node("Y", "y0", vec![("year".into(), Value::Float(10.0))])?;
    db.insert_node("Y", "y1", vec![("year".into(), Value::Float(11.9))])?;
    db.insert_node("Y", "y2", vec![("year".into(), Value::Float(12.0))])?;
    db.insert_node("Y", "y3", vec![("year".into(), Value::Float(-0.0))])?;
    db.insert_node("Y", "y4", vec![("year".into(), Value::Float(0.0))])?;
    db.create_rule(RuleDef {
        name: "nw".into(),
        src_label: "Y".into(),
        dst_label: "Y".into(),
        predicate: Predicate::NumericWithin {
            field: "year".into(),
            tolerance: 2.0,
        },
        edge_type: "NW".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
    })?;
    db.create_rule(RuleDef {
        name: "nz".into(),
        src_label: "Y".into(),
        dst_label: "Y".into(),
        predicate: Predicate::NumericWithin {
            field: "year".into(),
            tolerance: 0.0,
        },
        edge_type: "NZ".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
    })?;

    // Geo: Paris/London cross-cell; ±180 at lat 70 (antimeridian wrap); NYC far.
    db.insert_node("G", "g0", vec![("loc".into(), loc(48.8566, 2.3522))])?;
    db.insert_node("G", "g1", vec![("loc".into(), loc(51.5074, -0.1278))])?;
    db.insert_node("G", "g2", vec![("loc".into(), loc(70.0, 179.9))])?;
    db.insert_node("G", "g3", vec![("loc".into(), loc(70.0, -179.9))])?;
    db.insert_node("G", "g4", vec![("loc".into(), loc(40.7128, -74.0060))])?;
    db.create_rule(RuleDef {
        name: "geo".into(),
        src_label: "G".into(),
        dst_label: "G".into(),
        predicate: Predicate::GeoRadius {
            field: "loc".into(),
            km: 400.0,
        },
        edge_type: "GEO".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
    })?;

    // Vector: [1,0] vs near-threshold 0.95; orthogonal [0,1] does not match.
    db.insert_node("V", "v0", vec![("emb".into(), emb(&[1.0, 0.0]))])?;
    db.insert_node(
        "V",
        "v1",
        vec![("emb".into(), emb(&[0.95, (1.0_f64 - 0.95 * 0.95).sqrt()]))],
    )?;
    db.insert_node("V", "v2", vec![("emb".into(), emb(&[0.0, 1.0]))])?;
    db.create_rule(RuleDef {
        name: "vec".into(),
        src_label: "V".into(),
        dst_label: "V".into(),
        predicate: Predicate::VectorSimilar {
            field: "emb".into(),
            min: 0.9,
        },
        edge_type: "VEC".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
    })?;

    // Approximate vector rule (vec_approx): 8 VA nodes in 4 pairs across quadrants.
    // n=8 > IVF_K_MIN=4, so k=4 and IVF is genuinely active (no scan-all fallback).
    // Pair A (≈[1,0]): va0↔va1 cos≈0.98. Pair B (≈[0,1]): va2↔va3 cos≈0.98.
    // Pair C (≈[-1,0]): va4↔va5 cos≈0.98. Pair D (≈[0,-1]): va6↔va7 cos≈0.98.
    // Cross-pair cosines are ≤0.2 — no cross-pair edges.
    db.insert_node("VA", "va0", vec![("emb".into(), emb(&[1.0, 0.0]))])?;
    db.insert_node("VA", "va1", vec![("emb".into(), emb(&[0.98, 0.2]))])?;
    db.insert_node("VA", "va2", vec![("emb".into(), emb(&[0.0, 1.0]))])?;
    db.insert_node(
        "VA",
        "va3",
        vec![("emb".into(), emb(&[-0.2, (1.0_f64 - 0.04_f64).sqrt()]))],
    )?;
    db.insert_node("VA", "va4", vec![("emb".into(), emb(&[-1.0, 0.0]))])?;
    db.insert_node("VA", "va5", vec![("emb".into(), emb(&[-0.98, 0.2]))])?;
    db.insert_node("VA", "va6", vec![("emb".into(), emb(&[0.0, -1.0]))])?;
    db.insert_node("VA", "va7", vec![("emb".into(), emb(&[0.2, -0.98]))])?;
    db.create_rule(RuleDef {
        name: "vec_approx".into(),
        src_label: "VA".into(),
        dst_label: "VA".into(),
        predicate: Predicate::VectorSimilar {
            field: "emb".into(),
            min: 0.9,
        },
        edge_type: "VAPPROX".into(),
        weight_prop: None,
        max_edges: None,
        approximate: true,
    })?;

    // Snapshot while km, ov, and Plan-7 rules are live (no dummy rule).
    db.snapshot()?;

    // set_prop calls post-snapshot: retract + re-create KM edges.
    // n0.f: "n6" → "n7" (retract n0→n6, create n0→n7)
    db.set_prop("n0", "f", Value::Str("n7".into()))?;
    // n0.f: "n7" → "n6" (retract n0→n7, restore n0→n6)
    db.set_prop("n0", "f", Value::Str("n6".into()))?;
    // n2.f: "n8" → "n10" (retract n2→n8, create n2→n10; n4→n10 still exists)
    db.set_prop("n2", "f", Value::Str("n10".into()))?;

    // --- Plan 4 mutations (post-snapshot WAL tail) ---
    // User edge + delete_edge (not rule-owned).
    db.insert_edge("E", "n0", "n1")?;
    db.delete_edge("E", "n0", "n1")?;

    // remove_prop of a dedicated field (does not disturb L0.f / L1.tags checks).
    db.set_prop("n4", "tmp", Value::Int(1))?;
    db.remove_prop("n4", "tmp")?;

    // Small atomic batch: two nodes + a user edge.
    db.batch()
        .insert_node(
            "L0",
            "n12",
            vec![
                ("f".into(), Value::Str("n13".into())),
                ("tags".into(), tags(&["batch"])),
            ],
        )
        .insert_node("L1", "n13", vec![("tags".into(), tags(&["batch"]))])
        .insert_edge("E", "n12", "n13")
        .commit()?;

    // Ingest as one Batch (auto-FK KeyMatch → n11 under L1).
    let mut row = BTreeMap::new();
    row.insert("id".into(), Value::Str("x0".into()));
    row.insert("org_id".into(), Value::Str("n11".into()));
    db.ingest(
        "L2",
        vec![row],
        &IngestOptions {
            key_field: "id".into(),
            auto_fk: AutoFk::Auto {
                suffix: "_id".into(),
            },
        },
    )?;

    // WAL-logged rebuild mid-stream (replay-consistent after T6).
    db.rebuild_rule("km")?;

    // Plan 7 incremental: y2 crosses two numeric buckets (12.0 → 16.1);
    // v1 moves below the cosine min (no longer matches v0, still misses v2).
    db.set_prop("y2", "year", Value::Float(16.1))?;
    db.set_prop("v1", "emb", emb(&[0.5, 0.5]))?;
    db.rebuild_rule("nw")?;

    // delete_node of a node that currently owns derived OV edges (n6 ↔ n7, n6 ↔ n8)
    // plus the KM inbound from n0 (n0.f is back on "n6").
    db.delete_node("n6")?;

    Ok(())
}

/// Collect all Out-direction edges for `rule`'s edge type across workload nodes
/// that currently exist in `db`.
fn collect_rule_edges(db: &GraphDb<SimFs>, rule: &RuleDef) -> BTreeSet<(String, String)> {
    let mut set = BTreeSet::new();
    for key in WORKLOAD_KEYS {
        if !db.has_node(key) {
            continue;
        }
        if let Ok(ns) = db.neighbors(key, &rule.edge_type, Direction::Out) {
            for n in ns {
                set.insert((key.to_string(), n));
            }
        }
    }
    set
}

fn collect_all_edges(db: &GraphDb<SimFs>) -> BTreeSet<(String, String, String)> {
    let mut set = BTreeSet::new();
    for key in WORKLOAD_KEYS {
        if !db.has_node(key) {
            continue;
        }
        for etype in WORKLOAD_ETYPES {
            if let Ok(ns) = db.neighbors(key, etype, Direction::Out) {
                for n in ns {
                    set.insert((etype.to_string(), key.to_string(), n));
                }
            }
        }
    }
    set
}

/// Stats equality minus `fires`.
///
/// Parked T6 ruling: a crash between snapshot-write and WAL-truncation with
/// leftover WAL tail can double-bump fires on replay of BOTH `RebuildRule`
/// AND `DeleteRule` records (`DeleteRule` internally rebuilds same-etype
/// survivors, bumping their fires) over a snapshot that already captured the
/// increment. Edges / tripped / provenance-derived state are unaffected. DST
/// zeroes fires for all cross-window comparisons — do not assert fires
/// equality across that window.
fn stats_minus_fires(stats: Stats) -> Stats {
    Stats {
        rules: stats
            .rules
            .into_iter()
            .map(|r| RuleStats { fires: 0, ..r })
            .collect(),
        ..stats
    }
}

/// Rebuild an independent oracle from recovered live state and compare
/// brute-force `evaluate` edges to the engine. User edges of type `E` are
/// copied in; every other etype is derived solely via `Oracle::all_edges`.
fn oracle_from_db(db: &GraphDb<SimFs>) -> Oracle {
    let mut o = Oracle::new();
    for label in WORKLOAD_LABELS {
        for n in db.nodes_with_label(label) {
            let key = n.key().to_string();
            let mut props = Vec::new();
            for field in WORKLOAD_FIELDS {
                if let Some(v) = n.prop(field) {
                    props.push(((*field).to_string(), v.clone()));
                }
            }
            assert!(
                o.insert_node(label, &key, &props),
                "duplicate key {key} while rebuilding oracle"
            );
        }
    }
    for rule in db.rules() {
        // Approximate rules use IVF candidates, not brute-force exact evaluation.
        // They are excluded from oracle-equivalence checks; recall is verified
        // separately via `approx_recall()` in `assert_recovered_invariants`.
        if rule.approximate {
            continue;
        }
        assert!(o.create_rule(rule), "oracle rejected a recovered live rule");
    }
    for key in WORKLOAD_KEYS {
        if !db.has_node(key) {
            continue;
        }
        if let Ok(ns) = db.neighbors(key, "E", Direction::Out) {
            for n in ns {
                let _ = o.insert_edge("E", key, &n);
            }
        }
    }
    o
}

fn assert_oracle_equiv(db: &GraphDb<SimFs>, label: &str) {
    let oracle = oracle_from_db(db);
    // Approximate-rule edges are excluded from the oracle (IVF recall is checked
    // separately via approx_recall).  Filter the engine side to match.
    let approx_etypes: BTreeSet<String> = db
        .rules()
        .iter()
        .filter(|r| r.approximate)
        .map(|r| r.edge_type.clone())
        .collect();
    let engine: BTreeSet<(String, String, String)> = collect_all_edges(db)
        .into_iter()
        .filter(|(et, _, _)| !approx_etypes.contains(et))
        .collect();
    let expected = oracle.all_edges();
    assert_eq!(
        engine, expected,
        "{label}: recovered edges != brute-force evaluate oracle"
    );
}

fn assert_recovered_invariants(recovered: &mut GraphDb<SimFs>, label: &str) {
    let n = recovered.node_count();
    assert!(
        n <= WORKLOAD_MAX_SLOTS,
        "{label}: impossible node-slot count {n}"
    );
    for i in 0..6usize {
        if recovered.has_node(&format!("n{i}")) {
            assert!(
                recovered.get_prop(&format!("n{i}"), "f").is_some(),
                "{label}: L0 node n{i} exists but f prop is missing"
            );
        }
    }
    for i in 6..12usize {
        if recovered.has_node(&format!("n{i}")) {
            assert!(
                recovered.get_prop(&format!("n{i}"), "tags").is_some(),
                "{label}: L1 node n{i} exists but tags prop is missing"
            );
        }
    }
    for key in ["y0", "y1", "y2", "y3", "y4"] {
        if recovered.has_node(key) {
            assert!(
                recovered.get_prop(key, "year").is_some(),
                "{label}: {key} exists but year prop is missing"
            );
        }
    }
    for key in ["g0", "g1", "g2", "g3", "g4"] {
        if recovered.has_node(key) {
            assert!(
                recovered.get_prop(key, "loc").is_some(),
                "{label}: {key} exists but loc prop is missing"
            );
        }
    }
    for key in ["v0", "v1", "v2"] {
        if recovered.has_node(key) {
            assert!(
                recovered.get_prop(key, "emb").is_some(),
                "{label}: {key} exists but emb prop is missing"
            );
        }
    }
    // n6 is the planned delete_node target: if the delete landed, the key is gone.
    // If it has not landed, n6 is still a live L1 with tags (checked above).
    for key in ["va0", "va1", "va2", "va3", "va4", "va5", "va6", "va7"] {
        if recovered.has_node(key) {
            assert!(
                recovered.get_prop(key, "emb").is_some(),
                "{label}: {key} exists but emb prop is missing"
            );
        }
    }

    assert_oracle_equiv(recovered, label);

    let edges_before = collect_all_edges(recovered);
    let stats_before = stats_minus_fires(recovered.stats());
    let rules = recovered.rules();
    for rule in &rules {
        let before = collect_rule_edges(recovered, rule);
        recovered.rebuild_rule(&rule.name).unwrap();
        let after = collect_rule_edges(recovered, rule);
        assert_eq!(
            before, after,
            "{label}: rebuild_rule changed edges for rule {:?}",
            rule.name
        );
    }
    let edges_after = collect_all_edges(recovered);
    assert_eq!(
        edges_before, edges_after,
        "{label}: rebuild_rule changed the full edge set"
    );
    assert_eq!(
        stats_before,
        stats_minus_fires(recovered.stats()),
        "{label}: rebuild_rule changed stats (fires zeroed; parked T6 fires-skew)"
    );

    // Approximate-rule recall assertion: if vec_approx is live and VA nodes exist,
    // recall must be ≥ APPROX_RECALL_FLOOR_RECOVERY at every crash-recovery state.
    let approx_rule_live = recovered
        .rules()
        .iter()
        .any(|r| r.name == "vec_approx" && r.approximate);
    let va_nodes_exist = ["va0", "va1", "va2", "va3", "va4", "va5", "va6", "va7"]
        .iter()
        .any(|k| recovered.has_node(k));
    if approx_rule_live && va_nodes_exist {
        let r = approx_recall(recovered);
        assert!(
            r >= APPROX_RECALL_FLOOR_RECOVERY,
            "{label}: approximate vec_approx recall {:.3} < floor {:.3}",
            r,
            APPROX_RECALL_FLOOR_RECOVERY
        );
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn recovery_is_consistent_at_every_crash_offset() {
    // Run to completion to measure total bytes appended.
    let total = {
        let mut db = GraphDb::open_with(SimFs::new()).unwrap();
        workload(&mut db).unwrap();
        db.fs_total_appended()
    };
    assert!(total > 0);

    for crash_at in 0..=total {
        let mut db = GraphDb::open_with(SimFs::with_crash_after(crash_at)).unwrap();
        let _ = workload(&mut db); // errors expected once the crash fires
        let survivor = db.into_fs().surviving_state();

        // Invariant 1: recovery never panics or reports corruption.
        let recovered = GraphDb::open_with(survivor).unwrap();

        // Invariant 2: recovered state is internally consistent.
        let n = recovered.node_count() as i64;
        for i in 0..n {
            assert!(
                recovered.has_node(&format!("n{i}")),
                "crash_at={crash_at}: missing n{i}"
            );
            assert_eq!(
                recovered.get_prop(&format!("n{i}"), "i"),
                Some(&Value::Int(i)),
                "crash_at={crash_at}: node exists but its logged props are missing"
            );
        }
        // Edges only ever connect existing, consecutive nodes.
        assert!(
            recovered.edge_count() <= (n.max(1) - 1) as u64,
            "crash_at={crash_at}"
        );
    }
}

/// Byte-offset sweep over `workload_with_rules`: existing-style sweep with the
/// new rule-aware workload and the additional rebuild_rule no-op invariant (c).
#[test]
fn recovery_byte_sweep_rules() {
    let total_bytes = {
        let mut db = GraphDb::open_with(SimFs::new()).unwrap();
        workload_with_rules(&mut db).unwrap();
        db.into_fs().total_appended()
    };
    assert!(total_bytes > 0, "workload must append at least one byte");
    eprintln!(
        "DST byte-offset sweep: 0..={total_bytes} ({} crash points)",
        total_bytes + 1
    );

    for crash_at in 0..=total_bytes {
        let mut db = GraphDb::open_with(SimFs::with_crash_after(crash_at)).unwrap();
        let _ = workload_with_rules(&mut db);
        let survivor = db.into_fs().surviving_state();

        // Invariant (a): open_with never panics or errors.
        let mut recovered = GraphDb::open_with(survivor).unwrap();

        // Invariants (b)+(c): internally consistent + rebuild-is-noop.
        // Stats compared with fires zeroed (parked T6 snapshot+RebuildRule skew).
        assert_recovered_invariants(&mut recovered, &format!("crash_at={crash_at}"));
    }
}

/// Op-count sweep over `workload_with_rules`: injects crashes at every Fs call
/// boundary (append/sync/read/write_atomic).  This covers crashes *at* the
/// snapshot `write_atomic` and the WAL-truncation `write_atomic` — closing the
/// Plan 1 DST carryover gap.
///
/// EXPECTATION: every crash point recovers correctly.  If any op index causes an
/// invariant failure, that is a deterministic engine-bug repro — report and stop,
/// do not weaken the invariant.
#[test]
fn recovery_op_sweep_rules() {
    let total_ops = {
        let mut db = GraphDb::open_with(SimFs::new()).unwrap();
        workload_with_rules(&mut db).unwrap();
        db.into_fs().total_ops()
    };
    assert!(total_ops > 0, "workload must make at least one Fs call");
    eprintln!(
        "DST op-count sweep: 0..={total_ops} ({} crash points)",
        total_ops + 1
    );

    for crash_op in 0..=total_ops {
        // open_with may itself fail when crash_op fires on an early read call.
        // In that case no writes occurred, so the surviving state is empty.
        let survivor = match GraphDb::open_with(SimFs::with_crash_after_ops(crash_op)) {
            Ok(mut db) => {
                let _ = workload_with_rules(&mut db);
                db.into_fs().surviving_state()
            }
            Err(_) => {
                // Crashed during open_with reads before any write; survivor is empty.
                SimFs::new()
            }
        };

        // Invariant (a): recovery never panics or errors.
        let mut recovered = GraphDb::open_with(survivor).unwrap();

        // Invariants (b)+(c): internally consistent + rebuild-is-noop.
        // Stats compared with fires zeroed (parked T6 snapshot+RebuildRule skew).
        assert_recovered_invariants(&mut recovered, &format!("crash_op={crash_op}"));
    }
}
