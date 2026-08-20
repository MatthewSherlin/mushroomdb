use core_api::{EdgeInfo, GraphDb, GraphError, NodeInfo, Predicate, RuleDef, Value};
use std::collections::BTreeMap;

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("graphdb-node-api-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn skills(items: &[&str]) -> Value {
    Value::List(items.iter().map(|s| Value::Str((*s).into())).collect())
}

fn open_mixed(name: &str) -> GraphDb<core_storage::fs::RealFs> {
    let mut db = GraphDb::open(&tmp(name)).unwrap();
    db.insert_node(
        "Org",
        "acme",
        vec![("skills".into(), skills(&["graph", "rust"]))],
    )
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
    // Non-alpha insert order so node_info props must come back as BTreeMap order.
    db.insert_node(
        "Member",
        "ada",
        vec![
            ("years".into(), Value::Int(8)),
            ("name".into(), Value::Str("Ada".into())),
            ("skills".into(), skills(&["graph", "rust"])),
            ("ok".into(), Value::Bool(true)),
            ("rating".into(), Value::Float(0.9)),
        ],
    )
    .unwrap();
    db.insert_node("Member", "bob", vec![("years".into(), Value::Int(2))])
        .unwrap();
    db.insert_edge("COHORT", "bob", "ada").unwrap();
    db.insert_edge("WORKS_AT", "ada", "acme").unwrap();
    db
}

#[test]
fn node_info_returns_label_and_props_in_deterministic_order() {
    let db = open_mixed("info-exact");
    let info = db.node_info("ada").expect("ada");
    assert_eq!(info.key, "ada");
    assert_eq!(info.label, "Member");
    let keys: Vec<&str> = info.props.keys().map(String::as_str).collect();
    assert_eq!(keys, vec!["name", "ok", "rating", "skills", "years"]);
    assert_eq!(info.props.get("name"), Some(&Value::Str("Ada".into())));
    assert_eq!(info.props.get("ok"), Some(&Value::Bool(true)));
    assert_eq!(info.props.get("rating"), Some(&Value::Float(0.9)));
    assert_eq!(info.props.get("years"), Some(&Value::Int(8)));
    assert_eq!(info.props.get("skills"), Some(&skills(&["graph", "rust"])));

    let empty = db.node_info("bob").expect("bob");
    assert_eq!(
        empty,
        NodeInfo {
            key: "bob".into(),
            label: "Member".into(),
            props: BTreeMap::from([("years".into(), Value::Int(2))]),
        }
    );
    assert!(db.node_info("ghost").is_none());
}

#[test]
fn node_edges_marks_user_and_derived_and_sorts() {
    let db = open_mixed("edges-mixed");
    let edges = db.node_edges("ada").expect("ada");
    assert_eq!(
        edges,
        vec![
            EdgeInfo {
                edge_type: "COHORT".into(),
                src_key: "bob".into(),
                dst_key: "ada".into(),
                derived: false,
            },
            EdgeInfo {
                edge_type: "FIT".into(),
                src_key: "ada".into(),
                dst_key: "acme".into(),
                derived: true,
            },
            EdgeInfo {
                edge_type: "WORKS_AT".into(),
                src_key: "ada".into(),
                dst_key: "acme".into(),
                derived: false,
            },
        ]
    );

    let err = db.node_edges("ghost").expect_err("unknown key");
    assert!(
        matches!(err, GraphError::KeyNotFound { ref key } if key == "ghost"),
        "expected KeyNotFound ghost, got {err:?}"
    );
}

#[test]
fn node_edges_self_loop_is_emitted_once() {
    let mut db = GraphDb::open(&tmp("self-loop")).unwrap();
    db.insert_node("Member", "ada", vec![]).unwrap();
    db.insert_edge("LOOP", "ada", "ada").unwrap();
    let edges = db.node_edges("ada").expect("ada");
    assert_eq!(
        edges,
        vec![EdgeInfo {
            edge_type: "LOOP".into(),
            src_key: "ada".into(),
            dst_key: "ada".into(),
            derived: false,
        }]
    );
}
