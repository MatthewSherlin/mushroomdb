use core_api::{GraphDb, GraphError, Predicate, ResultSet, RuleDef, Value};
use std::collections::BTreeMap;

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("graphdb-query-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn tags(items: &[&str]) -> Value {
    Value::List(items.iter().map(|s| Value::Str((*s).into())).collect())
}

fn overlap(name: &str, field: &str, edge_type: &str) -> RuleDef {
    RuleDef {
        name: name.into(),
        src_label: "Org".into(),
        dst_label: "Person".into(),
        predicate: Predicate::Overlap {
            field: field.into(),
            min: 0.2,
        },
        edge_type: edge_type.into(),
        weight_prop: Some("score".into()),
    }
}

/// Generic two-label graph. Two scored Overlap rules (Org → Person) so a
/// dogfood-shaped 3-MATCH can filter on two rule-derived `score` props.
///
/// Jaccard (hand): inter/union of list tokens. t1 industries {a,b,c,d},
/// specialties {w,x,y,z}.
///
/// | org     | industries | ind | specialties | spec | both ≥ 0.5? |
/// |---------|------------|-----|-------------|------|-------------|
/// | acme    | a,b,c,d    | 1.0 | w,x,y,z     | 1.0  | yes         |
/// | zeta    | a,b,c      | .75 | w,x,y,z     | 1.0  | yes         |
/// | beta    | a,b,c      | .75 | w,x,y       | .75  | yes         |
/// | echo    | a,b        | .50 | w,x         | .50  | yes         |
/// | gamma   | a          | .25 | w,x,y,z     | 1.0  | no (ind)    |
/// | delta   | a,b,c,d    | 1.0 | w           | .25  | no (spec)   |
/// | foxtrot | a,b,c,d    | 1.0 | q,r         | —    | no (no edge)|
fn open_fixture(name: &str) -> GraphDb<core_storage::fs::RealFs> {
    let mut db = GraphDb::open(&tmp(name)).unwrap();
    db.insert_node(
        "Person",
        "t1",
        vec![
            ("id".into(), Value::Str("t1".into())),
            ("industries".into(), tags(&["a", "b", "c", "d"])),
            ("specialties".into(), tags(&["w", "x", "y", "z"])),
        ],
    )
    .unwrap();
    db.insert_node("Person", "t2", vec![("id".into(), Value::Str("t2".into()))])
        .unwrap();
    for (key, industries, specialties) in [
        ("acme", &["a", "b", "c", "d"][..], &["w", "x", "y", "z"][..]),
        ("zeta", &["a", "b", "c"], &["w", "x", "y", "z"]),
        ("beta", &["a", "b", "c"], &["w", "x", "y"]),
        ("echo", &["a", "b"], &["w", "x"]),
        ("gamma", &["a"], &["w", "x", "y", "z"]),
        ("delta", &["a", "b", "c", "d"], &["w"]),
        ("foxtrot", &["a", "b", "c", "d"], &["q", "r"]),
    ] {
        db.insert_node(
            "Org",
            key,
            vec![
                ("industries".into(), tags(industries)),
                ("specialties".into(), tags(specialties)),
            ],
        )
        .unwrap();
    }
    db.create_rule(overlap("industry_align", "industries", "INDUSTRY"))
        .unwrap();
    db.create_rule(overlap("specialty_match", "specialties", "SPECIALTY"))
        .unwrap();
    db.insert_edge("KNOWS", "t1", "t2").unwrap();
    db
}

fn tid(id: &str) -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("tid".into(), Value::Str(id.into()));
    p
}

fn f(x: f64) -> Value {
    Value::Float(x)
}

fn s(x: &str) -> Value {
    Value::Str(x.into())
}

fn rows_of(rs: &ResultSet) -> Vec<Vec<Option<Value>>> {
    (0..rs.len()).map(|i| rs.row(i).to_vec()).collect()
}

#[test]
fn dogfood_shaped_scored_query_orders_by_rule_weights() {
    let db = open_fixture("dogfood");
    let rs = db
        .query(
            "\
MATCH (t:Person {id: $tid})
MATCH (c:Org)-[i:INDUSTRY]->(t)
MATCH (c)-[s:SPECIALTY]->(t)
WHERE i.score >= 0.5 AND s.score >= 0.5
RETURN c, i.score AS industry, s.score AS specialty
ORDER BY industry DESC, specialty DESC
LIMIT 10",
            &tid("t1"),
        )
        .expect("dogfood-shaped query");
    assert_eq!(
        rs.columns(),
        &[
            "c".to_string(),
            "industry".to_string(),
            "specialty".to_string()
        ]
    );
    // industry DESC, specialty DESC. gamma (.25 ind) / delta (.25 spec) /
    // foxtrot (no SPECIALTY edge) dropped by WHERE / second MATCH.
    assert_eq!(
        rows_of(&rs),
        vec![
            vec![Some(s("acme")), Some(f(1.0)), Some(f(1.0))],
            vec![Some(s("zeta")), Some(f(0.75)), Some(f(1.0))],
            vec![Some(s("beta")), Some(f(0.75)), Some(f(0.75))],
            vec![Some(s("echo")), Some(f(0.5)), Some(f(0.5))],
        ]
    );
}

#[test]
fn props_map_param_query_resolves_bound_node() {
    let db = open_fixture("props-param");
    let hit = db
        .query("MATCH (t:Person {id: $tid}) RETURN t", &tid("t1"))
        .expect("props + param");
    assert_eq!(hit.columns(), &["t".to_string()]);
    assert_eq!(rows_of(&hit), vec![vec![Some(s("t1"))]]);

    let miss = db
        .query("MATCH (t:Person {id: $tid}) RETURN t", &tid("nope"))
        .expect("unknown param value is Ok(empty)");
    assert!(miss.is_empty());
}

#[test]
fn undirected_query_finds_both_orientations() {
    let db = open_fixture("undirected");
    let rs = db
        .query(
            "MATCH (a:Person)-[r:KNOWS]-(b:Person) RETURN a, b",
            &BTreeMap::new(),
        )
        .expect("undirected");
    assert_eq!(
        rows_of(&rs),
        vec![
            vec![Some(s("t1")), Some(s("t2"))],
            vec![Some(s("t2")), Some(s("t1"))],
        ]
    );
}

#[test]
fn syntax_error_is_query_error_with_detail() {
    let db = open_fixture("syntax");
    let err = db
        .query("MATCH (n)", &BTreeMap::new())
        .expect_err("missing RETURN is a query error");
    match &err {
        GraphError::QueryError { detail } => {
            assert!(
                detail.starts_with("parse:"),
                "syntax errors must be prefixed parse:, got: {detail}"
            );
            let d = detail.to_ascii_lowercase();
            assert!(
                d.contains("token") || d.contains("return"),
                "detail must name the failing token, got: {detail}"
            );
        }
        other => panic!("expected QueryError, got {other:?}"),
    }
    let shown = err.to_string();
    assert!(
        shown.to_ascii_lowercase().contains("query"),
        "Display must mention query, got: {shown}"
    );
}

#[test]
fn cypher_neighbors_match_grouped_by_edge_type() {
    let db = open_fixture("cross-check");
    let t1 = db.node_ref("t1").expect("t1");
    let grouped = t1.grouped_by_edge_type();
    let mut traversal = grouped
        .get("INDUSTRY")
        .cloned()
        .expect("INDUSTRY neighbors from traversal");
    traversal.sort();
    traversal.dedup();

    let rs = db
        .query(
            "MATCH (t:Person {id: $tid})-[r:INDUSTRY]-(n) RETURN n",
            &tid("t1"),
        )
        .expect("cypher neighbor fetch");
    let mut cypher: Vec<String> = (0..rs.len())
        .map(|i| match rs.get(i, "n") {
            Some(Value::Str(k)) => k.clone(),
            other => panic!("row {i} not a node key: {other:?}"),
        })
        .collect();
    cypher.sort();
    cypher.dedup();
    assert_eq!(traversal, cypher);
}

#[test]
fn query_stage_prefixes_lex_and_execute() {
    let db = open_fixture("stage-prefix");
    match db.query("@", &BTreeMap::new()) {
        Err(GraphError::QueryError { detail }) => {
            assert!(
                detail.starts_with("lex:"),
                "lex errors must be prefixed lex:, got: {detail}"
            );
        }
        other => panic!("expected QueryError, got {other:?}"),
    }
    match db.query("MATCH (t:Person {id: $tid}) RETURN t", &BTreeMap::new()) {
        Err(GraphError::QueryError { detail }) => {
            assert!(
                detail.starts_with("execute:"),
                "execute errors must be prefixed execute:, got: {detail}"
            );
            assert!(
                detail.contains("tid"),
                "missing-param execute error must name the parameter, got: {detail}"
            );
        }
        other => panic!("expected QueryError, got {other:?}"),
    }
}
