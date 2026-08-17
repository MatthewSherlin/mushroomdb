use core_api::{Direction, GraphDb, Predicate, RuleDef, Value};
use core_storage::fs::Fs;
use sim_harness::SimFs;
use std::collections::BTreeSet;

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
    "n0", "n1", "n2", "n3", "n4", "n5", "n6", "n7", "n8", "n9", "n10", "n11",
];

fn tags(items: &[&str]) -> Value {
    Value::List(items.iter().map(|s| Value::Str((*s).into())).collect())
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
    })?;
    db.delete_rule("dummy")?;

    // Snapshot while km and ov rules are both live (no dummy rule).
    db.snapshot()?;

    // set_prop calls post-snapshot: retract + re-create KM edges.
    // n0.f: "n6" → "n7" (retract n0→n6, create n0→n7)
    db.set_prop("n0", "f", Value::Str("n7".into()))?;
    // n0.f: "n7" → "n6" (retract n0→n7, restore n0→n6)
    db.set_prop("n0", "f", Value::Str("n6".into()))?;
    // n2.f: "n8" → "n10" (retract n2→n8, create n2→n10; n4→n10 still exists)
    db.set_prop("n2", "f", Value::Str("n10".into()))?;

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

    for crash_at in 0..=total_bytes {
        let mut db = GraphDb::open_with(SimFs::with_crash_after(crash_at)).unwrap();
        let _ = workload_with_rules(&mut db);
        let survivor = db.into_fs().surviving_state();

        // Invariant (a): open_with never panics or errors.
        let mut recovered = GraphDb::open_with(survivor).unwrap();

        // Invariant (b): recovered state is internally consistent.
        let n = recovered.node_count();
        assert!(n <= 12, "crash_at={crash_at}: impossible node count {n}");
        for i in 0..6usize {
            if recovered.has_node(&format!("n{i}")) {
                assert!(
                    recovered.get_prop(&format!("n{i}"), "f").is_some(),
                    "crash_at={crash_at}: L0 node n{i} exists but f prop is missing"
                );
            }
        }
        for i in 6..12usize {
            if recovered.has_node(&format!("n{i}")) {
                assert!(
                    recovered.get_prop(&format!("n{i}"), "tags").is_some(),
                    "crash_at={crash_at}: L1 node n{i} exists but tags prop is missing"
                );
            }
        }

        // Invariant (c): rebuild_rule is a no-op for every surviving rule.
        let rules = recovered.rules();
        for rule in &rules {
            let before = collect_rule_edges(&recovered, rule);
            recovered.rebuild_rule(&rule.name).unwrap();
            let after = collect_rule_edges(&recovered, rule);
            assert_eq!(
                before, after,
                "crash_at={crash_at}: rebuild_rule changed edges for rule {:?}",
                rule.name
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

        // Invariant (b): recovered state is internally consistent.
        let n = recovered.node_count();
        assert!(n <= 12, "crash_op={crash_op}: impossible node count {n}");
        for i in 0..6usize {
            if recovered.has_node(&format!("n{i}")) {
                assert!(
                    recovered.get_prop(&format!("n{i}"), "f").is_some(),
                    "crash_op={crash_op}: L0 node n{i} exists but f prop is missing"
                );
            }
        }
        for i in 6..12usize {
            if recovered.has_node(&format!("n{i}")) {
                assert!(
                    recovered.get_prop(&format!("n{i}"), "tags").is_some(),
                    "crash_op={crash_op}: L1 node n{i} exists but tags prop is missing"
                );
            }
        }

        // Invariant (c): rebuild_rule is a no-op for every surviving rule.
        let rules = recovered.rules();
        for rule in &rules {
            let before = collect_rule_edges(&recovered, rule);
            recovered.rebuild_rule(&rule.name).unwrap();
            let after = collect_rule_edges(&recovered, rule);
            assert_eq!(
                before, after,
                "crash_op={crash_op}: rebuild_rule changed edges for rule {:?}",
                rule.name
            );
        }
    }
}
