use core_api::{CmpOp, Dir, Filter, GraphDb, Predicate, ResultSet, RuleDef, Value};

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("graphdb-trav-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn skills(items: &[&str]) -> Value {
    Value::List(items.iter().map(|s| Value::Str((*s).into())).collect())
}

/// Generic stand-in for the talentco `cohort|fit|works_at` grouped-fetch:
/// user `COHORT` + scored Overlap-derived `FIT` + user `WORKS_AT`.
fn open_fixture(name: &str) -> GraphDb<core_storage::fs::RealFs> {
    let mut db = GraphDb::open(&tmp(name)).unwrap();
    db.insert_node(
        "Org",
        "acme",
        vec![("skills".into(), skills(&["graph", "rust"]))],
    )
    .unwrap();
    db.insert_node("Org", "beta", vec![("skills".into(), skills(&["sales"]))])
        .unwrap();
    db.create_rule(RuleDef {
        name: "skill_fit".into(),
        src_label: "Member".into(),
        dst_label: "Org".into(),
        predicate: Predicate::Overlap {
            field: "skills".into(),
            min: 0.5,
        },
        edge_type: "FIT".into(),
        weight_prop: Some("score".into()),
            max_edges: None,
        approximate: false,
    })
    .unwrap();
    db.insert_node(
        "Member",
        "ada",
        vec![
            ("skills".into(), skills(&["graph", "rust"])),
            ("years".into(), Value::Int(8)),
            ("rating".into(), Value::Float(0.9)),
        ],
    )
    .unwrap();
    db.insert_node(
        "Member",
        "bob",
        vec![
            ("skills".into(), skills(&["sales"])),
            ("years".into(), Value::Int(2)),
            ("rating".into(), Value::Float(0.3)),
        ],
    )
    .unwrap();
    db.insert_node(
        "Member",
        "cara",
        vec![
            ("skills".into(), skills(&["graph", "rust"])),
            ("years".into(), Value::Int(10)),
            ("rating".into(), Value::Float(0.85)),
        ],
    )
    .unwrap();
    // User edges: bob → ada → acme (user hop then derived hop for depth-2),
    // plus cara → ada (second COHORT) and ada → acme WORKS_AT (third group).
    db.insert_edge("COHORT", "bob", "ada").unwrap();
    db.insert_edge("COHORT", "cara", "ada").unwrap();
    db.insert_edge("WORKS_AT", "ada", "acme").unwrap();
    db
}

fn row_key_label_depth(rs: &ResultSet, i: usize) -> (&str, &str, i64) {
    match (rs.get(i, "key"), rs.get(i, "label"), rs.get(i, "depth")) {
        (Some(Value::Str(k)), Some(Value::Str(l)), Some(Value::Int(d))) => (k, l, *d),
        other => panic!("row {i} not (Str,Str,Int): {other:?}"),
    }
}

#[test]
fn rules_compose_with_grouped_and_neighborhood() {
    let db = open_fixture("compose");
    let ada = db.node_ref("ada").expect("ada inserted");
    assert_eq!(ada.key(), "ada");
    assert_eq!(ada.label(), "Member");
    assert_eq!(ada.prop("years"), Some(&Value::Int(8)));

    // Spec §6 grouped fetch: neighbors bucketed by edge-type name.
    let grouped = ada.grouped_by_edge_type();
    let types: Vec<&str> = grouped.keys().map(String::as_str).collect();
    assert_eq!(types, vec!["COHORT", "FIT", "WORKS_AT"]);
    assert_eq!(
        grouped["COHORT"],
        vec!["bob".to_string(), "cara".to_string()]
    );
    assert_eq!(grouped["FIT"], vec!["acme".to_string()]);
    assert_eq!(grouped["WORKS_AT"], vec!["acme".to_string()]);

    let hop = ada.neighborhood(1, Some(&["FIT"]), Dir::Out);
    assert_eq!(
        hop.columns(),
        &["key".to_string(), "label".to_string(), "depth".to_string()]
    );
    assert_eq!(hop.len(), 1);
    assert_eq!(row_key_label_depth(&hop, 0), ("acme", "Org", 1));

    // Same derived neighbor via untyped 1-hop (start excluded).
    let both = ada.neighborhood(1, None, Dir::Both);
    let keys: Vec<&str> = (0..both.len())
        .map(|i| row_key_label_depth(&both, i).0)
        .collect();
    assert!(keys.contains(&"acme"));
    assert!(!keys.contains(&"ada"));
}

#[test]
fn find_nodes_and_cmp_narrows() {
    let db = open_fixture("find");
    let members: Vec<String> = db
        .nodes_with_label("Member")
        .iter()
        .map(|n| n.key().to_string())
        .collect();
    assert_eq!(members, vec!["ada", "bob", "cara"]); // dense-id insert order

    let years = Filter::Cmp {
        field: "years".into(),
        op: CmpOp::Ge,
        value: Value::Int(8),
    };
    let rating = Filter::Cmp {
        field: "rating".into(),
        op: CmpOp::Gt,
        value: Value::Float(0.8),
    };
    let by_years: Vec<String> = db
        .find_nodes("Member", &years)
        .iter()
        .map(|n| n.key().to_string())
        .collect();
    assert_eq!(by_years, vec!["ada", "cara"]);

    let both = Filter::And(vec![years, rating]);
    let narrowed: Vec<String> = db
        .find_nodes("Member", &both)
        .iter()
        .map(|n| n.key().to_string())
        .collect();
    // cara.rating = 0.85 still passes Gt 0.8; raise the bar to drop her.
    assert_eq!(narrowed, vec!["ada", "cara"]);

    let stricter = Filter::And(vec![
        Filter::Cmp {
            field: "years".into(),
            op: CmpOp::Ge,
            value: Value::Int(8),
        },
        Filter::Cmp {
            field: "rating".into(),
            op: CmpOp::Gt,
            value: Value::Float(0.88),
        },
    ]);
    let only_ada: Vec<String> = db
        .find_nodes("Member", &stricter)
        .iter()
        .map(|n| n.key().to_string())
        .collect();
    assert_eq!(only_ada, vec!["ada"]);
}

#[test]
fn unknowns_and_depth2_bfs_across_user_and_derived() {
    let db = open_fixture("unknowns");
    assert!(db.node_ref("ghost").is_none());
    assert!(db.nodes_with_label("Nope").is_empty());
    assert!(db.find_nodes("Nope", &Filter::And(vec![])).is_empty());

    let bob = db.node_ref("bob").expect("bob inserted");
    // Unknown edge-type name is skipped, not an error.
    let none = bob.neighborhood(2, Some(&["NOPE"]), Dir::Out);
    assert!(none.is_empty());
    assert_eq!(
        none.columns(),
        &["key".to_string(), "label".to_string(), "depth".to_string()]
    );

    // bob -COHORT-> ada -FIT-> acme, plus bob -FIT-> beta at depth 1.
    // FIT is interned at rule create, COHORT later, so BFS first-hop is FIT then COHORT.
    let rs = bob.neighborhood(2, Some(&["COHORT", "FIT"]), Dir::Out);
    assert_eq!(rs.len(), 3);
    assert_eq!(row_key_label_depth(&rs, 0), ("beta", "Org", 1));
    assert_eq!(row_key_label_depth(&rs, 1), ("ada", "Member", 1));
    assert_eq!(row_key_label_depth(&rs, 2), ("acme", "Org", 2));
}
