//! QL-08: `MERGE (a:L {id: 'x'})-[:T]->(b:M {id: 'y'})` parses to a
//! `MergeStmt` with `edge: Some(MergeEdgePat)`.

use core_query::cypher::{is_write_tokens, lex, parse_write, MergeStmt, RelDir, WriteStatement};
use core_storage::Value;

fn parse(q: &str) -> Result<WriteStatement, String> {
    parse_write(&lex(q).unwrap())
}

fn merge(q: &str) -> MergeStmt {
    match parse(q).unwrap() {
        WriteStatement::Merge(s) => s,
        other => panic!("expected Merge, got {other:?}"),
    }
}

#[test]
fn a_single_node_merge_has_no_edge() {
    assert_eq!(merge("MERGE (a:P {id: 'a'})").edge, None);
}

#[test]
fn a_right_arrow_pattern_fills_the_edge() {
    let s = merge("MERGE (a:P {id: 'a'})-[r:K]->(b:Q {id: 'b', ns: 't'})");
    assert_eq!(s.label, "P");
    assert_eq!(s.var.as_deref(), Some("a"));
    let e = s.edge.expect("edge");
    assert_eq!(e.etype, "K");
    assert_eq!(e.dir, RelDir::Right);
    assert_eq!(e.end_label, "Q");
    assert_eq!(e.end_key_field, "id");
    assert_eq!(e.end_key_value, Value::Str("b".into()));
    assert_eq!(e.end_ns, Some(Value::Str("t".into())));
    assert_eq!(e.end_var.as_deref(), Some("b"));
    assert_eq!(e.rel_var.as_deref(), Some("r"));
}

#[test]
fn a_left_arrow_pattern_keeps_its_direction() {
    let e = merge("MERGE (a:P {id: 'a'})<-[:K]-(b:P {id: 'b'})")
        .edge
        .unwrap();
    assert_eq!(e.dir, RelDir::Left);
    assert_eq!(e.end_var.as_deref(), Some("b"));
    assert_eq!(e.rel_var, None);
}

#[test]
fn on_clauses_and_return_follow_the_pattern() {
    let s = merge(
        "MERGE (a:P {id: 'a'})-[:K]->(b:P {id: 'b'}) \
         ON CREATE SET a.n = 1, b.n = 2 ON MATCH SET a.m = 1 RETURN a, b",
    );
    assert_eq!(s.on_create.len(), 2);
    assert_eq!(s.on_create[1].var, "b");
    assert_eq!(s.on_match.len(), 1);
    assert_eq!(s.returns.map(|r| r.len()), Some(2));
    assert!(s.edge.is_some());
}

#[test]
fn keywords_are_case_insensitive_and_the_statement_is_a_write() {
    let q = "merge (a:P {id: 'a'})-[:K]->(b:P {id: 'b'}) on create set a.n = 1";
    assert!(merge(q).edge.is_some());
    assert!(is_write_tokens(&lex(q).unwrap()));
}

#[test]
fn unsupported_patterns_are_refused_by_name() {
    for (q, want) in [
        (
            "MERGE (a:P {id: 'a'})-[:K]-(b:P {id: 'b'})",
            "not supported in mushroomdb 0.10: MERGE with an undirected relationship; \
             give it a direction (-[:T]-> or <-[:T]-)",
        ),
        (
            "MERGE (a:P {id: 'a'})-[:K*1..3]->(b:P {id: 'b'})",
            "not supported in mushroomdb 0.10: MERGE with a variable-length relationship",
        ),
        (
            "MERGE (a:P {id: 'a'})-[:K|J]->(b:P {id: 'b'})",
            "not supported in mushroomdb 0.10: MERGE with alternative relationship types",
        ),
        (
            "MERGE (a:P {id: 'a'})-[:K]->(b:P {id: 'b'})-[:K]->(c:P {id: 'c'})",
            "not supported in mushroomdb 0.10: MERGE with more than one relationship",
        ),
    ] {
        assert_eq!(parse(q).unwrap_err(), want, "{q}");
    }
}

#[test]
fn malformed_patterns_are_errors() {
    for q in [
        "MERGE (a:P {id: 'a'})-[:K]->",
        "MERGE (a:P {id: 'a'})-[]->(b:P {id: 'b'})",
        "MERGE (a:P {id: 'a'})-[:K $props]->(b:P {id: 'b'})",
    ] {
        assert!(parse(q).is_err(), "{q}");
    }
    // QL-NM: an end node with no label, no property or several properties
    // matches by what it names.
    for q in [
        "MERGE (a:P {id: 'a'})-[:K]->(b)",
        "MERGE (a:P {id: 'a'})-[:K]->(b:P)",
        "MERGE (a:P {id: 'a'})-[:K]->(b:P {id: 'b', x: 1})",
    ] {
        assert!(parse(q).is_ok(), "{q}");
    }
    // QL-RP: a relationship map is part of what MERGE matches.
    assert!(parse("MERGE (a:P {id: 'a'})-[:K {w: 1}]->(b:P {id: 'b'})").is_ok());
}
