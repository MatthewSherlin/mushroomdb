//! QL-BX: comparisons and boolean operators in every expression position,
//! evaluated three-valued, through the full lex/parse/plan/execute path.

use super::tests::{eval, run, Fx};
use crate::cypher::ast::{Expr, Operand};
use crate::cypher::{lex, parse};
use crate::filter::CmpOp;
use core_storage::Value;
use proptest::prelude::*;
use std::collections::BTreeMap;

fn b(v: bool) -> Option<Value> {
    Some(Value::Bool(v))
}

fn bools(xs: &[Option<bool>]) -> Vec<Option<Value>> {
    xs.iter().map(|x| x.map(Value::Bool)).collect()
}

/// Three people: `a` is an adult with a high score, `b` a minor with a low
/// one, `c` has no properties at all.
fn people() -> Fx {
    let mut fx = Fx::new();
    fx.add(
        "P",
        "a",
        vec![
            ("age", Value::Int(30)),
            ("score", Value::Int(12)),
            ("active", Value::Bool(true)),
            ("name", Value::Str("ann".into())),
        ],
    );
    fx.add(
        "P",
        "b",
        vec![
            ("age", Value::Int(15)),
            ("score", Value::Int(5)),
            ("active", Value::Bool(false)),
            ("name", Value::Str("bob".into())),
        ],
    );
    fx.add("P", "c", vec![]);
    fx
}

fn col_with(fx: &Fx, q: &str, name: &str, params: &BTreeMap<String, Value>) -> Vec<Option<Value>> {
    let rs = run(&fx.view(), q, params).unwrap_or_else(|e| panic!("`{q}` failed: {e}"));
    (0..rs.len()).map(|i| rs.get(i, name).cloned()).collect()
}

fn col(fx: &Fx, q: &str, name: &str) -> Vec<Option<Value>> {
    col_with(fx, q, name, &BTreeMap::new())
}

fn keys(fx: &Fx, q: &str) -> Vec<String> {
    col(fx, q, "k")
        .into_iter()
        .map(|v| match v {
            Some(Value::Str(s)) => s,
            other => panic!("`{q}`: {other:?}"),
        })
        .collect()
}

#[test]
fn a_comparison_is_a_return_item() {
    let fx = people();
    assert_eq!(
        col(
            &fx,
            "MATCH (n:P) RETURN n.key AS k, n.score > 10 AS big ORDER BY k",
            "big"
        ),
        bools(&[Some(true), Some(false), None])
    );
}

#[test]
fn is_null_on_a_parameter_is_a_return_item() {
    let fx = Fx::new();
    let q = "RETURN $p IS NULL AS r";
    let p = |v: Value| BTreeMap::from([("p".to_string(), v)]);
    assert_eq!(col_with(&fx, q, "r", &p(Value::Int(1))), vec![b(false)]);
    assert_eq!(col_with(&fx, q, "r", &p(Value::Null)), vec![b(true)]);
    assert_eq!(
        col_with(&fx, "RETURN $p IS NOT NULL AS r", "r", &p(Value::Null)),
        vec![b(false)]
    );
}

#[test]
fn not_is_a_return_item() {
    let fx = people();
    assert_eq!(
        col(
            &fx,
            "MATCH (n:P) RETURN n.key AS k, NOT n.active AS r ORDER BY k",
            "r"
        ),
        bools(&[Some(false), Some(true), None])
    );
}

#[test]
fn predicates_inside_lists_and_comprehensions() {
    assert_eq!(
        eval("[x IN [0, 1, 2] | x > 1]"),
        Some(Value::List(vec![
            Value::Bool(false),
            Value::Bool(false),
            Value::Bool(true)
        ]))
    );
    assert_eq!(
        eval("[1 > 0, true AND false]"),
        Some(Value::List(vec![Value::Bool(true), Value::Bool(false)]))
    );
    // A null predicate is a null element.
    assert_eq!(
        eval("[x IN [1, null] | x > 0]"),
        Some(Value::List(vec![Value::Bool(true), Value::Null]))
    );
    assert_eq!(
        eval("[x IN [1, 2, 3] WHERE x > 1 OR x = 1 | x <> 2]"),
        Some(Value::List(vec![
            Value::Bool(true),
            Value::Bool(false),
            Value::Bool(true)
        ]))
    );
}

#[test]
fn a_comparison_is_a_with_item() {
    let fx = people();
    let q = "MATCH (n:P) WITH n.key AS k, n.age >= 18 AS adult RETURN k, adult ORDER BY k";
    assert_eq!(
        col(&fx, q, "adult"),
        bools(&[Some(true), Some(false), None])
    );
    // And the alias filters like any boolean value.
    let q = "MATCH (n:P) WITH n.key AS k, n.age >= 18 AS adult WHERE adult RETURN k";
    assert_eq!(keys(&fx, q), vec!["a"]);
}

#[test]
fn predicates_as_function_arguments_and_case_branches() {
    assert_eq!(eval("coalesce(n.a IS NULL, false)"), b(true));
    assert_eq!(eval("coalesce(n.a > 1, false)"), b(false));
    assert_eq!(
        eval("size([x IN [1, 2, 3] WHERE x > 1])"),
        Some(Value::Int(2))
    );
    let fx = people();
    assert_eq!(
        col(
            &fx,
            "MATCH (n:P) RETURN n.key AS k, CASE WHEN n.age IS NOT NULL THEN n.score > 10 ELSE 1 < 0 END AS r ORDER BY k",
            "r"
        ),
        bools(&[Some(true), Some(false), Some(false)])
    );
    assert_eq!(eval("CASE WHEN true THEN n.a > 1 END"), None);
}

#[test]
fn predicates_in_map_values_reduce_unwind_and_order_by() {
    let fx = people();
    assert_eq!(
        col(
            &fx,
            "MATCH (n:P) RETURN n.key AS k, {big: n.score > 10, named: n.name IS NOT NULL} AS m ORDER BY k",
            "m"
        ),
        vec![
            Some(Value::Map(BTreeMap::from([
                ("big".to_string(), Value::Bool(true)),
                ("named".to_string(), Value::Bool(true)),
            ]))),
            Some(Value::Map(BTreeMap::from([
                ("big".to_string(), Value::Bool(false)),
                ("named".to_string(), Value::Bool(true)),
            ]))),
            Some(Value::Map(BTreeMap::from([
                ("big".to_string(), Value::Null),
                ("named".to_string(), Value::Bool(false)),
            ]))),
        ]
    );
    assert_eq!(
        eval("reduce(acc = true, x IN [1, 2, 3] | acc AND x > 0)"),
        b(true)
    );
    assert_eq!(
        eval("reduce(acc = false, x IN [1, 2, 3] | acc OR x = 2)"),
        b(true)
    );
    assert_eq!(
        col(
            &Fx::new(),
            "UNWIND [1 > 0, 1 < 0, null = 1] AS x RETURN x",
            "x"
        ),
        bools(&[Some(true), Some(false), None])
    );
    // ORDER BY a predicate: false before true, null last.
    assert_eq!(
        keys(
            &fx,
            "MATCH (n:P) RETURN n.key AS k ORDER BY n.score > 10, k"
        ),
        vec!["b", "a", "c"]
    );
}

#[test]
fn infix_string_predicates_xor_and_in_are_values() {
    assert_eq!(eval("'abc' STARTS WITH 'a'"), b(true));
    assert_eq!(eval("'abc' ENDS WITH 'c'"), b(true));
    assert_eq!(eval("'abc' CONTAINS 'z'"), b(false));
    // A missing property is null, so neither the predicate nor its NOT matches.
    assert_eq!(eval("n.missing CONTAINS 'z'"), None);
    assert_eq!(eval("NOT n.missing STARTS WITH 'z'"), None);
    assert_eq!(eval("2 IN [1, 2]"), b(true));
    assert_eq!(eval("5 IN $l"), b(false));
    assert_eq!(eval("true XOR true"), b(false));
    assert_eq!(eval("true XOR false"), b(true));
    assert_eq!(eval("false xor false"), b(false));
    assert_eq!(eval("true IN [1 > 0]"), b(true));
}

#[test]
fn precedence_follows_open_cypher() {
    // NOT binds looser than comparison: NOT (1 = 2), not (NOT 1) = 2.
    assert_eq!(eval("NOT 1 = 2"), b(true));
    // AND binds tighter than OR.
    assert_eq!(eval("true OR false AND false"), b(true));
    assert_eq!(eval("false AND false OR true"), b(true));
    // Additive binds tighter than comparison, comparison tighter than AND.
    assert_eq!(eval("1 + 2 > 2 AND true"), b(true));
    assert_eq!(eval("2 * 3 = 6 AND 10 / 2 = 5"), b(true));
    // XOR sits between OR and AND.
    assert_eq!(eval("true OR true XOR true"), b(true));
    assert_eq!(eval("false AND true XOR true"), b(true));
    // IS NULL and IN bind tighter than comparison.
    assert_eq!(eval("1 = null IS NULL"), b(false));
    assert_eq!(eval("true = 1 IN [1]"), b(true));
    // Additive binds tighter than IN.
    assert_eq!(eval("1 + 1 IN [2]"), b(true));
    // Parentheses group a predicate and an arithmetic expression alike.
    assert_eq!(eval("(true OR false) AND false"), b(false));
    assert_eq!(eval("(1 + 2) * 3 > 8"), b(true));
    assert_eq!(eval("NOT NOT true"), b(true));
    assert_eq!(eval("(1 > 0) = true"), b(true));
}

#[test]
fn predicates_over_null_are_null() {
    assert_eq!(eval("null > 1"), None);
    assert_eq!(eval("null IS NULL"), b(true));
    assert_eq!(eval("null IS NOT NULL"), b(false));
    assert_eq!(eval("NOT null"), None);
    assert_eq!(eval("null = null"), None);
    assert_eq!(eval("null <> 1"), None);
    assert_eq!(eval("n.missing < 3"), None);
    // AND / OR / XOR follow the three-valued truth tables.
    assert_eq!(eval("null AND false"), b(false));
    assert_eq!(eval("false AND null"), b(false));
    assert_eq!(eval("null AND true"), None);
    assert_eq!(eval("null OR true"), b(true));
    assert_eq!(eval("true OR null"), b(true));
    assert_eq!(eval("null OR false"), None);
    assert_eq!(eval("null XOR true"), None);
    // IN: a match is true; otherwise a null makes it null.
    assert_eq!(eval("1 IN [1, null]"), b(true));
    assert_eq!(eval("1 IN [2, null]"), None);
    assert_eq!(eval("null IN []"), b(false));
    assert_eq!(eval("null IN [1]"), None);
    assert_eq!(eval("1 IN null"), None);
    // Values of different types do not order.
    assert_eq!(eval("1 < 'a'"), None);
    // Equality of different types is false, not null.
    assert_eq!(eval("1 = 'a'"), b(false));
    assert_eq!(eval("[1, null] = [1, null]"), None);
    assert_eq!(eval("[1, null] = [2, null]"), b(false));
}

#[test]
fn where_is_unchanged_and_now_takes_parenthesised_arithmetic() {
    let fx = people();
    assert_eq!(
        keys(
            &fx,
            "MATCH (n:P) WHERE (n.score > 10) = true RETURN n.key AS k"
        ),
        vec!["a"]
    );
    assert_eq!(
        keys(&fx, "MATCH (n:P) WHERE (n.age + 1) > 16 RETURN n.key AS k"),
        vec!["a"]
    );
    assert_eq!(
        keys(
            &fx,
            "MATCH (n:P) WHERE n.active XOR n.score > 10 RETURN n.key AS k ORDER BY k"
        ),
        Vec::<String>::new()
    );
    assert_eq!(
        keys(
            &fx,
            "MATCH (n:P) WHERE n.active XOR n.age < 18 RETURN n.key AS k ORDER BY k"
        ),
        vec!["a", "b"]
    );
    // A null row is dropped by WHERE and returned as null by a projection.
    assert_eq!(
        keys(
            &fx,
            "MATCH (n:P) WHERE n.score > 1 RETURN n.key AS k ORDER BY k"
        ),
        vec!["a", "b"]
    );
}

/// The keys a WHERE keeps over `people()`, through the staged path, the
/// bounded (fused scan+filter) path and an indexed view; all three agree.
fn where_keys_all_paths(w: &str) -> Vec<String> {
    use core_storage::property_index::PropertyIndex;
    let mut fx = Fx::new();
    let a = fx.add(
        "P",
        "a",
        vec![
            ("age", Value::Int(30)),
            ("score", Value::Int(12)),
            ("name", Value::Str("ann".into())),
        ],
    );
    let b_ = fx.add(
        "P",
        "b",
        vec![
            ("age", Value::Int(15)),
            ("score", Value::Int(5)),
            ("name", Value::Str("bob".into())),
        ],
    );
    fx.add("P", "c", vec![]);
    let mut pi = PropertyIndex::new();
    pi.enable("P", "age");
    pi.set("P", "age", a, &Value::Int(30));
    pi.set("P", "age", b_, &Value::Int(15));
    let p = BTreeMap::new();
    let get = |view: &crate::view::GraphView, q: &str| -> Vec<String> {
        let rs = run(view, q, &p).unwrap_or_else(|e| panic!("`{q}`: {e}"));
        let mut k: Vec<String> = (0..rs.len())
            .map(|i| match rs.get(i, "k") {
                Some(Value::Str(s)) => s.clone(),
                other => panic!("`{q}`: {other:?}"),
            })
            .collect();
        k.sort();
        k
    };
    let staged = get(
        &fx.view(),
        &format!("MATCH (n:P) WHERE {w} RETURN n.key AS k"),
    );
    let bounded = get(
        &fx.view(),
        &format!("MATCH (n:P) WHERE {w} RETURN n.key AS k LIMIT 100"),
    );
    let indexed = get(
        &fx.view_indexed(&pi),
        &format!("MATCH (n:P) WHERE {w} RETURN n.key AS k LIMIT 100"),
    );
    assert_eq!(staged, bounded, "LIMIT changed `{w}`");
    assert_eq!(staged, indexed, "an index changed `{w}`");
    staged
}

#[test]
fn where_is_three_valued_under_not_and_and_or() {
    let k = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let none: Vec<String> = vec![];
    for (w, want) in [
        // `c` has no age: the comparison is null, so is its NOT; c is dropped.
        ("NOT n.age = 30", k(&["b"])),
        ("NOT n.age > 18", k(&["b"])),
        ("NOT n.age IN [30]", k(&["b"])),
        ("NOT n.name STARTS WITH 'a'", k(&["b"])),
        ("NOT n.missing = 1", none.clone()),
        // NOT (a AND null): true AND null is null, false AND null is false.
        ("NOT (n.age > 18 AND n.missing = 1)", k(&["b"])),
        ("NOT (n.age > 18 AND null)", k(&["b"])),
        ("NOT (false AND null)", k(&["a", "b", "c"])),
        // AND: false wins over null; true AND null is null.
        ("n.missing = 1 AND false", none.clone()),
        ("n.age = 30 AND n.missing = 1", none.clone()),
        ("NOT (n.missing = 1 AND false)", k(&["a", "b", "c"])),
        // OR: true wins over null; false OR null is null.
        ("null OR true", k(&["a", "b", "c"])),
        ("n.missing = 1 OR true", k(&["a", "b", "c"])),
        ("null OR false", none.clone()),
        ("NOT (null OR false)", none.clone()),
        ("NOT (n.missing = 1 OR n.age = 15)", none.clone()),
        ("NOT (n.age = 30 OR n.missing = 1)", none.clone()),
        ("n.age = 30 OR NOT n.age = 30", k(&["a", "b"])),
        // Unchanged by the ruling: no NOT over a null.
        ("n.age = 30", k(&["a"])),
        ("n.age <> 30", k(&["b"])),
        ("n.age IS NULL", k(&["c"])),
        ("NOT n.age IS NULL", k(&["a", "b"])),
    ] {
        assert_eq!(where_keys_all_paths(w), want, "`{w}`");
    }
}

#[test]
fn case_when_and_a_comprehension_filter_are_three_valued_too() {
    // NOT null is null, so the branch is not taken.
    assert_eq!(
        eval("CASE WHEN NOT n.missing = 1 THEN 't' ELSE 'f' END"),
        Some(Value::Str("f".into()))
    );
    // A null element's NOT is null, so the element is dropped.
    assert_eq!(
        eval("[x IN [1, null, 3] WHERE NOT x = 1]"),
        Some(Value::List(vec![Value::Int(3)]))
    );
}

#[test]
fn in_reads_its_elements_lazily_as_where_always_did() {
    // A match stops the scan, and a null needle needs no element, so an
    // element that would fail is never evaluated.
    assert_eq!(eval("1 IN [1, 1 - 'a']"), b(true));
    assert_eq!(eval("null IN [1, 1 - 'a']"), None);
    // With no match, every element is read, so the bad one errors.
    super::tests::eval_err("2 IN [1, 1 - 'a']");
    let fx = people();
    assert_eq!(
        keys(
            &fx,
            "MATCH (n:P) WHERE n.missing IN [12, 1 - 'a'] RETURN n.key AS k"
        ),
        Vec::<String>::new()
    );
}

#[test]
fn where_keeps_exactly_the_rows_a_projection_calls_true() {
    let fx = people();
    assert_eq!(
        keys(
            &fx,
            "MATCH (n:P) WHERE NOT n.age = 30 RETURN n.key AS k ORDER BY k"
        ),
        vec!["b"]
    );
    assert_eq!(
        col(
            &fx,
            "MATCH (n:P) RETURN n.key AS k, NOT n.age = 30 AS r ORDER BY k",
            "r"
        ),
        bools(&[Some(false), Some(true), None])
    );
}

#[test]
fn a_chained_comparison_is_a_parse_error() {
    for src in [
        "RETURN 1 < 2 < 3",
        "MATCH (n) WHERE n.a = n.b = n.c RETURN n",
    ] {
        assert!(parse(&lex(src).expect("lex")).is_err(), "{src}");
    }
}

#[test]
fn a_projected_predicate_is_never_an_index_lookup() {
    use core_storage::property_index::PropertyIndex;
    let mut fx = Fx::new();
    let a = fx.add("L", "a", vec![("x", Value::Int(1))]);
    let b_ = fx.add("L", "b", vec![("x", Value::Int(2))]);
    fx.add("L", "c", vec![]);
    let mut pi = PropertyIndex::new();
    pi.enable("L", "x");
    pi.set("L", "x", a, &Value::Int(1));
    pi.set("L", "x", b_, &Value::Int(2));
    let q = "MATCH (n:L) RETURN n.key AS k, n.x = 1 AS hit ORDER BY k";
    let p = BTreeMap::new();
    for view in [fx.view(), fx.view_indexed(&pi)] {
        let rs = run(&view, q, &p).unwrap();
        let hits: Vec<Option<Value>> = (0..rs.len()).map(|i| rs.get(i, "hit").cloned()).collect();
        assert_eq!(hits, bools(&[Some(true), Some(false), None]));
    }
    // The same predicate under WHERE still filters, indexed or not.
    for view in [fx.view(), fx.view_indexed(&pi)] {
        let rs = run(&view, "MATCH (n:L) WHERE n.x = 1 RETURN n.key AS k", &p).unwrap();
        assert_eq!(rs.len(), 1);
    }
}

#[test]
fn a_projected_predicate_column_is_named_expr() {
    let fx = Fx::new();
    let rs = run(&fx.view(), "RETURN 1 > 0", &BTreeMap::new()).unwrap();
    assert_eq!(rs.get(0, "<expr>"), b(true).as_ref());
}

#[test]
fn deep_nesting_is_an_error_not_a_crash() {
    for (open, close) in [("(", ")"), ("NOT (", ")"), ("[", "]"), ("coalesce(", ")")] {
        // Within the limits it parses and runs...
        let src = format!("{}1 > 0{}", open.repeat(40), close.repeat(40));
        assert!(eval(&src).is_some(), "{open} x40");
        // ...and far past them it is refused, without a stack overflow.
        let src = format!("RETURN {}1{}", open.repeat(5000), close.repeat(5000));
        let toks = lex(&src).expect("lex");
        let err = parse(&toks).expect_err("x5000 must be refused");
        assert!(err.contains("nesting too deep"), "{open}: {err}");
    }
    // A long AND chain is fine to parse; evaluating it is bounded.
    let chain = vec!["true"; 40].join(" AND ");
    assert_eq!(eval(&chain), b(true));
    let nots = format!("{}true", "NOT ".repeat(20));
    assert_eq!(eval(&nots), b(true));
}

// ── Differential: WHERE parses to the same predicate tree as before ────────

/// A predicate in the grammar WHERE accepted before QL-BX, with the tree the
/// old parser built for it.
#[derive(Debug, Clone)]
enum P {
    Leaf(String, Expr),
    Not(Box<P>),
    And(Box<P>, Box<P>),
    Or(Box<P>, Box<P>),
    Paren(Box<P>),
}

impl P {
    fn prec(&self) -> u8 {
        match self {
            P::Or(..) => 1,
            P::And(..) => 2,
            P::Not(_) => 3,
            P::Leaf(..) | P::Paren(_) => 4,
        }
    }

    fn text(&self) -> String {
        let wrap = |p: &P, paren: bool| {
            if paren {
                format!("({})", p.text())
            } else {
                p.text()
            }
        };
        match self {
            P::Leaf(t, _) => t.clone(),
            P::Paren(c) => format!("({})", c.text()),
            P::Not(c) => format!("NOT {}", wrap(c, c.prec() <= 3)),
            P::And(l, r) => format!("{} AND {}", wrap(l, l.prec() < 2), wrap(r, r.prec() <= 2)),
            P::Or(l, r) => format!("{} OR {}", wrap(l, false), wrap(r, r.prec() <= 1)),
        }
    }

    fn tree(&self) -> Expr {
        match self {
            P::Leaf(_, e) => e.clone(),
            P::Paren(c) => c.tree(),
            P::Not(c) => Expr::Not(Box::new(c.tree())),
            P::And(l, r) => Expr::And(Box::new(l.tree()), Box::new(r.tree())),
            P::Or(l, r) => Expr::Or(Box::new(l.tree()), Box::new(r.tree())),
        }
    }
}

fn operand() -> impl Strategy<Value = (String, Operand)> {
    let prop = |f: &str| Operand::Prop {
        var: "n".into(),
        field: f.into(),
    };
    prop_oneof![
        Just(("n.x".to_string(), prop("x"))),
        Just(("n.y".to_string(), prop("y"))),
        (0i64..3).prop_map(|v| (v.to_string(), Operand::Lit(Value::Int(v)))),
        Just(("null".to_string(), Operand::Lit(Value::Null))),
        Just(("$p".to_string(), Operand::Param("p".into()))),
        Just((
            "n.x + 1".to_string(),
            Operand::BinArith {
                op: crate::cypher::ast::ArithOp::Add,
                left: Box::new(prop("x")),
                right: Box::new(Operand::Lit(Value::Int(1))),
            }
        )),
    ]
}

fn cmp_op() -> impl Strategy<Value = (&'static str, CmpOp)> {
    prop_oneof![
        Just(("=", CmpOp::Eq)),
        Just(("<>", CmpOp::Ne)),
        Just(("<", CmpOp::Lt)),
        Just(("<=", CmpOp::Le)),
        Just((">", CmpOp::Gt)),
        Just((">=", CmpOp::Ge)),
    ]
}

fn leaf() -> impl Strategy<Value = P> {
    prop_oneof![
        (operand(), cmp_op(), operand()).prop_map(|((lt, l), (ot, op), (rt, r))| {
            P::Leaf(format!("{lt} {ot} {rt}"), Expr::Cmp { lhs: l, op, rhs: r })
        }),
        (operand(), any::<bool>()).prop_map(|((t, o), not)| if not {
            P::Leaf(format!("{t} IS NOT NULL"), Expr::IsNotNull(o))
        } else {
            P::Leaf(format!("{t} IS NULL"), Expr::IsNull(o))
        }),
        (operand(), operand(), operand()).prop_map(|((t, o), (at, a), (bt, b))| {
            P::Leaf(
                format!("{t} IN [{at}, {bt}]"),
                Expr::In {
                    expr: o,
                    list: vec![a, b],
                },
            )
        }),
        operand().prop_map(|(t, o)| P::Leaf(t, Expr::Truthy(o))),
        Just(P::Leaf(
            "n.s STARTS WITH 'a'".to_string(),
            Expr::Truthy(Operand::FuncCall {
                name: "startsWith".into(),
                args: vec![
                    Operand::Prop {
                        var: "n".into(),
                        field: "s".into()
                    },
                    Operand::Lit(Value::Str("a".into())),
                ],
            }),
        )),
    ]
}

fn predicate() -> impl Strategy<Value = P> {
    leaf().prop_recursive(4, 24, 2, |inner| {
        prop_oneof![
            inner.clone().prop_map(|p| P::Not(Box::new(p))),
            inner.clone().prop_map(|p| P::Paren(Box::new(p))),
            (inner.clone(), inner.clone()).prop_map(|(l, r)| P::And(Box::new(l), Box::new(r))),
            (inner.clone(), inner).prop_map(|(l, r)| P::Or(Box::new(l), Box::new(r))),
        ]
    })
}

/// Nodes with every mix of x and y present, absent, and of another type.
fn mixed() -> Fx {
    let mut fx = Fx::new();
    let vals = [
        None,
        Some(Value::Int(0)),
        Some(Value::Int(1)),
        Some(Value::Int(2)),
        Some(Value::Str("a".into())),
    ];
    let mut n = 0;
    for x in &vals {
        for y in &vals {
            let mut props = Vec::new();
            if let Some(x) = x {
                props.push(("x", x.clone()));
            }
            if let Some(y) = y {
                props.push(("y", y.clone()));
            }
            if n % 2 == 0 {
                props.push(("s", Value::Str("abc".into())));
            }
            fx.add("M", &format!("k{n:02}"), props);
            n += 1;
        }
    }
    fx
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn where_parses_to_the_tree_the_old_grammar_built(p in predicate()) {
        let src = format!("MATCH (n:M) WHERE {} RETURN n.key AS k", p.text());
        let q = parse(&lex(&src).expect("lex")).unwrap_or_else(|e| panic!("`{src}`: {e}"));
        prop_assert_eq!(q.where_expr, Some(p.tree()), "`{}`", src);
    }

    #[test]
    fn where_keeps_the_rows_where_the_projection_is_true(p in predicate()) {
        let fx = mixed();
        let params = BTreeMap::from([("p".to_string(), Value::Int(1))]);
        let w = p.text();
        let kept = col_with(
            &fx,
            &format!("MATCH (n:M) WHERE {w} RETURN n.key AS k ORDER BY k"),
            "k",
            &params,
        );
        let bounded = col_with(
            &fx,
            &format!("MATCH (n:M) WHERE {w} RETURN n.key AS k ORDER BY k LIMIT 1000"),
            "k",
            &params,
        );
        prop_assert_eq!(&kept, &bounded, "LIMIT changed `{}`", w);
        // WHERE keeps exactly the rows whose projected predicate is true.
        let rs = run(
            &fx.view(),
            &format!("MATCH (n:M) RETURN n.key AS k, {w} AS r ORDER BY k"),
            &params,
        )
        .unwrap_or_else(|e| panic!("`{w}`: {e}"));
        let truthy: Vec<Option<Value>> = (0..rs.len())
            .filter(|i| match rs.get(*i, "r") {
                Some(Value::Bool(b)) => *b,
                None | Some(Value::Null) => false,
                Some(Value::Int(n)) => *n != 0,
                Some(Value::Str(s)) => !s.is_empty(),
                Some(other) => panic!("`{w}` gave {other:?}"),
            })
            .map(|i| rs.get(i, "k").cloned())
            .collect();
        prop_assert_eq!(kept, truthy, "`{}`", w);
    }
}
