//! QL-12: `CALL ns.name(args) [YIELD …] [WHERE …] [RETURN …]`, and the
//! openCypher forms this release recognises and refuses by name.

use super::tests::Fx;
use crate::cypher::ast::{ProcCallStmt, YieldItem};
use crate::cypher::proc::{execute_call, ProcCatalog, SchemaRow};
use crate::cypher::{
    is_call_tokens, is_write_tokens, lex, parse_call, parse_read, parse_write, Operand, Params,
};
use crate::result::ResultSet;
use core_storage::Value;
use std::collections::BTreeMap;

/// A catalog with nothing in it: `db.ping` reads none of it.
struct EmptyCatalog;

impl ProcCatalog for EmptyCatalog {
    fn labels(&self) -> Vec<String> {
        Vec::new()
    }
    fn relationship_types(&self) -> Vec<String> {
        Vec::new()
    }
    fn property_keys(&self) -> Vec<String> {
        Vec::new()
    }
    fn indexes(&self) -> Vec<SchemaRow> {
        Vec::new()
    }
    fn constraints(&self) -> Vec<SchemaRow> {
        Vec::new()
    }
}

/// The error any statement gives, routed as the read and write entry points
/// route it: a call, a write, or a read.
fn statement_err(q: &str) -> String {
    let toks = match lex(q) {
        Ok(toks) => toks,
        Err(e) => return e,
    };
    if is_call_tokens(&toks) {
        parse_call(&toks).expect_err(q)
    } else if is_write_tokens(&toks) {
        parse_write(&toks).map(|_| ()).expect_err(q)
    } else {
        parse_read(&toks).map(|_| ()).expect_err(q)
    }
}

fn call(q: &str) -> ProcCallStmt {
    parse_call(&lex(q).unwrap()).unwrap_or_else(|e| panic!("{q}: {e}"))
}

fn run_call(q: &str, params: &BTreeMap<String, Value>) -> Result<ResultSet, String> {
    let fx = Fx::new();
    let toks = lex(q).unwrap();
    assert!(is_call_tokens(&toks), "{q}");
    let stmt = parse_call(&toks)?;
    execute_call(&fx.view(), &stmt, &EmptyCatalog, &Params(params))
}

fn rows(rs: &ResultSet) -> Vec<Vec<Option<Value>>> {
    (0..rs.len()).map(|i| rs.row(i).to_vec()).collect()
}

const NS: &str = "not supported in mushroomdb 0.10: ";

// ── Refusals ────────────────────────────────────────────────────────────────

#[test]
fn the_plan_s_four_refusals_are_exact() {
    for (q, want) in [
        ("FOREACH (x IN [1,2] | CREATE (n:X {id:x}))", "FOREACH"),
        (
            "MATCH (n) WHERE EXISTS { (n)-[:K]->() } RETURN n",
            "EXISTS { } subquery",
        ),
        ("CALL { MATCH (n) RETURN n }", "CALL subquery"),
        ("MATCH (n) REMOVE n:Label", "REMOVE n:Label (label removal)"),
    ] {
        assert_eq!(statement_err(q), format!("{NS}{want}"), "{q}");
    }
}

#[test]
fn call_with_a_brace_is_a_subquery_and_with_a_name_is_a_procedure() {
    // Pinned disambiguation (Review Focus 5).
    assert_eq!(
        statement_err("CALL { MATCH (n) RETURN n }"),
        format!("{NS}CALL subquery")
    );
    assert_eq!(
        statement_err("CALL (n) { RETURN n }"),
        format!("{NS}CALL subquery")
    );
    assert_eq!(
        call("CALL db.labels()"),
        ProcCallStmt {
            ns: "db".into(),
            name: "labels".into(),
            args: vec![],
            yields: None,
            where_expr: None,
            ret: None,
        }
    );
}

#[test]
fn each_refused_form_is_named_wherever_it_stands() {
    for (q, want) in [
        // FOREACH after a MATCH, on the write path.
        ("MATCH (n) FOREACH (x IN [1] | SET n.a = x)", "FOREACH"),
        // EXISTS { } in RETURN; COUNT { } and COLLECT { } subqueries.
        (
            "MATCH (n) RETURN EXISTS { (n)-->() } AS e",
            "EXISTS { } subquery",
        ),
        (
            "MATCH (n) RETURN COUNT { (n)-->() } AS c",
            "COUNT { } subquery",
        ),
        (
            "MATCH (n) WHERE COUNT { (n)-->() } > 1 RETURN n",
            "COUNT { } subquery",
        ),
        (
            "MATCH (n) RETURN COLLECT { MATCH (n)-->(m) RETURN m } AS c",
            "COLLECT { } subquery",
        ),
        // CALL { } after another clause.
        (
            "MATCH (n) CALL { WITH n RETURN 1 AS x } RETURN x",
            "CALL subquery",
        ),
        // A procedure call inside a larger query, before or after it.
        (
            "MATCH (n) CALL db.labels() YIELD label RETURN label",
            "CALL inside a larger query",
        ),
        (
            "MATCH (n) WITH n CALL db.ping() RETURN 1",
            "CALL inside a larger query",
        ),
        (
            "UNWIND [1] AS x CALL db.ping() RETURN x",
            "CALL inside a larger query",
        ),
        (
            "MATCH (n) RETURN n CALL db.ping()",
            "CALL inside a larger query",
        ),
        (
            "CALL db.ping() MATCH (n) RETURN n",
            "CALL inside a larger query",
        ),
        (
            "CALL db.ping() YIELD success RETURN success UNION RETURN true AS success",
            "CALL inside a larger query",
        ),
        // Labels are not written or tested in expressions.
        ("MATCH (n) SET n:Label", "SET n:Label (label assignment)"),
        (
            "MATCH (n) SET n.x = 1, n:Label",
            "SET n:Label (label assignment)",
        ),
        (
            "MATCH (n) WHERE n:Person RETURN n",
            "a label predicate (n:Label) in an expression; put the label in the pattern",
        ),
        (
            "MATCH (n) RETURN n:Person AS p",
            "a label predicate (n:Label) in an expression; put the label in the pattern",
        ),
        (
            "MATCH (n:A:B) RETURN n",
            "more than one label (n:A:B, n:A|B); a node has one label",
        ),
        (
            "MATCH (n:A|B) RETURN n",
            "more than one label (n:A:B, n:A|B); a node has one label",
        ),
        // Patterns as expressions.
        (
            "MATCH (n) WHERE (n)-[:K]->() RETURN n",
            "pattern predicate (a relationship pattern used as an expression)",
        ),
        (
            "MATCH (n) WHERE NOT (n)-->() RETURN n",
            "pattern predicate (a relationship pattern used as an expression)",
        ),
        (
            "MATCH (n) WHERE (n)<-[:K]-(:M {a: 1}) RETURN n",
            "pattern predicate (a relationship pattern used as an expression)",
        ),
        (
            "MATCH (n) WHERE exists((n)-[:K]->()) RETURN n",
            "pattern predicate (a relationship pattern used as an expression)",
        ),
        (
            "MATCH (n) RETURN size((n)-->()) AS d",
            "pattern predicate (a relationship pattern used as an expression)",
        ),
        (
            "MATCH (n) RETURN [(n)-->(m) | m.name] AS names",
            "pattern comprehension [(n)-->(m) | …]",
        ),
        // Projections.
        ("MATCH (n) RETURN *", "RETURN * / WITH * (name each item)"),
        (
            "MATCH (n) RETURN DISTINCT *",
            "RETURN * / WITH * (name each item)",
        ),
        (
            "MATCH (n) WITH * RETURN n",
            "RETURN * / WITH * (name each item)",
        ),
        (
            "MATCH (n) RETURN n {.name, .age} AS m",
            "map projection (n {.key, …})",
        ),
        // Clauses and statements.
        (
            "LOAD CSV FROM 'file:///x.csv' AS row RETURN row",
            "LOAD CSV",
        ),
        (
            "LOAD CSV WITH HEADERS FROM 'file:///x.csv' AS row RETURN row",
            "LOAD CSV",
        ),
        ("USE other MATCH (n) RETURN n", "USE (database selection)"),
        (
            "OPTIONAL MATCH (n) RETURN n",
            "OPTIONAL MATCH as the first clause; begin with MATCH",
        ),
        (
            "MATCH (a), (b) CREATE (a)-[:K]->(b)",
            "CREATE or MERGE after a MATCH clause; use UNWIND [1] AS _ MATCH … CREATE … for now",
        ),
        (
            "MATCH (a {id: 'x'}) MERGE (a)-[:K]->(b:B {id: 'y'})",
            "CREATE or MERGE after a MATCH clause; use UNWIND [1] AS _ MATCH … CREATE … for now",
        ),
        (
            "MATCH p = allShortestPaths((a)-[*]-(b)) RETURN p",
            "allShortestPaths",
        ),
        // The lexer refuses the regular-expression operator.
        (
            "MATCH (n) WHERE n.name =~ 'a.*' RETURN n",
            "regular-expression match (=~)",
        ),
    ] {
        assert_eq!(statement_err(q), format!("{NS}{want}"), "{q}");
    }
}

#[test]
fn a_refused_form_never_panics_in_any_casing() {
    for q in [
        "foreach (x in [1] | create (n:X {id: x}))",
        "match (n) where exists { (n)-->() } return n",
        "call { match (n) return n }",
        "Call Db.Ping() Match (n) Return n",
    ] {
        assert!(statement_err(q).starts_with(NS), "{q}");
    }
}

#[test]
fn the_refused_words_still_name_things_where_they_parse() {
    // The refusals look only at a failure point: these parse as before.
    for q in [
        "MATCH (call {name: 'a'}) RETURN call",
        "MATCH (foreach) RETURN foreach.exists AS yield",
        "MATCH (n:Call) WHERE n.call = 1 AND n.yield = 2 RETURN n.exists",
        "MATCH (n) RETURN exists(n.name) AS e, count(n) AS c",
        "MATCH (n) WHERE (n.x) - 1 > 0 RETURN n",
        "MATCH (n) WHERE (n.x) < -1 RETURN n",
        "UNWIND [{call: 1, yield: 2}] AS load RETURN load.call, load.yield",
        "MATCH (n)-[:K|:J]->(m) RETURN m",
        "MATCH (n) RETURN [x IN [1, 2] | x] AS l",
        "MATCH (n) WHERE n.s = 'a=~b' RETURN n",
    ] {
        let toks = lex(q).unwrap_or_else(|e| panic!("{q}: {e}"));
        assert!(!is_call_tokens(&toks), "{q}");
        assert!(parse_read(&toks).is_ok(), "{q}: {:?}", parse_read(&toks));
    }
}

// ── Parsing CALL ────────────────────────────────────────────────────────────

#[test]
fn call_parses_its_name_arguments_and_yield() {
    let stmt = call("CALL db.schema.visualization()");
    assert_eq!(
        (stmt.ns.as_str(), stmt.name.as_str()),
        ("db.schema", "visualization")
    );
    // Parentheses are optional when there are no arguments.
    assert_eq!(call("CALL db.ping"), call("CALL db.ping()"));
    let stmt = call("call db.ping(1, $x, 'a' + 'b') yield success AS ok, other");
    assert_eq!(stmt.args.len(), 3);
    assert_eq!(stmt.args[1], Operand::Param("x".into()));
    assert_eq!(
        stmt.yields,
        Some(vec![
            YieldItem {
                field: "success".into(),
                alias: Some("ok".into()),
            },
            YieldItem {
                field: "other".into(),
                alias: None,
            },
        ])
    );
    assert_eq!(call("CALL db.ping() YIELD *").yields, None);
    let stmt = call("CALL db.ping() YIELD success WHERE success RETURN success AS s LIMIT 1");
    assert!(stmt.where_expr.is_some());
    assert!(stmt.ret.is_some());
}

#[test]
fn a_malformed_call_names_what_it_expected() {
    for (q, want) in [
        ("CALL ping()", "namespace.name"),
        ("CALL", "namespace.name"),
        ("CALL db.ping(1", "procedure arguments"),
        ("CALL db.ping() YIELD", "column name after YIELD"),
        ("CALL db.ping() YIELD a, a", "YIELD binds `a` twice"),
        (
            "CALL db.ping() YIELD a AS b, c AS b",
            "YIELD binds `b` twice",
        ),
        ("CALL db.ping() garbage", "unexpected tokens after CALL"),
    ] {
        let err = statement_err(q);
        assert!(err.contains(want), "{q}: {err}");
    }
}

#[test]
fn only_a_leading_call_is_a_call_statement() {
    assert!(is_call_tokens(&lex("CALL db.ping()").unwrap()));
    assert!(is_call_tokens(&lex("call { }").unwrap()));
    assert!(!is_call_tokens(&lex("MATCH (call) RETURN call").unwrap()));
    assert!(!is_write_tokens(&lex("CALL db.ping()").unwrap()));
}

// ── Running CALL ────────────────────────────────────────────────────────────

#[test]
fn db_ping_answers_one_row() {
    let rs = run_call("CALL db.ping()", &BTreeMap::new()).unwrap();
    assert_eq!(rs.columns(), ["success"]);
    assert_eq!(rows(&rs), vec![vec![Some(Value::Bool(true))]]);
}

#[test]
fn yield_where_and_return_shape_the_rows() {
    let p = BTreeMap::new();
    let rs = run_call("CALL db.ping() YIELD success AS ok RETURN ok", &p).unwrap();
    assert_eq!(rs.columns(), ["ok"]);
    assert_eq!(rows(&rs), vec![vec![Some(Value::Bool(true))]]);
    // YIELD without RETURN returns the yielded names.
    let rs = run_call("CALL db.ping() YIELD success AS ok", &p).unwrap();
    assert_eq!(rs.columns(), ["ok"]);
    assert_eq!(rs.len(), 1);
    // WHERE filters; the columns stay.
    let rs = run_call("CALL db.ping() YIELD success WHERE success = false", &p).unwrap();
    assert_eq!(rs.columns(), ["success"]);
    assert!(rs.is_empty());
    // RETURN takes expressions and aggregates over the yielded names.
    let rs = run_call(
        "CALL db.ping() YIELD success RETURN count(*) AS n, NOT success AS down",
        &p,
    )
    .unwrap();
    assert_eq!(rs.len(), 1);
    assert_eq!(rs.get(0, "n"), Some(&Value::Int(1)));
    assert_eq!(rs.get(0, "down"), Some(&Value::Bool(false)));
    let rs = run_call("CALL db.ping() YIELD * RETURN success", &p).unwrap();
    assert_eq!(rs.columns(), ["success"]);
    // A name the YIELD renamed is gone under its old name.
    let err = run_call("CALL db.ping() YIELD success AS ok RETURN success", &p).unwrap_err();
    assert!(err.contains("success"), "{err}");
}

#[test]
fn yield_of_an_unknown_column_names_it() {
    let err = run_call("CALL db.ping() YIELD nope", &BTreeMap::new()).unwrap_err();
    assert!(err.contains("`nope`"), "{err}");
    assert!(err.contains("success"), "lists what it returns: {err}");
}

#[test]
fn an_unknown_procedure_is_an_error_naming_it() {
    let err = run_call("CALL unknown.proc()", &BTreeMap::new()).unwrap_err();
    assert_eq!(err, "unknown procedure: unknown.proc");
    // Names are case-sensitive.
    let err = run_call("CALL DB.PING()", &BTreeMap::new()).unwrap_err();
    assert_eq!(err, "unknown procedure: DB.PING");
}

#[test]
fn arguments_are_evaluated_parameters_included() {
    let err = run_call("CALL db.ping($x)", &BTreeMap::new()).unwrap_err();
    assert!(err.contains("missing parameter"), "{err}");
    let p = BTreeMap::from([("x".to_string(), Value::Int(1))]);
    let err = run_call("CALL db.ping($x, 1 + 1)", &p).unwrap_err();
    assert_eq!(err, "db.ping takes no arguments, got 2");
    // An argument reads no variable.
    let err = run_call("CALL db.ping(n.x)", &BTreeMap::new()).unwrap_err();
    assert!(err.contains("unbound variable"), "{err}");
}

#[test]
fn on_match_is_not_a_match_clause() {
    // MERGE … ON MATCH SET parses as on the base commit.
    for q in [
        "MERGE (a:P {id: 'a'}) ON MATCH SET a.n = 1",
        "MERGE (a:P {id: 'a'}) ON CREATE SET a.n = 0 ON MATCH SET a.n = 1 RETURN a.n",
    ] {
        assert!(parse_write(&lex(q).unwrap()).is_ok(), "{q}");
    }
    // A failure after ON MATCH is not reported as MATCH … CREATE.
    let err = statement_err("MERGE (a:P {id: 'a'}) ON MATCH SET a.n = 1 CREATE (b:P {id: 'q'})");
    assert!(!err.contains("after a MATCH clause"), "{err}");
}

#[test]
fn the_pattern_lookahead_claims_only_relationship_shapes() {
    // A list subtraction that is not a relationship shape still parses.
    for q in [
        "MATCH (x) RETURN (x.v) - [1] AS d",
        "MATCH (x) RETURN (x) - [1] AS d",
        "MATCH (x) RETURN (x) - 1 AS d",
    ] {
        assert!(parse_read(&lex(q).unwrap()).is_ok(), "{q}");
    }
    // `(x) - [y]` reads as a relationship and is refused (it was a type error
    // at run time on the base commit).
    assert_eq!(
        statement_err("MATCH (x) RETURN (x) - [y] AS d"),
        format!("{NS}pattern predicate (a relationship pattern used as an expression)")
    );
}
