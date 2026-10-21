//! QL-06: `MATCH … SET n = {…}` / `SET n += {…}` parse to
//! `WriteStatement::MatchSetMap`.

use core_query::cypher::{is_write_tokens, lex, parse_write, Operand, WriteStatement};
use core_storage::Value;

fn parse(q: &str) -> Result<WriteStatement, String> {
    parse_write(&lex(q).unwrap())
}

fn map_stmt(q: &str) -> core_query::cypher::MatchSetMapStmt {
    match parse(q).unwrap() {
        WriteStatement::MatchSetMap(s) => s,
        other => panic!("expected MatchSetMap, got {other:?}"),
    }
}

#[test]
fn replace_and_merge_forms_set_the_merge_flag() {
    let s = map_stmt("MATCH (n:P) SET n = {a: 1}");
    assert!(!s.merge);
    assert_eq!(s.var, "n");
    assert_eq!(s.map, vec![("a".to_string(), Operand::Lit(Value::Int(1)))]);
    let s = map_stmt("MATCH (n:P) SET n += {a: 1}");
    assert!(s.merge);
}

#[test]
fn where_return_params_expressions_and_null_parse() {
    let s =
        map_stmt("MATCH (n:P) WHERE n.id = 'a' SET n += {x: $p, y: n.y + 1, z: null} RETURN n.x");
    assert!(s.where_expr.is_some());
    assert_eq!(s.returns.map(|r| r.len()), Some(1));
    let names: Vec<&str> = s.map.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(names, vec!["x", "y", "z"]);
    assert_eq!(s.map[0].1, Operand::Param("p".into()));
    assert_eq!(s.map[2].1, Operand::Lit(Value::Null));
}

#[test]
fn empty_map_and_case_insensitive_keyword_parse() {
    assert!(map_stmt("MATCH (n) set n = {}").map.is_empty());
    assert!(map_stmt("MATCH (n) SeT n += {}").merge);
}

#[test]
fn the_property_form_still_parses_as_match_set() {
    assert!(matches!(
        parse("MATCH (n) SET n.a = 1, n.b = 2"),
        Ok(WriteStatement::MatchSet(_))
    ));
}

#[test]
fn a_parameter_right_side_parses_and_other_right_sides_are_refused_by_name() {
    let s = map_stmt("MATCH (n) SET n = $props RETURN n.a");
    assert_eq!(s.param.as_deref(), Some("props"));
    assert!(s.map.is_empty() && !s.merge && s.returns.is_some());
    let s = map_stmt("MATCH (n) SET n += $props");
    assert_eq!(s.param.as_deref(), Some("props"));
    assert!(s.merge);
    assert_eq!(map_stmt("MATCH (n) SET n = {a: 1}").param, None);
    for (q, expected) in [
        (
            "MATCH (n) SET n = m",
            "not supported in mushroomdb 0.10: SET n = with a right side that is neither a map literal ({key: value, …}) nor a $parameter",
        ),
        (
            "MATCH (n) SET n += 5",
            "not supported in mushroomdb 0.10: SET n += with a right side that is neither a map literal ({key: value, …}) nor a $parameter",
        ),
    ] {
        assert_eq!(parse(q).unwrap_err(), expected, "{q}");
    }
    assert!(parse("MATCH (n) SET n = $props junk").is_err());
    assert!(parse("MATCH (n) SET n = $props, n.a = 1").is_err());
}

#[test]
fn merge_set_with_a_map_is_refused_by_name() {
    for q in [
        "MERGE (n:L {id: 'x'}) ON CREATE SET n = {a: 1}",
        "MERGE (n:L {id: 'x'}) ON MATCH SET n += $p",
        "MERGE (n:L {id: 'x'}) ON CREATE SET n.a = 1, n = {b: 2}",
    ] {
        assert_eq!(
            parse(q).unwrap_err(),
            "not supported in mushroomdb 0.10: MERGE … SET with a map",
            "{q}"
        );
    }
    assert!(parse("MERGE (n:L {id: 'x'}) ON CREATE SET n.a = 1").is_ok());
}

#[test]
fn mixed_unbound_relationship_and_malformed_forms_are_errors() {
    for q in [
        "MATCH (n) SET n = {a: 1}, n.b = 2",
        "MATCH (n) SET n = {a: 1}, m = {b: 2}",
        "MATCH (n) SET z = {a: 1}",
        "MATCH (a)-[r:T]->(b) SET r = {w: 1}",
        "MATCH (n) SET n = {a: 1",
        "MATCH (n) SET n = {a: 1,}",
        "MATCH (n) SET n = {a: 1, a: 2}",
        "MATCH (n) SET n = {a: 1} junk",
        "MATCH (n) SET n + {a: 1}",
        "MATCH (n) SET n =",
    ] {
        assert!(parse(q).is_err(), "{q}");
    }
}

#[test]
fn map_forms_are_write_statements() {
    for q in ["MATCH (n) SET n = {a: 1}", "MATCH (n) SET n += {a: 1}"] {
        assert!(is_write_tokens(&lex(q).unwrap()), "{q}");
    }
}
