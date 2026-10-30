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
        int(6)
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
                int(4),
                float(2.0)
            ],
            vec![
                Some(Value::Str("b".into())),
                int(5),
                int(5),
                int(5),
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
    assert_eq!(one(&fx, "MATCH (n:N) WITH n.v AS x RETURN sum(x)"), int(14));
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
        Some(Value::Str(t)) => format!("'{t}'"),
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
        (Some(Value::List(x)), Some(Value::List(y))) => {
            x.len() == y.len()
                && x.iter()
                    .zip(y)
                    .all(|(p, q)| close(&Some(p.clone()), &Some(q.clone())))
        }
        _ => a == b,
    }
}

/// openCypher order across kinds, as far as the generator reaches: strings
/// sort before numbers (openCypher: string < boolean < number); strings
/// lexically, numbers by value.
fn rank(v: &Option<Value>) -> (u8, f64, String) {
    match v {
        Some(Value::Str(t)) => (0, 0.0, t.clone()),
        other => (1, num(other).unwrap(), String::new()),
    }
}

fn less(a: &Option<Value>, b: &Option<Value>) -> bool {
    let (x, y) = (rank(a), rank(b));
    (x.0, x.1, &x.2).partial_cmp(&(y.0, y.1, &y.2)) == Some(std::cmp::Ordering::Less)
}

fn distinct(vals: &[Option<Value>]) -> Vec<Option<Value>> {
    let mut out: Vec<Option<Value>> = Vec::new();
    for v in vals {
        // 1 and 1.0 are the same value to DISTINCT.
        if v.is_some() && !out.iter().any(|o| rank(o) == rank(v)) {
            out.push(v.clone());
        }
    }
    out
}

/// Reference answers computed by hand from the generated values. `vals` holds
/// numbers (and strings for min / max / count / collect only).
fn reference(f: &str, vals: &[Option<Value>]) -> Option<Value> {
    let present: Vec<Option<Value>> = vals.iter().filter(|v| v.is_some()).cloned().collect();
    let nums: Vec<f64> = present.iter().filter_map(num).collect();
    let all_int = present
        .iter()
        .filter(|v| num(v).is_some())
        .all(|v| matches!(v, Some(Value::Int(_))));
    let pick = |largest: bool| {
        let mut best: Option<Option<Value>> = None;
        for v in &present {
            best = Some(match best {
                None => v.clone(),
                Some(b) => {
                    if (largest && less(&b, v)) || (!largest && less(v, &b)) {
                        v.clone()
                    } else {
                        b
                    }
                }
            });
        }
        best.flatten()
    };
    let mean = nums.iter().sum::<f64>() / nums.len().max(1) as f64;
    let ss: f64 = nums.iter().map(|x| (x - mean).powi(2)).sum();
    let sorted = {
        let mut v = nums.clone();
        v.sort_by(|a, b| a.total_cmp(b));
        v
    };
    match f {
        "count" => int(present.len() as i64),
        "collect" => Some(Value::List(
            present.iter().map(|v| v.clone().unwrap()).collect(),
        )),
        "sum" if nums.is_empty() => None,
        "sum" if all_int => int(present
            .iter()
            .filter_map(|v| match v {
                Some(Value::Int(n)) => Some(*n),
                _ => None,
            })
            .sum()),
        "sum" => float(nums.iter().sum()),
        "avg" => (!nums.is_empty()).then_some(Value::Float(mean)),
        "max" => pick(true),
        "min" => pick(false),
        "stDev" => float(if nums.len() < 2 {
            0.0
        } else {
            (ss / (nums.len() - 1) as f64).sqrt()
        }),
        "stDevP" => float(if nums.is_empty() {
            0.0
        } else {
            (ss / nums.len() as f64).sqrt()
        }),
        "pcont" | "pdisc" => {
            if sorted.is_empty() {
                return None;
            }
            let p = 0.3;
            let last = sorted.len() - 1;
            if f == "pcont" {
                let rank = p * last as f64;
                let (lo, hi) = (rank.floor() as usize, rank.ceil() as usize);
                float(sorted[lo] + (sorted[hi] - sorted[lo]) * (rank - lo as f64))
            } else {
                let k = ((p * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
                let x = sorted[k - 1];
                // percentileDisc returns the input value itself
                present
                    .iter()
                    .find(|v| num(v) == Some(x))
                    .cloned()
                    .flatten()
            }
        }
        _ => unreachable!(),
    }
}

fn query_for(f: &str, arg: &str, dist: bool) -> String {
    let d = if dist { "DISTINCT " } else { "" };
    match f {
        "pcont" => format!("percentileCont({arg}, 0.3)"),
        "pdisc" => format!("percentileDisc({arg}, 0.3)"),
        _ => format!("{f}({d}{arg})"),
    }
}

fn gen_values(rng: &mut Lcg, strings: bool) -> Vec<Option<Value>> {
    let len = (rng.next() % 8) as usize;
    (0..len)
        .map(|_| match rng.next() % if strings { 7 } else { 5 } {
            0 => None,
            1 | 2 => int((rng.next() % 11) as i64 - 5),
            3 | 4 => float(((rng.next() % 81) as f64 - 40.0) / 8.0),
            _ => Some(Value::Str(
                ["a", "b", "c", "d"][(rng.next() % 4) as usize].into(),
            )),
        })
        .collect()
}

#[test]
fn randomized_differential_against_a_hand_reference() {
    let fx = Fx::new();
    let mut rng = Lcg(0x5eed_0157);
    for case in 0..300 {
        // Numbers and nulls: every aggregate. Then numbers, strings and nulls: the
        // ones that take any value.
        for (strings, funcs) in [
            (
                false,
                &[
                    "count", "sum", "avg", "max", "min", "collect", "stDev", "stDevP", "pcont",
                    "pdisc",
                ][..],
            ),
            (true, &["count", "max", "min", "collect"][..]),
        ] {
            let vals = gen_values(&mut rng, strings);
            let list = vals.iter().map(literal).collect::<Vec<_>>().join(", ");
            let maps = vals
                .iter()
                .map(|v| format!("{{v: {}}}", literal(v)))
                .collect::<Vec<_>>()
                .join(", ");
            for &f in funcs {
                for dist in [false, true] {
                    if dist && matches!(f, "stDev" | "stDevP" | "pcont" | "pdisc") {
                        continue;
                    }
                    let input = if dist { distinct(&vals) } else { vals.clone() };
                    let mut want = reference(f, &input);
                    // collect(DISTINCT x) keeps first occurrences, as distinct() does.
                    if f == "collect" && dist {
                        want = reference(f, &input);
                    }
                    for q in [
                        format!("UNWIND [{list}] AS x RETURN {}", query_for(f, "x", dist)),
                        format!(
                            "UNWIND [{list}] AS x WITH x AS y RETURN {}",
                            query_for(f, "y", dist)
                        ),
                        format!("UNWIND [{maps}] AS r RETURN {}", query_for(f, "r.v", dist)),
                    ] {
                        let got = one(&fx, &q);
                        assert!(
                            close(&got, &want),
                            "case {case}: `{q}` -> {got:?}, want {want:?}"
                        );
                    }
                }
            }
            // Grouped by a two-valued key.
            let keyed: Vec<(i64, Option<Value>)> = vals
                .iter()
                .map(|v| ((rng.next() % 2) as i64, v.clone()))
                .collect();
            let maps = keyed
                .iter()
                .map(|(k, v)| format!("{{k: {k}, v: {}}}", literal(v)))
                .collect::<Vec<_>>()
                .join(", ");
            for &f in funcs {
                let q = format!(
                    "UNWIND [{maps}] AS r WITH r.k AS k, r.v AS x RETURN k, {} ORDER BY k",
                    query_for(f, "x", false)
                );
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
                    let want = reference(f, &group);
                    assert!(
                        close(&row[1], &want),
                        "case {case}: `{q}` group {k}: {:?}, want {want:?}",
                        row[1]
                    );
                }
            }
        }
    }
}

#[test]
fn sum_of_integers_is_an_integer() {
    let fx = Fx::new();
    assert_eq!(one(&fx, "UNWIND [1, 2, 3] AS x RETURN sum(x)"), int(6));
    assert_eq!(one(&fx, "UNWIND [1, 2.5] AS x RETURN sum(x)"), float(3.5));
    assert_eq!(one(&fx, "UNWIND [2.5, 1] AS x RETURN sum(x)"), float(3.5));
    assert_eq!(one(&fx, "UNWIND [] AS x RETURN sum(x)"), None);
    // Exact above 2^53, where a float sum would round.
    assert_eq!(
        one(&fx, "UNWIND [9007199254740993, 1] AS x RETURN sum(x)"),
        int(9007199254740994)
    );
    let e = try_rows(&fx, "UNWIND [9223372036854775807, 1] AS x RETURN sum(x)").unwrap_err();
    assert!(e.contains("overflow"), "{e}");
}

#[test]
fn a_null_scalar_variable_is_not_counted_or_collected() {
    let fx = Fx::new();
    assert_eq!(one(&fx, "WITH null AS x RETURN count(x)"), int(0));
    assert_eq!(
        one(&fx, "WITH null AS x RETURN collect(x)"),
        Some(Value::List(vec![]))
    );
    assert_eq!(one(&fx, "UNWIND [1, null, 2] AS x RETURN count(x)"), int(2));
}
