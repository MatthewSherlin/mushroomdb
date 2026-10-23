//! QL-AG (defect-ledger row 157): an aggregate over a bare variable uses its value.
//!
//! `UNWIND [1,2] AS x RETURN max(x)` returned null: `sum`, `avg`, `min` and
//! `max` had arms for a property argument and for nothing else, so a variable
//! bound by `UNWIND` / `WITH` fell to the catch-all arm and contributed no row.
//! `count`, `collect`, `stDev` and the percentiles already read a variable.

use super::tests::{run, Fx};
use core_storage::Value;
use std::collections::BTreeMap;

type Rows = Vec<Vec<Option<Value>>>;

fn try_rows(fx: &Fx, q: &str) -> Result<Rows, String> {
    let rs = run(&fx.view(), q, &BTreeMap::new())?;
    Ok((0..rs.len()).map(|i| rs.row(i).to_vec()).collect())
}

fn rows(fx: &Fx, q: &str) -> Rows {
    try_rows(fx, q).unwrap_or_else(|e| panic!("`{q}` failed: {e}"))
}

fn one(fx: &Fx, q: &str) -> Option<Value> {
    let r = rows(fx, q);
    assert_eq!(r.len(), 1, "`{q}` -> {r:?}");
    r[0][0].clone()
}

fn int(n: i64) -> Option<Value> {
    Some(Value::Int(n))
}

fn float(f: f64) -> Option<Value> {
    Some(Value::Float(f))
}

#[test]
fn the_reported_query_returns_the_maximum() {
    let fx = Fx::new();
    assert_eq!(one(&fx, "UNWIND [1,2] AS x RETURN max(x)"), int(2));
}

#[test]
fn every_aggregate_over_an_unwound_variable() {
    let fx = Fx::new();
    let l = "UNWIND [3, 1, null, 2.5, 1] AS x";
    assert_eq!(one(&fx, &format!("{l} RETURN max(x)")), int(3));
    assert_eq!(one(&fx, &format!("{l} RETURN min(x)")), int(1));
    assert_eq!(one(&fx, &format!("{l} RETURN sum(x)")), float(7.5));
    assert_eq!(one(&fx, &format!("{l} RETURN avg(x)")), float(1.875));
    assert_eq!(one(&fx, &format!("{l} RETURN count(x)")), int(4));
    assert_eq!(one(&fx, &format!("{l} RETURN count(DISTINCT x)")), int(3));
    assert_eq!(one(&fx, &format!("{l} RETURN sum(DISTINCT x)")), float(6.5));
    assert_eq!(one(&fx, &format!("{l} RETURN max(DISTINCT x)")), int(3));
    assert_eq!(one(&fx, &format!("{l} RETURN min(DISTINCT x)")), int(1));
    assert_eq!(
        one(&fx, &format!("{l} RETURN avg(DISTINCT x)")),
        float(6.5 / 3.0)
    );
    assert_eq!(
        one(&fx, &format!("{l} RETURN collect(x)")),
        Some(Value::List(vec![
            Value::Int(3),
            Value::Int(1),
            Value::Float(2.5),
            Value::Int(1)
        ]))
    );
    assert_eq!(
        one(&fx, "UNWIND [1,2,3,4] AS x RETURN percentileDisc(x, 0.5)"),
        int(2)
    );
    assert_eq!(
        one(&fx, "UNWIND [1,2,3,4] AS x RETURN percentileCont(x, 0.5)"),
        float(2.5)
    );
    assert_eq!(
        one(&fx, "UNWIND [2,4,4,4,5,5,7,9] AS x RETURN stDevP(x)"),
        float(2.0)
    );
}

#[test]
fn strings_have_a_minimum_and_a_maximum() {
    let fx = Fx::new();
    let l = "UNWIND ['b', 'a', null, 'c'] AS x";
    assert_eq!(
        one(&fx, &format!("{l} RETURN max(x)")),
        Some(Value::Str("c".into()))
    );
    assert_eq!(
        one(&fx, &format!("{l} RETURN min(x)")),
        Some(Value::Str("a".into()))
    );
    // Not a number: sum and avg skip it, as for a property.
    assert_eq!(one(&fx, &format!("{l} RETURN sum(x)")), None);
}

#[test]
fn empty_and_all_null_inputs() {
    let fx = Fx::new();
    for l in ["UNWIND [] AS x", "UNWIND [null, null] AS x"] {
        for f in ["max", "min", "sum", "avg"] {
            assert_eq!(one(&fx, &format!("{l} RETURN {f}(x)")), None, "{l} {f}");
        }
        assert_eq!(one(&fx, &format!("{l} RETURN count(x)")), int(0));
    }
}

#[test]
fn a_variable_carried_through_with() {
    let fx = Fx::new();
    assert_eq!(one(&fx, "UNWIND [1,2,3] AS x WITH x RETURN max(x)"), int(3));
    assert_eq!(
        one(&fx, "UNWIND [1,2,3] AS x WITH x AS y RETURN sum(y)"),
        float(6.0)
    );
    assert_eq!(
        one(
            &fx,
            "UNWIND [1,2,3] AS x WITH x AS y WHERE y > 1 RETURN min(y)"
        ),
        int(2)
    );
    // An aggregate in WITH, aggregated again after it.
    assert_eq!(
        rows(
            &fx,
            "UNWIND [1,2,3] AS x WITH max(x) AS m, min(x) AS n RETURN max(m), min(n)"
        ),
        vec![vec![int(3), int(1)]]
    );
}

#[test]
fn grouping_keys() {
    let fx = Fx::new();
    let r = rows(
        &fx,
        "UNWIND [{k: 'a', v: 1}, {k: 'b', v: 5}, {k: 'a', v: 3}, {k: 'b', v: null}] AS r \
         WITH r.k AS k, r.v AS x RETURN k, max(x), min(x), sum(x), avg(x) ORDER BY k",
    );
    assert_eq!(
        r,
        vec![
            vec![
                Some(Value::Str("a".into())),
                int(3),
                int(1),
                float(4.0),
                float(2.0)
            ],
            vec![
                Some(Value::Str("b".into())),
                int(5),
                int(5),
                float(5.0),
                float(5.0)
            ],
        ]
    );
}

#[test]
fn a_variable_from_match_and_a_property_agree() {
    let mut fx = Fx::new();
    for (k, v) in [("a", 4), ("b", 9), ("c", 1)] {
        fx.add("N", k, vec![("v", Value::Int(v))]);
    }
    assert_eq!(one(&fx, "MATCH (n:N) RETURN max(n.v)"), int(9));
    assert_eq!(one(&fx, "MATCH (n:N) WITH n.v AS x RETURN max(x)"), int(9));
    assert_eq!(one(&fx, "MATCH (n:N) WITH n.v AS x RETURN min(x)"), int(1));
    assert_eq!(
        one(&fx, "MATCH (n:N) WITH n.v AS x RETURN sum(x)"),
        float(14.0)
    );
    // A node variable still counts and collects; min / max / sum / avg of a node is a named error.
    assert_eq!(one(&fx, "MATCH (n:N) RETURN count(n)"), int(3));
    for f in ["max", "min", "sum", "avg"] {
        let e = try_rows(&fx, &format!("MATCH (n:N) RETURN {f}(n)")).unwrap_err();
        assert!(e.contains("graph element"), "{f}: {e}");
    }
}

/// Deterministic generator for the differential.
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
}

fn literal(v: &Option<Value>) -> String {
    match v {
        None => "null".into(),
        Some(Value::Int(n)) => n.to_string(),
        Some(Value::Float(f)) => format!("{f:?}"),
        Some(other) => panic!("{other:?}"),
    }
}

fn num(v: &Option<Value>) -> Option<f64> {
    match v {
        Some(Value::Int(n)) => Some(*n as f64),
        Some(Value::Float(f)) => Some(*f),
        _ => None,
    }
}

fn close(a: &Option<Value>, b: &Option<Value>) -> bool {
    match (a, b) {
        (Some(Value::Float(x)), Some(Value::Float(y))) => (x - y).abs() < 1e-9 * x.abs().max(1.0),
        _ => a == b,
    }
}

/// Reference answers computed by hand from the generated values.
fn reference(f: &str, vals: &[Option<Value>]) -> Option<Value> {
    let present: Vec<&Option<Value>> = vals.iter().filter(|v| v.is_some()).collect();
    let nums: Vec<f64> = present.iter().filter_map(|v| num(v)).collect();
    let pick = |largest: bool| {
        present
            .iter()
            .copied()
            .cloned()
            .reduce(|a, b| {
                let (x, y) = (num(&a).unwrap(), num(&b).unwrap());
                if (y > x) == largest && y != x {
                    b
                } else {
                    a
                }
            })
            .flatten()
    };
    match f {
        "count" => int(present.len() as i64),
        "sum" => (!nums.is_empty()).then(|| Value::Float(nums.iter().sum())),
        "avg" => {
            (!nums.is_empty()).then(|| Value::Float(nums.iter().sum::<f64>() / nums.len() as f64))
        }
        "max" => pick(true),
        "min" => pick(false),
        _ => unreachable!(),
    }
}

#[test]
fn randomized_differential_against_a_hand_reference() {
    let fx = Fx::new();
    let mut rng = Lcg(0x5eed_0157);
    for case in 0..300 {
        let len = (rng.next() % 8) as usize;
        let vals: Vec<Option<Value>> = (0..len)
            .map(|_| match rng.next() % 5 {
                0 => None,
                1 | 2 => int((rng.next() % 41) as i64 - 20),
                _ => float(((rng.next() % 401) as f64 - 200.0) / 8.0),
            })
            .collect();
        let list = vals.iter().map(literal).collect::<Vec<_>>().join(", ");
        for f in ["count", "sum", "avg", "max", "min"] {
            let q = format!("UNWIND [{list}] AS x RETURN {f}(x)");
            let got = one(&fx, &q);
            let want = reference(f, &vals);
            assert!(
                close(&got, &want),
                "case {case}: `{q}` -> {got:?}, want {want:?}"
            );
            // The same values through WITH, and through a property of a map.
            let q2 = format!("UNWIND [{list}] AS x WITH x AS y RETURN {f}(y)");
            assert!(close(&one(&fx, &q2), &want), "case {case}: `{q2}`");
            let maps = vals
                .iter()
                .map(|v| format!("{{v: {}}}", literal(v)))
                .collect::<Vec<_>>()
                .join(", ");
            let q3 = format!("UNWIND [{maps}] AS r RETURN {f}(r.v)");
            assert!(close(&one(&fx, &q3), &want), "case {case}: `{q3}`");
        }
        // Grouped: two keys, same values.
        let keyed: Vec<(i64, Option<Value>)> = vals
            .iter()
            .map(|v| ((rng.next() % 2) as i64, v.clone()))
            .collect();
        let maps = keyed
            .iter()
            .map(|(k, v)| format!("{{k: {k}, v: {}}}", literal(v)))
            .collect::<Vec<_>>()
            .join(", ");
        for f in ["sum", "avg", "max", "min", "count"] {
            let q =
                format!("UNWIND [{maps}] AS r WITH r.k AS k, r.v AS x RETURN k, {f}(x) ORDER BY k");
            let got = rows(&fx, &q);
            for k in 0..2 {
                let group: Vec<Option<Value>> = keyed
                    .iter()
                    .filter(|(kk, _)| *kk == k)
                    .map(|(_, v)| v.clone())
                    .collect();
                if group.is_empty() {
                    continue;
                }
                let row = got
                    .iter()
                    .find(|r| r[0] == int(k))
                    .unwrap_or_else(|| panic!("case {case}: `{q}` has no group {k}"));
                assert!(
                    close(&row[1], &reference(f, &group)),
                    "case {case}: `{q}` group {k}: {:?}",
                    row[1]
                );
            }
        }
    }
}
