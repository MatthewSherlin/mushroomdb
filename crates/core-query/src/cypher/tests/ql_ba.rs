//! QL-BA (defect row 171): bare-arrow relationship patterns `-->`, `<--`, `--`
//! are accepted and produce the same results as their bracketed equivalents
//! `-[]->`, `<-[]-`, `-[]-`.
//!
//! Scope: MATCH, OPTIONAL MATCH, WHERE predicates (pattern-predicate refusal
//! still fires; the parser can now reach that refusal cleanly). Arithmetic
//! `a - -1` and `a - - b` are unaffected.

use super::tests::{run, Fx};
use crate::cypher::{lex, parse};
use core_storage::Value;
use std::collections::BTreeMap;

fn s(v: &str) -> Value {
    Value::Str(v.into())
}

/// `ada -[:K]-> bob -[:K]-> cam` chain, used for traversal tests.
fn chain() -> Fx {
    let mut fx = Fx::new();
    let ada = fx.add("Person", "ada", vec![]);
    let bob = fx.add("Person", "bob", vec![]);
    let cam = fx.add("Person", "cam", vec![]);
    fx.edge("K", ada, bob, vec![]);
    fx.edge("K", bob, cam, vec![]);
    fx
}

fn run_q(fx: &Fx, q: &str) -> Vec<Option<Value>> {
    let rs =
        run(&fx.view(), q, &BTreeMap::new()).unwrap_or_else(|e| panic!("`{q}` failed: {e}"));
    (0..rs.len()).map(|i| rs.row(i)[0].clone()).collect()
}

fn parse_ok(q: &str) {
    let toks = lex(q).unwrap_or_else(|e| panic!("lex `{q}`: {e}"));
    parse(&toks).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
}

fn parse_err(q: &str) -> String {
    let toks = lex(q).unwrap_or_else(|e| panic!("lex `{q}`: {e}"));
    parse(&toks).expect_err("must fail to parse")
}

// ── Parse tests ────────────────────────────────────────────────────────────────

#[test]
fn bare_right_arrow_parses() {
    parse_ok("MATCH (a)-->(b) RETURN a");
}

#[test]
fn bare_left_arrow_parses() {
    parse_ok("MATCH (a)<--(b) RETURN a");
}

#[test]
fn bare_undirected_parses() {
    parse_ok("MATCH (a)--(b) RETURN a");
}

#[test]
fn bare_arrows_with_node_labels_parse() {
    parse_ok("MATCH (a:Person)-->(b:Person) RETURN a");
    parse_ok("MATCH (a:Person)<--(b:Person) RETURN a");
    parse_ok("MATCH (a:Person)--(b:Person) RETURN a");
}

#[test]
fn bare_arrows_in_chain_parse() {
    parse_ok("MATCH (a)-->(b)-->(c) RETURN a");
    parse_ok("MATCH (a)<--(b)<--(c) RETURN a");
    parse_ok("MATCH (a)--(b)--(c) RETURN a");
}

#[test]
fn bare_arrows_in_optional_match_parse() {
    parse_ok("OPTIONAL MATCH (a)-->(b) RETURN a");
    parse_ok("OPTIONAL MATCH (a)<--(b) RETURN a");
    parse_ok("OPTIONAL MATCH (a)--(b) RETURN a");
}

// Pattern predicates in WHERE are already refused, and the parser must now
// reach that refusal cleanly for the bare forms too.
#[test]
fn bare_arrows_in_where_predicate_are_refused_as_pattern_predicate() {
    let e = parse_err("MATCH (n) WHERE NOT (n)-->() RETURN n");
    assert!(
        e.contains("pattern predicate"),
        "expected pattern-predicate refusal, got: {e}"
    );
    let e = parse_err("MATCH (n) WHERE (n)<--() RETURN n");
    assert!(
        e.contains("pattern predicate"),
        "expected pattern-predicate refusal, got: {e}"
    );
    let e = parse_err("MATCH (n) WHERE (n)--() RETURN n");
    assert!(
        e.contains("pattern predicate"),
        "expected pattern-predicate refusal, got: {e}"
    );
}

// ── Execution tests ────────────────────────────────────────────────────────────

/// `-->` reaches the same destination as `-[]->`.
#[test]
fn bare_right_arrow_matches_same_as_bracketed() {
    let fx = chain();
    let bare = run_q(&fx, "MATCH (a {id:'ada'})-->(b) RETURN b.id");
    let bracketed = run_q(&fx, "MATCH (a {id:'ada'})-[]->(b) RETURN b.id");
    assert_eq!(bare, bracketed);
    assert_eq!(bare, vec![Some(s("bob"))]);
}

/// `<--` reaches the same sources as `<-[]-`.
#[test]
fn bare_left_arrow_matches_same_as_bracketed() {
    let fx = chain();
    let bare = run_q(&fx, "MATCH (b {id:'bob'})<--(a) RETURN a.id");
    let bracketed = run_q(&fx, "MATCH (b {id:'bob'})<-[]-(a) RETURN a.id");
    assert_eq!(bare, bracketed);
    assert_eq!(bare, vec![Some(s("ada"))]);
}

/// `--` matches regardless of direction, same as `-[]-`.
#[test]
fn bare_undirected_matches_same_as_bracketed() {
    let fx = chain();
    let mut bare = run_q(&fx, "MATCH (b {id:'bob'})--(x) RETURN x.id");
    let mut bracketed = run_q(&fx, "MATCH (b {id:'bob'})-[]-(x) RETURN x.id");
    bare.sort_by_key(|v| format!("{v:?}"));
    bracketed.sort_by_key(|v| format!("{v:?}"));
    assert_eq!(bare, bracketed);
    // bob connects to ada (incoming) and cam (outgoing) — two neighbours.
    assert_eq!(bare.len(), 2);
}

/// A two-hop bare chain matches what two bracketed hops give.
#[test]
fn two_hop_bare_right_arrow_chain() {
    let fx = chain();
    let bare = run_q(&fx, "MATCH (a {id:'ada'})-->(b)-->(c) RETURN c.id");
    let bracketed = run_q(
        &fx,
        "MATCH (a {id:'ada'})-[]->(b)-[]->(c) RETURN c.id",
    );
    assert_eq!(bare, bracketed);
    assert_eq!(bare, vec![Some(s("cam"))]);
}

/// Mix bare and bracketed in one pattern.
#[test]
fn mixed_bare_and_bracketed_in_one_pattern() {
    let fx = chain();
    let mixed = run_q(&fx, "MATCH (a {id:'ada'})-->(b)-[]->(c) RETURN c.id");
    let all_bracketed = run_q(
        &fx,
        "MATCH (a {id:'ada'})-[]->(b)-[]->(c) RETURN c.id",
    );
    assert_eq!(mixed, all_bracketed);
}

// ── Ambiguity tests ─────────────────────────────────────────────────────────────

/// `a - -1` is subtraction of a negative literal, not a bare arrow.
#[test]
fn double_dash_in_arithmetic_is_subtraction_of_negative_literal() {
    parse_ok("MATCH (n) WHERE n.x - -1 > 0 RETURN n");
    // also parses as RETURN expression
    parse_ok("MATCH (n) RETURN n.x - -1 AS v");
}

/// `<-` in a less-than-negative-number comparison is not a left arrow.
#[test]
fn less_than_negative_number_is_not_a_left_arrow() {
    parse_ok("MATCH (n) WHERE n.x < -1 RETURN n");
}

/// Subtraction followed by a negative literal: `n.x - -1.5`.
#[test]
fn subtraction_of_negative_float_is_not_a_bare_arrow() {
    parse_ok("MATCH (n) RETURN n.x - -1.5 AS v");
}
