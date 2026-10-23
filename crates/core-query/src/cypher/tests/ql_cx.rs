//! QL-CX: the forms QL-12 refused by name and this unit runs — top-level
//! `MATCH … CREATE/MERGE`, `RETURN *` / `WITH *`, label predicates and a
//! leading `OPTIONAL MATCH` — as the parser reads them, and the label test
//! through the executor.

use super::tests::{run, Fx};
use crate::cypher::ast::{RetItem, RetVal, LABEL_TEST_FN, ONE_ROW_VAR};
use crate::cypher::{is_write_tokens, lex, parse, parse_read, parse_write, Operand};
use crate::cypher::{UnwindExpr, WriteStatement};
use core_storage::Value;
use std::collections::BTreeMap;

fn write_stmt(q: &str) -> WriteStatement {
    let toks = lex(q).unwrap();
    assert!(is_write_tokens(&toks), "{q}");
    parse_write(&toks).unwrap_or_else(|e| panic!("{q}: {e}"))
}

fn names(items: &[RetItem]) -> Vec<String> {
    items
        .iter()
        .map(|i| match &i.value {
            RetVal::Var(v) => v.clone(),
            other => format!("{other:?}"),
        })
        .collect()
}

#[test]
fn match_then_create_or_merge_is_the_unwind_form_with_a_hidden_alias() {
    for body in [
        "MATCH (a), (b) CREATE (a)-[:K]->(b)",
        "MATCH (a:P) WHERE a.v > 1 CREATE (c:C {id: a.id + 'c'}) SET a.done = true RETURN c.id",
        "MATCH (a {id: 'x'}) MERGE (a)-[:K]->(b:B {id: 'y'}) ON CREATE SET b.n = 1",
        "MATCH (a:P) MERGE (g:G {id: a.g}) RETURN g",
        "MATCH (a:P) WITH a MATCH (b:P) WHERE b.v = a.v CREATE (a)-[:SAME]->(b)",
    ] {
        let top = write_stmt(body);
        let unwound = write_stmt(&format!("UNWIND [1] AS _ {body}"));
        let WriteStatement::UnwindWrite(stmt) = &top else {
            panic!("{body}: {top:?}");
        };
        assert_eq!(stmt.read.unwinds[0].alias, ONE_ROW_VAR, "{body}");
        assert_eq!(
            stmt.read.unwinds[0].list,
            UnwindExpr::Lit(vec![Value::Int(1)])
        );
        // Apart from the alias, the two statements are the same.
        let renamed = format!("{unwound:?}").replace("\"_\"", &format!("{ONE_ROW_VAR:?}"));
        assert_eq!(format!("{top:?}"), renamed, "{body}");
    }
}

#[test]
fn only_a_statement_with_a_create_or_merge_clause_changes_form() {
    for q in [
        "MATCH (n) SET n.a = 1",
        "MATCH (n) DETACH DELETE n",
        "MATCH (n) REMOVE n.a",
    ] {
        assert!(
            !matches!(write_stmt(q), WriteStatement::UnwindWrite(_)),
            "{q}"
        );
    }
    assert!(!is_write_tokens(&lex("MATCH (n) RETURN n").unwrap()));
}

#[test]
fn merge_from_a_bound_node_alone_is_an_error() {
    let err = parse_write(&lex("MATCH (a) MERGE (a)").unwrap()).unwrap_err();
    assert!(err.contains("already bound"), "{err}");
}

#[test]
fn star_expands_to_the_names_in_scope_in_alphabetical_order() {
    let q = parse(&lex("MATCH p = (b)-[r]->(a) UNWIND [1] AS x RETURN *").unwrap()).unwrap();
    assert_eq!(names(&q.returns), ["a", "b", "p", "r", "x"]);
    let q = parse(&lex("MATCH (n), (m) WITH *, n.v AS v RETURN DISTINCT *").unwrap()).unwrap();
    assert_eq!(names(&q.stages[0].items)[..2], ["m", "n"]);
    assert_eq!(q.stages[0].items[2].alias.as_deref(), Some("v"));
    assert_eq!(names(&q.returns), ["m", "n", "v"]);
    assert!(q.distinct);
    // A WITH replaces the scope.
    let q = parse(&lex("MATCH (n), (m) WITH n AS k RETURN *").unwrap()).unwrap();
    assert_eq!(names(&q.returns), ["k"]);
    // The made-up names of a leading OPTIONAL MATCH and of a write are hidden.
    let q = parse(&lex("OPTIONAL MATCH (n) RETURN *").unwrap()).unwrap();
    assert_eq!(names(&q.returns), ["n"]);
    let WriteStatement::UnwindWrite(stmt) =
        write_stmt("MATCH (a) CREATE (:X {id: 'x'})-[:K]->(a) RETURN *")
    else {
        panic!()
    };
    assert_eq!(names(&stmt.returns.unwrap().items), ["a"]);
}

#[test]
fn star_after_call_takes_the_yield_names() {
    use crate::cypher::parse_call;
    let stmt = parse_call(&lex("CALL db.ping() YIELD success AS ok RETURN *").unwrap()).unwrap();
    assert_eq!(names(&stmt.ret.unwrap().items), ["ok"]);
    let err = parse_call(&lex("CALL db.ping() RETURN *").unwrap()).unwrap_err();
    assert!(err.contains("needs a YIELD list"), "{err}");
}

#[test]
fn a_label_predicate_parses_to_the_label_test() {
    let q = parse(&lex("MATCH (n) RETURN n:Person AS p, NOT n:Person").unwrap()).unwrap();
    assert_eq!(
        q.returns[0].value,
        RetVal::ScalarExpr(Operand::FuncCall {
            name: LABEL_TEST_FN.to_string(),
            args: vec![
                Operand::Var("n".into()),
                Operand::Lit(Value::Str("Person".into()))
            ],
        })
    );
    // A map key and a pattern label are not label predicates.
    for q in [
        "MATCH (n:Person {name: 'x'}) RETURN {k: n.name} AS m",
        "MATCH (n)-[r:K]->(m) WHERE n.k = 1 RETURN m",
    ] {
        assert!(parse_read(&lex(q).unwrap()).is_ok(), "{q}");
    }
}

fn graph() -> Fx {
    let mut fx = Fx::new();
    let a = fx.add("Person", "a", vec![]);
    let c = fx.add("Company", "c", vec![("label", Value::Str("Person".into()))]);
    fx.edge("WORKS_AT", a, c, vec![]);
    fx
}

fn column(fx: &Fx, q: &str, col: &str) -> Vec<Option<Value>> {
    let rs = run(&fx.view(), q, &BTreeMap::new()).unwrap_or_else(|e| panic!("{q}: {e}"));
    (0..rs.len()).map(|i| rs.get(i, col).cloned()).collect()
}

#[test]
fn the_label_test_reads_the_label_or_the_type_and_null_for_null() {
    let fx = graph();
    assert_eq!(
        column(&fx, "MATCH (n) WHERE n:Person RETURN n.key AS k", "k"),
        [Some(Value::Str("a".into()))],
        "a stored `label` property does not count"
    );
    assert_eq!(
        column(
            &fx,
            "MATCH (n) OPTIONAL MATCH (n)-[r]->(m) RETURN n.key AS k, m:Company AS t, \
             r:WORKS_AT AS w ORDER BY k",
            "t"
        ),
        [Some(Value::Bool(true)), None]
    );
    assert_eq!(
        column(
            &fx,
            "MATCH (n)-[r]->(m) RETURN r:WORKS_AT AS w, r:OTHER AS o",
            "o"
        ),
        [Some(Value::Bool(false))]
    );
    let err = run(
        &fx.view(),
        "UNWIND [1] AS x RETURN x:Person AS p",
        &BTreeMap::new(),
    )
    .unwrap_err();
    assert_eq!(err, "x:Person: `x` is not a node or a relationship");
}

#[test]
fn a_leading_optional_match_runs_over_one_row() {
    let fx = graph();
    assert_eq!(
        column(&fx, "OPTIONAL MATCH (n:Nobody) RETURN n.key AS k", "k"),
        [None]
    );
    assert_eq!(
        column(&fx, "OPTIONAL MATCH (n:Person) RETURN n.key AS k", "k"),
        [Some(Value::Str("a".into()))]
    );
}
