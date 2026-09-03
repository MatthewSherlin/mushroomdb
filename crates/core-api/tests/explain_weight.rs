//! explain() must report a score for every derived edge, including rules that
//! store no weight property (KeyMatch / FieldEqual with weight_prop: None).
use core_api::{GraphDb, Predicate, RuleDef, Value};

fn tmp(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!(
        "explain-weight-{name}-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn rule(name: &str, pred: Predicate, weight_prop: Option<&str>) -> RuleDef {
    RuleDef {
        name: name.into(),
        src_label: "Person".into(),
        dst_label: "Org".into(),
        predicate: pred,
        edge_type: name.to_uppercase(),
        weight_prop: weight_prop.map(str::to_string),
        max_edges: None,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
    }
}

#[test]
fn fieldequal_without_weight_prop_explains_as_one() {
    let dir = tmp("fe");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node(
        "Org",
        "o1",
        vec![("industry".into(), Value::Str("design".into()))],
    )
    .unwrap();
    db.insert_node(
        "Person",
        "p1",
        vec![("industry".into(), Value::Str("design".into()))],
    )
    .unwrap();
    db.create_rule(rule(
        "same_industry",
        Predicate::FieldEqual {
            field: "industry".into(),
        },
        None,
    ))
    .unwrap();
    let ex = db.explain("p1", "o1").unwrap();
    assert_eq!(ex.len(), 1);
    assert_eq!(ex[0].rule, "same_industry");
    assert_eq!(
        ex[0].weight,
        Some(1.0),
        "FieldEqual score must be reported even with no weight_prop"
    );
}

#[test]
fn overlap_without_weight_prop_explains_recomputed_jaccard() {
    let dir = tmp("ov");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node(
        "Org",
        "o1",
        vec![(
            "skills".into(),
            Value::List(vec![Value::Str("a".into()), Value::Str("b".into())]),
        )],
    )
    .unwrap();
    db.insert_node(
        "Person",
        "p1",
        vec![(
            "skills".into(),
            Value::List(vec![
                Value::Str("a".into()),
                Value::Str("b".into()),
                Value::Str("c".into()),
                Value::Str("d".into()),
            ]),
        )],
    )
    .unwrap();
    db.create_rule(rule(
        "fit",
        Predicate::Overlap {
            field: "skills".into(),
            min: 0.1,
        },
        None,
    ))
    .unwrap();
    let ex = db.explain("p1", "o1").unwrap();
    let w = ex[0].weight.expect("score");
    assert!((w - 0.5).abs() < 1e-9, "jaccard 2/4 expected, got {w}");
}

#[test]
fn stored_weight_prop_still_wins() {
    let dir = tmp("stored");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node(
        "Org",
        "o1",
        vec![("industry".into(), Value::Str("x".into()))],
    )
    .unwrap();
    db.insert_node(
        "Person",
        "p1",
        vec![("industry".into(), Value::Str("x".into()))],
    )
    .unwrap();
    db.create_rule(rule(
        "same",
        Predicate::FieldEqual {
            field: "industry".into(),
        },
        Some("weight"),
    ))
    .unwrap();
    let rs = db
        .query(
            "MATCH (p:Person)-[r:SAME]->(o:Org) RETURN r.weight",
            &Default::default(),
        )
        .unwrap();
    assert_eq!(rs.get(0, "r.weight"), Some(&Value::Float(1.0)));
    assert_eq!(db.explain("p1", "o1").unwrap()[0].weight, Some(1.0));
}

/// A via-hop rule scores `(via, dst)`, not `(src, dst)`. The fixture gives the
/// src node its own matching `industry` so a naive recompute over the pair
/// would return a fabricated 1.0; explain must report no score instead.
fn via_rule(weight_prop: Option<&str>) -> RuleDef {
    RuleDef {
        name: "fit".into(),
        src_label: "Person".into(),
        dst_label: "Project".into(),
        predicate: Predicate::FieldEqual {
            field: "industry".into(),
        },
        edge_type: "FIT".into(),
        weight_prop: weight_prop.map(str::to_string),
        max_edges: None,
        approximate: false,
        via_label: Some("Org".into()),
        via_edge: Some("WORKS_AT".into()),
        via_dir: None,
    }
}

fn via_fixture(dir: &std::path::Path) -> GraphDb<core_storage::fs::RealFs> {
    let mut db = GraphDb::open(dir).unwrap();
    db.insert_node(
        "Org",
        "techcorp",
        vec![("industry".into(), Value::Str("tech".into()))],
    )
    .unwrap();
    // alice carries the field herself — the naive recompute trap.
    db.insert_node(
        "Person",
        "alice",
        vec![("industry".into(), Value::Str("tech".into()))],
    )
    .unwrap();
    db.insert_node(
        "Project",
        "proj_a",
        vec![("industry".into(), Value::Str("tech".into()))],
    )
    .unwrap();
    db.insert_edge("WORKS_AT", "alice", "techcorp").unwrap();
    db
}

#[test]
fn via_hop_without_weight_prop_reports_no_score() {
    let mut db = via_fixture(&tmp("via-none"));
    db.create_rule(via_rule(None)).unwrap();
    let ex = db.explain("alice", "proj_a").unwrap();
    assert_eq!(ex.len(), 1);
    assert_eq!(ex[0].rule, "fit");
    assert_eq!(
        ex[0].weight, None,
        "via-hop scores over the via set; explain must not recompute over (src, dst)"
    );
}

#[test]
fn via_hop_with_weight_prop_reports_the_stored_score() {
    let mut db = via_fixture(&tmp("via-stored"));
    db.create_rule(via_rule(Some("score"))).unwrap();
    let ex = db.explain("alice", "proj_a").unwrap();
    assert_eq!(ex.len(), 1);
    assert_eq!(
        ex[0].weight,
        Some(1.0),
        "a via-hop rule that stores its score still reports it"
    );
}
