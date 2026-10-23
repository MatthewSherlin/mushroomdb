//! QL-03e: `elementId`, `startNode`, `endNode`, `nodes`, `relationships` and
//! `length`, through the full lex/parse/plan/execute path.
//!
//! `elementId` is the one definition Bolt sends as a node's or relationship's
//! element id; `nodes(p)`, `relationships(p)` and `length(p)` read what `p.nodes`,
//! `p.relationships` and `p.length` read, kind included; `startNode(r)` and
//! `endNode(r)` are nodes.

use super::tests::{run, Fx};
use crate::cypher::attach_entities;
use crate::result::{Entity, EntityKind, ResultSet};
use crate::visible::VisibleSet;
use crate::{node_element_id, relationship_element_id, GraphView};
use core_storage::Value;
use std::collections::BTreeMap;

fn s(v: &str) -> Value {
    Value::Str(v.into())
}

fn strs(xs: &[&str]) -> Value {
    Value::List(xs.iter().map(|x| s(x)).collect())
}

fn rel(etype: &str, start: &str, end: &str) -> Value {
    Value::Map(BTreeMap::from([
        ("type".to_owned(), s(etype)),
        ("start".to_owned(), s(start)),
        ("end".to_owned(), s(end)),
    ]))
}

/// ada -K-> bob -K-> cam; ada -OWNS-> sec.
fn graph() -> Fx {
    let mut fx = Fx::new();
    let ada = fx.add("Person", "ada", vec![]);
    let bob = fx.add("Person", "bob", vec![]);
    let cam = fx.add("Person", "cam", vec![]);
    let sec = fx.add("Secret", "sec", vec![]);
    fx.edge("K", ada, bob, vec![]);
    fx.edge("K", bob, cam, vec![]);
    fx.edge("OWNS", ada, sec, vec![]);
    fx
}

fn result(view: &GraphView, q: &str) -> ResultSet {
    run(view, q, &BTreeMap::new()).unwrap_or_else(|e| panic!("`{q}` failed: {e}"))
}

fn column(view: &GraphView, q: &str) -> Vec<Option<Value>> {
    let rs = result(view, q);
    (0..rs.len()).map(|i| rs.row(i)[0].clone()).collect()
}

fn one(view: &GraphView, q: &str) -> Option<Value> {
    let mut col = column(view, q);
    assert_eq!(col.len(), 1, "`{q}` gave {col:?}");
    col.remove(0)
}

fn err(view: &GraphView, q: &str) -> String {
    run(view, q, &BTreeMap::new()).expect_err("must fail")
}

#[test]
fn element_id_of_a_node_is_the_bolt_element_id() {
    let fx = graph();
    let view = fx.view();
    assert_eq!(
        one(&view, "MATCH (n {id: 'ada'}) RETURN elementId(n)"),
        Some(s(&node_element_id("ada")))
    );
    assert_eq!(node_element_id("ada"), "n:ada");
}

#[test]
fn element_id_of_a_relationship_is_the_bolt_element_id() {
    let fx = graph();
    let view = fx.view();
    assert_eq!(
        one(&view, "MATCH ({id: 'ada'})-[r:K]->() RETURN elementId(r)"),
        Some(s(&relationship_element_id("K", "ada", "bob")))
    );
}

#[test]
fn function_names_are_case_insensitive() {
    let fx = graph();
    let view = fx.view();
    assert_eq!(
        one(&view, "MATCH (n {id: 'ada'}) RETURN ELEMENTID(n)"),
        Some(s("n:ada"))
    );
}

#[test]
fn start_node_and_end_node_give_the_end_keys() {
    let fx = graph();
    let view = fx.view();
    let q = "MATCH (a)-[r:K]->(b) RETURN startNode(r) AS s, endNode(r) AS e ORDER BY s";
    let rs = result(&view, q);
    let rows: Vec<_> = (0..rs.len()).map(|i| rs.row(i).to_vec()).collect();
    assert_eq!(
        rows,
        vec![
            vec![Some(s("ada")), Some(s("bob"))],
            vec![Some(s("bob")), Some(s("cam"))]
        ]
    );
}

#[test]
fn start_node_and_end_node_are_node_columns() {
    let fx = graph();
    let view = fx.view();
    let mut rs = result(
        &view,
        "MATCH (a)-[r:OWNS]->(b) RETURN startNode(r), endNode(r)",
    );
    assert_eq!(rs.column_kind(0), Some(EntityKind::Node));
    assert_eq!(rs.column_kind(1), Some(EntityKind::Node));
    attach_entities(&view, &mut rs);
    let Some(Entity::Node(start)) = rs.entity(0, 0) else {
        panic!("no start node entity")
    };
    let Some(Entity::Node(end)) = rs.entity(0, 1) else {
        panic!("no end node entity")
    };
    assert_eq!((start.key.as_str(), end.key.as_str()), ("ada", "sec"));
}

#[test]
fn start_node_and_end_node_are_null_when_that_end_is_masked() {
    let fx = graph();
    let visible: VisibleSet = ["ada", "bob", "cam"]
        .iter()
        .map(|k| fx.ids.get(k).unwrap())
        .collect();
    let view = GraphView {
        mask: Some(&visible),
        ..fx.view()
    };
    // A masked end never binds, so no row carries it; the function's own
    // mask check is the second line of defence.
    let rows = column(&view, "MATCH (a)-[r]->(b) RETURN endNode(r)");
    assert!(
        !rows.contains(&Some(s("sec"))),
        "a masked end must not appear: {rows:?}"
    );
}

#[test]
fn nodes_of_a_path_is_the_keys_in_walk_order() {
    let fx = graph();
    let view = fx.view();
    assert_eq!(
        one(&view, "MATCH p = ({id: 'ada'})-[:K]->(b) RETURN nodes(p)"),
        Some(strs(&["ada", "bob"]))
    );
}

#[test]
fn relationships_of_a_path_is_the_maps_in_path_order() {
    let fx = graph();
    let view = fx.view();
    assert_eq!(
        one(
            &view,
            "MATCH p = ({id: 'ada'})-[:K*2]->(c) RETURN relationships(p)"
        ),
        Some(Value::List(vec![
            rel("K", "ada", "bob"),
            rel("K", "bob", "cam")
        ]))
    );
}

#[test]
fn length_of_a_path_is_its_hop_count() {
    let fx = graph();
    let view = fx.view();
    let mut got = column(
        &view,
        "MATCH p = ({id: 'ada'})-[:K*1..2]->(b) RETURN length(p) ORDER BY length(p)",
    );
    got.sort_by_key(|v| match v {
        Some(Value::Int(n)) => *n,
        _ => -1,
    });
    assert_eq!(got, vec![Some(Value::Int(1)), Some(Value::Int(2))]);
}

#[test]
fn the_functions_equal_the_path_fields() {
    let fx = graph();
    let view = fx.view();
    let q = "MATCH p = ({id: 'ada'})-[:K*1..2]->(b) RETURN ";
    for (func, field) in [
        ("nodes(p)", "p.nodes"),
        ("relationships(p)", "p.relationships"),
        ("length(p)", "p.length"),
    ] {
        assert_eq!(
            column(&view, &format!("{q}{func}")),
            column(&view, &format!("{q}{field}")),
            "{func} vs {field}"
        );
    }
}

#[test]
fn nodes_and_relationships_are_list_columns_with_entities() {
    let fx = graph();
    let view = fx.view();
    let mut rs = result(
        &view,
        "MATCH p = ({id: 'ada'})-[:K]->(b) RETURN nodes(p), relationships(p), length(p)",
    );
    assert_eq!(rs.column_kind(0), Some(EntityKind::NodeList));
    assert_eq!(rs.column_kind(1), Some(EntityKind::RelationshipList));
    assert_eq!(rs.column_kind(2), None);
    attach_entities(&view, &mut rs);
    let Some(Entity::List(nodes)) = rs.entity(0, 0) else {
        panic!("no node list entity")
    };
    assert_eq!(nodes.len(), 2);
    assert!(matches!(nodes[0], Some(Entity::Node(_))));
    let Some(Entity::List(rels)) = rs.entity(0, 1) else {
        panic!("no relationship list entity")
    };
    assert!(matches!(rels[0], Some(Entity::Relationship(_))));
}

#[test]
fn null_in_gives_null_out() {
    let fx = graph();
    let view = fx.view();
    for q in [
        "RETURN nodes(null)",
        "RETURN relationships(null)",
        "RETURN length(null)",
        "RETURN elementId(null)",
        "RETURN startNode(null)",
        "RETURN endNode(null)",
    ] {
        assert_eq!(one(&view, q), None, "{q}");
    }
}

#[test]
fn an_unmatched_optional_binding_gives_null() {
    let fx = graph();
    let view = fx.view();
    for f in ["elementId(n)", "elementId(r)", "startNode(r)", "endNode(r)"] {
        let q = format!("MATCH (a {{id: 'cam'}}) OPTIONAL MATCH (a)-[r:K]->(n) RETURN {f}");
        assert_eq!(one(&view, &q), None, "{q}");
    }
    let q = "MATCH (a {id: 'cam'}) OPTIONAL MATCH p = (a)-[:K]->(n) RETURN nodes(p), length(p)";
    let rs = result(&view, q);
    assert_eq!(rs.row(0), &[None, None]);
}

#[test]
fn a_wrong_type_is_an_error_naming_the_function() {
    let fx = graph();
    let view = fx.view();
    let cases = [
        ("MATCH (n {id: 'ada'}) RETURN nodes(n)", "nodes()"),
        (
            "MATCH (n {id: 'ada'}) RETURN relationships(n)",
            "relationships()",
        ),
        ("MATCH (n {id: 'ada'}) RETURN length(n)", "length()"),
        ("RETURN length(5)", "length()"),
        ("RETURN elementId('x')", "elementId()"),
        ("MATCH (n {id: 'ada'}) RETURN startNode(n)", "startNode()"),
        ("MATCH (n {id: 'ada'}) RETURN endNode(n)", "endNode()"),
        (
            "MATCH p = ({id: 'ada'})-[:K]->(b) RETURN elementId(p)",
            "elementId()",
        ),
    ];
    for (q, name) in cases {
        let e = err(&view, q);
        assert!(e.contains(name), "`{q}` gave `{e}`, want it to name {name}");
    }
}

#[test]
fn arity_is_checked() {
    let fx = graph();
    let view = fx.view();
    for (q, name) in [
        ("RETURN elementId()", "elementId()"),
        ("RETURN nodes(1, 2)", "nodes()"),
        ("RETURN length()", "length()"),
    ] {
        let e = err(&view, q);
        assert!(e.contains(name) && e.contains("exactly 1"), "`{q}`: {e}");
    }
}

#[test]
fn the_functions_are_listed_as_supported() {
    let fx = graph();
    let view = fx.view();
    let unknown = err(&view, "RETURN nosuchfn(1)");
    for f in [
        "elementId",
        "startNode",
        "endNode",
        "nodes",
        "relationships",
        "length",
    ] {
        assert!(unknown.contains(f), "SCALAR_FUNCS lacks {f}: {unknown}");
    }
}

#[test]
fn id_still_returns_the_key() {
    let fx = graph();
    let view = fx.view();
    assert_eq!(
        one(&view, "MATCH (n {id: 'ada'}) RETURN id(n)"),
        Some(s("ada"))
    );
}
