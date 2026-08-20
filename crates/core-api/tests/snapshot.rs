use core_api::{Direction, GraphDb, GraphError, Predicate, RuleDef, Value};

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("graphdb-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

#[test]
fn snapshot_plus_wal_tail_recovers_everything() {
    let dir = tmp("snap");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("N", "a", vec![("v".into(), Value::Int(1))])
            .unwrap();
        db.insert_node("N", "b", vec![]).unwrap();
        db.insert_edge("E", "a", "b").unwrap();
        db.snapshot().unwrap();
        // post-snapshot writes live only in the wal tail
        db.insert_node("N", "c", vec![]).unwrap();
        db.insert_edge("E", "b", "c").unwrap();
    }
    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(db.node_count(), 3);
    assert_eq!(db.edge_count(), 2);
    assert_eq!(db.get_prop("a", "v"), Some(&Value::Int(1)));
    assert_eq!(db.neighbors("b", "E", Direction::Out).unwrap(), vec!["c"]);
}

#[test]
fn corrupt_snapshot_fails_loudly() {
    let dir = tmp("snapcorrupt");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("N", "a", vec![]).unwrap();
        db.snapshot().unwrap();
    }
    let path = dir.join("snapshot.bin");
    let mut bytes = std::fs::read(&path).unwrap();
    let n = bytes.len();
    bytes[n - 1] ^= 0xFF;
    std::fs::write(&path, bytes).unwrap();
    assert!(GraphDb::open(&dir).is_err()); // spec §8: refuse loudly, never guess
}

#[test]
fn snapshot_preserves_rules_provenance_and_scores() {
    let dir = tmp("snap-rules");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node(
            "A",
            "a",
            vec![("tags".into(), Value::List(vec![Value::Str("x".into())]))],
        )
        .unwrap();
        db.create_rule(RuleDef {
            name: "rel".into(),
            src_label: "A".into(),
            dst_label: "A".into(),
            predicate: Predicate::Overlap {
                field: "tags".into(),
                min: 0.5,
            },
            edge_type: "REL".into(),
            weight_prop: Some("score".into()),
            max_edges: None,
            approximate: false,
        })
        .unwrap();
        db.insert_node(
            "A",
            "b",
            vec![("tags".into(), Value::List(vec![Value::Str("x".into())]))],
        )
        .unwrap();
        db.snapshot().unwrap();
        // post-snapshot wal-tail write
        db.insert_node(
            "A",
            "c",
            vec![("tags".into(), Value::List(vec![Value::Str("x".into())]))],
        )
        .unwrap();
    }
    let mut db = GraphDb::open(&dir).unwrap();
    assert_eq!(db.rules().len(), 1);
    assert_eq!(db.edge_count(), 6); // a,b,c pairwise, both directions
                                    // derived edges still owned after recovery (guard works)
    assert!(matches!(
        db.insert_edge("REL", "a", "b"),
        Err(GraphError::RuleOwned { .. })
    ));
    // and incremental firing still works after reopen (indexes were rebuilt)
    db.insert_node(
        "A",
        "d",
        vec![("tags".into(), Value::List(vec![Value::Str("x".into())]))],
    )
    .unwrap();
    assert_eq!(db.edge_count(), 12);
}

#[test]
fn version_2_snapshot_is_rejected() {
    let dir = tmp("snap-v2");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("N", "a", vec![]).unwrap();
        db.snapshot().unwrap();
    }
    let path = dir.join("snapshot.bin");
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[4] = 2;
    bytes[5] = 0; // stamp VERSION=2
    std::fs::write(&path, bytes).unwrap();
    match GraphDb::open(&dir) {
        Err(GraphError::Corrupt { detail }) => assert!(detail.contains("version 2"), "{detail}"),
        Ok(_) => panic!("expected Corrupt, got Ok"),
        Err(e) => panic!("expected Corrupt, got other error: {e:?}"),
    }
}

#[test]
fn crash_between_snapshot_and_wal_truncation_recovers() {
    // === Phase 1: CreateRule idempotency ===
    // Snapshot captures rule; crash leaves pre-snapshot WAL (with CreateRule) intact.
    // Reopen must not fail with RuleInvalid on the duplicate CreateRule replay.
    let dir = tmp("crash-create-rule");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node(
            "A",
            "a",
            vec![("tags".into(), Value::List(vec![Value::Str("x".into())]))],
        )
        .unwrap();
        db.create_rule(RuleDef {
            name: "rel".into(),
            src_label: "A".into(),
            dst_label: "A".into(),
            predicate: Predicate::Overlap {
                field: "tags".into(),
                min: 0.5,
            },
            edge_type: "REL".into(),
            weight_prop: None,
            max_edges: None,
            approximate: false,
        })
        .unwrap();
        db.insert_node(
            "A",
            "b",
            vec![("tags".into(), Value::List(vec![Value::Str("x".into())]))],
        )
        .unwrap();
        // save pre-snapshot WAL (has InsertNode a, CreateRule rel, InsertNode b)
        let pre_snap_wal = std::fs::read(dir.join("wal.bin")).unwrap();
        db.snapshot().unwrap();
        // simulate crash: restore pre-snapshot WAL before truncation took effect
        std::fs::write(dir.join("wal.bin"), &pre_snap_wal).unwrap();
    }
    let mut db = GraphDb::open(&dir).unwrap();
    assert_eq!(db.rules().len(), 1);
    assert_eq!(db.edge_count(), 2); // a↔b both directions
                                    // incremental firing still works after recovery
    db.insert_node(
        "A",
        "c",
        vec![("tags".into(), Value::List(vec![Value::Str("x".into())]))],
    )
    .unwrap();
    assert_eq!(db.edge_count(), 6); // a,b,c pairwise

    // === Phase 2: DeleteRule idempotency ===
    // Snapshot captures state without a deleted rule; crash leaves WAL (with DeleteRule) intact.
    // Reopen must not fail with RuleNotFound on the replay of a delete for an absent rule.
    let dir2 = tmp("crash-delete-rule");
    {
        let mut db = GraphDb::open(&dir2).unwrap();
        db.insert_node("A", "x", vec![]).unwrap();
        db.create_rule(RuleDef {
            name: "gone".into(),
            src_label: "A".into(),
            dst_label: "A".into(),
            predicate: Predicate::Overlap {
                field: "tags".into(),
                min: 0.5,
            },
            edge_type: "GONE".into(),
            weight_prop: None,
            max_edges: None,
            approximate: false,
        })
        .unwrap();
        // first snapshot: rule "gone" in engine
        db.snapshot().unwrap();
        // delete the rule; WAL now contains [DeleteRule "gone"]
        db.delete_rule("gone").unwrap();
        let pre_snap_wal = std::fs::read(dir2.join("wal.bin")).unwrap();
        // second snapshot: captures state WITHOUT "gone"; truncates WAL
        db.snapshot().unwrap();
        // simulate crash before WAL truncation
        std::fs::write(dir2.join("wal.bin"), &pre_snap_wal).unwrap();
    }
    // reopen: snapshot has no "gone", WAL replays DeleteRule "gone" for missing rule → must not error
    let db = GraphDb::open(&dir2).unwrap();
    assert_eq!(db.rules().len(), 0); // "gone" is still gone
}

// ---------------------------------------------------------------------------
// V4 snapshot tests
// ---------------------------------------------------------------------------

fn emb(xs: &[f64]) -> Value {
    Value::List(xs.iter().copied().map(Value::Float).collect())
}

/// V3-stamped snapshot must be rejected with a clear error mentioning "version 3".
#[test]
fn v3_snapshot_is_rejected_with_clear_message() {
    let dir = tmp("snap-v3-reject");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("N", "a", vec![]).unwrap();
        db.snapshot().unwrap(); // writes V4
    }
    let path = dir.join("snapshot.bin");
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[4] = 3;
    bytes[5] = 0; // stamp VERSION=3
    std::fs::write(&path, bytes).unwrap();
    match GraphDb::open(&dir) {
        Err(GraphError::Corrupt { detail }) => {
            assert!(
                detail.contains("version 3"),
                "error should mention version 3; got: {detail}"
            );
        }
        Ok(_) => panic!("expected Corrupt for V3 snapshot, got Ok"),
        Err(e) => panic!("expected Corrupt for V3 snapshot, got: {e:?}"),
    }
}

/// V4 round-trip: exact rule — provenance, derived edges, and incremental
/// firing all survive snapshot → reopen.
#[test]
fn v4_round_trip_exact_rule() {
    let dir = tmp("snap-v4-exact");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node(
            "A",
            "a",
            vec![("tags".into(), Value::List(vec![Value::Str("x".into())]))],
        )
        .unwrap();
        db.insert_node(
            "A",
            "b",
            vec![("tags".into(), Value::List(vec![Value::Str("x".into())]))],
        )
        .unwrap();
        db.create_rule(RuleDef {
            name: "rel".into(),
            src_label: "A".into(),
            dst_label: "A".into(),
            predicate: Predicate::Overlap {
                field: "tags".into(),
                min: 0.5,
            },
            edge_type: "REL".into(),
            weight_prop: None,
            max_edges: None,
            approximate: false,
        })
        .unwrap();
        // a↔b derived
        assert_eq!(db.edge_count(), 2);
        db.snapshot().unwrap();
        // post-snapshot WAL write
        db.insert_node(
            "A",
            "c",
            vec![("tags".into(), Value::List(vec![Value::Str("x".into())]))],
        )
        .unwrap();
    }
    let mut db = GraphDb::open(&dir).unwrap();
    // a,b,c pairwise = 6
    assert_eq!(db.edge_count(), 6);
    assert_eq!(db.rules().len(), 1);
    // incremental firing works after V4 recovery
    db.insert_node(
        "A",
        "d",
        vec![("tags".into(), Value::List(vec![Value::Str("x".into())]))],
    )
    .unwrap();
    assert_eq!(db.edge_count(), 12); // a,b,c,d pairwise
}

/// V4 round-trip: approximate rule — IVF state (centroids) is restored from
/// snapshot, not re-fitted.  Verify by comparing the edge set before and after
/// reopen: with loaded centroids the assignments are deterministic so the edge
/// set must be identical.
#[test]
fn v4_round_trip_approx_rule_edge_set_identical() {
    let dir = tmp("snap-v4-approx");

    // Build 8 VA nodes in 4 pairs across quadrants of the 2-D unit circle.
    // Pair A≈[1,0]: va0↔va1 cos≈0.98. Pair B≈[0,1]: va2↔va3. etc.
    // Cross-pair cosines ≤0.2 — no cross-pair edges.
    let va_data: &[(&str, [f64; 2])] = &[
        ("va0", [1.0, 0.0]),
        ("va1", [0.98, 0.2]),
        ("va2", [0.0, 1.0]),
        ("va3", [-0.2, (1.0_f64 - 0.04_f64).sqrt()]),
        ("va4", [-1.0, 0.0]),
        ("va5", [-0.98, 0.2]),
        ("va6", [0.0, -1.0]),
        ("va7", [0.2, -0.98]),
    ];

    let edges_before_snap: Vec<(String, Vec<String>)>;
    {
        let mut db = GraphDb::open(&dir).unwrap();
        for (k, v) in va_data {
            db.insert_node("VA", k, vec![("emb".into(), emb(v))]).unwrap();
        }
        db.create_rule(RuleDef {
            name: "vapprox".into(),
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
        })
        .unwrap();

        // Record edges before snapshot for comparison.
        edges_before_snap = va_data
            .iter()
            .map(|(k, _)| {
                (
                    k.to_string(),
                    db.neighbors(k, "VAPPROX", Direction::Out)
                        .unwrap_or_default(),
                )
            })
            .collect();

        db.snapshot().unwrap();
    }

    // Reopen: IVF state is loaded from V4 snapshot, not re-fitted.
    let db = GraphDb::open(&dir).unwrap();
    let edges_after_snap: Vec<(String, Vec<String>)> = va_data
        .iter()
        .map(|(k, _)| {
            (
                k.to_string(),
                db.neighbors(k, "VAPPROX", Direction::Out)
                    .unwrap_or_default(),
            )
        })
        .collect();

    assert_eq!(
        edges_before_snap, edges_after_snap,
        "edge set after reopen must equal edge set before snapshot (IVF loaded, not re-fitted)"
    );
    // Sanity: at least some pairs should have edges (≥ 50% recall for an approximate rule).
    // We do not require 100% recall — approximate rules have a recall floor, not a ceiling.
    // The primary correctness guarantee is the edge-set identity check above.
    let total_approx: usize = edges_after_snap.iter().map(|(_, ns)| ns.len()).sum();
    assert!(
        total_approx >= 4,
        "expected ≥4 approx edges (≥50% of 8 exact edges), got {total_approx}"
    );
}

/// Replay-identity: build db (incl. approximate rule) → snapshot → mutations
/// → close → reopen → derived set must equal a reference db that was never
/// closed and received the same op sequence.
///
/// The reference db and the snapshot db both use the real filesystem; the
/// reference db is an independent second directory.  After reopen, the edge
/// sets (which are deterministic once the centroids are fixed) must match.
#[test]
fn v4_replay_identity_with_approx_rule() {
    let va_data: &[(&str, [f64; 2])] = &[
        ("va0", [1.0, 0.0]),
        ("va1", [0.98, 0.2]),
        ("va2", [0.0, 1.0]),
        ("va3", [-0.2, (1.0_f64 - 0.04_f64).sqrt()]),
        ("va4", [-1.0, 0.0]),
        ("va5", [-0.98, 0.2]),
        ("va6", [0.0, -1.0]),
        ("va7", [0.2, -0.98]),
    ];

    let dir = tmp("snap-v4-replay-id");
    let ref_dir = tmp("snap-v4-replay-id-ref");

    // Build the reference db — never snapshot, receives all ops.
    let mut ref_db = GraphDb::open(&ref_dir).unwrap();
    for (k, v) in va_data {
        ref_db
            .insert_node("VA", k, vec![("emb".into(), emb(v))])
            .unwrap();
    }
    ref_db
        .create_rule(RuleDef {
            name: "vapprox".into(),
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
        })
        .unwrap();

    // Build the snapshot db: same ops up to the snapshot, then more mutations.
    {
        let mut db = GraphDb::open(&dir).unwrap();
        for (k, v) in va_data {
            db.insert_node("VA", k, vec![("emb".into(), emb(v))]).unwrap();
        }
        db.create_rule(RuleDef {
            name: "vapprox".into(),
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
        })
        .unwrap();
        db.snapshot().unwrap();
        // Post-snapshot mutations: insert two more nodes.
        db.insert_node("VA", "va8", vec![("emb".into(), emb(&[0.99, 0.14]))])
            .unwrap();
        db.insert_node("VA", "va9", vec![("emb".into(), emb(&[-0.99, 0.14]))])
            .unwrap();
    }

    // Apply the same post-snapshot mutations to the reference db.
    ref_db
        .insert_node("VA", "va8", vec![("emb".into(), emb(&[0.99, 0.14]))])
        .unwrap();
    ref_db
        .insert_node("VA", "va9", vec![("emb".into(), emb(&[-0.99, 0.14]))])
        .unwrap();

    // Collect reference edges before closing.
    let all_keys = [
        "va0", "va1", "va2", "va3", "va4", "va5", "va6", "va7", "va8", "va9",
    ];
    let ref_edges: std::collections::BTreeSet<(String, String)> = all_keys
        .iter()
        .flat_map(|k| {
            ref_db
                .neighbors(k, "VAPPROX", Direction::Out)
                .unwrap_or_default()
                .into_iter()
                .map(|n| (k.to_string(), n))
                .collect::<Vec<_>>()
        })
        .collect();

    // Reopen the snapshot db.
    let recovered = GraphDb::open(&dir).unwrap();
    let recovered_edges: std::collections::BTreeSet<(String, String)> = all_keys
        .iter()
        .filter(|k| recovered.has_node(k))
        .flat_map(|k| {
            recovered
                .neighbors(k, "VAPPROX", Direction::Out)
                .unwrap_or_default()
                .into_iter()
                .map(|n| (k.to_string(), n))
                .collect::<Vec<_>>()
        })
        .collect();

    assert_eq!(
        recovered_edges, ref_edges,
        "recovered approx edge set must match reference db (replay-identity)"
    );
}

/// V4 crash-between-snapshot-and-WAL-truncation with approximate rule.
///
/// Simulates a crash that leaves the pre-snapshot WAL intact after a V4
/// snapshot write.  The approximate rule (vec_approx) must be recovered
/// with IVF state from the V4 snapshot, and incremental WAL replay fires
/// correctly.
#[test]
fn v4_crash_between_snapshot_and_wal_truncation_with_approx_rule() {
    let va_data: &[(&str, [f64; 2])] = &[
        ("va0", [1.0, 0.0]),
        ("va1", [0.98, 0.2]),
        ("va2", [0.0, 1.0]),
        ("va3", [-0.2, (1.0_f64 - 0.04_f64).sqrt()]),
        ("va4", [-1.0, 0.0]),
        ("va5", [-0.98, 0.2]),
        ("va6", [0.0, -1.0]),
        ("va7", [0.2, -0.98]),
    ];

    let dir = tmp("snap-v4-crash-approx");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        for (k, v) in va_data {
            db.insert_node("VA", k, vec![("emb".into(), emb(v))]).unwrap();
        }
        db.create_rule(RuleDef {
            name: "vapprox".into(),
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
        })
        .unwrap();

        // Save the pre-snapshot WAL (has InsertNode * 8 + CreateRule).
        let pre_snap_wal = std::fs::read(dir.join("wal.bin")).unwrap();
        db.snapshot().unwrap(); // V4 snapshot written; WAL truncated
        // Simulate crash: restore pre-snapshot WAL (CreateRule + inserts still there).
        std::fs::write(dir.join("wal.bin"), &pre_snap_wal).unwrap();
    }
    // Reopen: V4 snapshot loaded (with IVF state), WAL replays CreateRule idempotently.
    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(db.rules().len(), 1);
    // All 8 pairs should produce ≥8 approx edges.
    // At least some edges should be present (recovery didn't drop the rule state).
    let total_approx: usize = va_data
        .iter()
        .map(|(k, _)| db.neighbors(k, "VAPPROX", Direction::Out).unwrap_or_default().len())
        .sum();
    assert!(
        total_approx >= 4,
        "expected ≥4 approx edges after crash+reopen (≥50% recall), got {total_approx}"
    );
}

/// Torn-write safety: truncating a V4 snapshot file to various byte counts must
/// always return a Corrupt error, never a silently-wrong database.
/// Covers both mid-header (bad-magic) and mid-payload (crc-mismatch) truncation.
/// Exercises a snapshot that includes both an exact rule and an approximate/IVF rule,
/// ensuring V4's ivf_state section is part of the torn region.
#[test]
fn v4_torn_snapshot_write_is_rejected() {
    let dir = tmp("snap-v4-torn");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        // Exact rule: two nodes with overlapping tags → derived edges.
        db.insert_node(
            "A",
            "a",
            vec![("tags".into(), Value::List(vec![Value::Str("x".into())]))],
        )
        .unwrap();
        db.insert_node(
            "A",
            "b",
            vec![("tags".into(), Value::List(vec![Value::Str("x".into())]))],
        )
        .unwrap();
        db.create_rule(RuleDef {
            name: "exact".into(),
            src_label: "A".into(),
            dst_label: "A".into(),
            predicate: Predicate::Overlap {
                field: "tags".into(),
                min: 0.5,
            },
            edge_type: "REL".into(),
            weight_prop: None,
            max_edges: None,
            approximate: false,
        })
        .unwrap();
        // Approximate/IVF rule: a few vector nodes → IVF state in snapshot.
        let va_pts: &[(&str, [f64; 2])] = &[
            ("va0", [1.0, 0.0]),
            ("va1", [0.98, 0.2]),
            ("va2", [0.0, 1.0]),
            ("va3", [-0.2, (1.0_f64 - 0.04_f64).sqrt()]),
        ];
        for (k, v) in va_pts {
            db.insert_node("VA", k, vec![("emb".into(), emb(v))]).unwrap();
        }
        db.create_rule(RuleDef {
            name: "approx".into(),
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
        })
        .unwrap();
        db.snapshot().unwrap();
    }

    let path = dir.join("snapshot.bin");
    let good = std::fs::read(&path).unwrap();
    let n = good.len();
    assert!(n > 20, "snapshot must be non-trivial; got {n} bytes");

    // Truncation points: mid-header (5 B), just-past-header (10 B),
    // early payload (n/3), mid-payload (n/2), one byte short (n-1).
    // Any truncation must yield Err — never a silently-wrong open.
    for &trunc in &[5usize, 10, n / 3, n / 2, n - 1] {
        std::fs::write(&path, &good[..trunc]).unwrap();
        assert!(
            GraphDb::open(&dir).is_err(),
            "expected Err for {trunc}-byte truncation of {n}-byte V4 snapshot",
        );
    }
}

/// Weight round-trip: an Overlap rule with weight_prop stores Jaccard scores as
/// edge properties.  After snapshot → reopen the full derived-edge set including
/// score values must be identical to a never-closed reference db.
#[test]
fn v4_weight_prop_round_trip() {
    let dir = tmp("snap-v4-weight");
    let ref_dir = tmp("snap-v4-weight-ref");

    let node_data: &[(&str, &[&str])] = &[
        ("a", &["x", "y"]),
        ("b", &["x", "y", "z"]),
        ("c", &["x"]),
    ];

    let make_tags = |tags: &[&str]| {
        Value::List(tags.iter().map(|t| Value::Str(t.to_string())).collect())
    };

    let rule = || RuleDef {
        name: "overlap_weighted".into(),
        src_label: "A".into(),
        dst_label: "A".into(),
        predicate: Predicate::Overlap {
            field: "tags".into(),
            min: 0.5,
        },
        edge_type: "REL".into(),
        weight_prop: Some("score".into()),
        max_edges: None,
        approximate: false,
    };

    // Build reference db — never closed, never snapshotted.
    let mut ref_db = GraphDb::open(&ref_dir).unwrap();
    for (k, tags) in node_data {
        ref_db
            .insert_node("A", k, vec![("tags".into(), make_tags(tags))])
            .unwrap();
    }
    ref_db.create_rule(rule()).unwrap();

    // Build snapshot db with same data, then snapshot and drop.
    {
        let mut db = GraphDb::open(&dir).unwrap();
        for (k, tags) in node_data {
            db.insert_node("A", k, vec![("tags".into(), make_tags(tags))])
                .unwrap();
        }
        db.create_rule(rule()).unwrap();
        db.snapshot().unwrap();
    }

    // Reopen and compare full derived-edge set including score weights.
    let snap_db = GraphDb::open(&dir).unwrap();
    let keys: Vec<&str> = node_data.iter().map(|(k, _)| *k).collect();
    for &src in &keys {
        for &dst in &keys {
            if src == dst {
                continue;
            }
            let ref_expl = ref_db.explain(src, dst).unwrap();
            let snap_expl = snap_db.explain(src, dst).unwrap();
            assert_eq!(
                ref_expl.len(),
                snap_expl.len(),
                "explanation count mismatch for {src}→{dst}"
            );
            for (r, s) in ref_expl.iter().zip(snap_expl.iter()) {
                assert_eq!(
                    r.weight, s.weight,
                    "score weight mismatch for {src}→{dst} rule {}: ref={:?} snap={:?}",
                    r.rule, r.weight, s.weight
                );
            }
        }
    }
    // Sanity: all pairs of distinct nodes should share at least one directed edge with a score.
    assert!(
        snap_db
            .explain("a", "b")
            .unwrap()
            .iter()
            .any(|e| e.weight.is_some()),
        "expected at least one weighted derived edge between a and b"
    );
}
