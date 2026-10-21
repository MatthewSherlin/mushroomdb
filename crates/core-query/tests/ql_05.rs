//! QL-05: `MATCH … REMOVE var.prop [, …]` parses to `WriteStatement::MatchRemove`.

use core_query::cypher::{is_write_tokens, lex, parse_write, WriteStatement};

fn parse(q: &str) -> Result<WriteStatement, String> {
    parse_write(&lex(q).unwrap())
}

#[test]
fn remove_items_parse_in_source_order() {
    let WriteStatement::MatchRemove(s) =
        parse("MATCH (n:P)-[:T]->(m:P) WHERE n.id = 'a' REMOVE n.x, m.y RETURN n.id").unwrap()
    else {
        panic!("expected MatchRemove");
    };
    assert_eq!(
        s.removals,
        vec![("n".into(), "x".into()), ("m".into(), "y".into())]
    );
    assert!(s.where_expr.is_some());
    assert_eq!(s.returns.map(|r| r.len()), Some(1));
}

#[test]
fn remove_keyword_is_case_insensitive() {
    for q in ["MATCH (n) remove n.x", "MATCH (n) ReMoVe n.x"] {
        assert!(
            matches!(parse(q), Ok(WriteStatement::MatchRemove(_))),
            "{q}"
        );
    }
}

#[test]
fn label_removal_is_refused_by_name() {
    for q in ["MATCH (n) REMOVE n:Label", "MATCH (n) REMOVE n.x, n:Other"] {
        assert_eq!(
            parse(q).unwrap_err(),
            "not supported in mushroomdb 0.10: REMOVE n:Label (label removal)",
            "{q}"
        );
    }
}

#[test]
fn malformed_remove_is_a_parse_error() {
    for q in [
        "MATCH (n) REMOVE",
        "MATCH (n) REMOVE n",
        "MATCH (n) REMOVE n.",
        "MATCH (n) REMOVE n.x,",
        "MATCH (n) REMOVE n.x junk",
        "MATCH (n) REMOVE z.x",
        "MATCH (a)-[r:T]->(b) REMOVE r.w",
    ] {
        assert!(parse(q).is_err(), "{q}");
    }
}

#[test]
fn is_write_tokens_sees_remove_only_in_clause_position() {
    let write = |q: &str| is_write_tokens(&lex(q).unwrap());
    assert!(write("MATCH (n) REMOVE n.x"));
    assert!(write("MATCH (n) REMOVE n:L"));
    assert!(!write("MATCH (remove:Remove) RETURN remove.remove"));
    assert!(!write(
        "MATCH (n:Remove {remove: 1}) RETURN n.remove AS remove"
    ));
    assert!(!write(
        "MATCH (n) WITH n AS remove WHERE remove.x = 1 RETURN remove"
    ));
}
