//! QL-VE: the compile-time semantic check (`check.rs`) and the runtime type
//! errors the openCypher TCK names, plus defect row 160 (booleans compare).
//!
//! Each refused query is the TCK's own (Match1, Match2, Match6, Create1,
//! MatchWhere1, ReturnOrderBy2, WithOrderBy2, With4, Return2, Union3, Graph4,
//! Graph6, Graph9, List5, List6, Map1, Path3). Every refusal must come from the
//! parse entry points, before a plan is built or a row is read, and name its
//! openCypher error kind first. The `valid_*` corpus pins the other half of the
//! contract: queries that ran before still parse.

use super::tests::{eval, eval_err, run, Fx};
use crate::cypher::{lex, parse_read, parse_write};
use core_storage::Value;
use std::collections::BTreeMap;

fn read_err(src: &str) -> String {
    match parse_read(&lex(src).unwrap_or_else(|e| panic!("lex `{src}`: {e}"))) {
        Ok(_) => panic!("`{src}` must be refused at compile time"),
        Err(e) => e,
    }
}

fn write_err(src: &str) -> String {
    match parse_write(&lex(src).unwrap_or_else(|e| panic!("lex `{src}`: {e}"))) {
        Ok(_) => panic!("`{src}` must be refused at compile time"),
        Err(e) => e,
    }
}

fn assert_kind(src: &str, kind: &str, err: &str) {
    assert!(
        err.starts_with(&format!("{kind}: ")),
        "`{src}` must be refused as {kind}, got: {err}"
    );
}

fn assert_read_refused(kind: &str, queries: &[&str]) {
    for src in queries {
        assert_kind(src, kind, &read_err(src));
    }
}

// ---------------------------------------------------------------------------
// Variables
// ---------------------------------------------------------------------------

#[test]
fn a_name_used_as_two_kinds_of_thing_is_a_type_conflict() {
    assert_read_refused(
        "VariableTypeConflict",
        &[
            // Match1 [7]: a relationship, then a node.
            "MATCH ()-[r]-() MATCH (r) RETURN r",
            "MATCH (x), (a)-[q]-(b), (s), (s)-[r]->(t)<-[]-(b) MATCH (r) RETURN r",
            // Match1 [8]: a path, then a node.
            "MATCH r = ()-[]-() MATCH (r) RETURN r",
            "MATCH (), r = ()-[*]->() MATCH (r) RETURN r",
            // Match1 [9]: a relationship and a node in one pattern.
            "MATCH ()-[r]-(r) RETURN r",
            "MATCH ()-[r*]-()-[]-(r) RETURN r",
            "MATCH (s)-[r]-(t), (r)-[]-(t) RETURN r",
            // Match1 [10]: a path and a node in one MATCH.
            "MATCH r = ()-[]-(), (r) RETURN r",
            "MATCH (x), r = (s)-[p]-(t)-[]-(b), (a)-[q]-(r) RETURN r",
            // Match1 [11] / Match2 [13]: a value, then a node or relationship.
            "WITH 123 AS n MATCH (n) RETURN n",
            "WITH 'foo' AS n MATCH (n) RETURN n",
            "WITH [10] AS n MATCH (n) RETURN n",
            "WITH {x: []} AS n MATCH (n) RETURN n",
            "WITH true AS r MATCH ()-[r]-() RETURN r",
            // Match2 [9]: a node, then a relationship.
            "MATCH (r) MATCH ()-[r]-() RETURN r",
            "MATCH (s)-[]-(t), (s)-[]-(r) MATCH ()-[r]-() RETURN r",
            // Match2 [10]: a path, then a relationship.
            "MATCH r = ()-[p*]-() MATCH ()-[r]-() RETURN r",
            // Match2 [11]: a node and a relationship in one pattern.
            "MATCH (r)-[r]-() RETURN r",
            "MATCH (r), ()-[r]-() RETURN r",
            "MATCH ()-[]-(r)-[r*]-() RETURN r",
            // Match2 [12]: a path and a relationship in one MATCH.
            "MATCH r = ()-[]-(), ()-[r*]-() RETURN r",
            // A kind carried through WITH keeps its kind.
            "MATCH (a) WITH a AS r MATCH ()-[r]-() RETURN r",
            // OPTIONAL MATCH binds as MATCH does.
            "MATCH (r) OPTIONAL MATCH ()-[r]-() RETURN r",
        ],
    );
    let err = read_err("RETURN properties(1)");
    assert!(err.contains("properties() cannot take an integer"), "{err}");
    let err = read_err("MATCH ()-[r]-() MATCH (r) RETURN r");
    assert!(err.contains("`r`"), "the message names the variable: {err}");
}

#[test]
fn a_path_name_that_is_already_bound_is_refused() {
    assert_read_refused(
        "VariableAlreadyBound",
        &[
            // Match6 [21] / [22]: a node or relationship, then a path.
            "MATCH (p)-[]-() MATCH p = ()-[]-() RETURN p",
            "MATCH ()-[p*]-() MATCH p = ()-[]-() RETURN p",
            // Match6 [23] / [24]: in the same pattern or MATCH.
            "MATCH p = (p)-[]-() RETURN p",
            "MATCH (p)-[]-(), p = ()-[]-() RETURN p",
            "MATCH p = ()-[p]-() RETURN p",
            // Match6 [25]: a value, then a path.
            "WITH 123 AS p MATCH p = ()-[]-() RETURN p",
            "MATCH p = ()-[]-() MATCH p = ()-[]-() RETURN p",
        ],
    );
}

#[test]
fn creating_a_node_that_is_already_bound_is_refused() {
    // Create1 [13].
    let err = write_err("MATCH (a) CREATE (a)");
    assert_kind("MATCH (a) CREATE (a)", "VariableAlreadyBound", &err);
    assert!(err.contains("`a`"), "{err}");
    // A bound node may still be an end of a new relationship.
    parse_write(&lex("MATCH (a:A) CREATE (a)-[:T]->(b:B {id: 'b'})").unwrap())
        .expect("a bound node as an end of a new relationship");
    parse_write(&lex("MATCH (a:A), (b:B) CREATE (a)-[:T]->(b)").unwrap())
        .expect("two bound nodes joined by a new relationship");
}

// ---------------------------------------------------------------------------
// Aggregation and projection
// ---------------------------------------------------------------------------

#[test]
fn an_aggregate_where_no_aggregation_is_allowed_is_refused() {
    assert_read_refused(
        "InvalidAggregation",
        &[
            // MatchWhere1 [15].
            "MATCH (a) WHERE count(a) > 10 RETURN a",
            "MATCH (a) WITH a WHERE max(a.x) > 1 RETURN a",
            "MATCH (a) OPTIONAL MATCH (a)-[]->(b) WHERE count(b) > 1 RETURN a",
            // ReturnOrderBy2 [14].
            "MATCH (n) RETURN n.num1 ORDER BY max(n.num2)",
            // WithOrderBy2 [25].
            "MATCH (n) WITH n.num1 AS foo ORDER BY count(1) RETURN foo AS foo",
            "MATCH (n) WITH n.num1 AS foo ORDER BY count(1 + n.num1) RETURN foo AS foo",
            "MATCH (n) WITH n.num1 AS foo ORDER BY n.name DESC, max(n.num2) ASC RETURN foo AS foo",
        ],
    );
}

#[test]
fn with_items_need_an_alias_and_a_name_of_their_own() {
    // With4 [5].
    assert_read_refused(
        "NoExpressionAlias",
        &[
            "MATCH (a) WITH a, count(*) RETURN a",
            "MATCH (a) WITH a, sum(a.x) RETURN a",
        ],
    );
    // With4 [4].
    assert_read_refused(
        "ColumnNameConflict",
        &[
            "WITH 1 AS a, 2 AS a RETURN a",
            "MATCH (a) WITH a, a.x AS a RETURN a",
        ],
    );
}

#[test]
fn union_parts_must_agree_on_kind_and_columns() {
    // Union3 [1] / [2].
    assert_read_refused(
        "InvalidClauseComposition",
        &[
            "RETURN 1 AS a UNION RETURN 2 AS a UNION ALL RETURN 3 AS a",
            "RETURN 1 AS a UNION ALL RETURN 2 AS a UNION RETURN 3 AS a",
        ],
    );
    // Union1 [5] / Union2 [5].
    assert_read_refused(
        "DifferentColumnsInUnion",
        &[
            "RETURN 1 AS a UNION RETURN 2 AS b",
            "RETURN 1 AS a UNION ALL RETURN 2 AS b",
        ],
    );
}

// ---------------------------------------------------------------------------
// Functions and argument types
// ---------------------------------------------------------------------------

#[test]
fn an_unknown_function_is_refused_before_any_row_is_read() {
    // Return2 [18]: on a graph with no node, the call is never evaluated, so
    // only a compile-time check can refuse it.
    assert_read_refused(
        "UnknownFunction",
        &["MATCH (a) RETURN foo(a)", "RETURN nosuch.fn(1) AS x"],
    );
    let err = crate::cypher::parse(&lex("MATCH (a:Nope) RETURN foo(a)").unwrap())
        .expect_err("refused before a plan is built");
    assert!(err.starts_with("UnknownFunction: "), "{err}");
    assert!(err.contains("`foo`"), "{err}");
}

#[test]
fn an_argument_of_a_kind_the_function_cannot_take_is_refused() {
    assert_read_refused(
        "InvalidArgumentType",
        &[
            // Graph4 [7], Path3 [2] / [3], List6 [5].
            "MATCH (r) RETURN type(r)",
            "MATCH (n) RETURN length(n)",
            "MATCH ()-[r]->() RETURN length(r)",
            "MATCH p = (a)-[*]->(b) RETURN size(p)",
            // Graph9 [5]-[7].
            "RETURN properties(1)",
            "RETURN properties('Cypher')",
            "RETURN properties([true, false])",
            // List5 [42].
            "RETURN 1 IN true",
            "RETURN 1 IN 123.4",
            "RETURN 1 IN 'foo'",
            "RETURN 1 IN {x: []}",
            // Graph6 [9] / Map1 [6]: a property of a value that has none.
            "WITH 123 AS nonMap RETURN nonMap.num",
            "WITH 'string' AS nonMap RETURN nonMap.num",
            "WITH [123, true] AS nonMap RETURN nonMap.num",
            "WITH false AS nonMap RETURN nonMap.num",
            // MatchWhere1 [14]: a path has no `name`.
            "MATCH (n) MATCH r = (n)-[*]->() WHERE r.name = 'apa' RETURN r",
        ],
    );
}

// ---------------------------------------------------------------------------
// What still parses
// ---------------------------------------------------------------------------

#[test]
fn valid_queries_pass_the_check() {
    let mut refused = Vec::new();
    for src in [
        // The same node or relationship again, in the same or a later MATCH.
        "MATCH (a)-[r]->(b) MATCH (a)-[r]->(b) RETURN a, r, b",
        "MATCH (a)-[r]->(b), (b)-[s]->(a) RETURN r, s",
        "MATCH (a)-[:T]->(b)-[:T]->(a) RETURN a",
        "MATCH (a) WITH a AS m MATCH (m)-[]->(b) RETURN b",
        "MATCH ()-[r]-() WITH r AS x MATCH ()-[x]-() RETURN x",
        // A name dropped by WITH may be bound again as another kind.
        "MATCH (r) WITH 1 AS one MATCH ()-[r]-() RETURN r",
        "MATCH (n) WITH count(n) AS c MATCH p = ()-[]->() RETURN c, p",
        // Values whose kind is unknown at compile time may be matched.
        "UNWIND $nodes AS n MATCH (n)-[]->(m) RETURN m",
        "WITH $x AS n MATCH (n) RETURN n",
        "WITH coalesce($a, $b) AS n MATCH (n) RETURN n",
        "WITH null AS n MATCH (n) RETURN n",
        // Paths: their length, nodes and relationships.
        "MATCH p = (a)-[*1..3]->(b) RETURN p.length, p.nodes, p.relationships, p",
        "MATCH p = (a)-[]->(b) WHERE p.length > 0 RETURN size(p.nodes)",
        "MATCH ()-[r*1..2]->() RETURN r.length",
        // Functions on the kinds they take.
        "MATCH ()-[r]->() RETURN type(r), properties(r)",
        "MATCH (n) RETURN labels(n), properties(n), keys(n), size(n.tags), id(n), key(n)",
        "RETURN properties({a: 1}) AS p, properties(null) AS q",
        "RETURN toLower('A') AS a, date.truncate('year', date('2024-05-05')) AS d",
        // A local variable shadows an outer one of the same name.
        "WITH 1 AS x RETURN [x IN [{a: 1}] | x.a] AS l",
        "WITH 1 AS x RETURN all(x IN [{a: 1}] WHERE x.a = 1) AS q",
        "WITH 1 AS acc RETURN reduce(acc = {a: 0}, y IN [1] | {a: acc.a + y}) AS r",
        // A map's entries are properties.
        "WITH {num: 1} AS m RETURN m.num",
        "UNWIND [{a: 1}] AS m RETURN m.a",
        // Aggregates in RETURN / WITH, and an ORDER BY over a projected one.
        "MATCH (n) RETURN count(n) ORDER BY count(n)",
        "MATCH (n) RETURN n.x AS x, count(*) AS c ORDER BY c DESC",
        "MATCH (n) WITH n.x AS x, count(*) AS c WHERE c > 1 RETURN x",
        "MATCH (n) WITH n, count(*) AS c RETURN n, c",
        // Aliased WITH items, WITH *, a variable repeated across stages.
        "MATCH (a) WITH a.x AS x, a RETURN x, a",
        "MATCH (a) WITH * RETURN a",
        "MATCH (a) WITH a WITH a RETURN a",
        // An unaliased scalar in WITH names itself (an extension kept from
        // before QL-VE; only an unaliased aggregate is refused).
        "MATCH (a) WITH a, a.x RETURN a",
        "MATCH (t) WITH t, t.location[0] AS lat, t.location[1] RETURN lat",
        // IN over a list, a parameter or a property.
        "RETURN 1 IN [1, 2] AS a, 1 IN $l AS b, null IN [1] AS c",
        "MATCH (n) WHERE n.x IN n.tags RETURN n",
        "RETURN 1 IN null AS x",
        // UNION parts that agree, and aliases the check cannot see.
        "RETURN 1 AS a UNION RETURN 2 AS a",
        "RETURN 1 AS a UNION ALL RETURN 2 AS a UNION ALL RETURN 3 AS a",
        "MATCH (n) RETURN n UNION MATCH (n) RETURN n",
        // Truthy and label tests.
        "MATCH (n) WHERE n:Person RETURN n",
        "MATCH (n) WHERE textMatches(n.bio, 'x') RETURN n",
        // shortestPath, OPTIONAL MATCH.
        "MATCH (a), (b) MATCH p = shortestPath((a)-[*]-(b)) RETURN p",
        "MATCH (a) OPTIONAL MATCH (a)-[r]->(b) RETURN a, r, b",
        "OPTIONAL MATCH (a) RETURN a",
        // ORDER BY after RETURN sees RETURN's aliases, which shadow an
        // earlier binding of the same name (QL-VE review I1).
        "MATCH p = (a:Person)-[:KNOWS]->(b) RETURN a AS p ORDER BY p.name",
        "WITH 1 AS m RETURN {a: m} AS m ORDER BY m.a",
        "WITH [1, 2] AS tags RETURN {x: tags} AS tags ORDER BY tags.x",
        "WITH 'abc' AS s MATCH (n:Person) RETURN n AS s ORDER BY s.name",
    ] {
        let tokens = lex(src).unwrap_or_else(|e| panic!("lex `{src}`: {e}"));
        if let Err(e) = parse_read(&tokens) {
            refused.push(format!("`{src}`: {e}"));
        }
    }
    for src in [
        "MATCH (a:A) CREATE (a)-[:T]->(b:B {id: 'b'})",
        "MATCH (a:A) SET a.x = 1 RETURN a",
        "MATCH (a:A)-[r:T]->(b) DELETE r",
        "MATCH (a:A) DETACH DELETE a",
        "UNWIND $rows AS row MERGE (n:N {id: row.id}) SET n.x = row.x",
        "UNWIND [1, 2] AS x CREATE (:N {id: toString(x)})",
        "UNWIND $rows AS row MATCH (a:A {id: row.id}) SET a.x = row.x",
    ] {
        let tokens = lex(src).unwrap_or_else(|e| panic!("lex `{src}`: {e}"));
        if let Err(e) = parse_write(&tokens) {
            refused.push(format!("`{src}`: {e}"));
        }
    }
    assert!(
        refused.is_empty(),
        "valid queries must pass the check:\n{}",
        refused.join("\n")
    );
}

// ---------------------------------------------------------------------------
// Runtime type errors (List1, List11, Map2, Graph9): a type error is an
// error, not a null (controller ruling 2026-10-23)
// ---------------------------------------------------------------------------

#[test]
fn indexing_a_value_that_is_not_a_list_or_map_is_an_error() {
    // List1 [6] / [7]; Map2 [8].
    for base in ["true", "123", "4.7", "'1'"] {
        let err = eval_err(&format!("{base}[0]"));
        assert!(err.contains("cannot be indexed"), "{base}: {err}");
    }
    // List1 [8] / [9]: a list takes an integer.
    for idx in ["true", "4.7", "'1'", "[1]", "{x: 3}"] {
        let err = eval_err(&format!("[1, 2, 3][{idx}]"));
        assert!(
            err.contains("list index must be an integer"),
            "{idx}: {err}"
        );
    }
    // Map2 [6] / [7]: a map takes a string.
    for idx in ["0", "12.3", "true"] {
        let err = eval_err(&format!("{{name: 'Apa'}}[{idx}]"));
        assert!(err.contains("map key must be a string"), "{idx}: {err}");
    }
    // Through parameters, as the TCK passes them.
    let mut fx = Fx::new();
    fx.add("N", "x", vec![]);
    let mut params = BTreeMap::new();
    params.insert("expr".to_string(), Value::Int(100));
    params.insert("idx".to_string(), Value::Int(0));
    let err = run(
        &fx.view(),
        "WITH $expr AS expr, $idx AS idx RETURN expr[idx] AS r",
        &params,
    )
    .expect_err("an integer cannot be indexed");
    assert!(err.contains("cannot be indexed"), "{err}");
}

#[test]
fn indexing_of_the_right_types_is_unchanged() {
    assert_eq!(eval("[1, 2, 3][1]"), Some(Value::Int(2)));
    assert_eq!(eval("[1, 2, 3][-1]"), Some(Value::Int(3)));
    assert_eq!(eval("[1, 2, 3][1.0]"), Some(Value::Int(2)));
    assert_eq!(eval("[1, 2, 3][7]"), None);
    assert_eq!(eval("[1, 2, 3][null]"), None);
    assert_eq!(eval("null[0]"), None);
    assert_eq!(eval("{a: 1}['a']"), Some(Value::Int(1)));
    assert_eq!(eval("{a: 1}['b']"), None);
    // A subscript of a node variable reads nothing, as before.
    assert_eq!(eval("n['x']"), None);
}

#[test]
fn range_takes_integers_only() {
    // List11 [5].
    for args in [
        "true, 1, 1",
        "0, true, 1",
        "0, 1, true",
        "-1.1, 1, 1",
        "0.0, 1, 1",
        "0, 1.1, 1",
        "0, 1, 1.1",
        "'xyz', 1, 1",
        "0, 'xyz', 1",
        "[0], 1, 1",
        "0, [1], 1",
        "{start: 0}, 1, 1",
        "0, 1, {step: 1}",
    ] {
        let err = eval_err(&format!("range({args})"));
        assert!(
            err.contains("range() expects integer arguments"),
            "range({args}): {err}"
        );
    }
    assert_eq!(
        eval("range(0, 2)"),
        Some(Value::List(vec![
            Value::Int(0),
            Value::Int(1),
            Value::Int(2)
        ]))
    );
    assert_eq!(eval("range(null, 2)"), None);
}

#[test]
fn properties_of_a_value_that_has_none_is_an_error() {
    // Graph9 [5]-[7] at run time, through a parameter.
    let mut fx = Fx::new();
    fx.add("N", "x", vec![]);
    for v in [
        Value::Int(1),
        Value::Str("Cypher".into()),
        Value::List(vec![]),
    ] {
        let mut params = BTreeMap::new();
        params.insert("v".to_string(), v.clone());
        let err =
            run(&fx.view(), "RETURN properties($v) AS p", &params).expect_err("no properties");
        assert!(err.contains("properties() expects"), "{v:?}: {err}");
    }
    assert_eq!(eval("properties(null)"), None);
    assert!(matches!(eval("properties(n)"), Some(Value::Map(_))));
}

// ---------------------------------------------------------------------------
// Row 160: booleans compare, false before true
// ---------------------------------------------------------------------------

#[test]
fn row_160_booleans_order_false_before_true() {
    for (expr, want) in [
        ("false < true", true),
        ("false <= true", true),
        ("true > false", true),
        ("true >= true", true),
        ("true < true", false),
        ("true <= false", false),
        ("false > true", false),
        ("false >= false", true),
    ] {
        assert_eq!(eval(expr), Some(Value::Bool(want)), "{expr}");
    }
    // A boolean and a number still do not order.
    assert_eq!(eval("true < 1"), None);

    let mut fx = Fx::new();
    fx.add("B", "t", vec![("b", Value::Bool(true))]);
    fx.add("B", "f", vec![("b", Value::Bool(false))]);
    let params = BTreeMap::new();
    let rs = run(
        &fx.view(),
        "MATCH (n:B) WHERE n.b > false RETURN key(n) AS k",
        &params,
    )
    .unwrap();
    assert_eq!(rs.len(), 1);
    assert_eq!(rs.get(0, "k"), Some(&Value::Str("t".into())));
    let rs = run(
        &fx.view(),
        "MATCH (n:B) RETURN key(n) AS k ORDER BY n.b",
        &params,
    )
    .unwrap();
    assert_eq!(rs.get(0, "k"), Some(&Value::Str("f".into())));
    assert_eq!(rs.get(1, "k"), Some(&Value::Str("t".into())));
    let rs = run(
        &fx.view(),
        "MATCH (n:B) RETURN min(n.b) AS lo, max(n.b) AS hi",
        &params,
    )
    .unwrap();
    assert_eq!(rs.get(0, "lo"), Some(&Value::Bool(false)));
    assert_eq!(rs.get(0, "hi"), Some(&Value::Bool(true)));
}
