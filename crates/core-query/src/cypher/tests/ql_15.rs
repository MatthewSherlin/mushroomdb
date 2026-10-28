//! QL-15: list comprehensions `[x IN list WHERE pred | expr]`, evaluated
//! through the full lex/parse/plan/execute path.

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

fn strs(xs: &[&str]) -> Value {
    Value::List(xs.iter().map(|n| Value::Str((*n).into())).collect())
}

fn col(fx: &Fx, q: &str, name: &str) -> Vec<Option<Value>> {
    let rs = run(&fx.view(), q, &BTreeMap::new()).unwrap_or_else(|e| panic!("`{q}` failed: {e}"));
    (0..rs.len()).map(|i| rs.get(i, name).cloned()).collect()
}

#[test]
fn all_three_optional_parts() {
    assert_eq!(
        eval("[x IN [1,2,3,4] WHERE x > 2 | x * 2]"),
        Some(ints(&[6, 8]))
    );
    assert_eq!(eval("[x IN [1,2,3] | x * 10]"), Some(ints(&[10, 20, 30])));
    assert_eq!(eval("[x IN [1,2,3] WHERE x > 1]"), Some(ints(&[2, 3])));
    assert_eq!(eval("[x IN [] WHERE x > 0 | x]"), Some(ints(&[])));
    assert_eq!(eval("[x IN [1,2,3]]"), Some(ints(&[1, 2, 3])));
    assert_eq!(eval("[x in [1,2] where x > 1 | x]"), Some(ints(&[2])));
}

#[test]
fn null_list_yields_null_and_null_elements_are_real() {
    assert_eq!(eval("[x IN null WHERE x > 0 | x]"), None);
    // A non-list source is null too, like a subscript of a non-list.
    assert_eq!(eval("[x IN 5 | x]"), None);
    // A null element passes through the projection...
    assert_eq!(
        eval("[x IN [1, null, 3] | x]"),
        Some(Value::List(vec![Value::Int(1), Value::Null, Value::Int(3)]))
    );
    // ...and the mapped value may itself be null.
    assert_eq!(
        eval("[x IN [1, 2] | null]"),
        Some(Value::List(vec![Value::Null, Value::Null]))
    );
    // Under WHERE a null comparison counts as false.
    assert_eq!(eval("[x IN [1, null, 3] WHERE x > 0]"), Some(ints(&[1, 3])));
    assert_eq!(
        eval("[x IN [1, null, 3] WHERE x IS NULL]"),
        Some(Value::List(vec![Value::Null]))
    );
}

#[test]
fn variable_shadows_an_outer_one_inside_only() {
    let fx = Fx::new();
    let q = "UNWIND [1, 2] AS x RETURN x, [x IN [10, 20] | x + 1] AS inner, \
             [y IN [10, 20] | y + x] AS outer_used";
    assert_eq!(
        col(&fx, q, "x"),
        vec![Some(Value::Int(1)), Some(Value::Int(2))]
    );
    assert_eq!(col(&fx, q, "inner"), vec![Some(ints(&[11, 21])); 2]);
    assert_eq!(
        col(&fx, q, "outer_used"),
        vec![Some(ints(&[11, 21])), Some(ints(&[12, 22]))]
    );
    // The outer variable is intact in a later part of the same item list.
    let q2 = "UNWIND [5] AS x RETURN [x IN [1, 2] WHERE x > 1 | x] AS a, x + 0 AS b";
    assert_eq!(col(&fx, q2, "a"), vec![Some(ints(&[2]))]);
    assert_eq!(col(&fx, q2, "b"), vec![Some(Value::Int(5))]);
}

#[test]
fn variable_does_not_leak_out() {
    let err = compile_err("UNWIND [1] AS y RETURN [x IN [1, 2] | x] AS l, x");
    assert!(err.contains("unbound variable `x`"), "{err}");
}

#[test]
fn nesting() {
    assert_eq!(
        eval("[x IN [1, 2, 3] | [y IN [1, 2, 3] WHERE y <= x | y]]"),
        Some(Value::List(vec![
            ints(&[1]),
            ints(&[1, 2]),
            ints(&[1, 2, 3])
        ]))
    );
    // The inner comprehension reads the outer variable.
    assert_eq!(
        eval("[x IN [1, 2] | size([y IN [1, 2, 3] WHERE y > x])]"),
        Some(ints(&[2, 1]))
    );
    // The same name nested: the inner shadows, the outer resumes afterwards.
    assert_eq!(
        eval("[x IN [1, 2] | [x IN [10] | x]]"),
        Some(Value::List(vec![ints(&[10]), ints(&[10])]))
    );
    // The comprehension as the source of another.
    assert_eq!(
        eval("[x IN [y IN [1, 2, 3, 4] WHERE y > 1 | y * y] WHERE x < 10 | x]"),
        Some(ints(&[4, 9]))
    );
}

#[test]
fn in_where_return_order_by_unwind_and_function_args() {
    let mut fx = Fx::new();
    fx.add(
        "P",
        "a",
        vec![
            ("scores", ints(&[1, 2, 3])),
            ("tags", strs(&["old", "new"])),
        ],
    );
    fx.add(
        "P",
        "b",
        vec![("scores", ints(&[-1, 5])), ("tags", strs(&["old"]))],
    );
    fx.add(
        "P",
        "c",
        vec![
            ("scores", ints(&[4, 5, 6, -7])),
            ("tags", strs(&["x", "y"])),
        ],
    );

    let q = "MATCH (n:P) RETURN [x IN n.tags WHERE x <> \"old\" | x] AS active ORDER BY n.key";
    assert_eq!(
        col(&fx, q, "active"),
        vec![
            Some(strs(&["new"])),
            Some(strs(&[])),
            Some(strs(&["x", "y"]))
        ]
    );

    let q = "MATCH (n:P) WHERE size([x IN n.scores WHERE x > 0]) > 2 RETURN n.key AS k ORDER BY k";
    assert_eq!(
        col(&fx, q, "k"),
        vec![Some(Value::Str("a".into())), Some(Value::Str("c".into()))]
    );

    let q = "MATCH (n:P) RETURN n.key AS k ORDER BY size([x IN n.scores WHERE x > 0]) DESC, k";
    assert_eq!(
        col(&fx, q, "k"),
        vec![
            Some(Value::Str("a".into())),
            Some(Value::Str("c".into())),
            Some(Value::Str("b".into()))
        ]
    );

    let q = "UNWIND [x IN range(1,10) WHERE x % 2 = 0 | x] AS even RETURN even";
    assert_eq!(
        col(&fx, q, "even"),
        [2, 4, 6, 8, 10]
            .iter()
            .map(|n| Some(Value::Int(*n)))
            .collect::<Vec<_>>()
    );

    // As a function argument, and a function inside the parts.
    assert_eq!(
        eval("size([x IN range(1, 5) WHERE x > 2])"),
        Some(Value::Int(3))
    );
    assert_eq!(
        eval("[x IN [\"A\", \"b\"] | toLower(x)]"),
        Some(strs(&["a", "b"]))
    );
}

#[test]
fn parameter_source() {
    let fx = Fx::new();
    let mut params = BTreeMap::new();
    params.insert("xs".to_string(), ints(&[1, 2, 3]));
    let rs = run(
        &fx.view(),
        "UNWIND [0] AS z RETURN [x IN $xs WHERE x > z + 1 | x] AS r",
        &params,
    )
    .expect("run");
    assert_eq!(rs.get(0, "r").cloned(), Some(ints(&[2, 3])));
}

#[test]
fn malformed_comprehensions_are_parse_errors() {
    for q in [
        "RETURN [x IN [1, 2] WHERE | x]",
        "RETURN [x IN [1, 2] | x",
        "RETURN [x IN | x]",
    ] {
        assert!(compile_err(q).contains("expected"), "{q}");
    }
}

#[test]
fn deep_nesting_is_refused_cleanly() {
    let deep = |n: usize| {
        let mut s = String::from("[1]");
        for _ in 0..n {
            s = format!("[x IN {s} | x]");
        }
        s
    };
    let err = compile_err(&format!("RETURN {}", deep(200)));
    assert!(err.contains("too deep"), "{err}");
    assert!(eval(&deep(20)).is_some());
    // Right at the limit must run, not overflow the stack.
    assert!(eval(&deep(60)).is_some());
}
