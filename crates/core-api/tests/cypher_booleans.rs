//! `true` and `false` are literals in the Cypher subset.
//!
//! They used to fall through to identifier resolution, so `WHERE n.flag = true`
//! was an "unbound variable 'true'" error and the bare truthy form
//! (`WHERE n.flag`) was the only way to ask.

use mushroomdb::{GraphDb, ResultSet, Value};
use std::collections::BTreeMap;

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "graphdb-cypher-booleans-{}-{}",
        name,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn no_params() -> BTreeMap<String, Value> {
    BTreeMap::new()
}

/// Three nodes: `on` has `flag: true`, `off` has `flag: false`, `unset` has
/// no `flag` at all.
fn open_fixture(name: &str) -> GraphDb<core_storage::fs::RealFs> {
    let mut db = GraphDb::open(&tmp(name)).unwrap();
    db.insert_node("Item", "on", vec![("flag".into(), Value::Bool(true))])
        .unwrap();
    db.insert_node("Item", "off", vec![("flag".into(), Value::Bool(false))])
        .unwrap();
    db.insert_node("Item", "unset", vec![]).unwrap();
    db
}

fn keys(rs: &ResultSet) -> Vec<String> {
    (0..rs.len())
        .map(|i| match &rs.row(i)[0] {
            Some(Value::Str(s)) => s.clone(),
            other => panic!("expected a node key, got {other:?}"),
        })
        .collect()
}

fn ask(db: &GraphDb<core_storage::fs::RealFs>, cypher: &str) -> Vec<String> {
    keys(
        &db.query(cypher, &no_params())
            .unwrap_or_else(|e| panic!("{cypher}: {e}")),
    )
}

#[test]
fn where_equals_true_finds_only_the_true_node() {
    let db = open_fixture("eq-true");
    assert_eq!(
        ask(
            &db,
            "MATCH (n:Item) WHERE n.flag = true RETURN n ORDER BY n"
        ),
        vec!["on"]
    );
}

#[test]
fn where_equals_false_finds_only_the_false_node() {
    let db = open_fixture("eq-false");
    assert_eq!(
        ask(
            &db,
            "MATCH (n:Item) WHERE n.flag = false RETURN n ORDER BY n"
        ),
        vec!["off"]
    );
}

#[test]
fn where_not_equal_to_a_boolean_skips_nodes_without_the_property() {
    let db = open_fixture("ne");
    assert_eq!(
        ask(
            &db,
            "MATCH (n:Item) WHERE n.flag <> true RETURN n ORDER BY n"
        ),
        vec!["off"]
    );
    assert_eq!(
        ask(
            &db,
            "MATCH (n:Item) WHERE n.flag <> false RETURN n ORDER BY n"
        ),
        vec!["on"]
    );
}

#[test]
fn boolean_literals_are_case_insensitive() {
    let db = open_fixture("case");
    assert_eq!(
        ask(
            &db,
            "MATCH (n:Item) WHERE n.flag = TRUE RETURN n ORDER BY n"
        ),
        vec!["on"]
    );
    assert_eq!(
        ask(
            &db,
            "MATCH (n:Item) WHERE n.flag = False RETURN n ORDER BY n"
        ),
        vec!["off"]
    );
}

#[test]
fn a_boolean_literal_works_in_a_match_property_map() {
    let db = open_fixture("map");
    assert_eq!(
        ask(&db, "MATCH (n:Item {flag: true}) RETURN n ORDER BY n"),
        vec!["on"]
    );
    assert_eq!(
        ask(&db, "MATCH (n:Item {flag: false}) RETURN n ORDER BY n"),
        vec!["off"]
    );
}

#[test]
fn a_boolean_literal_can_be_returned() {
    let db = open_fixture("return");
    let rs = db
        .query(
            "MATCH (n:Item {flag: true}) RETURN true AS t, false AS f",
            &no_params(),
        )
        .unwrap();
    assert_eq!(
        rs.row(0).to_vec(),
        vec![Some(Value::Bool(true)), Some(Value::Bool(false))]
    );
}

#[test]
fn the_truthy_form_still_means_what_it_did() {
    let db = open_fixture("truthy");
    assert_eq!(
        ask(&db, "MATCH (n:Item) WHERE n.flag RETURN n ORDER BY n"),
        vec!["on"]
    );
}

#[test]
fn create_and_set_write_a_boolean_not_a_string() {
    let mut db = GraphDb::open(&tmp("write")).unwrap();
    db.query_write("CREATE (n:Item {id: 'a', flag: true})", &no_params())
        .unwrap();
    assert_eq!(
        db.query("MATCH (n:Item) RETURN n.flag", &no_params())
            .unwrap()
            .row(0)
            .to_vec(),
        vec![Some(Value::Bool(true))]
    );
    db.query_write(
        "MATCH (n:Item {flag: true}) SET n.flag = false",
        &no_params(),
    )
    .unwrap();
    assert_eq!(
        db.query("MATCH (n:Item) RETURN n.flag", &no_params())
            .unwrap()
            .row(0)
            .to_vec(),
        vec![Some(Value::Bool(false))]
    );
    assert_eq!(
        ask(&db, "MATCH (n:Item) WHERE n.flag = false RETURN n"),
        vec!["a"]
    );
}

#[test]
fn a_boolean_parameter_compares_like_the_literal() {
    let db = open_fixture("param");
    let mut params = BTreeMap::new();
    params.insert("want".to_string(), Value::Bool(false));
    let rs = db
        .query(
            "MATCH (n:Item) WHERE n.flag = $want RETURN n ORDER BY n",
            &params,
        )
        .unwrap();
    assert_eq!(keys(&rs), vec!["off"]);
}

/// Binding: `true` and `false` cannot name a variable or an alias. Where they
/// could, the name was read back as the literal in some positions and as the
/// variable in others: `WITH count(*) AS true RETURN true` answered `true`,
/// not the count, and `MATCH (true:Item) RETURN true` answered `true` a row.
#[test]
fn true_and_false_cannot_name_a_variable_or_an_alias() {
    let mut db = open_fixture("reserved");
    for cypher in [
        "MATCH (n:Item) WITH count(*) AS true RETURN true",
        "MATCH (n:Item) RETURN n.flag AS false",
        "MATCH (n:Item) RETURN count(n) AS TRUE",
        "MATCH (true:Item) RETURN true",
        "MATCH (a:Item)-[false:REL]->(b:Item) RETURN a",
        "MATCH (n:Item) UNWIND [1, 2] AS true RETURN true",
    ] {
        let err = db
            .query(cypher, &no_params())
            .expect_err(cypher)
            .to_string();
        assert!(
            err.contains("is a boolean literal and cannot name a variable or an alias"),
            "{cypher}: {err}"
        );
    }
    for cypher in [
        "CREATE (true:Item {id: 'x'})",
        "MERGE (false:Item {id: 'y'})",
    ] {
        let err = db
            .query_write(cypher, &no_params())
            .expect_err(cypher)
            .to_string();
        assert!(err.contains("is a boolean literal"), "{cypher}: {err}");
    }
    // A property or a map key of that name is not a variable.
    db.query_write("CREATE (n:Item {id: 'z', true: 1})", &no_params())
        .unwrap();
    assert_eq!(
        ask(&db, "MATCH (n:Item) WHERE n.true = 1 RETURN n"),
        vec!["z"]
    );
}

// ── Row 160: booleans order, false before true ─────────────────────────────
//
// `<`, `<=`, `>` and `>=` between two booleans gave null, so `WHERE n.flag >
// false` dropped every row, and `min` / `max` over a boolean gave null.
// openCypher orders `false` before `true`, as `ORDER BY` already did. A
// boolean and a value of another type still do not order.

#[test]
fn row_160_booleans_compare_false_before_true() {
    let mut db = open_fixture("row160-where");
    let cases = [
        ("n.flag > false", vec!["on"]),
        ("n.flag >= false", vec!["off", "on"]),
        ("n.flag < true", vec!["off"]),
        ("n.flag <= true", vec!["off", "on"]),
        ("n.flag > true", vec![]),
        ("n.flag < false", vec![]),
        // Another type does not order with a boolean.
        ("n.flag > 0", vec![]),
        ("n.flag < 'z'", vec![]),
    ];
    let check = |db: &GraphDb<core_storage::fs::RealFs>, when: &str| {
        for (cond, want) in &cases {
            // ORDER BY, and the fused scan-and-filter a lone comparison with
            // LIMIT runs.
            for tail in ["ORDER BY n", "LIMIT 10"] {
                let q = format!("MATCH (n:Item) WHERE {cond} RETURN n {tail}");
                let mut got = ask(db, &q);
                got.sort();
                assert_eq!(&got, want, "{when}: {q}");
            }
        }
    };
    check(&db, "live");
    db.snapshot().unwrap();
    check(&db, "after a snapshot");
    // A write statement's WHERE compares the same way.
    db.query_write(
        "MATCH (n:Item) WHERE n.flag > false SET n.seen = 1",
        &no_params(),
    )
    .unwrap();
    assert_eq!(
        ask(&db, "MATCH (n:Item) WHERE n.seen = 1 RETURN n ORDER BY n"),
        vec!["on"]
    );
}

#[test]
fn row_160_min_and_max_order_booleans() {
    let db = open_fixture("row160-agg");
    let one = |q: &str| {
        db.query(q, &no_params())
            .unwrap_or_else(|e| panic!("{q}: {e}"))
            .row(0)[0]
            .clone()
    };
    assert_eq!(
        one("MATCH (n:Item) RETURN max(n.flag)"),
        Some(Value::Bool(true))
    );
    assert_eq!(
        one("MATCH (n:Item) RETURN min(n.flag)"),
        Some(Value::Bool(false))
    );
    assert_eq!(
        one("MATCH (n:Item) WITH n.flag AS x RETURN max(x)"),
        Some(Value::Bool(true))
    );
    assert_eq!(
        one("MATCH (n:Item) WITH n.flag AS x RETURN min(x)"),
        Some(Value::Bool(false))
    );
    // ORDER BY already ordered them this way.
    let rs = db
        .query(
            "MATCH (n:Item) WHERE n.flag IS NOT NULL RETURN n.flag AS f ORDER BY f",
            &no_params(),
        )
        .unwrap();
    assert_eq!(
        (0..rs.len())
            .map(|i| rs.row(i)[0].clone())
            .collect::<Vec<_>>(),
        vec![Some(Value::Bool(false)), Some(Value::Bool(true))]
    );
}
