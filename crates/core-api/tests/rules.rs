use core_api::{
    wal_commit_count_at, Direction, GraphDb, GraphError, MutationEvent, Predicate,
    PredicateSummary, RuleDef, Value,
};
use core_rules::with_ivf_drift_rebuild;
use core_storage::fs::{FileId, Fs, RealFs};
use core_storage::wal::{decode_all, WalRecord};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("graphdb-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn fk_rule() -> RuleDef {
    RuleDef {
        name: "works_at".into(),
        src_label: "Person".into(),
        dst_label: "Org".into(),
        predicate: Predicate::KeyMatch {
            field: "org_id".into(),
        },
        edge_type: "WORKS_AT".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
    }
}

#[test]
fn rules_fire_on_insert_and_survive_reopen() {
    let dir = tmp("rules");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("Org", "o1", vec![]).unwrap();
        db.create_rule(fk_rule()).unwrap();
        db.insert_node(
            "Person",
            "p1",
            vec![("org_id".into(), Value::Str("o1".into()))],
        )
        .unwrap();
        assert_eq!(
            db.neighbors("p1", "WORKS_AT", Direction::Out).unwrap(),
            vec!["o1"]
        );
        // derived edge is rule-owned
        assert!(matches!(
            db.insert_edge("WORKS_AT", "p1", "o1"),
            Err(GraphError::RuleOwned { .. })
        ));
        assert_eq!(db.rules().len(), 1);
    }
    // replay (no snapshot) must re-derive identical edges
    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(
        db.neighbors("p1", "WORKS_AT", Direction::Out).unwrap(),
        vec!["o1"]
    );
    assert_eq!(db.rules().len(), 1);
}

#[test]
fn prop_update_retracts_and_relinks() {
    let dir = tmp("rules-update");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Org", "o1", vec![]).unwrap();
    db.insert_node("Org", "o2", vec![]).unwrap();
    db.create_rule(fk_rule()).unwrap();
    db.insert_node(
        "Person",
        "p1",
        vec![("org_id".into(), Value::Str("o1".into()))],
    )
    .unwrap();
    db.set_prop("p1", "org_id", Value::Str("o2".into()))
        .unwrap();
    assert_eq!(
        db.neighbors("p1", "WORKS_AT", Direction::Out).unwrap(),
        vec!["o2"]
    );
    assert_eq!(db.edge_count(), 1); // old edge retracted
}

#[test]
fn delete_rule_removes_only_derived_edges_and_bad_rules_rejected() {
    let dir = tmp("rules-delete");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Org", "o1", vec![]).unwrap();
    db.insert_node(
        "Person",
        "p1",
        vec![("org_id".into(), Value::Str("o1".into()))],
    )
    .unwrap();
    db.insert_edge("FRIEND", "p1", "o1").unwrap(); // unrelated user edge
    db.create_rule(fk_rule()).unwrap();
    assert_eq!(db.edge_count(), 2);
    db.delete_rule("works_at").unwrap();
    assert_eq!(db.edge_count(), 1);
    assert!(matches!(
        db.delete_rule("works_at"),
        Err(GraphError::RuleNotFound { .. })
    ));
    let mut bad = fk_rule();
    bad.edge_type = String::new();
    assert!(matches!(
        db.create_rule(bad),
        Err(GraphError::RuleInvalid { .. })
    ));
    assert!(matches!(db.create_rule(fk_rule()), Ok(())));
    assert!(matches!(
        db.create_rule(fk_rule()),
        Err(GraphError::RuleInvalid { .. })
    )); // dup name
}

#[test]
fn derived_edge_state_records_not_wal_logged_markers_are() {
    // State records (InsertEdge / InsertEdgeId) must NOT appear in the WAL for
    // derived edges — rules re-derive them deterministically on replay.
    // History markers (DerivedEdgeAdded) MUST appear so that edge_history and
    // was_linked can surface rule attribution.
    let dir = tmp("rules-walsize");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Org", "o1", vec![]).unwrap();
    db.create_rule(fk_rule()).unwrap();
    let before = std::fs::metadata(dir.join("wal.bin")).unwrap().len();
    db.insert_node(
        "Person",
        "p1",
        vec![("org_id".into(), Value::Str("o1".into()))],
    )
    .unwrap();
    let wal = std::fs::read(dir.join("wal.bin")).unwrap();
    assert!(wal.len() as u64 > before);
    let (recs, _) = core_storage::wal::decode_all(&wal[before as usize..]);

    let all_inner: Vec<&WalRecord> = recs
        .iter()
        .flat_map(|r| match r {
            WalRecord::Batch(inner) => inner.iter().collect::<Vec<_>>(),
            other => vec![other],
        })
        .collect();

    // State records must not be written for derived edges.
    let has_state_edge = all_inner.iter().any(|r| {
        matches!(
            r,
            WalRecord::InsertEdge { .. } | WalRecord::InsertEdgeId { .. }
        )
    });
    assert!(
        !has_state_edge,
        "derived edge state records must not be WAL-logged"
    );

    // History markers must be written.
    let has_marker = all_inner
        .iter()
        .any(|r| matches!(r, WalRecord::DerivedEdgeAdded { .. }));
    assert!(has_marker, "DerivedEdgeAdded marker must be WAL-logged");

    assert_eq!(db.edge_count(), 1); // the derived edge still exists in-memory
}

#[test]
fn explain_reports_rule_provenance_and_weights() {
    let dir = tmp("explain");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node(
        "Org",
        "o1",
        vec![("tags".into(), Value::List(vec![Value::Str("x".into())]))],
    )
    .unwrap();
    db.create_rule(fk_rule()).unwrap();
    db.create_rule(RuleDef {
        name: "shared".into(),
        src_label: "Person".into(),
        dst_label: "Org".into(),
        predicate: Predicate::Overlap {
            field: "tags".into(),
            min: 0.5,
        },
        edge_type: "SIMILAR".into(),
        weight_prop: Some("score".into()),
        max_edges: None,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
    })
    .unwrap();
    db.insert_node(
        "Person",
        "p1",
        vec![
            ("org_id".into(), Value::Str("o1".into())),
            ("tags".into(), Value::List(vec![Value::Str("x".into())])),
        ],
    )
    .unwrap();
    let ex = db.explain("p1", "o1").unwrap();
    assert_eq!(ex.len(), 2);
    assert_eq!(ex[0].rule, "shared");
    assert_eq!(ex[0].weight, Some(1.0));
    assert_eq!(ex[1].rule, "works_at");
    assert_eq!(ex[1].weight, None);
    db.insert_node("Org", "o2", vec![]).unwrap();
    assert!(db.explain("p1", "o2").unwrap().is_empty());
    assert!(matches!(
        db.explain("p1", "ghost"),
        Err(GraphError::KeyNotFound { .. })
    ));
}

#[test]
fn explain_high_degree_hub_returns_only_the_pair() {
    use core_api::{AutoFk, IngestOptions};
    use std::collections::BTreeMap;
    let dir = tmp("explain-hub");
    let mut db = GraphDb::open(&dir).unwrap();
    let opts = IngestOptions {
        key_field: "id".into(),
        auto_fk: AutoFk::Off,
    };
    let mut org = BTreeMap::new();
    org.insert("id".into(), Value::Str("hub".into()));
    db.ingest("Org", vec![org], &opts).unwrap();
    let people: Vec<_> = (0..1000)
        .map(|i| {
            let mut row = BTreeMap::new();
            row.insert("id".into(), Value::Str(format!("p{i}")));
            row.insert("org_id".into(), Value::Str("hub".into()));
            row
        })
        .collect();
    db.ingest("Person", people, &opts).unwrap();
    db.create_rule(fk_rule()).unwrap();
    let ex = db.explain("hub", "p0").unwrap();
    assert_eq!(ex.len(), 1);
    assert_eq!(ex[0].rule, "works_at");
    assert_eq!(ex[0].src_key, "p0");
    assert_eq!(ex[0].dst_key, "hub");
    assert!(db.explain("p0", "p1").unwrap().is_empty());
}

#[test]
fn explain_predicate_summary_key_match_and_all() {
    let dir = tmp("explain-pred");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Org", "o1", vec![("ind".into(), Value::Str("arch".into()))])
        .unwrap();
    db.create_rule(fk_rule()).unwrap();
    db.create_rule(RuleDef {
        name: "both".into(),
        src_label: "Person".into(),
        dst_label: "Org".into(),
        predicate: Predicate::All(vec![
            Predicate::FieldEqual {
                field: "ind".into(),
            },
            Predicate::Overlap {
                field: "tags".into(),
                min: 0.5,
            },
        ]),
        edge_type: "BOTH".into(),
        weight_prop: Some("score".into()),
        max_edges: None,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
    })
    .unwrap();
    db.insert_node(
        "Person",
        "p1",
        vec![
            ("org_id".into(), Value::Str("o1".into())),
            ("ind".into(), Value::Str("arch".into())),
            ("tags".into(), Value::List(vec![Value::Str("x".into())])),
        ],
    )
    .unwrap();
    // Overlap needs tags on both sides
    db.set_prop("o1", "tags", Value::List(vec![Value::Str("x".into())]))
        .unwrap();

    let ex = db.explain("p1", "o1").unwrap();
    let km = ex.iter().find(|e| e.rule == "works_at").unwrap();
    assert_eq!(km.predicate.kind, "key_match");
    assert_eq!(km.predicate.fields, vec!["org_id".to_string()]);
    assert!(km.predicate.parts.is_none());

    let all = ex.iter().find(|e| e.rule == "both").unwrap();
    assert_eq!(all.predicate.kind, "all");
    assert_eq!(
        all.predicate.fields,
        vec!["ind".to_string(), "tags".to_string()]
    );
    let parts = all.predicate.parts.as_ref().expect("all has parts");
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0].kind, "field_equal");
    assert_eq!(parts[0].fields, vec!["ind".to_string()]);
    assert_eq!(parts[1].kind, "overlap");
    assert_eq!(parts[1].fields, vec!["tags".to_string()]);
    assert_eq!(parts[1].min, Some(0.5));
}

#[test]
fn predicate_summary_kind_table() {
    struct Row {
        pred: Predicate,
        kind: &'static str,
        fields: &'static [&'static str],
        min: Option<f64>,
        tolerance: Option<f64>,
        km: Option<f64>,
        n_parts: Option<usize>,
    }
    let cases = [
        Row {
            pred: Predicate::KeyMatch { field: "fk".into() },
            kind: "key_match",
            fields: &["fk"],
            min: None,
            tolerance: None,
            km: None,
            n_parts: None,
        },
        Row {
            pred: Predicate::FieldEqual {
                field: "ind".into(),
            },
            kind: "field_equal",
            fields: &["ind"],
            min: None,
            tolerance: None,
            km: None,
            n_parts: None,
        },
        Row {
            pred: Predicate::Overlap {
                field: "tags".into(),
                min: 0.5,
            },
            kind: "overlap",
            fields: &["tags"],
            min: Some(0.5),
            tolerance: None,
            km: None,
            n_parts: None,
        },
        Row {
            pred: Predicate::All(vec![
                Predicate::FieldEqual {
                    field: "ind".into(),
                },
                Predicate::Overlap {
                    field: "tags".into(),
                    min: 0.4,
                },
            ]),
            kind: "all",
            fields: &["ind", "tags"],
            min: None,
            tolerance: None,
            km: None,
            n_parts: Some(2),
        },
        Row {
            pred: Predicate::NumericWithin {
                field: "year".into(),
                tolerance: 2.0,
            },
            kind: "numeric_within",
            fields: &["year"],
            min: None,
            tolerance: Some(2.0),
            km: None,
            n_parts: None,
        },
        Row {
            pred: Predicate::GeoRadius {
                field: "loc".into(),
                km: 400.0,
            },
            kind: "geo_radius",
            fields: &["loc"],
            min: None,
            tolerance: None,
            km: Some(400.0),
            n_parts: None,
        },
        Row {
            pred: Predicate::VectorSimilar {
                field: "emb".into(),
                min: 0.9,
            },
            kind: "vector_similar",
            fields: &["emb"],
            min: Some(0.9),
            tolerance: None,
            km: None,
            n_parts: None,
        },
    ];
    for row in &cases {
        let s = PredicateSummary::from(&row.pred);
        assert_eq!(s.kind, row.kind, "{}", row.kind);
        assert_eq!(
            s.fields,
            row.fields
                .iter()
                .map(|f| (*f).to_string())
                .collect::<Vec<_>>(),
            "{} fields",
            row.kind
        );
        assert_eq!(s.min, row.min, "{} min", row.kind);
        assert_eq!(s.tolerance, row.tolerance, "{} tolerance", row.kind);
        assert_eq!(s.km, row.km, "{} km", row.kind);
        match row.n_parts {
            None => assert!(s.parts.is_none(), "{} parts", row.kind),
            Some(n) => {
                assert_eq!(
                    s.parts.as_ref().map(Vec::len),
                    Some(n),
                    "{} parts",
                    row.kind
                )
            }
        }
    }
}

fn emb(xs: &[f64]) -> Value {
    Value::List(xs.iter().copied().map(Value::Float).collect())
}

/// All(VectorSimilar, FieldEqual) must Intersect indexes, not ScanAll via parts[0].
/// Extra candidates are allowed; missing a true match is not.
#[test]
fn all_vector_then_field_equal_does_not_scan_all() {
    let dir = tmp("all-vec-fe");
    let mut db = GraphDb::open(&dir).unwrap();
    db.create_rule(RuleDef {
        name: "fit".into(),
        src_label: "Person".into(),
        dst_label: "Org".into(),
        predicate: Predicate::All(vec![
            Predicate::VectorSimilar {
                field: "e".into(),
                min: 0.8,
            },
            Predicate::FieldEqual {
                field: "industry".into(),
            },
        ]),
        edge_type: "FIT".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
    })
    .unwrap();

    db.insert_node(
        "Person",
        "p",
        vec![
            ("e".into(), emb(&[1.0, 0.0])),
            ("industry".into(), Value::Str("tech".into())),
        ],
    )
    .unwrap();
    // Matching industry, cosine 0 < 0.8 → no edge.
    db.insert_node(
        "Org",
        "low_cos",
        vec![
            ("e".into(), emb(&[0.0, 1.0])),
            ("industry".into(), Value::Str("tech".into())),
        ],
    )
    .unwrap();
    // Cosine 1.0, different industry → no edge.
    db.insert_node(
        "Org",
        "wrong_ind",
        vec![
            ("e".into(), emb(&[1.0, 0.0])),
            ("industry".into(), Value::Str("law".into())),
        ],
    )
    .unwrap();
    // Both match → edge.
    db.insert_node(
        "Org",
        "both",
        vec![
            ("e".into(), emb(&[1.0, 0.0])),
            ("industry".into(), Value::Str("tech".into())),
        ],
    )
    .unwrap();

    let out = db.neighbors("p", "FIT", Direction::Out).unwrap();
    assert_eq!(out, vec!["both".to_string()]);
}

fn approx_vec_rule() -> RuleDef {
    RuleDef {
        name: "sim".into(),
        src_label: "V".into(),
        dst_label: "V".into(),
        predicate: Predicate::VectorSimilar {
            field: "emb".into(),
            min: 0.5,
        },
        edge_type: "SIM".into(),
        weight_prop: None,
        max_edges: None,
        approximate: true,
        via_label: None,
        via_edge: None,
        via_dir: None,
    }
}

#[test]
fn approximate_rule_rebuilds_after_drift_threshold() {
    let dir = tmp("approx-drift-rebuild");
    let mut db = GraphDb::open(&dir).unwrap();
    for i in 0..6 {
        let x = i as f64 * 0.2;
        db.insert_node(
            "V",
            &format!("v{i}"),
            vec![("emb".into(), emb(&[x, 1.0 - x]))],
        )
        .unwrap();
    }
    db.create_rule(approx_vec_rule()).unwrap();
    assert_eq!(db.ivf_dst_drift("sim"), Some(0));

    let evs = Arc::new(Mutex::new(Vec::new()));
    let sink = evs.clone();
    db.set_event_sink(Box::new(move |e| sink.lock().unwrap().push(e)));

    with_ivf_drift_rebuild(1, || {
        let before = wal_commit_count_at(&dir).unwrap();
        db.delete_node("v0").unwrap();
        // Each mutation that triggers rule retractions writes an additional
        // DerivedEdgeRetracted history-marker frame (state no-op).
        // first delete: DeleteNode + DerivedEdgeRetracted marker = 2 commits.
        assert_eq!(
            wal_commit_count_at(&dir).unwrap(),
            before + 2,
            "first delete is under threshold; DeleteNode + DerivedEdgeRetracted marker"
        );
        assert_eq!(db.ivf_dst_drift("sim"), Some(1));

        db.delete_node("v1").unwrap();
        // second delete: DeleteNode(1) + DerivedEdgeRetracted marker(1) = 2 more commits for v1.
        //              + RebuildRule(1) = 1 more commit; no rebuild marker because the
        //                streaming rebuild finds the graph already correct (no net edge changes).
        // Total: first_delete(2) + second_delete(2) + rebuild(1) = before + 5.
        assert_eq!(
            wal_commit_count_at(&dir).unwrap(),
            before + 5,
            "second delete trips drift > 1; DeleteNode + marker + RebuildRule (no rebuild marker)"
        );
        assert_eq!(
            db.ivf_dst_drift("sim"),
            Some(0),
            "rebuild_rule resets dst drift"
        );
    });

    let got = evs.lock().unwrap().clone();
    let rebuilt = got
        .iter()
        .filter(|e| matches!(e, MutationEvent::RuleRebuilt { name } if name == "sim"))
        .count();
    assert_eq!(
        rebuilt, 1,
        "exactly one auto RebuildRule, not a retrigger loop; got {got:?}"
    );

    // Explicit RebuildRule must not enqueue another rebuild (rebuild resets drift).
    // No rebuild marker because streaming rebuild detects graph already correct.
    let before = wal_commit_count_at(&dir).unwrap();
    db.rebuild_rule("sim").unwrap();
    assert_eq!(
        wal_commit_count_at(&dir).unwrap(),
        before + 1,
        "RebuildRule must not retrigger another RebuildRule; exactly 1 commit"
    );
    assert_eq!(db.ivf_dst_drift("sim"), Some(0));
}

/// WAL append that fails only `RebuildRule` frames when `fail_rebuild` is set.
struct FailRebuildWal {
    inner: RealFs,
    fail_rebuild: Arc<AtomicBool>,
}

impl Fs for FailRebuildWal {
    fn append(&mut self, file: FileId, data: &[u8]) -> std::io::Result<()> {
        if file == FileId::Wal
            && self.fail_rebuild.load(Ordering::SeqCst)
            && decode_all(data)
                .0
                .iter()
                .any(|r| matches!(r, WalRecord::RebuildRule { .. }))
        {
            return Err(std::io::Error::other("forced RebuildRule wal failure"));
        }
        self.inner.append(file, data)
    }

    fn sync(&mut self, file: FileId) -> std::io::Result<()> {
        self.inner.sync(file)
    }

    fn read(&self, file: FileId) -> std::io::Result<Vec<u8>> {
        self.inner.read(file)
    }

    fn write_atomic(&mut self, file: FileId, data: &[u8]) -> std::io::Result<()> {
        self.inner.write_atomic(file, data)
    }
}

#[test]
fn auto_rebuild_wal_failure_does_not_fail_user_write() {
    let dir = tmp("approx-rebuild-wal-fail");
    let fail_rebuild = Arc::new(AtomicBool::new(false));
    let fs = FailRebuildWal {
        inner: RealFs::new(&dir).unwrap(),
        fail_rebuild: fail_rebuild.clone(),
    };
    let mut db = GraphDb::open_with(fs).unwrap();
    for i in 0..6 {
        let x = i as f64 * 0.2;
        db.insert_node(
            "V",
            &format!("v{i}"),
            vec![("emb".into(), emb(&[x, 1.0 - x]))],
        )
        .unwrap();
    }
    db.create_rule(approx_vec_rule()).unwrap();

    fail_rebuild.store(true, Ordering::SeqCst);
    with_ivf_drift_rebuild(1, || {
        db.delete_node("v0").unwrap();
        let before = wal_commit_count_at(&dir).unwrap();
        db.delete_node("v1")
            .expect("user delete must succeed even if auto-rebuild WAL fails");
        assert!(
            !db.has_node("v1"),
            "user delete is durable when rebuild WAL fails"
        );
        // DeleteNode(1) + DerivedEdgeRetracted marker(1) = 2 commits.
        // RebuildRule was rejected by FailRebuildWal so no rebuild commit.
        assert_eq!(
            wal_commit_count_at(&dir).unwrap(),
            before + 2,
            "RebuildRule must not be committed when its WAL append fails; \
             only DeleteNode + DerivedEdgeRetracted marker"
        );
        assert_eq!(
            db.ivf_dst_drift("sim"),
            Some(2),
            "failed auto-rebuild must leave dst drift in place"
        );
    });

    fail_rebuild.store(false, Ordering::SeqCst);
    db.insert_node("V", "v9", vec![("emb".into(), emb(&[0.1, 0.9]))])
        .unwrap();
    assert_eq!(
        db.ivf_dst_drift("sim"),
        Some(0),
        "re-queued rebuild must run on a later write"
    );
}

/// `find_similar_vector` must rank nodes by cosine similarity even when no
/// rule exists (brute-force fallback path).  Without any edges in the graph
/// the HNSW fast path returns `None` and we fall through to the O(n) scan.
#[test]
fn find_similar_vector_without_edges() {
    let dir = tmp("find-similar-vector-no-edges");
    let mut db = GraphDb::open(&dir).unwrap();

    // Insert three 2-D nodes.  The cosine angles are chosen so:
    //   "a" ≈ [1,0]   →  exactly aligned with query [1,0]      → score 1.0
    //   "b" ≈ [1,1]/√2 → 45° from query                        → score ≈ 0.707
    //   "c" ≈ [0,1]   →  orthogonal to query                    → score 0.0
    db.insert_node("Item", "a", vec![("emb".into(), emb(&[1.0, 0.0]))])
        .unwrap();
    db.insert_node("Item", "b", vec![("emb".into(), emb(&[1.0, 1.0]))])
        .unwrap();
    db.insert_node("Item", "c", vec![("emb".into(), emb(&[0.0, 1.0]))])
        .unwrap();

    let query = vec![1.0_f64, 0.0];

    // k=2, min=0.5: should return "a" and "b" only (c is orthogonal).
    let hits = db.find_similar_vector("emb", "Item", &query, 2, 0.5);
    let keys: Vec<&str> = hits.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(keys, vec!["a", "b"], "top-2 with min=0.5");

    // Scores must be in descending order and within valid range.
    assert!(hits[0].1 > hits[1].1, "sorted descending");
    assert!((hits[0].1 - 1.0).abs() < 1e-9, "a is perfectly aligned");

    // k=3, min=0.0: all three nodes; c appears last with near-zero score.
    let all = db.find_similar_vector("emb", "Item", &query, 3, 0.0);
    assert_eq!(all.len(), 3);
    assert_eq!(all[2].0, "c");
    assert!(all[2].1.abs() < 1e-9, "c is orthogonal");

    // Label filter: only Items (no false positives from other labels).
    let filtered = db.find_similar_vector("emb", "Other", &query, 10, 0.0);
    assert!(filtered.is_empty(), "no Other-label nodes exist");
}

// ---------------------------------------------------------------------------
// Task 2: 3-node via-hop linking rules
// ---------------------------------------------------------------------------

/// Scenario: Person -[WORKS_AT]-> Org, Org.industry == Project.industry → FIT.
/// alice and bob both work at TechCorp (industry=tech).
/// ProjectA has industry=tech (matches); ProjectB has industry=law (no match).
/// Expected: FIT edges from alice→ProjectA and bob→ProjectA only.
#[test]
fn via_hop_rule_fires_only_matching_industry() {
    let dir = tmp("via-hop-basic");
    let mut db = GraphDb::open(&dir).unwrap();

    // Nodes
    db.insert_node(
        "Org",
        "techcorp",
        vec![("industry".into(), Value::Str("tech".into()))],
    )
    .unwrap();
    db.insert_node("Person", "alice", vec![]).unwrap();
    db.insert_node("Person", "bob", vec![]).unwrap();
    db.insert_node(
        "Project",
        "proj_a",
        vec![("industry".into(), Value::Str("tech".into()))],
    )
    .unwrap();
    db.insert_node(
        "Project",
        "proj_b",
        vec![("industry".into(), Value::Str("law".into()))],
    )
    .unwrap();

    // Via edges: person -[WORKS_AT]-> org
    db.insert_edge("WORKS_AT", "alice", "techcorp").unwrap();
    db.insert_edge("WORKS_AT", "bob", "techcorp").unwrap();

    // Via-hop rule: Person -[WORKS_AT/Out]-> Org, FieldEqual(industry), → FIT → Project
    let rule = RuleDef {
        name: "fit".into(),
        src_label: "Person".into(),
        dst_label: "Project".into(),
        predicate: Predicate::FieldEqual {
            field: "industry".into(),
        },
        edge_type: "FIT".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
        via_label: Some("Org".into()),
        via_edge: Some("WORKS_AT".into()),
        via_dir: None, // defaults to Out
    };
    db.create_rule(rule).unwrap();

    // Both alice and bob should have FIT→proj_a, neither should have FIT→proj_b.
    let alice_fit = db.neighbors("alice", "FIT", Direction::Out).unwrap();
    assert_eq!(alice_fit, vec!["proj_a"], "alice fits proj_a (tech)");

    let bob_fit = db.neighbors("bob", "FIT", Direction::Out).unwrap();
    assert_eq!(bob_fit, vec!["proj_a"], "bob fits proj_a (tech)");

    // proj_b (law) must not be linked to anyone via FIT.
    let proj_b_fit = db.neighbors("proj_b", "FIT", Direction::In).unwrap();
    assert!(proj_b_fit.is_empty(), "proj_b (law) gets no FIT edges");
}

/// Validate: via_label and via_edge must both be set or both absent.
#[test]
fn via_hop_validate_rejects_half_set() {
    let dir = tmp("via-hop-validate");
    let mut db = GraphDb::open(&dir).unwrap();

    // Only via_label set — should fail validate
    let bad_label_only = RuleDef {
        name: "r1".into(),
        src_label: "A".into(),
        dst_label: "B".into(),
        predicate: Predicate::FieldEqual { field: "f".into() },
        edge_type: "E".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
        via_label: Some("V".into()),
        via_edge: None,
        via_dir: None,
    };
    assert!(
        db.create_rule(bad_label_only).is_err(),
        "via_label without via_edge must be rejected"
    );

    // Only via_edge set — should fail validate
    let bad_edge_only = RuleDef {
        name: "r2".into(),
        src_label: "A".into(),
        dst_label: "B".into(),
        predicate: Predicate::FieldEqual { field: "f".into() },
        edge_type: "E".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
        via_label: None,
        via_edge: Some("VE".into()),
        via_dir: None,
    };
    assert!(
        db.create_rule(bad_edge_only).is_err(),
        "via_edge without via_label must be rejected"
    );
}

/// Incremental: when a via-edge is inserted after rule creation, the rule fires.
#[test]
fn via_hop_incremental_edge_insert() {
    let dir = tmp("via-hop-edge-insert");
    let mut db = GraphDb::open(&dir).unwrap();

    db.insert_node(
        "Org",
        "techcorp",
        vec![("industry".into(), Value::Str("tech".into()))],
    )
    .unwrap();
    db.insert_node("Person", "alice", vec![]).unwrap();
    db.insert_node(
        "Project",
        "proj_a",
        vec![("industry".into(), Value::Str("tech".into()))],
    )
    .unwrap();

    let rule = RuleDef {
        name: "fit".into(),
        src_label: "Person".into(),
        dst_label: "Project".into(),
        predicate: Predicate::FieldEqual {
            field: "industry".into(),
        },
        edge_type: "FIT".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
        via_label: Some("Org".into()),
        via_edge: Some("WORKS_AT".into()),
        via_dir: None,
    };
    db.create_rule(rule).unwrap();

    // No WORKS_AT edge yet → no FIT edges
    assert!(
        db.neighbors("alice", "FIT", Direction::Out)
            .unwrap()
            .is_empty(),
        "no FIT before WORKS_AT inserted"
    );

    // Insert the via edge → rule should fire
    db.insert_edge("WORKS_AT", "alice", "techcorp").unwrap();

    let fit = db.neighbors("alice", "FIT", Direction::Out).unwrap();
    assert_eq!(fit, vec!["proj_a"], "FIT fires after WORKS_AT inserted");
}

/// Incremental: when via-node property changes, rule re-evaluates.
#[test]
fn via_hop_incremental_via_prop_change() {
    let dir = tmp("via-hop-via-prop");
    let mut db = GraphDb::open(&dir).unwrap();

    db.insert_node(
        "Org",
        "techcorp",
        vec![("industry".into(), Value::Str("tech".into()))],
    )
    .unwrap();
    db.insert_node("Person", "alice", vec![]).unwrap();
    db.insert_node(
        "Project",
        "proj_a",
        vec![("industry".into(), Value::Str("tech".into()))],
    )
    .unwrap();
    db.insert_node(
        "Project",
        "proj_b",
        vec![("industry".into(), Value::Str("law".into()))],
    )
    .unwrap();
    db.insert_edge("WORKS_AT", "alice", "techcorp").unwrap();

    let rule = RuleDef {
        name: "fit".into(),
        src_label: "Person".into(),
        dst_label: "Project".into(),
        predicate: Predicate::FieldEqual {
            field: "industry".into(),
        },
        edge_type: "FIT".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
        via_label: Some("Org".into()),
        via_edge: Some("WORKS_AT".into()),
        via_dir: None,
    };
    db.create_rule(rule).unwrap();

    assert_eq!(
        db.neighbors("alice", "FIT", Direction::Out).unwrap(),
        vec!["proj_a"]
    );

    // Change techcorp industry to law → alice should now fit proj_b, not proj_a
    db.set_prop("techcorp", "industry", Value::Str("law".into()))
        .unwrap();

    let fit = db.neighbors("alice", "FIT", Direction::Out).unwrap();
    assert_eq!(
        fit,
        vec!["proj_b"],
        "FIT updated after via-node prop change"
    );
    let proj_a_fit = db.neighbors("proj_a", "FIT", Direction::In).unwrap();
    assert!(
        proj_a_fit.is_empty(),
        "proj_a FIT retracted after org industry changed"
    );
}

// ---------------------------------------------------------------------------
// Item 4: via_dir = Some(In) path
// ---------------------------------------------------------------------------

/// Verify that via_dir=In reverses the traversal direction: the edge goes
/// via-label → src-label, i.e. src is found by following In-neighbors from
/// the via-label over the via-edge type.
///
/// Setup: Org -[SPONSOR]-> Project (reversed; via_dir=In means "project points to org").
/// Rule: Project src → Org dst, via Org-via via SPONSOR dir=In → FIT.
///
/// Actually a clearer model: Person nodes that are *pointed to* by a Org via
/// EMPLOYS edge.  via_dir=In means Org -[EMPLOYS]-> Person, so Person is found
/// as the In-neighbor of Org.
///
/// Person (src_label) -?-> Project (dst_label), via Org (via_label),
/// via_edge EMPLOYS, via_dir=In means:
///   traverse Person <-[EMPLOYS]- Org   (Org employs Person, so edge Org→Person in In from Person's POV)
///
/// Let's use: the edge Org -[MEMBER_OF]-> Person, via_dir=In.
/// src=Project, dst=Person, via=Org, via_edge=MEMBER_OF, via_dir=In.
/// This means src hops via: find Orgs that have an In-neighbor src (Project ←[MEMBER_OF]− Org).
/// Then match those Orgs' fields against dst's (Person) fields.
///
/// Concrete:
///   Project "proj_x" and Org "org_x" share industry="tech".
///   Person "dev_alice" and Org "org_x" share industry="tech".
///   Edge: Org "org_x" -[MEMBER_OF]-> Project "proj_x" (so proj_x is In-neighbor of org_x w.r.t. MEMBER_OF In).
///   via_dir=In: from src=Project, find orgs via: orgs that have MEMBER_OF Out-neighbors matching src
///              → org_x Out-neighbor includes proj_x → org_x is the via-node.
///   Wait, via_dir=In means the edge direction FROM src TO via is "In", i.e., the edge goes via→src.
///   So the edge is org_x -[MEMBER_OF]-> proj_x, and via_dir=In means "go In to find via from src"
///   meaning follow MEMBER_OF edges that POINT TO proj_x, finding org_x.
///
/// Summary of setup:
///   src_label=Project, dst_label=Person, via_label=Org, via_edge=MEMBER_OF, via_dir=In
///   Edge: org_x -[MEMBER_OF]-> proj_x  (project is In-neighbor of org)
///   Shared field: industry="tech" on Org and Person
///   Expected: proj_x -[FIT]-> dev_alice (project hops in to org_x, matches alice's industry)
#[test]
fn via_hop_via_dir_in() {
    let dir = tmp("via-hop-dir-in");
    let mut db = GraphDb::open(&dir).unwrap();

    db.insert_node(
        "Project",
        "proj_x",
        vec![("industry".into(), Value::Str("tech".into()))],
    )
    .unwrap();
    db.insert_node(
        "Org",
        "org_x",
        vec![("industry".into(), Value::Str("tech".into()))],
    )
    .unwrap();
    db.insert_node(
        "Person",
        "dev_alice",
        vec![("industry".into(), Value::Str("tech".into()))],
    )
    .unwrap();
    db.insert_node(
        "Person",
        "dev_bob",
        vec![("industry".into(), Value::Str("law".into()))],
    )
    .unwrap();

    // Edge goes org_x → proj_x; via_dir=In means from proj_x follow In-direction
    // of MEMBER_OF to find org_x.
    db.insert_edge("MEMBER_OF", "org_x", "proj_x").unwrap();

    let rule = RuleDef {
        name: "project_person_fit".into(),
        src_label: "Project".into(),
        dst_label: "Person".into(),
        predicate: Predicate::FieldEqual {
            field: "industry".into(),
        },
        edge_type: "FIT".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
        via_label: Some("Org".into()),
        via_edge: Some("MEMBER_OF".into()),
        via_dir: Some(core_storage::Direction::In),
    };
    db.create_rule(rule).unwrap();

    // proj_x hops In over MEMBER_OF → org_x (industry=tech) → matches dev_alice (tech).
    let fit = db.neighbors("proj_x", "FIT", Direction::Out).unwrap();
    assert_eq!(
        fit,
        vec!["dev_alice"],
        "via_dir=In rule fires for matching industry"
    );

    // dev_bob (law) must not be linked.
    let bob_fit = db.neighbors("dev_bob", "FIT", Direction::In).unwrap();
    assert!(bob_fit.is_empty(), "dev_bob (law) gets no FIT via proj_x");
}

// ---------------------------------------------------------------------------
// Item 5: Snapshot/WAL-replay roundtrip with a via-hop rule
// ---------------------------------------------------------------------------

/// Derived edges from a via-hop rule must survive both WAL replay and a
/// snapshot+reopen cycle.
#[test]
fn via_hop_survives_snapshot_and_wal_replay() {
    let dir = tmp("via-hop-snapshot");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node(
            "Org",
            "org1",
            vec![("industry".into(), Value::Str("tech".into()))],
        )
        .unwrap();
        db.insert_node("Person", "alice", vec![]).unwrap();
        db.insert_node(
            "Project",
            "proj_a",
            vec![("industry".into(), Value::Str("tech".into()))],
        )
        .unwrap();
        db.insert_edge("WORKS_AT", "alice", "org1").unwrap();

        let rule = RuleDef {
            name: "fit".into(),
            src_label: "Person".into(),
            dst_label: "Project".into(),
            predicate: Predicate::FieldEqual {
                field: "industry".into(),
            },
            edge_type: "FIT".into(),
            weight_prop: None,
            max_edges: None,
            approximate: false,
            via_label: Some("Org".into()),
            via_edge: Some("WORKS_AT".into()),
            via_dir: None,
        };
        db.create_rule(rule).unwrap();

        assert_eq!(
            db.neighbors("alice", "FIT", Direction::Out).unwrap(),
            vec!["proj_a"],
            "FIT edge present before snapshot"
        );

        // Take a snapshot.
        db.snapshot().unwrap();
    }

    // Reopen from snapshot — derived edges must be re-derived.
    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(
        db.neighbors("alice", "FIT", Direction::Out).unwrap(),
        vec!["proj_a"],
        "FIT edge survives snapshot+reopen"
    );
    assert_eq!(db.rules().len(), 1, "rule survives snapshot+reopen");

    // Also drop the in-memory db and replay from WAL only.
    drop(db);
    let db2 = GraphDb::open(&dir).unwrap();
    assert_eq!(
        db2.neighbors("alice", "FIT", Direction::Out).unwrap(),
        vec!["proj_a"],
        "FIT edge survives WAL replay"
    );
}

// ---------------------------------------------------------------------------
// Item 6: New via-label node insert (after rule creation) triggers the as_via branch
// ---------------------------------------------------------------------------

/// When a new node with the via-label is inserted AFTER rule creation, the
/// rule must correctly evaluate it.  This exercises the `on_node_changed_via`
/// code path for a freshly inserted via-label node.
///
/// Because edges must point to existing nodes, the via-edge from the src to
/// the new via-node is inserted immediately after the via-node, exercising
/// the full as_via → as_src incremental chain starting from a post-creation
/// via-node insert.
#[test]
fn via_hop_new_via_node_insert_after_rule_creation() {
    let dir = tmp("via-hop-new-via");
    let mut db = GraphDb::open(&dir).unwrap();

    // Insert src and dst; no via-label node yet.
    db.insert_node("Person", "alice", vec![]).unwrap();
    db.insert_node(
        "Project",
        "proj_a",
        vec![("industry".into(), Value::Str("bio".into()))],
    )
    .unwrap();
    db.insert_node(
        "Project",
        "proj_b",
        vec![("industry".into(), Value::Str("tech".into()))],
    )
    .unwrap();

    let rule = RuleDef {
        name: "fit".into(),
        src_label: "Person".into(),
        dst_label: "Project".into(),
        predicate: Predicate::FieldEqual {
            field: "industry".into(),
        },
        edge_type: "FIT".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
        via_label: Some("Org".into()),
        via_edge: Some("WORKS_AT".into()),
        via_dir: None,
    };
    db.create_rule(rule).unwrap();

    // No Org node yet → backfill finds nothing.
    assert!(
        db.neighbors("alice", "FIT", Direction::Out)
            .unwrap()
            .is_empty(),
        "no FIT before any Org inserted"
    );

    // Now insert a new Org node (via-label) after rule creation.
    db.insert_node(
        "Org",
        "biotech_inc",
        vec![("industry".into(), Value::Str("bio".into()))],
    )
    .unwrap();
    // No WORKS_AT edge yet → as_via fires for biotech_inc but finds no alice-edge.
    assert!(
        db.neighbors("alice", "FIT", Direction::Out)
            .unwrap()
            .is_empty(),
        "no FIT after Org insert without WORKS_AT edge"
    );

    // Insert the via edge → rule fires (as_src for alice).
    db.insert_edge("WORKS_AT", "alice", "biotech_inc").unwrap();
    let fit = db.neighbors("alice", "FIT", Direction::Out).unwrap();
    assert_eq!(
        fit,
        vec!["proj_a"],
        "FIT fires after WORKS_AT to newly inserted Org"
    );

    // Confirm proj_b (tech) is not linked — no Org with tech industry.
    assert!(
        db.neighbors("proj_b", "FIT", Direction::In)
            .unwrap()
            .is_empty(),
        "proj_b (tech) gets no FIT"
    );
}
