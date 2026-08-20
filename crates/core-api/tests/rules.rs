use core_api::{Direction, GraphDb, GraphError, Predicate, PredicateSummary, RuleDef, Value};

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
fn derived_edges_are_not_wal_logged() {
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
    let after = std::fs::metadata(dir.join("wal.bin")).unwrap().len();
    // exactly one InsertNode record was appended — no edge records
    let node_only = core_storage::wal::encode_record(&core_storage::wal::WalRecord::InsertNode {
        label: "Person".into(),
        key: "p1".into(),
        props: vec![("org_id".into(), Value::Str("o1".into()))],
    })
    .len() as u64;
    assert_eq!(after - before, node_only);
    assert_eq!(db.edge_count(), 1); // yet the derived edge exists
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
