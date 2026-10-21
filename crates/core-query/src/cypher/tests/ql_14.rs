//! QL-14: `reduce(acc = init, x IN list | expr)`, evaluated through the
//! full lex/parse/plan/execute path.

use super::tests::{eval, run, Fx};
use crate::cypher::plan::plan;
use crate::cypher::{lex, parse};
use core_storage::Value;
use std::collections::BTreeMap;

/// The error a query gives before it runs: a parse or a plan error.
fn compile_err(q: &str) -> String {
    match parse(&lex(q).expect("lex")) {
        Err(e) => e,
        Ok(stmt) => plan(&stmt).expect_err("must fail to plan"),
    }
}

fn ints(xs: &[i64]) -> Value {
    Value::List(xs.iter().map(|n| Value::Int(*n)).collect())
}

fn col(fx: &Fx, q: &str, name: &str) -> Vec<Option<Value>> {
    let rs = run(&fx.view(), q, &BTreeMap::new()).unwrap_or_else(|e| panic!("`{q}` failed: {e}"));
    (0..rs.len()).map(|i| rs.get(i, name).cloned()).collect()
}

#[test]
fn folds_a_list() {
    assert_eq!(
        eval("reduce(acc = 0, x IN [1,2,3,4,5] | acc + x)"),
        Some(Value::Int(15))
    );
    assert_eq!(
        eval("reduce(acc = \"\", x IN [\"a\",\"b\",\"c\"] | acc + x)"),
        Some(Value::Str("abc".into()))
    );
    // An empty list gives the initial value.
    assert_eq!(
        eval("reduce(acc = 1, x IN [] | acc * x)"),
        Some(Value::Int(1))
    );
    // The keyword is case-insensitive, like the other function names.
    assert_eq!(
        eval("REDUCE(acc = 0, x in [1,2] | acc + x)"),
        Some(Value::Int(3))
    );
}

#[test]
fn null_list_yields_null() {
    assert_eq!(eval("reduce(acc = 0, x IN null | acc + x)"), None);
    // A non-list source is null too, as in a list comprehension.
    assert_eq!(eval("reduce(acc = 0, x IN 5 | acc + x)"), None);
}

#[test]
fn null_values_propagate_through_the_body() {
    // A null element makes `acc + x` null; the accumulator is then null.
    assert_eq!(eval("reduce(acc = 0, x IN [1, null, 3] | acc + x)"), None);
    // A null init is a real accumulator value.
    assert_eq!(
        eval("reduce(acc = null, x IN [1, 2] | coalesce(acc, 0) + x)"),
        Some(Value::Int(3))
    );
}

#[test]
fn variables_shadow_outer_names_inside_only() {
    let fx = Fx::new();
    let q = "UNWIND [10, 20] AS acc \
             RETURN acc, reduce(acc = 1, x IN [1, 2, 3] | acc + x) AS r, acc + 0 AS after";
    assert_eq!(
        col(&fx, q, "acc"),
        vec![Some(Value::Int(10)), Some(Value::Int(20))]
    );
    assert_eq!(col(&fx, q, "r"), vec![Some(Value::Int(7)); 2]);
    assert_eq!(
        col(&fx, q, "after"),
        vec![Some(Value::Int(10)), Some(Value::Int(20))]
    );
    // The init and the list are evaluated outside the reduce's scope.
    let q = "UNWIND [100] AS acc UNWIND [5] AS x \
             RETURN reduce(acc = acc, x IN [x, x] | acc + x) AS r";
    assert_eq!(col(&fx, q, "r"), vec![Some(Value::Int(110))]);
    // The iteration variable shadows too.
    let q = "UNWIND [1000] AS x RETURN reduce(a = 0, x IN [1, 2] | a + x) AS r, x";
    assert_eq!(col(&fx, q, "r"), vec![Some(Value::Int(3))]);
    assert_eq!(col(&fx, q, "x"), vec![Some(Value::Int(1000))]);
}

#[test]
fn variables_do_not_leak_out() {
    let err = compile_err("UNWIND [1] AS y RETURN reduce(a = 0, x IN [1, 2] | a + x) AS r, a");
    assert!(err.contains("unbound variable `a`"), "{err}");
    let err = compile_err("UNWIND [1] AS y RETURN reduce(a = 0, x IN [1, 2] | a + x) AS r, x");
    assert!(err.contains("unbound variable `x`"), "{err}");
    // Neither is visible to the init or the list.
    let err = compile_err("RETURN reduce(a = x, x IN [1] | a + x) AS r");
    assert!(err.contains("unbound variable `x`"), "{err}");
    let err = compile_err("RETURN reduce(a = 0, x IN [a] | a + x) AS r");
    assert!(err.contains("unbound variable `a`"), "{err}");
}

#[test]
fn nesting_with_reduce_and_comprehensions() {
    // Nested reduce: the inner one is the outer body.
    assert_eq!(
        eval("reduce(t = 0, x IN [1, 2, 3] | t + reduce(s = 0, y IN range(1, x) | s + y))"),
        Some(Value::Int(1 + 3 + 6))
    );
    // The same names nested: the inner shadows, the outer resumes afterwards.
    assert_eq!(
        eval("reduce(a = 0, x IN [1, 2] | reduce(a = a, x IN [10] | a + x) + x)"),
        Some(Value::Int(23))
    );
    // A reduce inside a comprehension.
    assert_eq!(
        eval("[x IN [1, 2, 3] | reduce(a = 0, y IN range(1, x) | a + y)]"),
        Some(ints(&[1, 3, 6]))
    );
    // A comprehension inside a reduce, as the source and as the body.
    assert_eq!(
        eval("reduce(a = 0, x IN [y IN [1, 2, 3, 4] WHERE y > 1 | y * y] | a + x)"),
        Some(Value::Int(29))
    );
    assert_eq!(
        eval("reduce(a = [], x IN [1, 2, 3] | a + [z IN [x] | z * 2])"),
        Some(ints(&[2, 4, 6]))
    );
}

#[test]
fn in_match_return_where_order_by_and_function_args() {
    let mut fx = Fx::new();
    fx.add("P", "a", vec![("scores", ints(&[1, 2, 3]))]);
    fx.add("P", "b", vec![("scores", ints(&[-1, 5]))]);
    fx.add("P", "c", vec![("scores", ints(&[]))]);

    let q = "MATCH (n:P) RETURN n.key AS k, reduce(acc = 0, x IN n.scores | acc + x) AS total \
             ORDER BY k";
    assert_eq!(
        col(&fx, q, "total"),
        vec![
            Some(Value::Int(6)),
            Some(Value::Int(4)),
            Some(Value::Int(0))
        ]
    );
    let q = "MATCH (n:P) WHERE reduce(acc = 0, x IN n.scores | acc + x) > 5 RETURN n.key AS k";
    assert_eq!(col(&fx, q, "k"), vec![Some(Value::Str("a".into()))]);
    let q =
        "MATCH (n:P) RETURN n.key AS k ORDER BY reduce(acc = 0, x IN n.scores | acc + x) DESC, k";
    assert_eq!(
        col(&fx, q, "k"),
        vec![
            Some(Value::Str("a".into())),
            Some(Value::Str("b".into())),
            Some(Value::Str("c".into()))
        ]
    );
    // As a function argument, and arithmetic around it.
    assert_eq!(
        eval("toString(reduce(a = 1, x IN [2, 3] | a * x)) + \"!\""),
        Some(Value::Str("6!".into()))
    );
    assert_eq!(
        eval("1 + reduce(a = 0, x IN [1, 2] | a + x) * 2"),
        Some(Value::Int(7))
    );
}

#[test]
fn parameter_source_and_a_row_dependent_body() {
    let fx = Fx::new();
    let mut params = BTreeMap::new();
    params.insert("xs".to_string(), ints(&[1, 2, 3]));
    let rs = run(
        &fx.view(),
        "UNWIND [10, 20] AS z RETURN reduce(a = z, x IN $xs | a + x * z) AS r",
        &params,
    )
    .unwrap();
    assert_eq!(rs.get(0, "r"), Some(&Value::Int(70)));
    assert_eq!(rs.get(1, "r"), Some(&Value::Int(140)));
}

#[test]
fn malformed_reduce_is_a_parse_error() {
    for q in [
        "RETURN reduce(a = 0, x IN [1])",
        "RETURN reduce(a, x IN [1] | a)",
        "RETURN reduce(a = 0, x [1] | a)",
        "RETURN reduce(a = 0 x IN [1] | a)",
        "RETURN reduce(a = 0, x IN [1] | a",
        "RETURN reduce()",
        "RETURN reduce(a = 0, x IN [1] | a, 5)",
    ] {
        let err = compile_err(q);
        assert!(err.contains("reduce"), "`{q}` gave: {err}");
    }
}

#[test]
fn deep_nesting_is_refused_cleanly() {
    let nest = |depth: usize| {
        let mut q = "1".to_string();
        for _ in 0..depth {
            q = format!("reduce(a = 0, x IN [1] | {q})");
        }
        format!("RETURN {q} AS r")
    };
    assert!(
        compile_err(&nest(200)).contains("nesting too deep"),
        "200 deep must be refused"
    );
    for depth in [20, 60] {
        let q = nest(depth);
        assert!(
            parse(&lex(&q).unwrap()).is_ok(),
            "{depth} deep must still parse"
        );
    }
}

#[test]
fn plus_joins_strings_and_lists_but_no_other_operator_does() {
    assert_eq!(eval("\"ab\" + \"cd\""), Some(Value::Str("abcd".into())));
    assert_eq!(eval("[1] + [2, 3]"), Some(ints(&[1, 2, 3])));
    assert_eq!(eval("\"ab\" + null"), None);
    for q in ["\"ab\" - \"b\"", "\"ab\" * \"b\"", "\"a\" + 1", "[1] + 2"] {
        let fx = Fx::new();
        let err = run(&fx.view(), &format!("RETURN {q} AS r"), &BTreeMap::new())
            .expect_err("must be a type error");
        assert!(err.contains("arithmetic operand must be numeric"), "{err}");
    }
}
