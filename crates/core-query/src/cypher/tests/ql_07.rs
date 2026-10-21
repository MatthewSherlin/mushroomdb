//! QL-07: named path variables (`MATCH p = (a)-[r]->(b)`), run through the
//! full lex/parse/plan/execute path.
//!
//! A path is a query-layer value: it renders as the map
//! `{nodes, relationships, length}` (see `exec::path_value`), so it needs no
//! new `Value` variant and renders on every surface as any map does.

use super::tests::{run, Fx};
use crate::cypher::plan::{plan, PlanOp};
use crate::cypher::{lex, parse};
use crate::view::GraphView;
use crate::visible::VisibleSet;
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

fn path(nodes: &[&str], rels: Vec<Value>) -> Value {
    Value::Map(BTreeMap::from([
        ("nodes".to_owned(), strs(nodes)),
        ("length".to_owned(), Value::Int(rels.len() as i64)),
        ("relationships".to_owned(), Value::List(rels)),
    ]))
}

fn compile_err(q: &str) -> String {
    match parse(&lex(q).expect("lex")) {
        Err(e) => e,
        Ok(stmt) => plan(&stmt).expect_err("must fail to plan"),
    }
}

fn col_in(view: &GraphView, q: &str, name: &str) -> Vec<Option<Value>> {
    let rs = run(view, q, &BTreeMap::new()).unwrap_or_else(|e| panic!("`{q}` failed: {e}"));
    (0..rs.len()).map(|i| rs.get(i, name).cloned()).collect()
}

fn col(fx: &Fx, q: &str, name: &str) -> Vec<Option<Value>> {
    col_in(&fx.view(), q, name)
}

/// ada -KNOWS-> bob -KNOWS-> cam -KNOWS-> dee, ada -LIKES-> acme, bob -WORKS_AT-> acme.
fn chain() -> Fx {
    let mut fx = Fx::new();
    let ada = fx.add("Person", "ada", vec![]);
    let bob = fx.add("Person", "bob", vec![]);
    let cam = fx.add("Person", "cam", vec![]);
    let dee = fx.add("Person", "dee", vec![]);
    let acme = fx.add("Company", "acme", vec![]);
    fx.edge("KNOWS", ada, bob, vec![]);
    fx.edge("KNOWS", bob, cam, vec![]);
    fx.edge("KNOWS", cam, dee, vec![]);
    fx.edge("LIKES", ada, acme, vec![]);
    fx.edge("WORKS_AT", bob, acme, vec![]);
    fx
}

#[test]
fn one_hop_path_is_a_value_with_nodes_and_relationships() {
    let fx = chain();
    let q = "MATCH p = (a:Person {id: 'x'})-[:KNOWS]->(b:Person) RETURN p";
    assert!(col(&fx, q, "p").is_empty());
    let q = "MATCH p = (a:Person)-[:KNOWS]->(b:Person) RETURN p ORDER BY b.id";
    let got = col(&fx, q, "p");
    assert_eq!(got.len(), 3);
    assert_eq!(
        got[0],
        Some(path(&["ada", "bob"], vec![rel("KNOWS", "ada", "bob")]))
    );
}

#[test]
fn path_properties_length_nodes_relationships() {
    let fx = chain();
    let q = "MATCH p = (a:Person)-[:KNOWS]->(b:Person) RETURN p.length AS n";
    assert_eq!(col(&fx, q, "n"), vec![Some(Value::Int(1)); 3]);
    let q = "MATCH p = (a:Person)-[:KNOWS]->(b:Person) RETURN p.nodes AS n ORDER BY a.id";
    assert_eq!(
        col(&fx, q, "n"),
        vec![
            Some(strs(&["ada", "bob"])),
            Some(strs(&["bob", "cam"])),
            Some(strs(&["cam", "dee"]))
        ]
    );
    let q = "MATCH p = (a:Person)-[:KNOWS]->(b:Person) RETURN p.relationships AS r ORDER BY a.id";
    assert_eq!(
        col(&fx, q, "r")[0],
        Some(Value::List(vec![rel("KNOWS", "ada", "bob")]))
    );
}

#[test]
fn variable_length_path_carries_every_node_and_edge() {
    let fx = chain();
    let q = "MATCH p = (a:Person)-[:KNOWS*1..3]->(b:Person) RETURN p.length AS n ORDER BY n";
    assert_eq!(
        col(&fx, q, "n"),
        [1, 1, 1, 2, 2, 3].map(|n| Some(Value::Int(n))).to_vec()
    );
    let q = "MATCH p = (a:Person)-[:KNOWS*3]->(b:Person) RETURN p";
    // `*3` is min=max=3: one path, ada to dee.
    assert_eq!(
        col(&fx, q, "p"),
        vec![Some(path(
            &["ada", "bob", "cam", "dee"],
            vec![
                rel("KNOWS", "ada", "bob"),
                rel("KNOWS", "bob", "cam"),
                rel("KNOWS", "cam", "dee")
            ]
        ))]
    );
}

#[test]
fn the_start_node_binding_still_works_next_to_the_path() {
    let fx = chain();
    let q = "MATCH p = (a:Person)-[:KNOWS*2]->(b:Person) RETURN a, b, p.length AS n ORDER BY a";
    assert_eq!(col(&fx, q, "a"), vec![Some(s("ada")), Some(s("bob"))]);
    assert_eq!(col(&fx, q, "b"), vec![Some(s("cam")), Some(s("dee"))]);
}

#[test]
fn a_multi_hop_pattern_concatenates_its_hops() {
    let fx = chain();
    // Fixed hop, then a variable-length hop, then a fixed hop.
    let q = "MATCH p = (a:Person)-[:KNOWS]->(b)-[:KNOWS*1..2]->(c)-[:KNOWS]->(d) \
             RETURN p";
    assert_eq!(col(&fx, q, "p").len(), 1, "only ada to dee takes 1 + 1 + 1");
    let q = "MATCH p = (a:Person)-[:KNOWS]->(b)-[:KNOWS*1..2]->(c) RETURN p.nodes AS n \
             ORDER BY p.length, a.id";
    let got = col(&fx, q, "n");
    assert_eq!(
        got,
        vec![
            Some(strs(&["ada", "bob", "cam"])),
            Some(strs(&["bob", "cam", "dee"])),
            Some(strs(&["ada", "bob", "cam", "dee"])),
        ]
    );
    let q = "MATCH p = (a:Person)-[:KNOWS]->(b)-[:WORKS_AT]->(c:Company) RETURN p";
    assert_eq!(
        col(&fx, q, "p"),
        vec![Some(path(
            &["ada", "bob", "acme"],
            vec![rel("KNOWS", "ada", "bob"), rel("WORKS_AT", "bob", "acme")]
        ))]
    );
}

#[test]
fn an_incoming_hop_reports_edges_in_their_stored_direction() {
    let fx = chain();
    let q = "MATCH p = (b:Person)<-[:KNOWS]-(a:Person {id: 'ada'}) RETURN p";
    assert_eq!(
        col(&fx, q, "p"),
        vec![Some(path(
            &["bob", "ada"],
            vec![rel("KNOWS", "ada", "bob")]
        ))]
    );
    let q = "MATCH (d:Person {id: 'dee'}) \
             MATCH p = (a:Person {id: 'bob'})-[:KNOWS*1..2]-(d) RETURN p.length AS n";
    assert_eq!(col(&fx, q, "n"), vec![Some(Value::Int(2))]);
}

#[test]
fn a_path_ending_at_an_already_bound_node_runs_the_walk_in_pattern_order() {
    let fx = chain();
    // The reversed-expand plan starts at `b` but the path still reads a -> b.
    let q = "MATCH (b:Person {id: 'bob'}) MATCH p = (a:Person)-[:KNOWS]->(b) RETURN p";
    assert_eq!(
        col(&fx, q, "p"),
        vec![Some(path(
            &["ada", "bob"],
            vec![rel("KNOWS", "ada", "bob")]
        ))]
    );
}

#[test]
fn a_zero_hop_path_is_one_node() {
    let fx = chain();
    let q = "MATCH p = (a:Company) RETURN p";
    assert_eq!(col(&fx, q, "p"), vec![Some(path(&["acme"], vec![]))]);
}

#[test]
fn a_named_shortest_path() {
    let fx = chain();
    let q = "MATCH (a:Person {id: 'ada'}) MATCH (d:Person {id: 'dee'}) \
             MATCH p = shortestPath((a)-[:KNOWS*..5]->(d)) RETURN p";
    assert_eq!(
        col(&fx, q, "p"),
        vec![Some(path(
            &["ada", "bob", "cam", "dee"],
            vec![
                rel("KNOWS", "ada", "bob"),
                rel("KNOWS", "bob", "cam"),
                rel("KNOWS", "cam", "dee")
            ]
        ))]
    );
}

#[test]
fn a_path_survives_with_and_aggregates() {
    let fx = chain();
    let q = "MATCH p = (a:Person)-[:KNOWS*1..3]->(b:Person) WITH p, a RETURN a, p.length AS n \
             ORDER BY n, a";
    assert_eq!(col(&fx, q, "n").len(), 6);
    let q = "MATCH p = (a:Person)-[:KNOWS*1..3]->(b:Person) RETURN count(p) AS c";
    assert_eq!(col(&fx, q, "c"), vec![Some(Value::Int(6))]);
    let q = "MATCH p = (a:Person)-[:KNOWS*1..3]->(b:Person) RETURN a, count(p) AS c ORDER BY a";
    assert_eq!(
        col(&fx, q, "c"),
        vec![
            Some(Value::Int(3)),
            Some(Value::Int(2)),
            Some(Value::Int(1))
        ]
    );
    let q = "MATCH p = (a:Person)-[:KNOWS*1..3]->(b:Person) RETURN collect(p) AS ps";
    let Some(Value::List(ps)) = &col(&fx, q, "ps")[0] else {
        panic!("collect gives a list")
    };
    assert_eq!(ps.len(), 6);
    let q = "MATCH p = (a:Person)-[:KNOWS*1..3]->(b:Person) RETURN DISTINCT p";
    assert_eq!(col(&fx, q, "p").len(), 6);
}

#[test]
fn order_by_a_path_follows_the_map_order_so_shorter_paths_come_first() {
    let fx = chain();
    let q = "MATCH p = (a:Person)-[:KNOWS*1..3]->(b:Person) RETURN p ORDER BY p";
    let lengths: Vec<i64> = col(&fx, q, "p")
        .into_iter()
        .map(|v| match v {
            Some(Value::Map(m)) => match m["length"] {
                Value::Int(n) => n,
                _ => panic!("length is an int"),
            },
            other => panic!("not a path: {other:?}"),
        })
        .collect();
    assert_eq!(lengths, vec![1, 1, 1, 2, 2, 3]);
    let q = "MATCH p = (a:Person)-[:KNOWS*1..3]->(b:Person) RETURN p ORDER BY p DESC LIMIT 1";
    let Some(Value::Map(m)) = &col(&fx, q, "p")[0] else {
        panic!("a path")
    };
    assert_eq!(m["length"], Value::Int(3));
}

#[test]
fn optional_match_with_a_named_path_is_null_when_nothing_matches() {
    let fx = chain();
    let q = "MATCH (a:Person) OPTIONAL MATCH p = (a)-[:LIKES]->(c:Company) \
             RETURN a, p ORDER BY a";
    let got = col(&fx, q, "p");
    assert_eq!(got.len(), 4);
    assert_eq!(
        got[0],
        Some(path(&["ada", "acme"], vec![rel("LIKES", "ada", "acme")]))
    );
    assert_eq!(&got[1..], &[None, None, None]);
}

#[test]
fn a_path_name_is_in_scope_for_where() {
    let fx = chain();
    let q = "MATCH p = (a:Person)-[:KNOWS*1..3]->(b:Person) WHERE p.length >= 3 RETURN b";
    assert_eq!(col(&fx, q, "b"), vec![Some(s("dee"))]);
}

#[test]
fn a_hidden_end_node_suppresses_the_whole_row() {
    let fx = chain();
    let ids =
        |keys: &[&str]| -> VisibleSet { keys.iter().map(|k| fx.ids.get(k).unwrap()).collect() };
    // `bob` is hidden: no path to or through him may appear.
    let visible = ids(&["ada", "cam", "dee", "acme"]);
    let view = GraphView {
        mask: Some(&visible),
        ..fx.view()
    };
    let q = "MATCH p = (a)-[:KNOWS]->(b) RETURN p";
    assert_eq!(
        col_in(&view, q, "p"),
        vec![Some(path(
            &["cam", "dee"],
            vec![rel("KNOWS", "cam", "dee")]
        ))]
    );
    // A variable-length walk cannot pass through the hidden node either.
    let q = "MATCH p = (a:Person {id: 'ada'})-[:KNOWS*1..3]->(b) RETURN p";
    assert!(col_in(&view, q, "p").is_empty());
    // And no field of a hidden node is reachable through the path.
    let q = "MATCH p = (a)-[:KNOWS*1..3]->(b) RETURN p.nodes AS n";
    for row in col_in(&view, q, "n") {
        let Some(Value::List(nodes)) = row else {
            panic!("a list")
        };
        assert!(!nodes.contains(&s("bob")), "{nodes:?}");
    }
}

#[test]
fn a_hidden_start_node_suppresses_the_row_too() {
    let fx = chain();
    let visible: VisibleSet = ["bob", "cam"]
        .iter()
        .map(|k| fx.ids.get(k).unwrap())
        .collect();
    let view = GraphView {
        mask: Some(&visible),
        ..fx.view()
    };
    let q = "MATCH p = (a)-[:KNOWS]->(b) RETURN p.nodes AS n";
    assert_eq!(col_in(&view, q, "n"), vec![Some(strs(&["bob", "cam"]))]);
}

#[test]
fn the_plan_binds_the_path_after_the_last_hop() {
    let ops =
        plan(&parse(&lex("MATCH p = (a)-[:T]->(b)-[:T*1..2]->(c) RETURN p").unwrap()).unwrap())
            .unwrap();
    let bind = ops
        .iter()
        .position(|op| matches!(op, PlanOp::BindPath { .. }))
        .expect("a BindPath op");
    assert!(ops[..bind].iter().any(|op| matches!(
        op,
        PlanOp::VarExpand {
            trail_var: Some(_),
            ..
        }
    )));
    assert!(matches!(ops[bind + 1], PlanOp::Project { .. }));
    // A pattern with no name emits none, and keeps no trail.
    let ops = plan(&parse(&lex("MATCH (a)-[:T*1..2]->(c) RETURN a").unwrap()).unwrap()).unwrap();
    assert!(!ops.iter().any(|op| matches!(op, PlanOp::BindPath { .. })));
    assert!(ops.iter().all(|op| !matches!(
        op,
        PlanOp::VarExpand {
            trail_var: Some(_),
            ..
        }
    )));
}

#[test]
fn a_path_is_not_a_node_or_a_relationship() {
    let fx = chain();
    for q in [
        "MATCH p = (a:Person)-[:KNOWS]->(b) RETURN type(p)",
        "MATCH p = (a:Person)-[:KNOWS]->(b) RETURN key(p)",
        "MATCH p = (a:Person)-[:KNOWS]->(b) RETURN labels(p)",
    ] {
        let err = run(&fx.view(), q, &BTreeMap::new()).expect_err(q);
        assert!(err.contains("`p`"), "{q}: {err}");
    }
}

#[test]
fn a_path_name_cannot_be_a_literal_or_missing() {
    assert!(compile_err("MATCH true = (a)-[:T]->(b) RETURN a").contains("boolean literal"));
    assert!(compile_err("MATCH p = RETURN 1").contains("parse error"));
    assert!(compile_err("MATCH p = (a)-[:T]->(b) RETURN q").contains("`q`"));
}

#[test]
fn an_equality_in_a_where_is_not_taken_for_a_path_name() {
    // `a = 1` sits in WHERE, after the pattern; only a pattern start binds.
    let fx = chain();
    let q = "MATCH (a:Person) WHERE a.id = 'ada' RETURN a";
    assert_eq!(col(&fx, q, "a"), vec![Some(s("ada"))]);
}

#[test]
fn distinct_and_grouping_tell_lists_and_maps_apart() {
    // Before QL-07 every list and map shared the null group, which a path
    // (rendered as a map) cannot live with.
    let fx = chain();
    let q = "UNWIND [[1], [2], [1], [1.0]] AS x RETURN DISTINCT x";
    assert_eq!(col(&fx, q, "x").len(), 2);
    let q = "UNWIND [{a: 1}, {a: 2}, {a: 1}] AS x RETURN x, count(*) AS n ORDER BY n";
    assert_eq!(
        col(&fx, q, "n"),
        vec![Some(Value::Int(1)), Some(Value::Int(2))]
    );
}

#[test]
fn the_parser_records_the_path_name_on_the_pattern() {
    let parse_q = |q: &str| parse(&lex(q).unwrap()).unwrap();
    let named = parse_q("MATCH p = (a)-[:T]->(b) RETURN p");
    assert_eq!(named.matches[0].path_var.as_deref(), Some("p"));
    let unnamed = parse_q("MATCH (a)-[:T]->(b) RETURN a");
    assert_eq!(unnamed.matches[0].path_var, None);
    let shortest =
        parse_q("MATCH (a) MATCH (b) MATCH q = shortestPath((a)-[:T*..3]->(b)) RETURN q");
    assert!(shortest.matches[2].shortest);
    assert_eq!(shortest.matches[2].path_var.as_deref(), Some("q"));
    let optional = parse_q("MATCH (a) OPTIONAL MATCH r = (a)-[:T]->(b) RETURN r");
    assert_eq!(
        optional.optional_clauses[0].patterns[0].path_var.as_deref(),
        Some("r")
    );
    let two = parse_q("MATCH p = (a)-[:T]->(b), q = (b)-[:T]->(c) RETURN p");
    assert_eq!(two.matches[1].path_var.as_deref(), Some("q"));
}
