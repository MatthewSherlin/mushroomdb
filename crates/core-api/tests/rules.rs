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
    // works_at stores no weight_prop; explain recomputes the KeyMatch score.
    assert_eq!(ex[1].weight, Some(1.0));
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
    let hits = db.find_similar_vector("emb", Some("Item"), &query, 2, 0.5);
    let keys: Vec<&str> = hits.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(keys, vec!["a", "b"], "top-2 with min=0.5");

    // Scores must be in descending order and within valid range.
    assert!(hits[0].1 > hits[1].1, "sorted descending");
    assert!((hits[0].1 - 1.0).abs() < 1e-9, "a is perfectly aligned");

    // k=3, min=0.0: all three nodes; c appears last with near-zero score.
    let all = db.find_similar_vector("emb", Some("Item"), &query, 3, 0.0);
    assert_eq!(all.len(), 3);
    assert_eq!(all[2].0, "c");
    assert!(all[2].1.abs() < 1e-9, "c is orthogonal");

    // Label filter: only Items (no false positives from other labels).
    let filtered = db.find_similar_vector("emb", Some("Other"), &query, 10, 0.0);
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

/// A list-valued FK field ingested from JSON: one edge per element that names a
/// live node, retracted per element when the element goes away.
#[test]
fn ingest_json_list_fk_plus_keymatch_rule_yields_edges() {
    use core_api::{AutoFk, EdgeEvent, IngestOptions};
    let dir = tmp("rules-list-fk");
    let mut db = GraphDb::open(&dir).unwrap();
    let opts = IngestOptions {
        key_field: "id".into(),
        auto_fk: AutoFk::Off,
    };
    db.ingest_json(
        "Mod",
        r#"[{"id": "a.rs"}, {"id": "b.rs"}, {"id": "c.rs"}]"#,
        &opts,
    )
    .unwrap();
    db.create_rule(RuleDef {
        name: "imports".into(),
        src_label: "File".into(),
        dst_label: "Mod".into(),
        predicate: Predicate::KeyMatch {
            field: "imports".into(),
        },
        edge_type: "IMPORTS".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
    })
    .unwrap();
    // "ghost.rs" names no node; the other two do.
    db.ingest_json(
        "File",
        r#"[{"id": "main.rs", "imports": ["a.rs", "ghost.rs", "b.rs"]}]"#,
        &opts,
    )
    .unwrap();

    assert_eq!(
        db.neighbors("main.rs", "IMPORTS", Direction::Out).unwrap(),
        vec!["a.rs", "b.rs"],
        "one edge per list element that names a live Mod"
    );

    // Drop "b.rs", add "c.rs": one prop write retracts one edge and fires one.
    db.set_prop(
        "main.rs",
        "imports",
        Value::List(vec![Value::Str("a.rs".into()), Value::Str("c.rs".into())]),
    )
    .unwrap();
    assert_eq!(
        db.neighbors("main.rs", "IMPORTS", Direction::Out).unwrap(),
        vec!["a.rs", "c.rs"]
    );

    let hist = db.edge_history("main.rs", "b.rs").unwrap();
    assert_eq!(hist.items.len(), 2, "b.rs history: {:?}", hist.items);
    assert_eq!(hist.items[0].event, EdgeEvent::Added);
    assert_eq!(hist.items[1].event, EdgeEvent::Retracted);
    assert_eq!(
        hist.items[1].rule,
        Some("imports".to_string()),
        "per-element retraction carries rule attribution"
    );

    // The removed element's retraction and the added element's firing are one
    // transaction: both land in the commit the single set_prop produced.
    let hist_c = db.edge_history("main.rs", "c.rs").unwrap();
    assert_eq!(hist_c.items.len(), 1, "c.rs history: {:?}", hist_c.items);
    assert_eq!(hist_c.items[0].event, EdgeEvent::Added);
    assert_eq!(
        hist.items[1].commit, hist_c.items[0].commit,
        "retracting b.rs and firing c.rs share one commit"
    );

    // The element present in both lists never churns: one Added, no retraction.
    let hist_a = db.edge_history("main.rs", "a.rs").unwrap();
    assert_eq!(hist_a.items.len(), 1, "a.rs history: {:?}", hist_a.items);
    assert_eq!(hist_a.items[0].event, EdgeEvent::Added);

    // Derived edges are not WAL-logged: replay must re-derive the same set.
    drop(db);
    let mut db = GraphDb::open(&dir).unwrap();
    assert_eq!(
        db.neighbors("main.rs", "IMPORTS", Direction::Out).unwrap(),
        vec!["a.rs", "c.rs"],
        "list-derived edges survive replay"
    );

    // Same over a snapshot base, where the candidate index is built lazily on
    // the first mutation rather than at open time.
    db.snapshot().unwrap();
    drop(db);
    let mut db = GraphDb::open(&dir).unwrap();
    assert_eq!(
        db.neighbors("main.rs", "IMPORTS", Direction::Out).unwrap(),
        vec!["a.rs", "c.rs"],
        "list-derived edges survive a snapshot"
    );
    db.set_prop(
        "main.rs",
        "imports",
        Value::List(vec![Value::Str("a.rs".into()), Value::Str("b.rs".into())]),
    )
    .unwrap();
    assert_eq!(
        db.neighbors("main.rs", "IMPORTS", Direction::Out).unwrap(),
        vec!["a.rs", "b.rs"],
        "element swap after a snapshot re-links through the rebuilt index"
    );
}

/// `explain` on a list-derived edge reports the rule's `KeyMatch { field }`
/// predicate exactly as it does for a scalar FK.
#[test]
fn explain_reports_keymatch_for_list_edge() {
    let dir = tmp("rules-list-explain");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Mod", "a.rs", vec![]).unwrap();
    db.create_rule(RuleDef {
        name: "imports".into(),
        src_label: "File".into(),
        dst_label: "Mod".into(),
        predicate: Predicate::KeyMatch {
            field: "imports".into(),
        },
        edge_type: "IMPORTS".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
    })
    .unwrap();
    db.insert_node(
        "File",
        "main.rs",
        vec![(
            "imports".into(),
            Value::List(vec![Value::Str("a.rs".into())]),
        )],
    )
    .unwrap();

    let ex = db.explain("main.rs", "a.rs").unwrap();
    assert_eq!(ex.len(), 1);
    assert_eq!(ex[0].rule, "imports");
    assert_eq!(ex[0].edge_type, "IMPORTS");
    assert_eq!(
        ex[0].predicate,
        PredicateSummary::from(&Predicate::KeyMatch {
            field: "imports".into()
        }),
        "explain reports KeyMatch for a list-derived edge"
    );
    // The rule stores no weight_prop, so explain recomputes the predicate
    // score — 1.0, the same as a scalar KeyMatch.
    assert_eq!(ex[0].weight, Some(1.0));
}

/// The capped apply path — `max_edges: Some(512)` is what every front door
/// (HTTP, MCP, Python, auto-FK) stores for a KeyMatch rule, and it routes
/// through `filter_src_top_k` + `apply_per_src_top_k` rather than the uncapped
/// branch the other list tests exercise.
#[test]
fn list_fk_fires_per_element_at_the_default_cap() {
    use core_api::{EdgeEvent, DEFAULT_KEYMATCH_TOP_K};
    let dir = tmp("rules-list-default-cap");
    let mut db = GraphDb::open(&dir).unwrap();
    for k in ["a.rs", "b.rs", "c.rs"] {
        db.insert_node("Mod", k, vec![]).unwrap();
    }
    db.create_rule(RuleDef {
        name: "imports".into(),
        src_label: "File".into(),
        dst_label: "Mod".into(),
        predicate: Predicate::KeyMatch {
            field: "imports".into(),
        },
        edge_type: "IMPORTS".into(),
        weight_prop: None,
        max_edges: Some(DEFAULT_KEYMATCH_TOP_K),
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
    })
    .unwrap();
    db.insert_node(
        "File",
        "main.rs",
        vec![(
            "imports".into(),
            Value::List(vec![
                Value::Str("a.rs".into()),
                Value::Str("b.rs".into()),
                Value::Str("c.rs".into()),
            ]),
        )],
    )
    .unwrap();
    assert_eq!(
        db.neighbors("main.rs", "IMPORTS", Direction::Out).unwrap(),
        vec!["a.rs", "b.rs", "c.rs"],
        "the stored default cap fires on every element, not just the first"
    );

    // Per-element retraction through the top-k apply path.
    db.set_prop(
        "main.rs",
        "imports",
        Value::List(vec![Value::Str("a.rs".into()), Value::Str("c.rs".into())]),
    )
    .unwrap();
    assert_eq!(
        db.neighbors("main.rs", "IMPORTS", Direction::Out).unwrap(),
        vec!["a.rs", "c.rs"]
    );
    let hist = db.edge_history("main.rs", "b.rs").unwrap();
    assert_eq!(hist.items.len(), 2, "b.rs history: {:?}", hist.items);
    assert_eq!(hist.items[1].event, EdgeEvent::Retracted);
    let hist_a = db.edge_history("main.rs", "a.rs").unwrap();
    assert_eq!(hist_a.items.len(), 1, "surviving element does not churn");
}

/// Under a cap smaller than the number of live targets, which elements win is
/// decided by **destination key ascending**, not by the list's stored order:
/// every list element scores 1.0, so `filter_src_top_k`'s score-DESC sort is a
/// tie and its key-ASC tiebreak decides.
#[test]
fn list_fk_under_a_small_cap_keeps_the_lowest_destination_keys() {
    let dir = tmp("rules-list-small-cap");
    let mut db = GraphDb::open(&dir).unwrap();
    for k in ["a.rs", "b.rs", "c.rs"] {
        db.insert_node("Mod", k, vec![]).unwrap();
    }
    db.create_rule(RuleDef {
        name: "imports".into(),
        src_label: "File".into(),
        dst_label: "Mod".into(),
        predicate: Predicate::KeyMatch {
            field: "imports".into(),
        },
        edge_type: "IMPORTS".into(),
        weight_prop: None,
        max_edges: Some(2),
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
    })
    .unwrap();
    // Stored in descending key order, so stored order and key order disagree.
    db.insert_node(
        "File",
        "main.rs",
        vec![(
            "imports".into(),
            Value::List(vec![
                Value::Str("c.rs".into()),
                Value::Str("b.rs".into()),
                Value::Str("a.rs".into()),
            ]),
        )],
    )
    .unwrap();
    assert_eq!(
        db.neighbors("main.rs", "IMPORTS", Direction::Out).unwrap(),
        vec!["a.rs", "b.rs"],
        "cap of 2 over 3 targets keeps the two lowest destination keys, \
         not the two first-listed elements"
    );
}

// ── `Any([KeyMatch, …])` on a plain rule ─────────────────────────────────────
//
// `KeyMatch` candidates are resolved by id lookup, so `CandidateSpec::ByKey`
// offers the candidate index nothing to probe. The FK fast path covers a
// predicate rooted at `KeyMatch`; one held under `Any` is covered by neither,
// so narrowing through the index would consider only the destinations the other
// branch reaches. Both branches must derive.

/// `Person.friend_id` names one destination; `Person.city` matches another by
/// equality. Neither destination is reachable through the other's branch.
fn seed_any_keymatch(db: &mut GraphDb<RealFs>) {
    db.insert_node("Person", "alice", vec![]).unwrap();
    db.insert_node("Person", "bob", vec![]).unwrap();
    db.insert_node("Person", "carol", vec![]).unwrap();
    db.create_rule(RuleDef {
        name: "linked".into(),
        src_label: "Person".into(),
        dst_label: "Person".into(),
        predicate: Predicate::Any(vec![
            Predicate::KeyMatch {
                field: "friend_id".into(),
            },
            Predicate::FieldEqual {
                field: "city".into(),
            },
        ]),
        edge_type: "LINKED".into(),
        weight_prop: None,
        max_edges: Some(10),
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
    })
    .unwrap();
    // bob is named by alice's FK and shares no city; carol shares alice's city
    // and is named by nothing.
    db.set_prop("carol", "city", Value::Str("berlin".into()))
        .unwrap();
    db.set_prop("bob", "city", Value::Str("lisbon".into()))
        .unwrap();
}

fn linked(db: &GraphDb<RealFs>, key: &str) -> Vec<String> {
    let mut v = db.neighbors(key, "LINKED", Direction::Out).unwrap();
    v.sort();
    v
}

#[test]
fn any_of_keymatch_and_field_equal_derives_both_branches() {
    let dir = tmp("any-keymatch-src");
    let mut db = GraphDb::open(&dir).unwrap();
    seed_any_keymatch(&mut db);

    // One write on the source, carrying both branches at once.
    db.set_prop("alice", "friend_id", Value::Str("bob".into()))
        .unwrap();
    db.set_prop("alice", "city", Value::Str("berlin".into()))
        .unwrap();
    assert_eq!(
        linked(&db, "alice"),
        vec!["bob", "carol"],
        "bob is reachable only through the KeyMatch branch and must be derived"
    );

    // A rebuild is a full recompute through the same candidate path.
    db.rebuild_rule("linked").unwrap();
    assert_eq!(linked(&db, "alice"), vec!["bob", "carol"], "rebuild");

    // Retracting the FK must cost the source only that one destination.
    db.set_prop("alice", "friend_id", Value::Str("nobody".into()))
        .unwrap();
    assert_eq!(linked(&db, "alice"), vec!["carol"]);
}

#[test]
fn any_of_keymatch_derives_when_the_destination_is_the_one_written() {
    // The dst-side probe: the write lands on the node the FK names, so the rule
    // has to find the *source* that points at it.
    let dir = tmp("any-keymatch-dst");
    let mut db = GraphDb::open(&dir).unwrap();
    seed_any_keymatch(&mut db);
    db.set_prop("alice", "friend_id", Value::Str("dave".into()))
        .unwrap();
    assert_eq!(linked(&db, "alice"), Vec::<String>::new());

    // dave appears later and shares no city with anyone.
    db.insert_node(
        "Person",
        "dave",
        vec![("city".into(), Value::Str("oslo".into()))],
    )
    .unwrap();
    assert_eq!(
        linked(&db, "alice"),
        vec!["dave"],
        "inserting the named destination must derive the KeyMatch branch"
    );
}

// ---------------------------------------------------------------------------
// Opening a store adopts the persisted vector index instead of rebuilding it
// ---------------------------------------------------------------------------

/// A deterministic 8-d unit vector for `i`, spread over the sphere by
/// splitmix64 so no two are near-duplicates.
fn unit_vec_8_raw(i: u32) -> Vec<f64> {
    let mut s = 0x9E37_79B9_7F4A_7C15u64 ^ (i as u64).wrapping_mul(0xD6E8_FEB8_6659_FD93);
    let mut out = Vec::with_capacity(8);
    for _ in 0..8 {
        s = s.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut x = s;
        x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        x ^= x >> 31;
        out.push((x as i64 as f64) / (i64::MAX as f64));
    }
    let n = out.iter().map(|x| x * x).sum::<f64>().sqrt();
    out.iter().map(|x| x / n).collect()
}

fn unit_vec_8(i: u32) -> Value {
    emb(&unit_vec_8_raw(i))
}

fn tight_vec_rule() -> RuleDef {
    RuleDef {
        name: "sim".into(),
        src_label: "V".into(),
        dst_label: "V".into(),
        predicate: Predicate::VectorSimilar {
            field: "emb".into(),
            min: 0.9,
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

/// Opening a store must adopt the persisted vector index, not rebuild one and
/// throw it away. Before 0.6.5/0.6.6 the open scan built the whole graph and
/// `load_hnsw_state` replaced it.
///
/// The rule's src and dst labels are both `V`, so `index_node_for_rule` offers
/// each node to *both* sides: one node is two `HnswIndex::insert` calls.
#[test]
fn reopening_does_not_rebuild_the_vector_index() {
    const N: u32 = 400;
    let dir = tmp("no-rebuild-on-open");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        for i in 0..N {
            db.insert_node("V", &format!("v{i}"), vec![("emb".into(), unit_vec_8(i))])
                .unwrap();
        }
        db.create_rule(tight_vec_rule()).unwrap();
        db.snapshot().unwrap();
    }

    // Clean open (the snapshot truncated the WAL): `ensure_indexes_populated`
    // is the path, tripped by the first mutation.
    core_rules::hnsw_insert_count_reset();
    let mut db = GraphDb::open(&dir).unwrap();
    assert!(
        !db.find_similar_vector("emb", Some("V"), &unit_vec_8_raw(7), 3, 0.0)
            .is_empty(),
        "force the index path so the count is meaningful"
    );
    db.insert_node("Other", "o", vec![("v".into(), Value::Int(1))])
        .unwrap();
    assert_eq!(
        core_rules::hnsw_insert_count(),
        0,
        "opening a store must not insert a single vector into the HNSW graph"
    );
    drop(db);

    // WAL-present open: `consume_retained_state_eager` is the path, and the one
    // post-snapshot node arrives through replay.
    {
        let mut w = GraphDb::open(&dir).unwrap();
        w.insert_node("V", "extra", vec![("emb".into(), unit_vec_8(9_999))])
            .unwrap();
    }
    core_rules::hnsw_insert_count_reset();
    let db = GraphDb::open(&dir).unwrap();
    assert!(db.has_node("extra"));
    assert_eq!(
        core_rules::hnsw_insert_count(),
        2,
        "only the post-snapshot node is inserted (once per rule side); the other \
         {N} are adopted"
    );
}

// ---------------------------------------------------------------------------
// Sliced HNSW builds (v0.6.6 §4)
// ---------------------------------------------------------------------------

/// Dimensionality of the sliced-build fixture. One axis per cluster, so
/// clusters are mutually orthogonal and the rule's edge set is exactly the
/// within-cluster pairs.
const SLICE_DIM: usize = 32;

/// Node `i` sits in cluster `i / 10`, on that cluster's axis, nudged along the
/// next axis so no two vectors are literally equal. Cosine within a cluster is
/// ~1.0 and between clusters ~0.0, which puts the 0.9 threshold nowhere near a
/// tie — the derived edge set is the same set whichever path builds the graph.
fn slice_vec(i: usize) -> Value {
    let axis = (i / 10) % SLICE_DIM;
    let mut xs = vec![0.0f64; SLICE_DIM];
    xs[axis] = 1.0;
    xs[(axis + 1) % SLICE_DIM] = (i % 10) as f64 * 0.001;
    emb(&xs)
}

fn store_with_vectors(name: &str, n: usize) -> (std::path::PathBuf, GraphDb<RealFs>) {
    let dir = tmp(name);
    let mut db = GraphDb::open(&dir).unwrap();
    for i in 0..n {
        db.insert_node("V", &format!("v{i}"), vec![("emb".into(), slice_vec(i))])
            .unwrap();
    }
    (dir, db)
}

fn slice_rule() -> RuleDef {
    RuleDef {
        name: "sim".into(),
        src_label: "V".into(),
        dst_label: "V".into(),
        predicate: Predicate::VectorSimilar {
            field: "emb".into(),
            min: 0.9,
        },
        edge_type: "SIM".into(),
        weight_prop: None,
        max_edges: None,
        approximate: true,
        via_label: None,
        via_dir: None,
        via_edge: None,
    }
}

/// Every derived edge of `et`, as a sorted flat list, so two runs compare
/// exactly.
fn edge_set(db: &GraphDb<RealFs>, et: &str, n: usize) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for i in 0..n {
        let k = format!("v{i}");
        for d in db.neighbors(&k, et, Direction::Out).unwrap_or_default() {
            out.push((k.clone(), d));
        }
    }
    out.sort();
    out
}

fn building_of(db: &GraphDb<RealFs>, rule: &str) -> Option<core_api::BuildProgress> {
    db.stats()
        .rules
        .iter()
        .find(|r| r.name == rule)
        .and_then(|r| r.building.clone())
}

fn edges_of(db: &GraphDb<RealFs>, rule: &str) -> u64 {
    db.stats()
        .rules
        .iter()
        .find(|r| r.name == rule)
        .map(|r| r.edges)
        .unwrap_or(0)
}

/// A corpus that fits in one slice behaves exactly as it did before 0.6.6:
/// one commit, edges present the moment `create_rule` returns.
#[test]
fn create_rule_under_the_slice_is_one_commit() {
    let (_dir, mut db) = store_with_vectors("slice-small", 100);
    db.create_rule(slice_rule()).unwrap();
    assert!(
        db.stats().rules.iter().all(|r| r.building.is_none()),
        "a 100-vector corpus must not defer anything"
    );
    assert!(
        edges_of(&db, "sim") > 0,
        "edges exist the moment create_rule returns"
    );
    assert!(
        db.pump_index_build().unwrap().is_empty(),
        "pumping a store with nothing pending must be a no-op"
    );
}

/// Above the slice the rule is installed, derives nothing yet, reports its
/// progress, and — once pumped — produces exactly the set the one-commit path
/// produces. Deferring must not change the answer.
#[test]
fn create_rule_over_the_slice_defers_then_matches() {
    let want = {
        let (_d, mut db) = store_with_vectors("slice-want", 300);
        db.create_rule(slice_rule()).unwrap();
        assert!(db.stats().rules[0].building.is_none());
        edge_set(&db, "SIM", 300)
    };
    assert!(!want.is_empty(), "the fixture must derive some edges");

    let (_d, mut db) = store_with_vectors("slice-defer", 300);
    core_rules::with_hnsw_build_batch(64, || {
        db.create_rule(slice_rule()).unwrap();
        let p = building_of(&db, "sim").expect("must report a build in progress");
        assert_eq!(p.indexed, 64, "create_rule does exactly one slice inline");
        assert_eq!(p.total, 300);
        assert_eq!(edges_of(&db, "sim"), 0, "no partial edge set, ever");
        assert_eq!(
            db.neighbors("v0", "SIM", Direction::Out).unwrap(),
            Vec::<String>::new()
        );
        while !db.pump_index_build().unwrap().is_empty() {}
    });
    assert!(building_of(&db, "sim").is_none());
    assert_eq!(
        edge_set(&db, "SIM", 300),
        want,
        "the deferred build derives the same edges"
    );
}

/// An ordinary write advances a pending build without anyone calling pump, and
/// the nodes those writes add are indexed like any other.
#[test]
fn a_write_pumps_the_build() {
    let want = {
        let (_d, mut db) = store_with_vectors("slice-write-want", 300);
        db.create_rule(slice_rule()).unwrap();
        edge_set(&db, "SIM", 300)
    };

    let (_d, mut db) = store_with_vectors("slice-write", 300);
    core_rules::with_hnsw_build_batch(64, || {
        db.create_rule(slice_rule()).unwrap();
        assert!(building_of(&db, "sim").is_some());
        let mut writes = 0;
        while building_of(&db, "sim").is_some() {
            db.insert_node(
                "Other",
                &format!("o{writes}"),
                vec![("v".into(), Value::Int(1))],
            )
            .unwrap();
            writes += 1;
            assert!(writes < 100, "the build never finished under plain writes");
        }
        assert!(writes >= 3, "a 64-vector slice should need several writes");
    });
    assert_eq!(
        edge_set(&db, "SIM", 300),
        want,
        "the write-driven build derives the same edges"
    );

    // A vector written while the build was outstanding is in the finished index.
    core_rules::with_hnsw_build_batch(64, || {
        db.insert_node("V", "late", vec![("emb".into(), slice_vec(3))])
            .unwrap();
        while !db.pump_index_build().unwrap().is_empty() {}
    });
    let late: Vec<String> = db.neighbors("late", "SIM", Direction::Out).unwrap();
    assert!(
        late.contains(&"v0".to_string()),
        "a node written during/after the build must link to its cluster; got {late:?}"
    );
}

/// A build interrupted by a snapshot + reopen resumes rather than restarting,
/// and still lands on the same edge set.
#[test]
fn an_interrupted_build_resumes_on_reopen() {
    let want = {
        let (_d, mut db) = store_with_vectors("slice-resume-want", 300);
        db.create_rule(slice_rule()).unwrap();
        edge_set(&db, "SIM", 300)
    };

    let (dir, mut db) = store_with_vectors("slice-resume", 300);
    core_rules::with_hnsw_build_batch(64, || {
        db.create_rule(slice_rule()).unwrap();
        db.pump_index_build().unwrap();
        let p = building_of(&db, "sim").expect("still building");
        assert_eq!(p.indexed, 128, "two slices in");
        db.snapshot().unwrap();
    });
    drop(db);

    let mut db = GraphDb::open(&dir).unwrap();
    assert_eq!(edges_of(&db, "sim"), 0, "the partial build derived nothing");
    core_rules::with_hnsw_build_batch(64, || while !db.pump_index_build().unwrap().is_empty() {});
    assert!(building_of(&db, "sim").is_none());
    assert_eq!(
        edge_set(&db, "SIM", 300),
        want,
        "the resumed build derives the same edges"
    );
}

/// No write after a clean reopen may be mistaken for an interrupted build.
///
/// The deferred candidate-index build is a full node scan, and it used to fire
/// from inside the engine hook — after `props.set` and the label assignment had
/// already landed. The scan therefore read the half-applied record and took the
/// in-flight node's vector for one the snapshot's graph should have carried,
/// which is the signature of a build the store was killed in the middle of. The
/// cost was a full `RebuildRule` — every node re-scanned, every edge re-derived
/// — on the first embedded write after every reopen.
///
/// Three shapes, because they fail for different reasons: (b) a brand-new
/// embedded node, (c) an embedding *update* on a node the snapshot held, and
/// (d) a node the snapshot held **without** an embedding that gains one. (d) is
/// the one an id-based cutoff cannot catch on its own: its id is below the
/// snapshot's line and the persisted graph genuinely does not contain it.
#[test]
fn no_write_after_a_clean_reopen_looks_like_an_interrupted_build() {
    for (case, dir_name) in [
        ("b-new", "slice-reopen-b"),
        ("c-update", "slice-reopen-c"),
        ("d-gains", "slice-reopen-d"),
    ] {
        let dir = tmp(dir_name);
        {
            let mut db = GraphDb::open(&dir).unwrap();
            for i in 0..300 {
                db.insert_node("V", &format!("v{i}"), vec![("emb".into(), slice_vec(i))])
                    .unwrap();
            }
            // A `V` the snapshot holds with no embedding at all — case (d)'s
            // subject. It is in the rule's dst label and below the snapshot's
            // id line, but no persisted HNSW graph can contain it.
            db.insert_node(
                "V",
                "bare",
                vec![("note".into(), Value::Str("no emb".into()))],
            )
            .unwrap();
            db.create_rule(slice_rule()).unwrap();
            assert!(
                db.stats().rules[0].building.is_none(),
                "{case}: 300 vectors is one commit at the production slice"
            );
            db.snapshot().unwrap();
        }

        let mut db = GraphDb::open(&dir).unwrap();
        let fires_before = db.stats().rules[0].fires;
        let wal_before = std::fs::metadata(dir.join("wal.bin")).map_or(0, |m| m.len());

        let subject = match case {
            "b-new" => {
                db.insert_node("V", "late", vec![("emb".into(), slice_vec(3))])
                    .unwrap();
                "late"
            }
            "c-update" => {
                db.set_prop("v0", "emb", slice_vec(3)).unwrap();
                "v0"
            }
            _ => {
                db.set_prop("bare", "emb", slice_vec(3)).unwrap();
                "bare"
            }
        };

        let wal = std::fs::read(dir.join("wal.bin")).unwrap();
        let (tail, _) = decode_all(&wal[wal_before as usize..]);
        assert!(
            !tail
                .iter()
                .any(|r| matches!(r, WalRecord::RebuildRule { .. })),
            "{case}: the first write after a reopen must not trigger a rebuild; \
             WAL tail: {tail:?}"
        );
        assert!(
            db.stats().rules[0].building.is_none(),
            "{case}: a completed rule must not be reported as building"
        );
        assert_eq!(
            db.stats().rules[0].fires - fires_before,
            1,
            "{case}: one write must evaluate the rule once, not once per node"
        );
        assert!(
            db.neighbors(subject, "SIM", Direction::Out)
                .unwrap()
                .contains(&"v0".to_string())
                || subject == "v0",
            "{case}: the write still derives its own edges"
        );
    }
}
/// A rule that is still building derives nothing, including for writes that
/// land while it builds; the backfill then derives the whole set.
#[test]
fn a_write_during_a_pending_build_derives_no_edges() {
    let want = {
        let (_d, mut db) = store_with_vectors("slice-noedge-want", 300);
        db.create_rule(slice_rule()).unwrap();
        db.insert_node("V", "late", vec![("emb".into(), slice_vec(3))])
            .unwrap();
        edge_set(&db, "SIM", 300)
    };

    let (_d, mut db) = store_with_vectors("slice-noedge", 300);
    db.set_hnsw_build_batch(Some(64));
    db.create_rule(slice_rule()).unwrap();
    db.insert_node("V", "late", vec![("emb".into(), slice_vec(3))])
        .unwrap();
    assert!(
        building_of(&db, "sim").is_some(),
        "one write does not finish a 300/64 build"
    );
    assert_eq!(
        edges_of(&db, "sim"),
        0,
        "a rule that is still building must derive nothing, not a partial set"
    );
    assert_eq!(
        db.neighbors("late", "SIM", Direction::Out).unwrap(),
        Vec::<String>::new()
    );

    while !db.pump_index_build().unwrap().is_empty() {}
    assert!(building_of(&db, "sim").is_none());
    assert_eq!(
        edge_set(&db, "SIM", 300),
        want,
        "the backfill derives the whole set, the write included"
    );
    assert!(db
        .neighbors("late", "SIM", Direction::Out)
        .unwrap()
        .contains(&"v0".to_string()));
}

/// A store killed between `CreateRule` and the `RebuildRule` that finishes its
/// build reopens resumable.
///
/// The kill is real: the WAL is truncated back to the byte offset `create_rule`
/// left it at, which discards the finishing `RebuildRule` frame and everything
/// after it. The reopen replays `CreateRule` under the same small slice, so the
/// replayed rule defers exactly as the original did.
#[test]
fn a_truncated_wal_replay_leaves_the_build_resumable() {
    let want = {
        let (_d, mut db) = store_with_vectors("slice-truncwal-want", 300);
        db.create_rule(slice_rule()).unwrap();
        edge_set(&db, "SIM", 300)
    };

    let dir = tmp("slice-truncwal");
    let after_create;
    {
        // No snapshot: the WAL is the whole store.
        let mut db = GraphDb::open(&dir).unwrap();
        for i in 0..300 {
            db.insert_node("V", &format!("v{i}"), vec![("emb".into(), slice_vec(i))])
                .unwrap();
        }
        db.set_hnsw_build_batch(Some(64));
        db.create_rule(slice_rule()).unwrap();
        assert_eq!(edges_of(&db, "sim"), 0);
        // A frame boundary: everything the deferred create wrote, and nothing
        // the build's completion will write.
        after_create = std::fs::metadata(dir.join("wal.bin")).unwrap().len();
        while !db.pump_index_build().unwrap().is_empty() {}
        assert!(
            edges_of(&db, "sim") > 0,
            "the build finished before the kill"
        );
    }

    // Kill it: drop the finishing RebuildRule and everything after.
    let full = std::fs::read(dir.join("wal.bin")).unwrap();
    let (discarded, _) = decode_all(&full[after_create as usize..]);
    assert!(
        discarded
            .iter()
            .any(|r| matches!(r, WalRecord::RebuildRule { .. })),
        "the bytes being truncated must include the finishing RebuildRule, \
         else this test proves nothing; got {discarded:?}"
    );
    std::fs::write(dir.join("wal.bin"), &full[..after_create as usize]).unwrap();

    // Replay under the same slice the original write used, so the replayed
    // `create_rule` defers rather than building the corpus in one commit.
    let mut db = core_rules::with_hnsw_build_batch(64, || GraphDb::open(&dir).unwrap());
    let p = building_of(&db, "sim").expect("the replayed build is reported as outstanding");
    assert_eq!(p.total, 300);
    assert_eq!(
        edges_of(&db, "sim"),
        0,
        "a rule still building owns no edges"
    );

    core_rules::with_hnsw_build_batch(64, || while !db.pump_index_build().unwrap().is_empty() {});
    assert!(building_of(&db, "sim").is_none());
    assert_eq!(
        edge_set(&db, "SIM", 300),
        want,
        "a replayed unfinished build lands on the one-shot edge set"
    );
}

/// A read-only handle cannot commit the backfill a finished build needs, so it
/// says so rather than advancing the index and losing the edges.
#[test]
fn pump_index_build_is_refused_read_only() {
    let dir = tmp("slice-readonly");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("V", "v0", vec![("emb".into(), slice_vec(0))])
            .unwrap();
        db.snapshot().unwrap();
    }
    let opts = core_api::OpenOptions {
        read_only: true,
        ..Default::default()
    };
    let mut db = GraphDb::open_with_options(&dir, opts).unwrap();
    assert!(matches!(db.pump_index_build(), Err(GraphError::ReadOnly)));
}

// ---------------------------------------------------------------------------
// 0.6.6 T4: an exact VectorSimilar rule finds its candidates through the index
// ---------------------------------------------------------------------------

/// A clustered 8-D unit vector: `i / PER` picks the cluster direction, the
/// remainder perturbs it. Within-cluster cosine lands near 0.99, cross-cluster
/// near zero, so a `min` of 0.90 separates them and the derived set is not
/// vacuous.
fn clustered_vec_8_raw(i: u32, per: u32) -> Vec<f64> {
    let dir = unit_vec_8_raw(0xC0FF_EE00 + i / per);
    let noise = unit_vec_8_raw(0x5EED_0000 + i);
    let mut out: Vec<f64> = dir
        .iter()
        .zip(noise.iter())
        .map(|(d, n)| d + 0.08 * n)
        .collect();
    let norm = out.iter().map(|x| x * x).sum::<f64>().sqrt();
    out.iter_mut().for_each(|x| *x /= norm);
    out
}

fn exact_vec_rule(min: f64) -> RuleDef {
    RuleDef {
        name: "sim".into(),
        src_label: "V".into(),
        dst_label: "V".into(),
        predicate: Predicate::VectorSimilar {
            field: "emb".into(),
            min,
        },
        edge_type: "SIM".into(),
        weight_prop: Some("w".into()),
        max_edges: None,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
    }
}

/// Every `SIM` edge with its weight, so two runs compare edge-for-edge and
/// bit-for-bit.
fn weighted_edges(
    db: &GraphDb<RealFs>,
    n: u32,
) -> std::collections::BTreeMap<(String, String), f64> {
    let mut out = std::collections::BTreeMap::new();
    for i in 0..n {
        let src = format!("v{i}");
        for dst in db
            .neighbors(&src, "SIM", Direction::Out)
            .unwrap_or_default()
        {
            let w = db
                .explain(&src, &dst)
                .unwrap()
                .into_iter()
                .find(|e| e.edge_type == "SIM" && e.src_key == src && e.dst_key == dst)
                .and_then(|e| e.weight)
                .expect("a derived SIM edge carries its weight");
            out.insert((src.clone(), dst), w);
        }
    }
    out
}

/// Build the rule over `n` clustered vectors and return its weighted edge set.
fn derive_exact_sim(
    name: &str,
    n: u32,
    per: u32,
    min: f64,
) -> std::collections::BTreeMap<(String, String), f64> {
    let dir = tmp(name);
    let mut db = GraphDb::open(&dir).unwrap();
    for i in 0..n {
        db.insert_node(
            "V",
            &format!("v{i}"),
            vec![("emb".into(), emb(&clustered_vec_8_raw(i, per)))],
        )
        .unwrap();
    }
    db.create_rule(exact_vec_rule(min)).unwrap();
    while !db.pump_index_build().unwrap().is_empty() {}
    weighted_edges(&db, n)
}

/// The brute-force answer, computed here: every ordered pair whose cosine is at
/// or above `min`, with the cosine as the weight.
fn brute_force_sim(
    n: u32,
    per: u32,
    min: f64,
) -> std::collections::BTreeMap<(String, String), f64> {
    let vs: Vec<Vec<f64>> = (0..n).map(|i| clustered_vec_8_raw(i, per)).collect();
    let mut out = std::collections::BTreeMap::new();
    for i in 0..n {
        for j in 0..n {
            if i == j {
                continue;
            }
            let dot: f64 = vs[i as usize]
                .iter()
                .zip(vs[j as usize].iter())
                .map(|(a, b)| a * b)
                .sum();
            if dot >= min {
                out.insert((format!("v{i}"), format!("v{j}")), dot);
            }
        }
    }
    out
}

/// On a set small enough for the beam to reach every vector, the index-backed
/// candidate path and the full-scan path derive identical edge sets — and both
/// equal the brute-force answer.
#[test]
fn index_backed_vector_rule_equals_brute_force_on_a_fixed_set() {
    const N: u32 = 200;
    const PER: u32 = 5;
    const MIN: f64 = 0.90;

    let truth = brute_force_sim(N, PER, MIN);
    assert!(
        truth.len() > 100,
        "a vacuous fixture proves nothing; got {} pairs above {MIN}",
        truth.len()
    );

    let index = derive_exact_sim("t4-equiv-index", N, PER, MIN);
    let scan =
        core_rules::with_vector_scan(true, || derive_exact_sim("t4-equiv-scan", N, PER, MIN));

    let keys = |m: &std::collections::BTreeMap<(String, String), f64>| {
        m.keys().cloned().collect::<std::collections::BTreeSet<_>>()
    };
    assert_eq!(
        keys(&index),
        keys(&truth),
        "index-backed rule must find every true pair"
    );
    assert_eq!(
        keys(&scan),
        keys(&truth),
        "the escape hatch must still be exact"
    );
    // And the weights agree to the bit, because scoring did not change.
    assert_eq!(index, scan, "scores are computed exactly on both paths");
    for ((s, d), w) in &truth {
        let got = index[&(s.clone(), d.clone())];
        assert!(
            (got - w).abs() < 1e-12,
            "weight {s}->{d}: index {got} brute force {w}"
        );
    }
}

/// Past the default beam width the **graph** is what answers — one full-width
/// beam whose worst hit is below `min`, which is the only outcome allowed to
/// narrow the candidate set — and it still finds every qualifying pair.
///
/// This fixture does not widen: 100 clusters of 6 means a 400-wide beam comes
/// back with its worst hit far below 0.90 on the first pass.
/// `the_widening_loop_runs_when_one_cluster_is_wider_than_the_beam` is the one
/// that widens.
#[test]
fn index_backed_vector_rule_answers_from_one_full_beam() {
    const N: u32 = 600;
    const PER: u32 = 6;
    const MIN: f64 = 0.90;

    let truth = brute_force_sim(N, PER, MIN);
    assert!(
        truth.len() > 500,
        "a vacuous fixture proves nothing; got {} pairs",
        truth.len()
    );
    core_rules::hnsw_search_count_reset();
    let index = derive_exact_sim("t4-beam", N, PER, MIN);
    let searches = core_rules::hnsw_search_count();
    assert!(
        searches > 0,
        "this fixture is past the default beam width, so the graph — not the \
         whole-index fallback — must be what answered"
    );
    assert!(
        searches < 2 * u64::from(N),
        "with 100 clusters of 6 the first beam already passes the floor, so no \
         source should need a second search; got {searches} for {N} sources"
    );
    assert_eq!(
        index
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>(),
        truth
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>(),
        "the beam must not drop a qualifying pair"
    );
}

/// `n` distinct 2-D unit vectors inside a cone narrow enough that **every** pair
/// is above `min`: `span` radians must be below `acos(min)` (0.451 for 0.90).
///
/// Two dimensions keep the distance kernel cheap, which matters for the
/// fixtures below — they are deliberately wider than the beam, so they are the
/// expensive ones.
fn cone_vec_2(i: u32, n: u32, span: f64) -> Vec<f64> {
    let t = span * f64::from(i) / f64::from(n - 1);
    vec![t.cos(), t.sin()]
}

/// `(src, dst)` for every derived `SIM` edge. No `explain`, so no per-edge
/// provenance lookup — these fixtures derive hundreds of thousands of edges.
fn sim_pairs(db: &GraphDb<RealFs>, n: u32) -> std::collections::BTreeSet<(String, String)> {
    let mut out = std::collections::BTreeSet::new();
    for i in 0..n {
        let src = format!("v{i}");
        for dst in db
            .neighbors(&src, "SIM", Direction::Out)
            .unwrap_or_default()
        {
            out.insert((src.clone(), dst));
        }
    }
    out
}

/// Build an exact rule over `n` vectors from `vec_of` and return its edge pairs.
fn derive_pairs_over(
    name: &str,
    n: u32,
    rule: RuleDef,
    vec_of: &dyn Fn(u32) -> Vec<f64>,
) -> std::collections::BTreeSet<(String, String)> {
    let dir = tmp(name);
    let mut db = GraphDb::open(&dir).unwrap();
    for i in 0..n {
        db.insert_node("V", &format!("v{i}"), vec![("emb".into(), emb(&vec_of(i)))])
            .unwrap();
    }
    db.create_rule(rule).unwrap();
    while !db.pump_index_build().unwrap().is_empty() {}
    sim_pairs(&db, n)
}

/// Layer 0 is not always one connected component — a corpus of identical
/// embeddings is the case that shows it — and a beam that exhausts its frontier
/// has proved nothing about the nodes it could not reach. The candidate set in
/// that case has to be every vector on the side, or the rule silently derives
/// only its own reachable component.
///
/// 420 nodes: one past the 400-wide default beam, which is the smallest corpus
/// that reaches the widening loop at all.
#[test]
fn a_beam_short_of_its_width_falls_back_to_every_vector() {
    const N: u32 = 420;
    let same = vec![1.0, 0.0];

    core_rules::hnsw_search_count_reset();
    let got = derive_pairs_over("t4-duplicates", N, exact_vec_rule(0.90), &|_| same.clone());
    assert!(
        core_rules::hnsw_search_count() > 0,
        "the graph must have been consulted, else this proves nothing"
    );
    assert_eq!(
        got.len(),
        (N as usize) * (N as usize - 1),
        "every pair of identical vectors is above any floor, so every ordered \
         pair must be derived — a short beam must fall back to the whole side"
    );
}

/// Reaching the beam ceiling with the worst hit still at or above `min` means
/// the beam never proved a thing about what it left out. A cluster denser than
/// the ceiling therefore costs a scan, never recall.
///
/// `with_ef_max(64)` puts the ceiling below the starting width so 420 vectors
/// reach it; in production it takes more than 4,096.
#[test]
fn the_beam_ceiling_falls_back_to_every_vector() {
    const N: u32 = 420;
    const MIN: f64 = 0.90;

    let got = core_rules::with_ef_max(64, || {
        derive_pairs_over("t4-ceiling", N, exact_vec_rule(MIN), &|i| {
            cone_vec_2(i, N, 0.40)
        })
    });
    assert_eq!(
        got.len(),
        (N as usize) * (N as usize - 1),
        "every vector in the cone is above {MIN} of every other, so capping the \
         beam must cost time and not pairs"
    );
}

/// The widening loop itself: the destination side is one cluster wider than two
/// beam widths, so every source's first 400-wide pass comes back full with its
/// worst hit still above `min`, the beam doubles to 800, that pass is full above
/// the floor too, and 1,600 is wider than the index — so the candidate set ends
/// as the whole side, after two searches.
///
/// The source side is deliberately small (20 query nodes against 810
/// destinations). The widening loop is a property of the *destination* index, and
/// 20 sources exercise it exactly as 810 would while keeping the pair
/// evaluations — which are quadratic in the corpus and nothing to do with the
/// beam — off the clock.
#[test]
fn the_widening_loop_runs_when_one_cluster_is_wider_than_the_beam() {
    const DSTS: u32 = 810;
    const SRCS: u32 = 20;
    const MIN: f64 = 0.90;
    const SPAN: f64 = 0.40;

    let dir = tmp("t4-widen");
    let mut db = GraphDb::open(&dir).unwrap();
    for i in 0..DSTS {
        db.insert_node(
            "V",
            &format!("v{i}"),
            vec![("emb".into(), emb(&cone_vec_2(i, DSTS, SPAN)))],
        )
        .unwrap();
    }
    // Query nodes spread through the same cone, so each one's beam is full of
    // qualifying destinations.
    for q in 0..SRCS {
        let at = q * (DSTS / SRCS);
        db.insert_node(
            "Q",
            &format!("q{q}"),
            vec![("emb".into(), emb(&cone_vec_2(at, DSTS, SPAN)))],
        )
        .unwrap();
    }

    let mut def = exact_vec_rule(MIN);
    def.src_label = "Q".into();
    def.max_edges = Some(8);

    core_rules::hnsw_search_count_reset();
    db.create_rule(def).unwrap();
    while !db.pump_index_build().unwrap().is_empty() {}
    let searches = core_rules::hnsw_search_count();
    assert!(
        searches >= 2 * u64::from(SRCS),
        "every source's first beam comes back full above the floor, so every \
         source must search at least twice; got {searches} for {SRCS} sources"
    );

    let mut derived: Vec<(String, String)> = Vec::new();
    for q in 0..SRCS {
        let src = format!("q{q}");
        for dst in db
            .neighbors(&src, "SIM", Direction::Out)
            .unwrap_or_default()
        {
            derived.push((src.clone(), dst));
        }
    }
    assert_eq!(
        derived.len(),
        (SRCS as usize) * 8,
        "top-8 per source over a cone where every destination qualifies"
    );

    // What the widened beam handed to the scorer, checked against the truth: no
    // derived destination may score below that source's 8th-best true
    // similarity. A beam that dropped a near neighbour would have put a worse
    // one in its place.
    let dvs: Vec<Vec<f64>> = (0..DSTS).map(|i| cone_vec_2(i, DSTS, SPAN)).collect();
    let cos = |a: &[f64], b: &[f64]| a.iter().zip(b.iter()).map(|(x, y)| x * y).sum::<f64>();
    for q in 0..SRCS {
        let qv = cone_vec_2(q * (DSTS / SRCS), DSTS, SPAN);
        let mut sims: Vec<f64> = dvs.iter().map(|v| cos(&qv, v)).collect();
        sims.sort_by(|a, b| b.partial_cmp(a).unwrap());
        let eighth = sims[7];
        for (_, dst) in derived.iter().filter(|(s, _)| *s == format!("q{q}")) {
            let j: usize = dst[1..].parse().unwrap();
            let got = cos(&qv, &dvs[j]);
            assert!(
                got >= eighth - 1e-12,
                "q{q}->{dst} scores {got}, below the 8th-best true {eighth}: \
                 the beam dropped a nearer neighbour"
            );
        }
    }
}

/// `MUSHROOMDB_VECTOR_SCAN=1` must not cost the store its persisted vector
/// index. A session with the variable set takes the full-scan candidate path and
/// never asks the graph anything, but the graph is still loaded from the
/// snapshot and written back out, so unsetting the variable does not pay for a
/// rebuild.
///
/// The write made while the variable was set reaches the graph through the
/// open-time scan on the next open — which is what `hnsw_build_count() == 0`
/// plus the full edge set together show.
#[test]
fn the_scan_escape_hatch_keeps_the_persisted_graph() {
    const N: u32 = 60;
    const PER: u32 = 5;
    const MIN: f64 = 0.90;

    let dir = tmp("t4-scan-keeps-graph");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        for i in 0..N {
            db.insert_node(
                "V",
                &format!("v{i}"),
                vec![("emb".into(), emb(&clustered_vec_8_raw(i, PER)))],
            )
            .unwrap();
        }
        db.create_rule(exact_vec_rule(MIN)).unwrap();
        db.snapshot().unwrap();
    }

    // A whole session under the escape hatch: open, write, snapshot.
    core_rules::with_vector_scan(true, || {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node(
            "V",
            &format!("v{N}"),
            vec![("emb".into(), emb(&clustered_vec_8_raw(N, PER)))],
        )
        .unwrap();
        db.snapshot().unwrap();
    });

    // Back without it: the graph must come from the snapshot, not a rebuild.
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Other", "o", vec![("v".into(), Value::Int(1))])
        .unwrap();
    assert_eq!(
        db.hnsw_build_count(),
        0,
        "a snapshot written under MUSHROOMDB_VECTOR_SCAN dropped the graph"
    );
    while !db.pump_index_build().unwrap().is_empty() {}
    let truth = brute_force_sim(N + 1, PER, MIN);
    assert_eq!(
        sim_pairs(&db, N + 1),
        truth
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>(),
        "the node written under the escape hatch must be in the edge set too"
    );
}

/// A rebuild of an exact vector rule — the one a finished sliced build asks
/// for, and the one `rebuild_rule` asks for — must leave the rule answering
/// from its graph. Resetting the index without re-initialising the graph is
/// silent: the rule stays correct and goes quietly back to O(n²).
#[test]
fn rebuilding_an_exact_vector_rule_keeps_its_graph() {
    // Just past the 400-wide default beam, which is what makes the graph — and
    // not the whole-index fallback — answer after the rebuild.
    const N: u32 = 420;
    const PER: u32 = 6;
    const MIN: f64 = 0.90;

    let dir = tmp("t4-rebuild-graph");
    let mut db = GraphDb::open(&dir).unwrap();
    for i in 0..N {
        db.insert_node(
            "V",
            &format!("v{i}"),
            vec![("emb".into(), emb(&clustered_vec_8_raw(i, PER)))],
        )
        .unwrap();
    }
    // A slice well under the corpus, so the build is deferred and its
    // completion is what issues the backfill.
    db.set_hnsw_build_batch(Some(64));
    db.create_rule(exact_vec_rule(MIN)).unwrap();
    assert!(
        !db.builds_in_progress().is_empty(),
        "{N} vectors past a 64-vector slice must defer"
    );
    while !db.pump_index_build().unwrap().is_empty() {}
    // Pairs, not weights: the weights are compared bit-for-bit against both the
    // scan path and the brute force in
    // `index_backed_vector_rule_equals_brute_force_on_a_fixed_set`, and
    // `explain` per edge is the expensive part of this fixture.
    let after_build = sim_pairs(&db, N);
    assert_eq!(
        after_build,
        brute_force_sim(N, PER, MIN)
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>(),
        "the backfill a finished slice-build triggers derives the whole set"
    );

    core_rules::hnsw_search_count_reset();
    db.rebuild_rule("sim").unwrap();
    assert!(
        core_rules::hnsw_search_count() > 0,
        "a rebuilt exact vector rule must still probe its graph"
    );
    assert_eq!(sim_pairs(&db, N), after_build, "and land on the same edges");
}

/// Changing an embedding retracts exactly the edges whose predicate stopped
/// holding, and putting it back re-derives them.
#[test]
fn changing_an_embedding_retracts_and_rederives() {
    let dir = tmp("t4-retract");
    let mut db = GraphDb::open(&dir).unwrap();
    let near = emb(&[1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
    let near2 = emb(&[0.99, 0.141, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
    let far = emb(&[0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0]);

    db.insert_node("V", "a", vec![("emb".into(), near.clone())])
        .unwrap();
    db.insert_node("V", "b", vec![("emb".into(), near2.clone())])
        .unwrap();
    db.insert_node("V", "c", vec![("emb".into(), far.clone())])
        .unwrap();
    db.create_rule(exact_vec_rule(0.9)).unwrap();

    let pairs = |db: &GraphDb<RealFs>| {
        let mut out = std::collections::BTreeSet::new();
        for k in ["a", "b", "c"] {
            for d in db.neighbors(k, "SIM", Direction::Out).unwrap_or_default() {
                out.insert((k.to_string(), d));
            }
        }
        out
    };
    let ab: std::collections::BTreeSet<(String, String)> = [("a", "b"), ("b", "a")]
        .iter()
        .map(|(s, d)| (s.to_string(), d.to_string()))
        .collect();
    let all: std::collections::BTreeSet<(String, String)> = [
        ("a", "b"),
        ("b", "a"),
        ("a", "c"),
        ("c", "a"),
        ("b", "c"),
        ("c", "b"),
    ]
    .iter()
    .map(|(s, d)| (s.to_string(), d.to_string()))
    .collect();

    assert_eq!(pairs(&db), ab, "c starts outside the cluster");

    // Move c into the cluster.
    db.set_prop("c", "emb", near.clone()).unwrap();
    assert_eq!(pairs(&db), all, "c in the cluster derives both its pairs");

    // Move it back out: exactly c's edges go, nothing stale is left.
    db.set_prop("c", "emb", far.clone()).unwrap();
    assert_eq!(pairs(&db), ab, "c leaving retracts exactly its own edges");

    // And in again.
    db.set_prop("c", "emb", near.clone()).unwrap();
    assert_eq!(pairs(&db), all, "the edges come back");
}
