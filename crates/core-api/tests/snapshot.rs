use core_api::{
    Direction, GraphDb, GraphError, Predicate, RuleDef, SnapshotOptions, Value, ViewDef, ViewSource,
};
use core_storage::wal::decode_all;

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
// V5 snapshot tests
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
        db.snapshot().unwrap(); // writes V5
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

/// V4-stamped snapshot must be refused with a clear error naming "version 4".
#[test]
fn v4_snapshot_is_rejected_with_clear_message() {
    let dir = tmp("snap-v4-reject");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("N", "a", vec![]).unwrap();
        db.snapshot().unwrap(); // writes V5
    }
    let path = dir.join("snapshot.bin");
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[4] = 4;
    bytes[5] = 0; // stamp VERSION=4
    std::fs::write(&path, bytes).unwrap();
    match GraphDb::open(&dir) {
        Err(GraphError::Corrupt { detail }) => {
            assert!(
                detail.contains("version 4"),
                "error should mention version 4; got: {detail}"
            );
        }
        Ok(_) => panic!("expected Corrupt for V4 snapshot, got Ok"),
        Err(e) => panic!("expected Corrupt for V4 snapshot, got: {e:?}"),
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
            db.insert_node("VA", k, vec![("emb".into(), emb(v))])
                .unwrap();
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
            db.insert_node("VA", k, vec![("emb".into(), emb(v))])
                .unwrap();
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
            db.insert_node("VA", k, vec![("emb".into(), emb(v))])
                .unwrap();
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
        .map(|(k, _)| {
            db.neighbors(k, "VAPPROX", Direction::Out)
                .unwrap_or_default()
                .len()
        })
        .sum();
    assert!(
        total_approx >= 4,
        "expected ≥4 approx edges after crash+reopen (≥50% recall), got {total_approx}"
    );
}

/// Torn-write safety: truncating a V6 snapshot file (with IVF state) to various
/// byte counts must always return a Corrupt error, never a silently-wrong database.
/// Covers mid-header (bad magic), mid-compressed-stream, and late-payload truncation.
/// Exercises a snapshot that includes an exact rule, an approximate/IVF rule,
/// and view_defs — ensuring the IVF + view_defs payload region is covered.
///
/// NOTE: this test was originally named `v5_torn_snapshot_write_is_rejected` and
/// exercised the V5 uncompressed format. Since `snapshot()` now writes V6, this
/// test covers V6 torn-write detection. V5 torn-write detection is covered by
/// `v5_torn_file_is_rejected` which uses the committed golden_v5.bin fixture.
#[test]
fn v6_torn_written_snapshot_with_ivf_state_is_rejected() {
    let dir = tmp("snap-v5-torn");
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
            db.insert_node("VA", k, vec![("emb".into(), emb(v))])
                .unwrap();
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
        // V5: create a view so view_defs region is populated in the snapshot.
        db.create_view(ViewDef {
            name: "deg_rel".into(),
            label: "A".into(),
            view_prop: "deg_rel".into(),
            source: ViewSource::Degree {
                edge_type: "REL".into(),
                direction: Direction::Out,
            },
        })
        .unwrap();
        db.snapshot().unwrap();
    }

    let path = dir.join("snapshot.bin");
    let good = std::fs::read(&path).unwrap();
    let n = good.len();
    assert!(n > 20, "snapshot must be non-trivial; got {n} bytes");

    // Truncation points: mid-header (5 B), just-past-compressed-header (10 B),
    // early stream (n/3), mid-stream (n/2), late stream covering IVF+view_defs
    // region (n*2/3, n*3/4, n-5), one byte short (n-1).
    // Any truncation must yield Err — never a silently-wrong open.
    for &trunc in &[
        5usize,
        10,
        n / 3,
        n / 2,
        n * 2 / 3,
        n * 3 / 4,
        n.saturating_sub(5),
        n - 1,
    ] {
        std::fs::write(&path, &good[..trunc]).unwrap();
        assert!(
            GraphDb::open(&dir).is_err(),
            "expected Err for {trunc}-byte truncation of {n}-byte V6 snapshot",
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

    let node_data: &[(&str, &[&str])] =
        &[("a", &["x", "y"]), ("b", &["x", "y", "z"]), ("c", &["x"])];

    let make_tags =
        |tags: &[&str]| Value::List(tags.iter().map(|t| Value::Str(t.to_string())).collect());

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

// ---------------------------------------------------------------------------
// V4 round-trip: top-k per-source rule
// ---------------------------------------------------------------------------

/// V4 round-trip test for top-k rules.
///
/// Brief requirement: "snapshot mid-stream with a top-k rule, reopen, more ops,
/// compare to never-closed reference — eviction state must survive persistence."
///
/// Top-k rules do NOT persist candidate ordering — it is recomputed on demand
/// from the live candidate index (same as the by_node reverse index).  The
/// materialized top-k provenance IS persisted in the V4 snapshot (it is part
/// of the normal provenance map), so this test verifies that:
///   1. The provenance (materialized top-k edges) survives snapshot → reopen.
///   2. Incremental ops after reopen continue to maintain the correct top-k.
///   3. The edge set after reopen + more ops equals a never-closed reference db
///      that received the same complete op sequence.
///
/// No VERSION bump is needed: candidate ordering is ephemeral (rebuilt on demand);
/// only provenance is persisted, which was already handled by the V4 format.
#[test]
fn v4_round_trip_topk_rule() {
    // Phase 1: build db with a top-k rule (k=2), insert some nodes, snapshot.
    let dir = tmp("snap-v4-topk");
    let ref_dir = tmp("snap-v4-topk-ref");

    // NumericWithin k=2: each src gets at most the 2 closest dsts by score.
    let rule = RuleDef {
        name: "near2".into(),
        src_label: "N".into(),
        dst_label: "N".into(),
        predicate: Predicate::NumericWithin {
            field: "year".into(),
            tolerance: 10.0,
        },
        edge_type: "NEAR2".into(),
        weight_prop: Some("score".into()),
        max_edges: Some(2),
        approximate: false,
    };

    // Build the reference db — never snapshot; receives all ops.
    let mut ref_db = GraphDb::open(&ref_dir).unwrap();
    ref_db.create_rule(rule.clone()).unwrap();

    // Phase 1 ops: 5 nodes at years 0, 1, 2, 5, 9.
    let phase1_nodes = [
        ("n0", 0.0f64),
        ("n1", 1.0),
        ("n2", 2.0),
        ("n5", 5.0),
        ("n9", 9.0),
    ];
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.create_rule(rule.clone()).unwrap();

        for (key, year) in phase1_nodes {
            let props = vec![("year".into(), Value::Float(year))];
            db.insert_node("N", key, props.clone()).unwrap();
            ref_db.insert_node("N", key, props).unwrap();
        }

        // Verify top-k is correct before snapshot.
        // n0 at year=0: closest are n1 (|Δ|=1, score=0.9) and n2 (|Δ|=2, score=0.8).
        let n0_out: Vec<String> = db
            .neighbors("n0", "NEAR2", Direction::Out)
            .unwrap_or_default();
        assert!(
            n0_out.contains(&"n1".to_string()),
            "n0 top-2 should include n1"
        );
        assert!(
            n0_out.contains(&"n2".to_string()),
            "n0 top-2 should include n2"
        );
        assert_eq!(
            n0_out.len(),
            2,
            "n0 should have exactly 2 derived edges (k=2)"
        );

        // Snapshot mid-stream.
        db.snapshot().unwrap();

        // Phase 1b post-snapshot WAL write: year=25.0 is out of tolerance=10
        // for all phase-1 nodes (max year=9), so this does not evict any
        // existing top-k edge — tests that provenance survives unchanged.
        let props = vec![("year".into(), Value::Float(25.0))];
        db.insert_node("N", "nfar", props.clone()).unwrap();
        ref_db.insert_node("N", "nfar", props).unwrap();
    }

    // Reopen from snapshot + WAL.
    let mut snap_db = GraphDb::open(&dir).unwrap();

    // Top-k edges for n0 must survive the reopen unchanged (nfar is out of
    // tolerance so it did not evict n1 or n2).
    let n0_out_after: Vec<String> = snap_db
        .neighbors("n0", "NEAR2", Direction::Out)
        .unwrap_or_default();
    assert_eq!(n0_out_after.len(), 2, "n0 top-2 must survive round-trip");
    assert!(
        n0_out_after.contains(&"n1".to_string()),
        "n0→n1 must survive round-trip"
    );
    assert!(
        n0_out_after.contains(&"n2".to_string()),
        "n0→n2 must survive round-trip (nfar is out of tolerance)"
    );

    // Phase 2 ops: add nodes that will trigger evictions to prove incremental
    // top-k works correctly after reopen.
    // n05 at year=0.5 (score 0.95 for n0) evicts n2 (score 0.8) from n0's top-2.
    let phase2_nodes = [("n05", 0.5f64), ("n06", 0.6)];
    for (key, year) in phase2_nodes {
        let props = vec![("year".into(), Value::Float(year))];
        snap_db.insert_node("N", key, props.clone()).unwrap();
        ref_db.insert_node("N", key, props).unwrap();
    }

    // Final comparison: snap_db edge set must equal ref_db edge set for NEAR2.
    let all_keys = ["n0", "n1", "n2", "n5", "n9", "nfar", "n05", "n06"];
    for key in all_keys {
        let snap_out: std::collections::BTreeSet<String> = snap_db
            .neighbors(key, "NEAR2", Direction::Out)
            .unwrap_or_default()
            .into_iter()
            .collect();
        let ref_out: std::collections::BTreeSet<String> = ref_db
            .neighbors(key, "NEAR2", Direction::Out)
            .unwrap_or_default()
            .into_iter()
            .collect();
        assert_eq!(
            snap_out, ref_out,
            "NEAR2 out-neighbors of {key}: snap={snap_out:?} ref={ref_out:?}"
        );
        // Each node should have at most k=2 derived edges.
        assert!(
            snap_out.len() <= 2,
            "top-k=2 violated: {key} has {} out-neighbors",
            snap_out.len()
        );
    }

    // Incremental firing still works: adding a new node triggers correct top-k.
    snap_db
        .insert_node("N", "n0b", vec![("year".into(), Value::Float(0.1))])
        .unwrap();
    let n0b_out: Vec<String> = snap_db
        .neighbors("n0b", "NEAR2", Direction::Out)
        .unwrap_or_default();
    // n0b at year=0.1 has many close neighbours (n0, n05, n06, n1, n2…).
    // Exact top-2 depends on score ordering; just verify the cap is enforced.
    assert_eq!(
        n0b_out.len(),
        2,
        "n0b top-2 should have exactly 2 out-edges after reopen"
    );
    // Most important: rule ownership is enforced after round-trip.
    // n05 (year=0.5) and n06 (year=0.6) are extremely close (score≈0.99) —
    // n05→n06 is guaranteed to be in n05's top-2 regardless of other candidates.
    assert!(
        matches!(
            snap_db.insert_edge("NEAR2", "n05", "n06"),
            Err(GraphError::RuleOwned { .. })
        ),
        "provenance must be retained after round-trip: insert of derived edge should be RuleOwned"
    );
}

// ---------------------------------------------------------------------------
// V6 container tests (zstd-compressed snapshots)
// ---------------------------------------------------------------------------

/// Golden V5 fixture pin: the pre-V6 snapshot byte sequence must remain readable.
/// Fixture was generated from base commit c3bffd1 with encode() writing VERSION=5.
/// Contains 2 nodes ("a", "b"), 1 edge ("E", a→b), prop v=42 on "a".
#[test]
fn golden_v5_pin() {
    let snap_bytes = include_bytes!("fixtures/golden_v5.bin");
    // Verify magic and version header.
    assert_eq!(
        &snap_bytes[0..4],
        b"GDB1",
        "V5 fixture must start with GDB1 magic"
    );
    assert_eq!(
        u16::from_le_bytes([snap_bytes[4], snap_bytes[5]]),
        5,
        "V5 fixture version field must be 5"
    );
    // Load into a real temp dir and open.
    let dir = tmp("golden-v5-pin");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("snapshot.bin"), snap_bytes).unwrap();
    // WAL must exist (may be empty) for open to succeed.
    std::fs::write(dir.join("wal.bin"), b"").unwrap();
    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(db.node_count(), 2, "V5 fixture must decode to 2 nodes");
    assert_eq!(db.edge_count(), 1, "V5 fixture must decode to 1 edge");
    assert_eq!(
        db.get_prop("a", "v"),
        Some(&Value::Int(42)),
        "V5 fixture must preserve prop v=42 on node 'a'"
    );
}

/// Golden V6 fixture pin: snapshot() now writes VERSION=6 (zstd-compressed).
/// Fixture generated after V6 implementation; decoding it verifies the wire format
/// is stable — future changes that silently alter byte output will fail this test.
///
/// To regenerate: `cargo run --example gen_v6_fixture` from workspace root.
#[test]
fn golden_v6_pin() {
    let snap_bytes = include_bytes!("fixtures/golden_v6.bin");
    // Verify magic and version header.
    assert_eq!(
        &snap_bytes[0..4],
        b"GDB1",
        "V6 fixture must start with GDB1 magic"
    );
    assert_eq!(
        u16::from_le_bytes([snap_bytes[4], snap_bytes[5]]),
        6,
        "V6 fixture version field must be 6"
    );
    // Load into a real temp dir and open.
    let dir = tmp("golden-v6-pin");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("snapshot.bin"), snap_bytes).unwrap();
    std::fs::write(dir.join("wal.bin"), b"").unwrap();
    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(db.node_count(), 2, "V6 fixture must decode to 2 nodes");
    assert_eq!(db.edge_count(), 1, "V6 fixture must decode to 1 edge");
    assert_eq!(
        db.get_prop("a", "v"),
        Some(&Value::Int(42)),
        "V6 fixture must preserve prop v=42 on node 'a'"
    );
}

/// V5 torn-file rejection: every truncation of the committed golden_v5.bin fixture
/// must return a Corrupt error.  This pins the V5 backward-compat decode path
/// (`decode_v5`) independently of the current encoder (which writes V6).  The
/// untruncated fixture must still open successfully.
#[test]
fn v5_torn_file_is_rejected() {
    let good = include_bytes!("fixtures/golden_v5.bin");
    let n = good.len();
    assert!(n > 10, "golden_v5.bin must be non-trivial; got {n} bytes");

    // Untruncated fixture must open cleanly (backward-compat is intact).
    let dir = tmp("v5-torn-good");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("snapshot.bin"), good).unwrap();
    std::fs::write(dir.join("wal.bin"), b"").unwrap();
    assert!(
        GraphDb::open(&dir).is_ok(),
        "untruncated golden_v5.bin must open without error"
    );

    // Truncation sweep: mid-header (3B), version-only (5B), crc-partial (8B),
    // just-past-header (9B), early payload (n/3), mid-payload (n/2),
    // late payload (n*3/4, n-3), one byte short (n-1).
    for &trunc in &[
        3usize,
        5,
        8,
        9,
        n / 3,
        n / 2,
        n * 3 / 4,
        n.saturating_sub(3),
        n - 1,
    ] {
        if trunc == 0 || trunc >= n {
            continue;
        }
        std::fs::write(dir.join("snapshot.bin"), &good[..trunc]).unwrap();
        assert!(
            GraphDb::open(&dir).is_err(),
            "expected Err for {trunc}-byte truncation of {n}-byte golden V5 fixture"
        );
    }
}

/// V6 round-trip: snapshot now writes V6; reopen recovers the full state.
#[test]
fn v6_snapshot_roundtrip() {
    let dir = tmp("snap-v6-rt");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("N", "a", vec![("v".into(), Value::Int(7))])
            .unwrap();
        db.insert_node("N", "b", vec![]).unwrap();
        db.insert_edge("E", "a", "b").unwrap();
        db.snapshot().unwrap();
        // Post-snapshot WAL tail.
        db.insert_node("N", "c", vec![]).unwrap();
    }
    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(db.node_count(), 3);
    assert_eq!(db.edge_count(), 1);
    assert_eq!(db.get_prop("a", "v"), Some(&Value::Int(7)));
    // Verify the snapshot file actually uses V6 container.
    let snap = std::fs::read(dir.join("snapshot.bin")).unwrap();
    assert_eq!(&snap[0..4], b"GDB1");
    assert_eq!(u16::from_le_bytes([snap[4], snap[5]]), 6);
}

/// V4-refuse is unchanged by V6: a V4-stamped snapshot must still be rejected.
#[test]
fn v4_refuse_unchanged_after_v6() {
    let dir = tmp("snap-v6-v4-refuse");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("N", "a", vec![]).unwrap();
        db.snapshot().unwrap(); // writes V6
    }
    let path = dir.join("snapshot.bin");
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[4] = 4;
    bytes[5] = 0; // stamp VERSION=4
    std::fs::write(&path, bytes).unwrap();
    match GraphDb::open(&dir) {
        Err(GraphError::Corrupt { detail }) => {
            assert!(
                detail.contains("version 4"),
                "error should mention version 4; got: {detail}"
            );
        }
        Ok(_) => panic!("expected Corrupt for V4 snapshot, got Ok"),
        Err(e) => panic!("expected Corrupt for V4 snapshot, got: {e:?}"),
    }
}

/// Torn-write safety for V6: truncating a V6 snapshot to any byte count must
/// always return Corrupt, never open silently.  Covers mid-header (bad magic or
/// version), post-header (truncated compressed stream), and mid-payload offsets.
#[test]
fn v6_torn_snapshot_write_is_rejected() {
    let dir = tmp("snap-v6-torn");
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
        // Add a view so the snapshot payload covers the view_defs region.
        db.create_view(ViewDef {
            name: "deg_rel".into(),
            label: "A".into(),
            view_prop: "deg_rel".into(),
            source: ViewSource::Degree {
                edge_type: "REL".into(),
                direction: Direction::Out,
            },
        })
        .unwrap();
        db.snapshot().unwrap(); // writes V6
    }

    let path = dir.join("snapshot.bin");
    let good = std::fs::read(&path).unwrap();
    let n = good.len();
    assert!(n > 20, "V6 snapshot must be non-trivial; got {n} bytes");

    // Truncation points: mid-header (3B), version-only (5B), just-past-header (7B),
    // early compressed stream (n/3), mid-stream (n/2), late compressed stream
    // (n*2/3, n*3/4, n-5), one byte short (n-1).
    // Any truncation must yield Err — never a silently-wrong open.
    for &trunc in &[
        3usize,
        5,
        7,
        n / 3,
        n / 2,
        n * 2 / 3,
        n * 3 / 4,
        n.saturating_sub(5),
        n - 1,
    ] {
        if trunc == 0 || trunc >= n {
            continue;
        }
        std::fs::write(&path, &good[..trunc]).unwrap();
        assert!(
            GraphDb::open(&dir).is_err(),
            "expected Err for {trunc}-byte truncation of {n}-byte V6 snapshot",
        );
    }
}

// ---------------------------------------------------------------------------
// keep_wal tests
// ---------------------------------------------------------------------------

/// keep_wal=true reopen equivalence: a database that took a keep_wal snapshot and
/// continued writing must produce the same state as a reference db that was never
/// closed.  This verifies that snapshot-over-WAL replay is idempotent and that
/// post-snapshot commits survive.
#[test]
fn keep_wal_reopen_equivalence() {
    let dir = tmp("keep-wal-equiv");
    let ref_dir = tmp("keep-wal-equiv-ref");

    // Build reference db — never snapshot, receives all ops.
    let mut ref_db = GraphDb::open(&ref_dir).unwrap();
    ref_db
        .insert_node("N", "a", vec![("v".into(), Value::Int(1))])
        .unwrap();
    ref_db.insert_node("N", "b", vec![]).unwrap();
    ref_db.insert_edge("E", "a", "b").unwrap();

    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("N", "a", vec![("v".into(), Value::Int(1))])
            .unwrap();
        db.insert_node("N", "b", vec![]).unwrap();
        db.insert_edge("E", "a", "b").unwrap();
        // Snapshot with keep_wal=true — WAL is preserved.
        db.snapshot_with(SnapshotOptions { keep_wal: true })
            .unwrap();
        // Post-snapshot writes, same as reference.
        db.insert_node("N", "c", vec![]).unwrap();
        db.insert_edge("E", "b", "c").unwrap();
    }
    ref_db.insert_node("N", "c", vec![]).unwrap();
    ref_db.insert_edge("E", "b", "c").unwrap();

    // Reopen and compare with reference.
    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(
        db.node_count(),
        ref_db.node_count(),
        "node count must match reference after keep_wal reopen"
    );
    assert_eq!(
        db.edge_count(),
        ref_db.edge_count(),
        "edge count must match reference after keep_wal reopen"
    );
    assert_eq!(
        db.get_prop("a", "v"),
        Some(&Value::Int(1)),
        "prop must survive keep_wal round-trip"
    );
    assert_eq!(
        db.neighbors("b", "E", Direction::Out).unwrap(),
        vec!["c"],
        "edge from b must survive keep_wal round-trip"
    );
}

/// keep_wal=true preserves open_at access to pre-snapshot commits.
///
/// After a keep_wal snapshot, old WAL frames remain on disk.  open_at must
/// be able to reach commits made BEFORE the snapshot — that is the entire point
/// of keep_wal.
#[test]
fn keep_wal_open_at_reaches_pre_snapshot_commits() {
    let dir = tmp("keep-wal-open-at");
    let wal_commit_before_snap;
    {
        let mut db = GraphDb::open(&dir).unwrap();
        // commit 0: insert "a"
        db.insert_node("N", "a", vec![("v".into(), Value::Int(10))])
            .unwrap();
        // commit 1: insert "b"
        db.insert_node("N", "b", vec![]).unwrap();
        // WAL now has 2 frames (commits 0 and 1).
        wal_commit_before_snap = 0; // commit 0 = first WAL frame
                                    // Snapshot with keep_wal=true — commits 0 and 1 stay in WAL.
        db.snapshot_with(SnapshotOptions { keep_wal: true })
            .unwrap();
        // Post-snapshot commits.
        db.insert_node("N", "c", vec![]).unwrap(); // commit 2
        db.insert_node("N", "d", vec![]).unwrap(); // commit 3
    }

    // open_at(0) = WAL replay of just the first frame = only node "a".
    let at0 = GraphDb::open_at(&dir, wal_commit_before_snap).unwrap();
    assert!(at0.has_node("a"), "commit 0 must have node 'a'");
    assert!(!at0.has_node("b"), "commit 0 must not yet have node 'b'");
    assert!(
        !at0.has_node("c"),
        "commit 0 must not yet have node 'c' (post-snapshot)"
    );
    assert_eq!(
        at0.get_prop("a", "v"),
        Some(&Value::Int(10)),
        "prop on 'a' must be correct at commit 0"
    );

    // open_at(1) = WAL replay of first 2 frames = nodes "a" and "b".
    let at1 = GraphDb::open_at(&dir, 1).unwrap();
    assert!(at1.has_node("a"));
    assert!(at1.has_node("b"));
    assert!(!at1.has_node("c"));
}

/// Fulltext baseline no double-log: with keep_wal=true, the WAL is not truncated
/// and no new baseline EnableFulltext records are appended.  The only EnableFulltext
/// records in the WAL are the original ones from the enable_fulltext() calls.
/// Assert the count is exactly the number of enabled declarations (not doubled).
#[test]
fn keep_wal_fulltext_baseline_not_doubled() {
    let dir = tmp("keep-wal-ft-nodbl");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node(
            "N",
            "a",
            vec![("text".into(), Value::Str("hello world".into()))],
        )
        .unwrap();
        db.insert_node(
            "N",
            "b",
            vec![("text".into(), Value::Str("graph storage".into()))],
        )
        .unwrap();
        // Enable fulltext for 1 pair → 1 EnableFulltext record written to WAL.
        db.enable_fulltext("N", "text").unwrap();
        db.snapshot_with(SnapshotOptions { keep_wal: true })
            .unwrap();
    }

    // Read WAL and count EnableFulltext records.
    let wal_bytes = std::fs::read(dir.join("wal.bin")).unwrap();
    let (records, _) = decode_all(&wal_bytes);
    let ft_count = records
        .iter()
        .filter(|r| matches!(r, core_storage::wal::WalRecord::EnableFulltext { .. }))
        .count();

    // Exactly 1 EnableFulltext record — same as the original enable call.
    // If keep_wal erroneously wrote a baseline, this would be 2.
    assert_eq!(
        ft_count, 1,
        "keep_wal=true must not append a baseline: expected 1 EnableFulltext record, got {ft_count}"
    );

    // Reopen must succeed and fulltext still works.
    let db = GraphDb::open(&dir).unwrap();
    let results = db.search("text", "hello");
    assert!(
        results.iter().any(|(k, _)| k == "a"),
        "fulltext must still find 'a' after keep_wal reopen"
    );
}

/// keep_wal=false (the default snapshot()) truncates the WAL to a minimal baseline.
/// After snapshot(), open_at returns CommitOutOfRange because all pre-snapshot commits
/// are gone.
#[test]
fn default_snapshot_truncates_wal_history() {
    let dir = tmp("snap-truncate-hist");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("N", "a", vec![]).unwrap(); // commit 0
        db.insert_node("N", "b", vec![]).unwrap(); // commit 1
                                                   // Default snapshot: truncates WAL.
        db.snapshot().unwrap();
        // Post-snapshot writes.
        db.insert_node("N", "c", vec![]).unwrap(); // new commit 0
    }

    // open_at(1) — these were pre-snapshot commits; WAL is now minimal (just
    // EnableFulltext baseline or empty), so commit 1 no longer exists.
    // CommitOutOfRange means the history was indeed truncated.
    match GraphDb::open_at(&dir, 1) {
        Err(GraphError::CommitOutOfRange { .. }) => {}
        Ok(_) => panic!("expected CommitOutOfRange after default snapshot, got Ok"),
        Err(e) => panic!("expected CommitOutOfRange, got {e:?}"),
    }
}
