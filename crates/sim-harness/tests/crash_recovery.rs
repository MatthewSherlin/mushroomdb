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

/// Byte-offset crash sweep for Cypher write statements.
///
/// Workload: CREATE two nodes via `query_write`, then SET a property via
/// `query_write`.  At every crash point we verify the recovered state is
/// internally consistent: if a node exists its `id` prop must be present and
/// any SET that landed must be durable.  Confirms that Cypher writes flow
/// through the WAL with the same durability guarantee as direct API mutations.
#[test]
fn cypher_write_dst_byte_sweep() {
    fn no_params() -> BTreeMap<String, Value> {
        BTreeMap::new()
    }

    fn cypher_workload<F: core_storage::fs::Fs>(
        db: &mut GraphDb<F>,
    ) -> core_api::Result<()> {
        // Three single-op Batch frames:
        db.query_write("CREATE (a:Person {id: 'dst_alice'})", &no_params())?;
        db.query_write("CREATE (b:Person {id: 'dst_bob'})", &no_params())?;
        db.query_write(
            "MATCH (p:Person {id: 'dst_alice'}) SET p.score = 42",
            &no_params(),
        )?;
        // One multi-op Batch frame: Batch([InsertNode(x), InsertNode(y), InsertEdge]).
        // Crashing inside this frame must leave NONE of the three ops applied.
        db.query_write(
            "CREATE (x:Peer {id: 'dst_x'})-[:LINK]->(y:Peer {id: 'dst_y'})",
            &no_params(),
        )?;
        Ok(())
    }

    let total_bytes = {
        let mut db = GraphDb::open_with(sim_harness::SimFs::new()).unwrap();
        cypher_workload(&mut db).unwrap();
        db.into_fs().total_appended()
    };
    assert!(total_bytes > 0, "Cypher workload must append bytes");

    for crash_at in 0..=total_bytes {
        let mut db =
            GraphDb::open_with(sim_harness::SimFs::with_crash_after(crash_at)).unwrap();
        let _ = cypher_workload(&mut db);
        let survivor = db.into_fs().surviving_state();

        let recovered = GraphDb::open_with(survivor).unwrap();

        // Single-op frame invariants.
        if recovered.has_node("dst_alice") {
            assert!(
                recovered.get_prop("dst_alice", "id").is_some(),
                "crash_at={crash_at}: dst_alice exists but id prop missing"
            );
        }
        if recovered.has_node("dst_bob") {
            assert!(
                recovered.get_prop("dst_bob", "id").is_some(),
                "crash_at={crash_at}: dst_bob exists but id prop missing"
            );
        }
        if let Some(score) = recovered.get_prop("dst_alice", "score") {
            assert_eq!(
                *score,
                Value::Int(42),
                "crash_at={crash_at}: dst_alice.score must be 42 when present"
            );
        }

        // Multi-op Batch none-or-complete invariant:
        // The Batch([InsertNode(x), InsertNode(y), InsertEdge]) frame is atomic.
        // After any crash the recovered state must satisfy: either all three ops
        // landed (both nodes present AND edge x→y exists) or none landed.
        let x_exists = recovered.has_node("dst_x");
        let y_exists = recovered.has_node("dst_y");
        let edge_exists = recovered
            .neighbors("dst_x", "LINK", Direction::Out)
            .unwrap_or_default()
            .contains(&"dst_y".to_string());
        if x_exists || y_exists || edge_exists {
            assert!(
                x_exists && y_exists && edge_exists,
                "crash_at={crash_at}: multi-op Batch must be none-or-complete: \
                 x_exists={x_exists} y_exists={y_exists} edge_exists={edge_exists}"
            );
        }
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

// ---------------------------------------------------------------------------
// Delete-heavy crash sweep
// ---------------------------------------------------------------------------

/// Scratch-reference oracle for the delete-heavy workload.
///
/// Runs the same workload on a healthy db to produce the expected final state:
/// returns (live_keys, rule_edges) where rule_edges is the full derived edge set.
fn delete_heavy_reference() -> (BTreeSet<String>, BTreeSet<(String, String, String)>) {
    let mut db = GraphDb::open_with(SimFs::new()).unwrap();
    delete_heavy_workload(&mut db).unwrap();
    let live: BTreeSet<String> = [
        "d0", "d1", "d2", "d3", "d4", "d5", "d6", "d7", "d8", "d9", "d10", "d11",
    ]
    .iter()
    .filter(|&&k| db.has_node(k))
    .map(|k| k.to_string())
    .collect();
    let mut edges = BTreeSet::new();
    for key in &live {
        if let Ok(ns) = db.neighbors(key, "DTAGS", Direction::Out) {
            for n in ns {
                edges.insert(("DTAGS".to_string(), key.clone(), n));
            }
        }
        if let Ok(ns) = db.neighbors(key, "DFE", Direction::Out) {
            for n in ns {
                edges.insert(("DFE".to_string(), key.clone(), n));
            }
        }
    }
    (live, edges)
}

/// Validate a recovered db against the delete-heavy oracle invariants:
///
/// - Every live key present in both recovered and reference must agree on
///   derived edge membership (a key may be absent if the delete landed;
///   a key may be present if it did not).
/// - No edge references a tombstoned node.
/// - Rebuild of every rule is a no-op.
fn assert_delete_heavy_invariants(recovered: &mut GraphDb<SimFs>, label: &str) {
    // All live keys must have consistent props (either d{i} exists with f prop or not).
    for i in 0..12 {
        let key = format!("d{i}");
        if recovered.has_node(&key) {
            // Nodes inserted with f prop.
            assert!(
                recovered.get_prop(&key, "f").is_some(),
                "{label}: {key} exists but f prop missing"
            );
        }
    }

    // No derived edge references a tombstoned node.
    let live_keys: BTreeSet<String> = (0..12)
        .filter(|&i| recovered.has_node(&format!("d{i}")))
        .map(|i| format!("d{i}"))
        .collect();

    for etype in &["DTAGS", "DFE"] {
        for key in &live_keys {
            for dst in recovered
                .neighbors(key, etype, Direction::Out)
                .unwrap_or_default()
            {
                assert!(
                    live_keys.contains(&dst),
                    "{label}: derived edge {key}→{dst} via {etype} references a non-live node"
                );
            }
        }
    }

    // Rebuild must be a no-op.
    let rules: Vec<_> = recovered.rules().iter().map(|r| r.name.clone()).collect();
    for rule in &rules {
        let before: BTreeSet<_> = live_keys
            .iter()
            .flat_map(|k| {
                let etype = if rule == "dtags" { "DTAGS" } else { "DFE" };
                recovered
                    .neighbors(k, etype, Direction::Out)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|d| (k.clone(), d))
            })
            .collect();
        recovered.rebuild_rule(rule).unwrap();
        let after: BTreeSet<_> = live_keys
            .iter()
            .flat_map(|k| {
                let etype = if rule == "dtags" { "DTAGS" } else { "DFE" };
                recovered
                    .neighbors(k, etype, Direction::Out)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|d| (k.clone(), d))
            })
            .collect();
        assert_eq!(
            before, after,
            "{label}: rebuild_rule({rule}) must be a no-op after recovery"
        );
    }
}

/// Delete-heavy workload: inserts 12 nodes then deletes 5 interleaved with
/// prop updates. Nodes d2, d4, d6, d8, d10 are deleted; remaining are live.
///
/// Rule "dtags": Overlap tags ≥ 0.5 → DTAGS edges (symmetric, fires on insert/delete).
/// Rule "dfe":   FieldEqual field "f" → DFE edges (symmetric, fires on set_prop).
///
/// Interleaving: insert 6, create rules, insert 6 more, then alternate
/// set_prop + delete so each delete removes a top-candidate for the remaining.
fn delete_heavy_workload<F: Fs>(db: &mut GraphDb<F>) -> core_api::Result<()> {
    // Insert first 6 nodes: d0..d5 with shared tag "alpha" and unique tags.
    let pairs: &[(&str, &[&str])] = &[
        ("d0", &["alpha", "beta"]),
        ("d1", &["alpha", "gamma"]),
        ("d2", &["alpha", "beta"]),  // will be deleted
        ("d3", &["alpha", "gamma"]),
        ("d4", &["alpha", "beta"]),  // will be deleted
        ("d5", &["alpha", "gamma"]),
    ];
    for (key, ts) in pairs {
        let tv = Value::List(ts.iter().map(|t| Value::Str((*t).into())).collect());
        db.insert_node(
            "D",
            key,
            vec![
                ("f".into(), Value::Str(key.to_string())),
                ("tags".into(), tv),
            ],
        )?;
    }

    // Create rules while d0..d5 exist.
    db.create_rule(RuleDef {
        name: "dtags".into(),
        src_label: "D".into(),
        dst_label: "D".into(),
        predicate: Predicate::Overlap {
            field: "tags".into(),
            min: 0.5,
        },
        edge_type: "DTAGS".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
    })?;
    db.create_rule(RuleDef {
        name: "dfe".into(),
        src_label: "D".into(),
        dst_label: "D".into(),
        predicate: Predicate::FieldEqual { field: "f".into() },
        edge_type: "DFE".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
    })?;

    // Insert 6 more nodes: d6..d11.
    let pairs2: &[(&str, &[&str])] = &[
        ("d6",  &["alpha", "beta"]),  // will be deleted
        ("d7",  &["alpha", "gamma"]),
        ("d8",  &["alpha", "beta"]),  // will be deleted
        ("d9",  &["alpha", "gamma"]),
        ("d10", &["alpha", "beta"]),  // will be deleted
        ("d11", &["alpha", "gamma"]),
    ];
    for (key, ts) in pairs2 {
        let tv = Value::List(ts.iter().map(|t| Value::Str((*t).into())).collect());
        db.insert_node(
            "D",
            key,
            vec![
                ("f".into(), Value::Str(key.to_string())),
                ("tags".into(), tv),
            ],
        )?;
    }

    // Interleaved deletes and prop updates.
    // d2 and d4 share "beta" with d0; deleting d2 first, then update d0.f.
    db.delete_node("d2")?;
    db.set_prop("d0", "f", Value::Str("d1".into()))?; // d0 and d1 now FieldEqual
    db.delete_node("d4")?;
    db.set_prop("d3", "f", Value::Str("d1".into()))?; // d3 joins d0→d1 cluster
    db.delete_node("d6")?;
    db.delete_node("d8")?;
    db.set_prop("d7", "f", Value::Str("d9".into()))?; // d7 and d9 FieldEqual
    db.delete_node("d10")?;

    Ok(())
}

/// Byte-offset crash sweep over the delete-heavy workload.
///
/// At every WAL byte offset, crashes the write path and verifies recovery:
///   (a) open_with never panics or errors
///   (b) no derived edge references a tombstoned node
///   (c) rebuild of every rule is a no-op
/// Satisfies the brief's requirement for "one crash-during-delete-heavy-workload sweep."
#[test]
fn recovery_delete_heavy_byte_sweep() {
    let total_bytes = {
        let mut db = GraphDb::open_with(SimFs::new()).unwrap();
        delete_heavy_workload(&mut db).unwrap();
        db.into_fs().total_appended()
    };
    assert!(total_bytes > 0, "delete-heavy workload must append at least one byte");
    eprintln!(
        "delete-heavy byte-offset sweep: 0..={total_bytes} ({} crash points)",
        total_bytes + 1
    );

    // Scratch reference: the fully-applied final state.
    let (reference_live, reference_edges) = delete_heavy_reference();
    eprintln!(
        "reference: {} live keys, {} derived edges",
        reference_live.len(),
        reference_edges.len()
    );

    for crash_at in 0..=total_bytes {
        let mut db = GraphDb::open_with(SimFs::with_crash_after(crash_at)).unwrap();
        let _ = delete_heavy_workload(&mut db);
        let survivor = db.into_fs().surviving_state();

        // (a) Recovery never errors.
        let mut recovered = GraphDb::open_with(survivor).unwrap();

        // (b)+(c) Internally consistent + rebuild-is-noop.
        assert_delete_heavy_invariants(&mut recovered, &format!("crash_at={crash_at}"));

        // Any subset of deletes may have landed; surviving live keys must be
        // a subset of or equal to the pre-delete full set.
        for i in 0..12 {
            let key = format!("d{i}");
            if recovered.has_node(&key) {
                // If the node is still alive its edges must only point to live nodes.
                for etype in &["DTAGS", "DFE"] {
                    for dst in recovered
                        .neighbors(&key, etype, Direction::Out)
                        .unwrap_or_default()
                    {
                        assert!(
                            recovered.has_node(&dst),
                            "crash_at={crash_at}: {key}→{dst} via {etype} but {dst} is not live"
                        );
                    }
                }
            }
        }

        // Final-state crash points: fully-recovered state must match reference.
        // We only check when ALL expected deletes have landed AND all prop-sets landed.
        let all_deleted = ["d2", "d4", "d6", "d8", "d10"]
            .iter()
            .all(|k| !recovered.has_node(k));
        let all_live = ["d0", "d1", "d3", "d5", "d7", "d9", "d11"]
            .iter()
            .all(|&k| recovered.has_node(k));
        if all_deleted && all_live {
            // Collect actual edges and assert subset of reference (recall check).
            let mut actual_edges = BTreeSet::new();
            for etype in &["DTAGS", "DFE"] {
                for key in &reference_live {
                    if let Ok(ns) = recovered.neighbors(key, etype, Direction::Out) {
                        for n in ns {
                            actual_edges.insert((etype.to_string(), key.clone(), n));
                        }
                    }
                }
            }
            assert_eq!(
                actual_edges, reference_edges,
                "crash_at={crash_at}: fully-recovered state must match reference"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Task 4: write_batch DST — large frame crash sweep + composition sweep
// ---------------------------------------------------------------------------

/// Crash sweep at every WAL byte offset inside a large `write_batch` frame.
///
/// The batch contains 12 ops: insert_node ×5, insert_edge ×2,
/// set_prop ×3 (two of which are rule-triggering), remove_prop ×1,
/// delete_node ×1.  At every crash point the recovered state must satisfy
/// the none-or-all invariant: either ALL batch effects landed or NONE did.
///
/// This is the DST gate specified in the task-4 brief ("crash sweep at byte
/// offsets inside a LARGE batch frame, 10+ mixed ops incl. delete_node and
/// rule-triggering set_props → none-applied at every point").
#[test]
fn write_batch_large_frame_dst_byte_sweep() {
    fn tags_v(xs: &[&str]) -> Value {
        Value::List(xs.iter().map(|s| Value::Str((*s).into())).collect())
    }

    fn workload<F: core_storage::fs::Fs>(db: &mut GraphDb<F>) -> core_api::Result<()> {
        // Pre-state: 3 nodes + 1 rule
        db.insert_node(
            "S",
            "pre0",
            vec![
                ("tags".into(), tags_v(&["alpha", "beta"])),
                ("v".into(), Value::Int(1)),
            ],
        )?;
        db.insert_node(
            "S",
            "pre1",
            vec![("tags".into(), tags_v(&["alpha", "beta"]))],
        )?;
        db.insert_node(
            "S",
            "del_target",
            vec![
                ("v".into(), Value::Int(7)),
                ("flag".into(), Value::Bool(true)),
            ],
        )?;
        db.create_rule(RuleDef {
            name: "ov_s".into(),
            src_label: "S".into(),
            dst_label: "S".into(),
            predicate: Predicate::Overlap {
                field: "tags".into(),
                min: 0.5,
            },
            edge_type: "STAG".into(),
            weight_prop: None,
            max_edges: None,
            approximate: false,
        })?;
        // 12-op write_batch: all-or-none atomicity gate
        db.write_batch(|b| {
            b.insert_node("S", "bn0", vec![("n".into(), Value::Str("n0".into()))]);
            b.insert_node(
                "S",
                "bn1",
                vec![("tags".into(), tags_v(&["alpha", "beta"]))],
            );
            b.insert_node("S", "bn2", vec![("v".into(), Value::Int(42))]);
            b.insert_node("S", "bn3", vec![]);
            b.insert_node(
                "S",
                "bn4",
                vec![("tags".into(), tags_v(&["alpha", "beta"]))],
            );
            b.insert_edge("LNK", "bn0", "bn1");
            b.insert_edge("LNK", "bn2", "bn3");
            b.set_prop("pre0", "name", Value::Str("upd".into())); // non-rule prop
            b.set_prop("pre0", "tags", tags_v(&["alpha", "beta", "gamma"])); // rule-triggering
            b.set_prop("pre1", "status", Value::Bool(true)); // non-rule prop
            b.remove_prop("del_target", "flag");
            b.delete_node("del_target");
        })?;
        Ok(())
    }

    let total_bytes = {
        let mut db = GraphDb::open_with(SimFs::new()).unwrap();
        workload(&mut db).unwrap();
        db.into_fs().total_appended()
    };
    assert!(total_bytes > 0, "workload must append bytes");

    for crash_at in 0..=total_bytes {
        let mut db = GraphDb::open_with(SimFs::with_crash_after(crash_at)).unwrap();
        let _ = workload(&mut db);
        let survivor = db.into_fs().surviving_state();
        let recovered = GraphDb::open_with(survivor).unwrap();

        // Core none-or-all invariant: batch nodes are all present or all absent.
        let batch_keys = ["bn0", "bn1", "bn2", "bn3", "bn4"];
        let any_batch = batch_keys.iter().any(|k| recovered.has_node(k));
        let all_batch = batch_keys.iter().all(|k| recovered.has_node(k));
        assert_eq!(
            any_batch, all_batch,
            "crash_at={crash_at}: batch nodes must be all-or-none (any={any_batch} all={all_batch})"
        );

        if any_batch {
            // Batch landed: batch props and delete must be visible.
            assert_eq!(
                recovered.get_prop("pre0", "name"),
                Some(&Value::Str("upd".into())),
                "crash_at={crash_at}: pre0.name must be 'upd' after batch"
            );
            assert!(
                !recovered.has_node("del_target"),
                "crash_at={crash_at}: del_target must be deleted after batch"
            );
            // LNK edges: bn0→bn1 and bn2→bn3.
            assert!(
                recovered
                    .neighbors("bn0", "LNK", Direction::Out)
                    .unwrap_or_default()
                    .contains(&"bn1".to_string()),
                "crash_at={crash_at}: LNK bn0→bn1 must exist after batch"
            );
            assert!(
                recovered
                    .neighbors("bn2", "LNK", Direction::Out)
                    .unwrap_or_default()
                    .contains(&"bn3".to_string()),
                "crash_at={crash_at}: LNK bn2→bn3 must exist after batch"
            );
        } else {
            // Batch not landed: pre-state must be unchanged where nodes exist.
            if recovered.has_node("pre0") {
                assert_eq!(
                    recovered.get_prop("pre0", "name"),
                    None,
                    "crash_at={crash_at}: pre0.name must not exist before batch"
                );
            }
            if recovered.has_node("del_target") {
                assert_eq!(
                    recovered.get_prop("del_target", "flag"),
                    Some(&Value::Bool(true)),
                    "crash_at={crash_at}: del_target.flag must be true before batch"
                );
                assert_eq!(
                    recovered.get_prop("del_target", "v"),
                    Some(&Value::Int(7)),
                    "crash_at={crash_at}: del_target.v must be 7 before batch"
                );
            }
            // LNK edges must be absent when the batch hasn't landed.
            // bn0 and bn2 don't exist yet, so neighbors returns Err → empty.
            // This assertion is symmetric with the landed-batch edge check above.
            assert!(
                recovered
                    .neighbors("bn0", "LNK", Direction::Out)
                    .unwrap_or_default()
                    .is_empty(),
                "crash_at={crash_at}: LNK edges from bn0 must be absent before batch"
            );
            assert!(
                recovered
                    .neighbors("bn2", "LNK", Direction::Out)
                    .unwrap_or_default()
                    .is_empty(),
                "crash_at={crash_at}: LNK edges from bn2 must be absent before batch"
            );
        }

        // Rule consistency: no derived STAG edge may reference a non-live node.
        let all_keys: &[&str] = &["pre0", "pre1", "del_target", "bn0", "bn1", "bn2", "bn3", "bn4"];
        for key in all_keys {
            if !recovered.has_node(key) {
                continue;
            }
            for dst in recovered
                .neighbors(key, "STAG", Direction::Out)
                .unwrap_or_default()
            {
                assert!(
                    recovered.has_node(&dst),
                    "crash_at={crash_at}: derived STAG edge {key}→{dst} but {dst} is not live"
                );
            }
        }
    }
}

/// Composition crash sweep: `write_batch` → `snapshot` → `delete_node`
/// (triggers top-k backfill) → `write_batch`.
///
/// Verifies that at every WAL byte offset the recovered state is consistent:
///   (a) no derived edge references a non-live node
///   (b) rebuild_rule is a no-op (engine state == brute-force desired state)
///   (c) second write_batch is atomic (c5 and c6 are none-or-all)
///
/// Uses `max_edges: Some(2)` to force top-k backfill after delete_node.
#[test]
fn write_batch_composition_sweep() {
    fn tags_v(xs: &[&str]) -> Value {
        Value::List(xs.iter().map(|s| Value::Str((*s).into())).collect())
    }

    fn composition_workload<F: core_storage::fs::Fs>(
        db: &mut GraphDb<F>,
    ) -> core_api::Result<()> {
        // Phase 1: write_batch — 5 nodes
        db.write_batch(|b| {
            b.insert_node("C", "c0", vec![("tags".into(), tags_v(&["x", "y"]))]);
            b.insert_node("C", "c1", vec![("tags".into(), tags_v(&["x", "y"]))]);
            b.insert_node("C", "c2", vec![("tags".into(), tags_v(&["x", "y"]))]);
            b.insert_node("C", "c3", vec![("tags".into(), tags_v(&["x", "y"]))]);
            b.insert_node("C", "c4", vec![("tags".into(), tags_v(&["x", "y"]))]);
        })?;

        // Create rule with max_edges=2 (top-2 per node).
        db.create_rule(RuleDef {
            name: "ctag".into(),
            src_label: "C".into(),
            dst_label: "C".into(),
            predicate: Predicate::Overlap {
                field: "tags".into(),
                min: 0.5,
            },
            edge_type: "CTAG".into(),
            weight_prop: None,
            max_edges: Some(2),
            approximate: false,
        })?;

        // Phase 2: snapshot — captures nodes + rule + derived edges.
        db.snapshot()?;

        // Phase 3: delete_node — triggers top-k backfill in rule engine.
        db.delete_node("c0")?;

        // Phase 4: second write_batch — 2 new nodes + 1 set_prop.
        db.write_batch(|b| {
            b.insert_node("C", "c5", vec![("tags".into(), tags_v(&["x", "y"]))]);
            b.insert_node("C", "c6", vec![("tags".into(), tags_v(&["x", "y"]))]);
            b.set_prop("c2", "note", Value::Str("updated".into()));
        })?;

        Ok(())
    }

    let total_bytes = {
        let mut db = GraphDb::open_with(SimFs::new()).unwrap();
        composition_workload(&mut db).unwrap();
        db.into_fs().total_appended()
    };
    assert!(total_bytes > 0, "composition workload must append bytes");

    let c_keys: &[&str] = &["c0", "c1", "c2", "c3", "c4", "c5", "c6"];

    for crash_at in 0..=total_bytes {
        let mut db = GraphDb::open_with(SimFs::with_crash_after(crash_at)).unwrap();
        let _ = composition_workload(&mut db);
        let survivor = db.into_fs().surviving_state();
        let mut recovered = GraphDb::open_with(survivor).unwrap();

        // (a) No derived CTAG edge references a non-live node.
        for key in c_keys {
            if !recovered.has_node(key) {
                continue;
            }
            for dst in recovered
                .neighbors(key, "CTAG", Direction::Out)
                .unwrap_or_default()
            {
                assert!(
                    recovered.has_node(&dst),
                    "crash_at={crash_at}: derived CTAG {key}→{dst} but {dst} is not live"
                );
            }
        }

        // (b) rebuild_rule is a no-op (rule engine == desired state).
        if recovered.rules().iter().any(|r| r.name == "ctag") {
            let before: BTreeSet<(String, String)> = c_keys
                .iter()
                .filter(|k| recovered.has_node(k))
                .flat_map(|k| {
                    recovered
                        .neighbors(k, "CTAG", Direction::Out)
                        .unwrap_or_default()
                        .into_iter()
                        .map(|d| (k.to_string(), d))
                })
                .collect();
            recovered.rebuild_rule("ctag").unwrap();
            let after: BTreeSet<(String, String)> = c_keys
                .iter()
                .filter(|k| recovered.has_node(k))
                .flat_map(|k| {
                    recovered
                        .neighbors(k, "CTAG", Direction::Out)
                        .unwrap_or_default()
                        .into_iter()
                        .map(|d| (k.to_string(), d))
                })
                .collect();
            assert_eq!(
                before, after,
                "crash_at={crash_at}: rebuild_rule(ctag) must be a no-op"
            );
        }

        // (c) Second write_batch is atomic: c5 and c6 are none-or-all,
        //     and c2.note is set iff the second batch landed.
        let c5 = recovered.has_node("c5");
        let c6 = recovered.has_node("c6");
        if c5 || c6 {
            assert!(
                c5 && c6,
                "crash_at={crash_at}: c5={c5} c6={c6} must be both-or-neither (same write_batch)"
            );
            assert_eq!(
                recovered.get_prop("c2", "note"),
                Some(&Value::Str("updated".into())),
                "crash_at={crash_at}: c2.note must be set when second write_batch landed"
            );
        }
        if !c5 && !c6 && recovered.has_node("c2") {
            assert_eq!(
                recovered.get_prop("c2", "note"),
                None,
                "crash_at={crash_at}: c2.note must not exist before second write_batch"
            );
        }
    }
}
