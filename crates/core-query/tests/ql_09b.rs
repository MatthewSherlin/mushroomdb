//! QL-09b: clauses after `UNWIND` parse, both as read queries (`UNWIND … MATCH
//! … RETURN`) and as write statements (`UNWIND … CREATE | MERGE | SET | REMOVE |
//! DELETE`), and every refusal is named.

use core_query::cypher::{
    is_write_tokens, lex, parse, parse_write, Operand, RowWrite, UnwindWriteStmt, WriteStatement,
};
use core_storage::Value;

fn write(q: &str) -> Result<UnwindWriteStmt, String> {
    match parse_write(&lex(q).unwrap())? {
        WriteStatement::UnwindWrite(s) => Ok(*s),
        other => panic!("expected UnwindWrite, got {other:?}"),
    }
}

fn is_write(q: &str) -> bool {
    is_write_tokens(&lex(q).unwrap())
}

#[test]
fn a_create_after_unwind_keeps_the_read_part_and_one_write_clause() {
    let s = write("UNWIND $rows AS r CREATE (n:Person {id: r.id, name: r.name})").unwrap();
    assert_eq!(s.read.unwinds.len(), 1);
    assert_eq!(s.read.unwinds[0].alias, "r");
    assert!(s.read.stages.is_empty());
    assert_eq!(s.scope, vec!["r".to_string()]);
    assert_eq!(s.writes.len(), 1);
    let RowWrite::Create { nodes, edges } = &s.writes[0] else {
        panic!("expected Create")
    };
    assert!(edges.is_empty());
    assert_eq!(nodes[0].var, "n");
    assert_eq!(nodes[0].label, "Person");
    assert_eq!(
        nodes[0].props[0],
        (
            "id".to_string(),
            Operand::Prop {
                var: "r".into(),
                field: "id".into()
            }
        )
    );
}

#[test]
fn the_statement_names_every_node_variable_including_the_ones_the_writes_bind() {
    let s = write(
        "UNWIND $rows AS r MATCH (m:A {id: r.id}) \
         CREATE (n:B {id: r.b}) MERGE (k:C {id: r.c}) SET n.x = m.x + 1",
    )
    .unwrap();
    assert_eq!(s.scope, vec!["r".to_string(), "m".to_string()]);
    assert_eq!(
        s.node_vars,
        vec!["m".to_string(), "n".to_string(), "k".to_string()]
    );
}

#[test]
fn match_after_unwind_becomes_a_stage_that_carries_the_alias() {
    let s = write("UNWIND $ids AS id MATCH (n {id: id}) WHERE n.x > 1 SET n.y = id").unwrap();
    assert_eq!(s.read.stages.len(), 1);
    let stage = &s.read.stages[0];
    assert_eq!(stage.items.len(), 1);
    assert_eq!(stage.matches.len(), 1);
    assert!(stage.post_where.is_some());
    assert_eq!(s.scope, vec!["id".to_string(), "n".to_string()]);
}

#[test]
fn a_read_query_may_match_after_unwind() {
    let q = parse(&lex("UNWIND $ids AS id MATCH (n:P {id: id}) RETURN n").unwrap()).unwrap();
    assert_eq!(q.unwinds.len(), 1);
    assert_eq!(q.stages.len(), 1);
    assert_eq!(q.stages[0].matches.len(), 1);
    assert!(
        parse(&lex("UNWIND $ids AS id OPTIONAL MATCH (n:P {id: id}) RETURN n").unwrap()).is_ok()
    );
    assert!(
        parse(&lex("UNWIND $a AS x WITH x WHERE x > 1 MATCH (n {id: x}) RETURN n").unwrap())
            .is_ok()
    );
}

#[test]
fn every_write_clause_form_parses() {
    for q in [
        "UNWIND $r AS x CREATE (a:A {id: x})-[:K]->(b:B {id: x})",
        "UNWIND $r AS x MATCH (a {id: x.a}), (b {id: x.b}) CREATE (a)-[:K]->(b)",
        "UNWIND $r AS x MERGE (a:A {id: x.id})",
        "UNWIND $r AS x MERGE (a:A {id: x.id}) ON CREATE SET a.n = x.n ON MATCH SET a.m = x.m",
        "UNWIND $r AS x MERGE (a:A {id: x.a})-[:K]->(b:A {id: x.b})",
        "UNWIND $r AS x MATCH (n {id: x.id}) SET n.a = x.a, n.b = 1",
        "UNWIND $r AS x MATCH (n {id: x.id}) SET n += x",
        "UNWIND $r AS x MATCH (n {id: x.id}) SET n = {a: x.a}",
        "UNWIND $r AS x MATCH (n {id: x.id}) REMOVE n.a, n.b",
        "UNWIND $r AS x MATCH (n {id: x.id}) DELETE n",
        "UNWIND $r AS x MATCH (n {id: x.id}) DETACH DELETE n",
        "UNWIND $r AS x CREATE (n:A {id: x}) SET n.a = 1 RETURN n",
        "UNWIND $r AS x WITH x.id AS i CREATE (:A {id: i})",
        "UNWIND $r AS x CREATE (:A {id: x}) RETURN count(*) AS c",
        "unwind $r as x create (:A {id: x})",
    ] {
        assert!(write(q).is_ok(), "{q}: {:?}", write(q).err());
    }
}

#[test]
fn merge_takes_a_row_value_as_its_key_and_still_allows_one_key_only() {
    let s = write("UNWIND $r AS x MERGE (a:A {id: x.id, ns: x.ns})").unwrap();
    let RowWrite::Merge(m) = &s.writes[0] else {
        panic!("expected Merge")
    };
    assert_eq!(m.start.key_field, "id");
    assert!(m.start.ns.is_some());
    // QL-NM: a second property makes it a pattern matched by every property.
    let s = write("UNWIND $r AS x MERGE (a:A {id: x.id, name: x.n})").unwrap();
    let RowWrite::Merge(m) = &s.writes[0] else {
        panic!("expected Merge")
    };
    assert!(!m.start.key_pattern);
    assert_eq!(m.start.props.len(), 2);
}

#[test]
fn write_statements_are_recognised_by_their_clauses() {
    assert!(is_write("UNWIND $r AS x CREATE (:A {id: x})"));
    assert!(is_write("UNWIND $r AS x MERGE (:A {id: x})"));
    assert!(is_write("UNWIND $r AS x MATCH (n) SET n.a = 1"));
    assert!(is_write("UNWIND $r AS x MATCH (n) DETACH DELETE n"));
    assert!(is_write("UNWIND $r AS x MATCH (n) REMOVE n.a"));
    assert!(!is_write("UNWIND $r AS x RETURN x"));
    assert!(!is_write(
        "UNWIND $r AS x MATCH (n {id: x}) RETURN n.remove"
    ));
}

#[test]
fn refusals_are_named() {
    let e = write("UNWIND [1] AS x CREATE (:A {id: 'a'}) MATCH (n) SET n.a = 1").unwrap_err();
    assert!(e.contains("after a write clause"), "{e}");
    let e = write("UNWIND [1] AS x CREATE (:A {id: 'a'}) WITH x SET x.a = 1").unwrap_err();
    assert!(e.contains("after a write clause"), "{e}");
    let e = write("UNWIND [1] AS x MATCH (a)-[r:K]->(b) DELETE r").unwrap_err();
    assert!(e.contains("DELETE of the relationship"), "{e}");
    let e = write("UNWIND [1] AS x MATCH (n) REMOVE n:Label").unwrap_err();
    assert!(e.contains("label removal"), "{e}");
    // QL-NM: `(a)` with `a` new is a new node with no label; a value is not.
    assert!(write("UNWIND [1] AS x CREATE (a)").is_ok());
    let e = write("UNWIND [1] AS x CREATE (x)").unwrap_err();
    assert!(e.contains("not a node"), "{e}");
    let e = write("UNWIND [1] AS x MATCH (n) SET x.a = 1").unwrap_err();
    assert!(e.contains("is a value, not a node variable"), "{e}");
    let e = write("UNWIND [1] AS x SET q.a = 1").unwrap_err();
    assert!(e.contains("not bound"), "{e}");
    let e = write("UNWIND [1] AS x MATCH (n) SET n += {a: 1}, n.b = 2").unwrap_err();
    assert!(
        e.contains("SET map form followed by another SET item"),
        "{e}"
    );
    let e = write("UNWIND [1] AS x MATCH (n) CREATE (n:A {id: 'a'})").unwrap_err();
    assert!(e.contains("already bound"), "{e}");
    let e = write("UNWIND [1] AS x RETURN x").unwrap_err();
    assert!(e.contains("expected CREATE, MERGE"), "{e}");
}

#[test]
fn literal_values_and_null_parse_as_operands() {
    let s = write("UNWIND [1] AS x CREATE (:A {id: 'a', n: 1, ok: true, z: null})").unwrap();
    let RowWrite::Create { nodes, .. } = &s.writes[0] else {
        panic!()
    };
    assert_eq!(nodes[0].props[1].1, Operand::Lit(Value::Int(1)));
    assert_eq!(nodes[0].props[3].1, Operand::Lit(Value::Null));
}
