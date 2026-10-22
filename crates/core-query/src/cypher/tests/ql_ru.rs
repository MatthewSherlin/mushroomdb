//! QL-RU (defect-ledger row 134): within one MATCH clause, across all its
//! hops and all its comma-separated parts, no relationship is bound twice.
//! Separate MATCH clauses (and an OPTIONAL MATCH after a MATCH) do not share
//! the constraint.
//!
//! Released 0.9.x checked a variable-length hop only against itself, so
//! `MATCH (a)<-[*1..1]-(b)-[:X]->(c)` over the single edge `n3 -X-> n1`
//! returned a row that used that edge twice. It also checked a fixed hop
//! against every relationship already in the row, including those of earlier
//! clauses, so `MATCH (a)-[r]->(b) MATCH (c)-[s]->(d)` over one edge returned
//! nothing. The randomized differential lives in `crates/core-api/tests/ql_ru.rs`.

use super::tests::{run, Fx};
use core_storage::Value;
use std::collections::BTreeMap;

type Rows = Vec<Vec<Option<Value>>>;

fn s(v: &str) -> Option<Value> {
    Some(Value::Str(v.into()))
}

fn int(n: i64) -> Option<Value> {
    Some(Value::Int(n))
}

fn rows_with(fx: &Fx, q: &str, params: &BTreeMap<String, Value>) -> Rows {
    let rs = run(&fx.view(), q, params).unwrap_or_else(|e| panic!("`{q}` failed: {e}"));
    (0..rs.len()).map(|i| rs.row(i).to_vec()).collect()
}

fn rows(fx: &Fx, q: &str) -> Rows {
    rows_with(fx, q, &BTreeMap::new())
}

fn sorted(mut v: Rows) -> Rows {
    v.sort_by_key(|r| format!("{r:?}"));
    v
}

/// The reviewer's reproduction: one edge, `n3 -X-> n1`.
fn one_edge() -> Fx {
    let mut fx = Fx::new();
    let n1 = fx.add("A", "n1", vec![]);
    let n3 = fx.add("A", "n3", vec![]);
    fx.edge("X", n3, n1, vec![]);
    fx
}

/// `a -R-> b -R-> c -R-> d`; `a`, `b` and `d` are active, `c` is not.
fn chain() -> Fx {
    let mut fx = Fx::new();
    let a = fx.add("Person", "a", vec![("active", Value::Bool(true))]);
    let b = fx.add("Person", "b", vec![("active", Value::Bool(true))]);
    let c = fx.add("Person", "c", vec![("active", Value::Bool(false))]);
    let d = fx.add("Person", "d", vec![("active", Value::Bool(true))]);
    fx.edge("R", a, b, vec![]);
    fx.edge("R", b, c, vec![]);
    fx.edge("R", c, d, vec![]);
    fx
}

#[test]
fn a_variable_length_hop_and_a_fixed_hop_never_share_an_edge() {
    let fx = one_edge();
    for q in [
        // The fixed-hop form was already right.
        "MATCH (a)<-[r1]-(b)-[r2:X]->(c) RETURN a, b, c",
        // Variable-length first, fixed second.
        "MATCH (a)<-[*1..1]-(b)-[:X]->(c) RETURN a, b, c",
        "MATCH (a)<-[*]-(b)-[:X]->(c) RETURN a, b, c",
        // Fixed first, variable-length second.
        "MATCH (a)-[r]->(b)-[*1]-(c) RETURN a, b, c",
        // Two variable-length hops.
        "MATCH (a)-[*1]->(b)-[*1]-(c) RETURN a, b, c",
        "MATCH (a)-[*..2]-(b)-[*..2]-(c) RETURN a, b, c",
    ] {
        assert_eq!(rows(&fx, q), Rows::new(), "{q}");
    }
}

#[test]
fn a_walk_skips_only_the_edges_other_hops_bound() {
    let fx = chain();
    // x walks back from b over a->b only; the fixed hop out of a has no
    // other edge, so there is no row.
    let q = "MATCH (s {id: 'b'})<-[*1..2]-(x)-[:R]->(y) RETURN x, y";
    assert_eq!(rows(&fx, q), Rows::new());
    // From d the walk back reaches c, b and a, and every out-edge of those
    // is one the walk itself used.
    let q = "MATCH (s {id: 'd'})<-[w*1..3]-(x)-[:R]->(y) RETURN x, y, w.length AS n";
    assert_eq!(sorted(rows(&fx, q)), Rows::new());
    let q = "MATCH (s {id: 'c'})-[*1]->(x)<-[*1..3]-(y) RETURN x, y";
    assert_eq!(sorted(rows(&fx, q)), Rows::new());
    // Undirected: from c the walk to b leaves a->b for the fixed hop; the
    // walk to d leaves nothing.
    let q = "MATCH (s {id: 'c'})-[w*1]-(x)-[:R]-(y) RETURN x, y";
    assert_eq!(sorted(rows(&fx, q)), vec![vec![s("b"), s("a")]]);
    let q = "MATCH (s {id: 'a'})-[*1..2]->(x)-[:R]->(y) RETURN x, y";
    assert_eq!(
        sorted(rows(&fx, q)),
        vec![vec![s("b"), s("c")], vec![s("c"), s("d")]]
    );
}

#[test]
fn comma_separated_parts_of_one_match_share_the_constraint() {
    let fx = one_edge();
    for q in [
        "MATCH (a)-[r]->(b), (c)-[s]->(d) RETURN a, b, c, d",
        "MATCH (a)-[*1]->(b), (c)-[s]->(d) RETURN a, b, c, d",
        "MATCH (a)-[r]->(b), (c)-[*1..2]->(d) RETURN a, b, c, d",
        "MATCH (a)-[r]->(b), (c)-[s]-(d) RETURN a, b, c, d",
        // The second part's destination is bound, so it is planned from the
        // other end; it is still the same clause.
        "MATCH (a)-[r]->(b), (c)-[s]->(b) RETURN a, b, c",
        "MATCH (a)-[r]->(b), (a)-[s]->(c) RETURN a, b, c",
        "MATCH (a)-[*1..3]->(b), (c)-[*1..3]-(d) RETURN a, b, c, d",
    ] {
        assert_eq!(rows(&fx, q), Rows::new(), "{q}");
    }
}

#[test]
fn separate_match_clauses_do_not_share_the_constraint() {
    let fx = one_edge();
    let both = vec![vec![s("n3"), s("n1"), s("n3"), s("n1")]];
    for q in [
        "MATCH (a)-[r]->(b) MATCH (c)-[s]->(d) RETURN a, b, c, d",
        "MATCH (a)-[*1]->(b) MATCH (c)-[s]->(d) RETURN a, b, c, d",
        "MATCH (a)-[r]->(b) MATCH (c)-[*1..2]->(d) RETURN a, b, c, d",
        "MATCH (a)-[r]->(b) WITH a, b MATCH (c)-[s]->(d) RETURN a, b, c, d",
        "MATCH (a)-[r]->(b) MATCH (c)-[s]->(b) RETURN a, b, c, b AS d",
    ] {
        assert_eq!(rows(&fx, q), both, "{q}");
    }
    // Each clause is still unique within itself.
    let q = "MATCH (a)-[r]->(b) MATCH (c)-[s]->(d), (e)-[t]->(f) RETURN a";
    assert_eq!(rows(&fx, q), Rows::new());
}

#[test]
fn optional_match_is_its_own_clause() {
    let fx = one_edge();
    // The optional part may use the edge the first clause bound.
    let q = "MATCH (a)-[r]->(b) OPTIONAL MATCH (a)-[s]->(x) RETURN a, b, x";
    assert_eq!(rows(&fx, q), vec![vec![s("n3"), s("n1"), s("n1")]]);
    let q = "MATCH (a)-[r]->(b) OPTIONAL MATCH (a)-[*1..2]-(x) RETURN a, b, x";
    assert_eq!(rows(&fx, q), vec![vec![s("n3"), s("n1"), s("n1")]]);
    // Within the optional part the hops are unique among themselves.
    let q = "MATCH (a {id: 'n1'}) OPTIONAL MATCH (a)<-[*1..1]-(b)-[:X]->(c) RETURN a, b, c";
    assert_eq!(rows(&fx, q), vec![vec![s("n1"), None, None]]);
    // The reviewer's example over a -> b -> c -> d: one null row, not two rows.
    let fx = chain();
    let q = "MATCH (a {id: 'd'}) \
             OPTIONAL MATCH (a)<-[*1..3]-(b:Person {active: true})-[:R]->(c:Person) \
             RETURN a, b, c";
    assert_eq!(rows(&fx, q), vec![vec![s("d"), None, None]]);
}

#[test]
fn a_named_path_never_repeats_a_relationship() {
    let fx = one_edge();
    let q = "MATCH p = (a)<-[*1..1]-(b)-[:X]->(c) RETURN p.length AS n";
    assert_eq!(rows(&fx, q), Rows::new());
    let q = "MATCH p = (a)-[*1]->(b)-[*1]-(c) RETURN p";
    assert_eq!(rows(&fx, q), Rows::new());
    // A triangle walked by two hops: every path's relationships are distinct.
    let mut fx = Fx::new();
    let x = fx.add("A", "x", vec![]);
    let y = fx.add("A", "y", vec![]);
    let z = fx.add("A", "z", vec![]);
    fx.edge("T", x, y, vec![]);
    fx.edge("T", y, z, vec![]);
    fx.edge("T", z, x, vec![]);
    let q = "MATCH p = (a {id: 'x'})-[*1..3]-(b)-[*1..3]-(c) RETURN p.relationships AS rs";
    let got = rows(&fx, q);
    assert!(!got.is_empty());
    for row in &got {
        let Some(Value::List(rels)) = &row[0] else {
            panic!("not a list: {row:?}");
        };
        let mut seen = rels.iter().map(|r| format!("{r:?}")).collect::<Vec<_>>();
        let before = seen.len();
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), before, "a relationship repeats in {rels:?}");
        assert!(
            before <= 3,
            "a path over three edges has at most three: {rels:?}"
        );
    }
}

#[test]
fn a_relationship_list_variable_counts_only_unique_walks() {
    let fx = chain();
    // From a: one walk of each length; the fixed hop after it must be the next
    // edge along, so the walk of length 3 (which ends at d) has no row.
    let q = "MATCH (s {id: 'a'})-[w*1..3]->(x)-[r]->(y) RETURN w.length AS n, y";
    assert_eq!(
        sorted(rows(&fx, q)),
        vec![vec![int(1), s("c")], vec![int(2), s("d")]]
    );
    // Undirected: from b, the walk may not reuse the edge the fixed hop takes.
    let q = "MATCH (s {id: 'b'})-[r]-(x), (s)-[w*1..2]-(y) RETURN x, y, w.length AS n";
    assert_eq!(
        sorted(rows(&fx, q)),
        sorted(vec![
            vec![s("a"), s("c"), int(1)],
            vec![s("a"), s("d"), int(2)],
            vec![s("c"), s("a"), int(1)],
        ])
    );
}

#[test]
fn an_undirected_self_loop_is_one_relationship() {
    let mut fx = Fx::new();
    let a = fx.add("A", "a", vec![]);
    fx.edge("L", a, a, vec![]);
    let q = "MATCH (x)-[r]-(y) RETURN x, y";
    assert_eq!(rows(&fx, q), vec![vec![s("a"), s("a")]]);
    let q = "MATCH (x)-[*1..3]-(y) RETURN x, y";
    assert_eq!(rows(&fx, q), vec![vec![s("a"), s("a")]]);
    for q in [
        "MATCH (x)-[r]-(y)-[t]-(z) RETURN x",
        "MATCH (x)-[*1..2]-(y)-[t]-(z) RETURN x",
        "MATCH (x)-[r]->(y)<-[*1]-(z) RETURN x",
        "MATCH (x)-[r]->(y), (z)-[*1]-(w) RETURN x",
    ] {
        assert_eq!(rows(&fx, q), Rows::new(), "{q}");
    }
}

#[test]
fn parameters_do_not_change_the_constraint() {
    let fx = one_edge();
    let params = BTreeMap::from([("id".to_owned(), Value::Str("n1".into()))]);
    let q = "MATCH (a {id: $id})<-[*1..2]-(b)-[:X]->(c) RETURN a, b, c";
    assert_eq!(rows_with(&fx, q, &params), Rows::new());
    let q = "MATCH (a {id: $id})<-[*1..2]-(b) MATCH (b)-[:X]->(c) RETURN a, b, c";
    assert_eq!(
        rows_with(&fx, q, &params),
        vec![vec![s("n1"), s("n3"), s("n1")]]
    );
}

#[test]
fn shortest_path_avoids_an_edge_another_part_bound() {
    // a -> b directly, and a -> m -> b the long way round.
    let mut fx = Fx::new();
    let a = fx.add("A", "a", vec![]);
    let b = fx.add("A", "b", vec![]);
    let m = fx.add("A", "m", vec![]);
    fx.edge("T", a, b, vec![]);
    fx.edge("T", a, m, vec![]);
    fx.edge("T", m, b, vec![]);
    let q = "MATCH (a {id: 'a'})-[r]->(b {id: 'b'}), p = shortestPath((a)-[*..3]->(b)) \
             RETURN p.nodes AS n";
    assert_eq!(
        rows(&fx, q),
        vec![vec![Some(Value::List(vec![
            Value::Str("a".into()),
            Value::Str("m".into()),
            Value::Str("b".into()),
        ]))]]
    );
    // In its own clause it takes the direct edge.
    let q = "MATCH (a {id: 'a'})-[r]->(b {id: 'b'}) MATCH p = shortestPath((a)-[*..3]->(b)) \
             RETURN p.length AS n";
    assert_eq!(rows(&fx, q), vec![vec![int(1)]]);
}

#[test]
fn every_executor_path_applies_the_constraint() {
    // Aggregate, grouped aggregate, LIMIT (pull executor) and the staged path
    // each expand on their own.
    let fx = one_edge();
    let cases: [(&str, Rows); 8] = [
        (
            "MATCH (a)-[r]->(b), (c)-[s]->(d) RETURN count(*) AS n",
            vec![vec![int(0)]],
        ),
        (
            "MATCH (a)-[r]->(b) MATCH (c)-[s]->(d) RETURN count(*) AS n",
            vec![vec![int(1)]],
        ),
        (
            "MATCH (a)<-[*1..1]-(b)-[:X]->(c) RETURN count(*) AS n",
            vec![vec![int(0)]],
        ),
        (
            "MATCH (a)-[r]->(b), (c)-[s]->(d) RETURN a, count(*) AS n",
            Rows::new(),
        ),
        (
            "MATCH (a)-[r]->(b) MATCH (c)-[s]->(d) RETURN a, count(*) AS n",
            vec![vec![s("n3"), int(1)]],
        ),
        (
            "MATCH (a)-[r]->(b), (c)-[s]->(d) RETURN a LIMIT 5",
            Rows::new(),
        ),
        (
            "MATCH (a)-[r]->(b) MATCH (c)-[s]->(d) RETURN a LIMIT 5",
            vec![vec![s("n3")]],
        ),
        (
            "MATCH (a)<-[*1..1]-(b)-[:X]->(c) RETURN a LIMIT 5",
            Rows::new(),
        ),
    ];
    for (q, want) in cases {
        assert_eq!(rows(&fx, q), want, "{q}");
    }
}

#[test]
fn the_plan_lists_only_the_earlier_hops_of_the_same_clause() {
    use crate::cypher::plan::{plan, PlanOp};
    use crate::cypher::{lex, parse};
    let ops = plan(
        &parse(
            &lex("MATCH (a)-[r]->(b)-[*1..2]->(c), (c)-[s]->(d) MATCH (d)-[t]->(e) RETURN e")
                .unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    let hops: Vec<(Option<String>, Vec<String>)> = ops
        .iter()
        .filter_map(|op| match op {
            PlanOp::Expand { distinct_from, .. } => Some((None, distinct_from.clone())),
            PlanOp::VarExpand {
                trail_var,
                distinct_from,
                ..
            } => Some((trail_var.clone(), distinct_from.clone())),
            _ => None,
        })
        .collect();
    let names = |xs: &[&str]| xs.iter().map(|x| (*x).to_owned()).collect::<Vec<_>>();
    assert_eq!(
        hops,
        vec![
            (None, vec![]),
            // A later hop of the clause needs the walk's edges: it keeps a trail.
            (Some("_r0#trail".into()), names(&["r"])),
            (None, names(&["r", "_r0#trail"])),
            // A new clause starts with nothing to avoid.
            (None, vec![]),
        ]
    );
    // A variable-length hop with nothing after it in its clause, and no path
    // name, keeps no trail.
    let ops =
        plan(&parse(&lex("MATCH (a)-[r]->(b)-[*1..2]->(c) RETURN c").unwrap()).unwrap()).unwrap();
    assert!(ops.iter().any(|op| matches!(
        op,
        PlanOp::VarExpand { trail_var: None, distinct_from, .. } if distinct_from == &["r".to_owned()]
    )));
}

#[test]
fn a_relationship_variable_bound_by_an_earlier_clause_is_that_relationship() {
    let fx = chain();
    // A later clause that names `r` again joins on it rather than rebinding
    // it (0.9.x and the base skipped `r` itself and bound every other edge).
    let q = "MATCH (x)-[r]->(y) MATCH (a)-[r]->(b) RETURN x, y, a, b";
    assert_eq!(
        sorted(rows(&fx, q)),
        vec![
            vec![s("a"), s("b"), s("a"), s("b")],
            vec![s("b"), s("c"), s("b"), s("c")],
            vec![s("c"), s("d"), s("c"), s("d")],
        ]
    );
    let q = "MATCH (x)-[r]->(y) MATCH (a)-[r]->(b) RETURN count(*) AS n";
    assert_eq!(rows(&fx, q), vec![vec![int(3)]]);
    let q = "MATCH (x)-[r]->(y) MATCH (a)-[r]->(b) RETURN a LIMIT 10";
    assert_eq!(rows(&fx, q).len(), 3);
    // One clause cannot bind one relationship to two hops.
    let q = "MATCH (a)-[r]->(b)-[r]->(c) RETURN a";
    assert_eq!(rows(&fx, q), Rows::new());
}
