//! QL-04: the `stDev`, `stDevP`, `percentileCont` and `percentileDisc`
//! aggregates, through the full lex/parse/plan/execute path.

use super::tests::{run, Fx};
use crate::cypher::plan::plan;
use crate::cypher::{lex, parse};
use core_storage::Value;
use std::collections::BTreeMap;

const WIKI: [f64; 8] = [2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0];

fn fx_scores(scores: &[Value]) -> Fx {
    let mut fx = Fx::new();
    for (i, s) in scores.iter().enumerate() {
        fx.add("N", &format!("n{i}"), vec![("score", s.clone())]);
    }
    fx
}

fn floats(xs: &[f64]) -> Fx {
    fx_scores(&xs.iter().map(|x| Value::Float(*x)).collect::<Vec<_>>())
}

fn ints(xs: &[i64]) -> Fx {
    fx_scores(&xs.iter().map(|x| Value::Int(*x)).collect::<Vec<_>>())
}

fn one(fx: &Fx, q: &str) -> Option<Value> {
    one_with(fx, q, &BTreeMap::new())
}

fn one_with(fx: &Fx, q: &str, params: &BTreeMap<String, Value>) -> Option<Value> {
    let rs = run(&fx.view(), q, params).unwrap_or_else(|e| panic!("`{q}` failed: {e}"));
    assert_eq!(rs.len(), 1, "`{q}` must give one row");
    rs.get(0, &rs.columns()[0]).cloned()
}

fn err(fx: &Fx, q: &str) -> String {
    run(&fx.view(), q, &BTreeMap::new()).expect_err("must fail")
}

/// The error a query gives before it runs: a parse or a plan error.
fn compile_err(q: &str) -> String {
    match parse(&lex(q).expect("lex")) {
        Err(e) => e,
        Ok(stmt) => plan(&stmt).expect_err("must fail to compile"),
    }
}

fn assert_close(got: Option<Value>, want: f64) {
    match got {
        Some(Value::Float(g)) => assert!((g - want).abs() < 1e-9, "got {g}, want {want}"),
        other => panic!("want Float({want}), got {other:?}"),
    }
}

#[test]
fn stdev_is_the_sample_deviation_and_stdevp_the_population_one() {
    let fx = floats(&WIKI);
    // sum of squared deviations is 32: sqrt(32/7) sample, sqrt(32/8) = 2 population.
    assert_close(
        one(&fx, "MATCH (n:N) RETURN stDev(n.score)"),
        (32.0f64 / 7.0).sqrt(),
    );
    assert_close(one(&fx, "MATCH (n:N) RETURN stDevP(n.score)"), 2.0);
}

#[test]
fn function_names_are_case_insensitive() {
    let fx = floats(&WIKI);
    assert_close(one(&fx, "MATCH (n:N) RETURN stdevp(n.score)"), 2.0);
    assert_close(one(&fx, "MATCH (n:N) RETURN STDEVP(n.score)"), 2.0);
    assert_eq!(
        one(
            &ints(&[1, 2, 3]),
            "MATCH (n:N) RETURN PERCENTILEDISC(n.score, 0.5)"
        ),
        Some(Value::Int(2))
    );
}

#[test]
fn stdev_of_fewer_than_two_values_is_zero_not_null() {
    let one_val = floats(&[3.0]);
    assert_close(one(&one_val, "MATCH (n:N) RETURN stDev(n.score)"), 0.0);
    assert_close(one(&one_val, "MATCH (n:N) RETURN stDevP(n.score)"), 0.0);
    let none = Fx::new();
    assert_close(one(&none, "MATCH (n:N) RETURN stDev(n.score)"), 0.0);
    assert_close(one(&none, "MATCH (n:N) RETURN stDevP(n.score)"), 0.0);
    // all-null input is the same as no input
    let nulls = fx_scores(&[Value::Null, Value::Null]);
    assert_close(one(&nulls, "MATCH (n:N) RETURN stDev(n.score)"), 0.0);
}

#[test]
fn nulls_are_skipped_and_ints_mix_with_floats() {
    let fx = fx_scores(&[Value::Int(1), Value::Float(3.0), Value::Null, Value::Int(5)]);
    assert_close(
        one(&fx, "MATCH (n:N) RETURN stDevP(n.score)"),
        (8.0f64 / 3.0).sqrt(),
    );
    // a missing property is null too
    let mut fx = ints(&[2, 4]);
    fx.add("N", "blank", vec![]);
    assert_close(one(&fx, "MATCH (n:N) RETURN stDev(n.score)"), 2.0f64.sqrt());
}

#[test]
fn percentile_disc_is_nearest_rank_and_keeps_the_input_type() {
    let fx = ints(&[5, 1, 4, 2, 3]);
    let q = |p: &str| format!("MATCH (n:N) RETURN percentileDisc(n.score, {p})");
    assert_eq!(one(&fx, &q("0.5")), Some(Value::Int(3)));
    assert_eq!(one(&fx, &q("0")), Some(Value::Int(1)));
    assert_eq!(one(&fx, &q("1")), Some(Value::Int(5)));
    assert_eq!(one(&fx, &q("0.2")), Some(Value::Int(1)));
    assert_eq!(one(&fx, &q("0.21")), Some(Value::Int(2)));
    let fl = floats(&[1.5, 2.5]);
    assert_eq!(one(&fl, &q("1")), Some(Value::Float(2.5)));
}

#[test]
fn percentile_cont_interpolates() {
    let fx = floats(&[1.0, 2.0, 3.0, 4.0, 5.0]);
    let q = |p: &str| format!("MATCH (n:N) RETURN percentileCont(n.score, {p})");
    assert_close(one(&fx, &q("0.5")), 3.0);
    assert_close(one(&fx, &q("0.1")), 1.4);
    assert_close(one(&fx, &q("0")), 1.0);
    assert_close(one(&fx, &q("1")), 5.0);
    // ints give a float
    assert_close(one(&ints(&[1, 2]), &q("0.5")), 1.5);
}

#[test]
fn percentile_of_nothing_is_null() {
    let none = Fx::new();
    assert_eq!(
        one(&none, "MATCH (n:N) RETURN percentileCont(n.score, 0.5)"),
        None
    );
    assert_eq!(
        one(&none, "MATCH (n:N) RETURN percentileDisc(n.score, 0.5)"),
        None
    );
}

#[test]
fn p_must_be_a_number_in_range() {
    let fx = ints(&[1, 2, 3]);
    for f in ["percentileCont", "percentileDisc"] {
        for p in ["1.5", "-0.1", "2"] {
            let e = err(&fx, &format!("MATCH (n:N) RETURN {f}(n.score, {p})"));
            assert!(e.contains(f) && e.contains("p must be in [0,1]"), "{e}");
        }
        let e = err(&fx, &format!("MATCH (n:N) RETURN {f}(n.score, 'x')"));
        assert!(e.contains(f) && e.contains("p must be a number"), "{e}");
    }
    // the range is checked even when there are no rows
    let e = err(&Fx::new(), "MATCH (n:N) RETURN percentileCont(n.score, 3)");
    assert!(e.contains("p must be in [0,1]"), "{e}");
}

#[test]
fn p_may_be_a_parameter() {
    let fx = ints(&[1, 2, 3]);
    let mut params = BTreeMap::new();
    params.insert("p".to_string(), Value::Float(0.5));
    assert_eq!(
        one_with(
            &fx,
            "MATCH (n:N) RETURN percentileDisc(n.score, $p)",
            &params
        ),
        Some(Value::Int(2))
    );
    params.insert("p".to_string(), Value::Float(7.0));
    let e = run(
        &fx.view(),
        "MATCH (n:N) RETURN percentileDisc(n.score, $p)",
        &params,
    )
    .expect_err("out of range");
    assert!(e.contains("p must be in [0,1]"), "{e}");
    let e = err(&fx, "MATCH (n:N) RETURN percentileDisc(n.score, $missing)");
    assert!(e.contains("percentileDisc"), "{e}");
}

#[test]
fn percentile_needs_p_and_stdev_takes_none() {
    let e = compile_err("MATCH (n:N) RETURN percentileCont(n.score)");
    assert!(e.contains("percentile"), "{e}");
    assert!(compile_err("MATCH (n:N) RETURN stDev(n.score, 0.5)").contains("')'"));
    for f in ["stDev", "stDevP", "percentileCont", "percentileDisc"] {
        let e = compile_err(&format!("MATCH (n:N) RETURN {f}(*, 0.5)"));
        assert!(!e.is_empty(), "{f}(*) must not compile");
    }
}

#[test]
fn a_non_numeric_input_is_a_type_error() {
    let fx = fx_scores(&[Value::Int(1), Value::Str("x".into())]);
    for f in [
        "stDev(n.score)",
        "stDevP(n.score)",
        "percentileCont(n.score, 0.5)",
        "percentileDisc(n.score, 0.5)",
    ] {
        let e = err(&fx, &format!("MATCH (n:N) RETURN {f}"));
        assert!(
            e.contains(f.split('(').next().unwrap()) && e.contains("number"),
            "{e}"
        );
    }
}

#[test]
fn grouping_distinct_and_with() {
    let mut fx = Fx::new();
    for (i, (g, s)) in [("a", 1), ("a", 3), ("a", 3), ("b", 10), ("b", 20)]
        .iter()
        .enumerate()
    {
        fx.add(
            "N",
            &format!("n{i}"),
            vec![("g", Value::Str((*g).into())), ("score", Value::Int(*s))],
        );
    }
    let rs = run(
        &fx.view(),
        "MATCH (n:N) RETURN n.g AS g, stDevP(n.score) AS sd, percentileDisc(n.score, 1) AS top ORDER BY g",
        &BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(rs.len(), 2);
    assert_close(rs.get(0, "sd").cloned(), (8.0f64 / 9.0).sqrt());
    assert_eq!(rs.get(0, "top"), Some(&Value::Int(3)));
    assert_close(rs.get(1, "sd").cloned(), 5.0);
    assert_eq!(rs.get(1, "top"), Some(&Value::Int(20)));
    // DISTINCT drops the repeated 3
    assert_close(
        one(
            &fx,
            "MATCH (n:N) WHERE n.g = 'a' RETURN stDevP(DISTINCT n.score)",
        ),
        1.0,
    );
    assert_eq!(
        one(
            &fx,
            "MATCH (n:N) WHERE n.g = 'a' RETURN percentileDisc(DISTINCT n.score, 0.5)"
        ),
        Some(Value::Int(1))
    );
    // aggregate WITH feeds a later stage
    assert_eq!(
        one(
            &fx,
            "MATCH (n:N) WITH n.g AS g, percentileCont(n.score, 0.5) AS m WHERE g = 'b' RETURN m"
        ),
        Some(Value::Float(15.0))
    );
    // several in one RETURN, with an empty match still one row
    let rs = run(
        &Fx::new().view(),
        "MATCH (n:N) RETURN stDev(n.score) AS a, percentileCont(n.score, 0.5) AS b, count(n) AS c",
        &BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(rs.len(), 1);
    assert_eq!(rs.get(0, "a"), Some(&Value::Float(0.0)));
    assert_eq!(rs.get(0, "b"), None);
    assert_eq!(rs.get(0, "c"), Some(&Value::Int(0)));
}

#[test]
fn percentile_disc_picks_the_exact_rank_at_a_boundary() {
    let q = |p: f64| format!("MATCH (n:N) RETURN percentileDisc(n.score, {p:?})");
    // 0.07 * 100 is 7.000000000000001 in floating point: still the 7th value.
    let hundred = ints(&(1..=100).collect::<Vec<_>>());
    assert_eq!(one(&hundred, &q(0.07)), Some(Value::Int(7)));
    for n in [1usize, 2, 3, 7, 10, 100] {
        let fx = ints(&(1..=n as i64).rev().collect::<Vec<_>>());
        assert_eq!(one(&fx, &q(0.0)), Some(Value::Int(1)), "n={n} p=0");
        assert_eq!(one(&fx, &q(1.0)), Some(Value::Int(n as i64)), "n={n} p=1");
        for k in 1..=n {
            let p = k as f64 / n as f64;
            assert_eq!(one(&fx, &q(p)), Some(Value::Int(k as i64)), "n={n} k={k}");
        }
    }
}

#[test]
fn percentile_cont_is_continuous_across_a_boundary() {
    // p * (n - 1) = 7.000000000000001: the interpolation lands on the 8th value.
    let fx = ints(&(1..=101).collect::<Vec<_>>());
    assert_close(
        one(&fx, "MATCH (n:N) RETURN percentileCont(n.score, 0.07)"),
        8.0,
    );
}

#[test]
fn stdev_is_stable_with_a_large_offset() {
    let xs: Vec<f64> = WIKI.iter().map(|x| 1e9 + x).collect();
    let fx = floats(&xs);
    assert_close(one(&fx, "MATCH (n:N) RETURN stDevP(n.score)"), 2.0);
    assert_close(
        one(&fx, "MATCH (n:N) RETURN stDev(n.score)"),
        (32.0f64 / 7.0).sqrt(),
    );
}
