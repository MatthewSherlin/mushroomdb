//! Defect-ledger row 157: `sum`, `avg`, `min` and `max` over a bare variable
//! use its value.
//!
//! `MATCH (o) UNWIND [1,2] AS x RETURN max(x)` returned null: the four had arms
//! for a property argument and for nothing else, so a variable bound by
//! `UNWIND` or `WITH` fell to the catch-all arm and contributed no row.
//! `count` and `collect` already read a variable.
//!
//! 0.9 starts a read with `MATCH` and its `UNWIND` list holds literals other
//! than `null` and maps, so a null reaches an aggregate here as a missing
//! property (`MATCH (n) WITH n.v AS x`).
//!
//! What 0.9.4 does not change: `min` / `max` still read numbers (and, from row
//! 160, booleans) only, and an aggregate of a node variable still contributes
//! nothing. 0.10 changes both, so neither is pinned here.

use mushroomdb::{GraphDb, Value};
use std::collections::BTreeMap;

type Db = GraphDb<core_storage::fs::RealFs>;
type Rows = Vec<Vec<Option<Value>>>;

fn tmp(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let d = std::env::temp_dir().join(format!(
        "graphdb-agg-vars-{name}-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// The anchor every `UNWIND` below runs off: one `One` node.
const ONE: &str = "MATCH (o:One)";

fn open(name: &str) -> Db {
    let mut db = GraphDb::open(&tmp(name)).unwrap();
    db.insert_node("One", "one", vec![]).unwrap();
    db
}

/// Insert one `label` node per value; `None` leaves `v` unset. `k`, when
/// given, is written as the `k` property.
fn nodes(db: &mut Db, label: &str, vals: &[(Option<i64>, Option<Value>)]) {
    for (i, (k, v)) in vals.iter().enumerate() {
        let mut props = Vec::new();
        if let Some(k) = k {
            props.push(("k".to_string(), Value::Int(*k)));
        }
        if let Some(v) = v {
            props.push(("v".to_string(), v.clone()));
        }
        db.insert_node(label, &format!("{label}-{i}"), props)
            .unwrap();
    }
}

fn try_rows(db: &Db, q: &str) -> Result<Rows, String> {
    let rs = db.query(q, &BTreeMap::new()).map_err(|e| e.to_string())?;
    Ok((0..rs.len()).map(|i| rs.row(i).to_vec()).collect())
}

fn rows(db: &Db, q: &str) -> Rows {
    try_rows(db, q).unwrap_or_else(|e| panic!("`{q}` failed: {e}"))
}

fn one(db: &Db, q: &str) -> Option<Value> {
    let r = rows(db, q);
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
    let db = open("reported");
    assert_eq!(
        one(&db, &format!("{ONE} UNWIND [1,2] AS x RETURN max(x)")),
        int(2)
    );
}

#[test]
fn every_aggregate_over_an_unwound_variable() {
    let db = open("every");
    let l = format!("{ONE} UNWIND [3, 1, 2.5, 1] AS x");
    assert_eq!(one(&db, &format!("{l} RETURN max(x)")), int(3));
    assert_eq!(one(&db, &format!("{l} RETURN min(x)")), int(1));
    assert_eq!(one(&db, &format!("{l} RETURN avg(x)")), float(1.875));
    assert_eq!(one(&db, &format!("{l} RETURN count(x)")), int(4));
    assert_eq!(one(&db, &format!("{l} RETURN count(DISTINCT x)")), int(3));
    assert_eq!(one(&db, &format!("{l} RETURN max(DISTINCT x)")), int(3));
    assert_eq!(one(&db, &format!("{l} RETURN min(DISTINCT x)")), int(1));
    assert_eq!(
        one(&db, &format!("{l} RETURN avg(DISTINCT x)")),
        float(6.5 / 3.0)
    );
    // A float input makes the sum a float, whichever type it is for integers.
    assert_eq!(one(&db, &format!("{l} RETURN sum(x)")), float(7.5));
    assert_eq!(one(&db, &format!("{l} RETURN sum(DISTINCT x)")), float(6.5));
}

#[test]
fn a_missing_value_contributes_nothing() {
    let mut db = open("nulls");
    nodes(
        &mut db,
        "N",
        &[
            (None, int(3)),
            (None, None),
            (None, float(2.5)),
            (None, int(1)),
        ],
    );
    let l = "MATCH (n:N) WITH n.v AS x";
    assert_eq!(one(&db, &format!("{l} RETURN max(x)")), int(3));
    assert_eq!(one(&db, &format!("{l} RETURN min(x)")), int(1));
    assert_eq!(one(&db, &format!("{l} RETURN sum(x)")), float(6.5));
    assert_eq!(one(&db, &format!("{l} RETURN avg(x)")), float(6.5 / 3.0));
    assert_eq!(one(&db, &format!("{l} RETURN count(x)")), int(3));
}

#[test]
fn empty_and_all_null_inputs() {
    let mut db = open("empty");
    nodes(&mut db, "Unset", &[(None, None), (None, None)]);
    for l in [
        format!("{ONE} UNWIND [] AS x"),
        "MATCH (n:Unset) WITH n.v AS x".to_string(),
    ] {
        for f in ["max", "min", "sum", "avg"] {
            assert_eq!(one(&db, &format!("{l} RETURN {f}(x)")), None, "{l} {f}");
        }
        assert_eq!(one(&db, &format!("{l} RETURN count(x)")), int(0));
    }
}

#[test]
fn a_variable_carried_through_with() {
    let db = open("with");
    assert_eq!(
        one(
            &db,
            &format!("{ONE} UNWIND [1,2,3] AS x WITH x RETURN max(x)")
        ),
        int(3)
    );
    assert_eq!(
        one(
            &db,
            &format!("{ONE} UNWIND [1,2,3] AS x WITH x AS y RETURN avg(y)")
        ),
        float(2.0)
    );
    assert_eq!(
        one(
            &db,
            &format!("{ONE} UNWIND [1,2,3] AS x WITH x AS y WHERE y > 1 RETURN min(y)")
        ),
        int(2)
    );
    // An aggregate in WITH, aggregated again after it.
    assert_eq!(
        rows(
            &db,
            &format!(
                "{ONE} UNWIND [1,2,3] AS x WITH max(x) AS m, min(x) AS n RETURN max(m), min(n)"
            )
        ),
        vec![vec![int(3), int(1)]]
    );
}

#[test]
fn grouping_keys() {
    let mut db = open("group");
    nodes(
        &mut db,
        "G",
        &[
            (Some(0), int(1)),
            (Some(1), int(5)),
            (Some(0), int(3)),
            (Some(1), None),
        ],
    );
    let r = rows(
        &db,
        "MATCH (n:G) WITH n.k AS k, n.v AS x RETURN k, max(x), min(x), avg(x) ORDER BY k",
    );
    assert_eq!(
        r,
        vec![
            vec![int(0), int(3), int(1), float(2.0)],
            vec![int(1), int(5), int(5), float(5.0)],
        ]
    );
}

#[test]
fn a_variable_from_match_and_a_property_agree() {
    let mut db = open("match");
    nodes(
        &mut db,
        "N",
        &[(None, int(4)), (None, int(9)), (None, int(1))],
    );
    assert_eq!(one(&db, "MATCH (n:N) RETURN max(n.v)"), int(9));
    assert_eq!(one(&db, "MATCH (n:N) WITH n.v AS x RETURN max(x)"), int(9));
    assert_eq!(one(&db, "MATCH (n:N) WITH n.v AS x RETURN min(x)"), int(1));
    for f in ["sum", "avg"] {
        assert_eq!(
            one(&db, &format!("MATCH (n:N) WITH n.v AS x RETURN {f}(x)")),
            one(&db, &format!("MATCH (n:N) RETURN {f}(n.v)")),
            "{f}"
        );
    }
    assert_eq!(one(&db, "MATCH (n:N) RETURN count(n)"), int(3));
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

fn literal(v: &Value) -> String {
    match v {
        Value::Int(n) => n.to_string(),
        Value::Float(f) => format!("{f:?}"),
        other => panic!("{other:?}"),
    }
}

fn num(v: &Option<Value>) -> Option<f64> {
    match v {
        Some(Value::Int(n)) => Some(*n as f64),
        Some(Value::Float(f)) => Some(*f),
        _ => None,
    }
}

/// Numbers compare by value; a float answer within rounding of the reference.
fn close(a: &Option<Value>, b: &Option<Value>) -> bool {
    match (a, b) {
        (Some(Value::Float(x)), Some(Value::Float(y))) => (x - y).abs() < 1e-9 * x.abs().max(1.0),
        _ => a == b,
    }
}

fn distinct(vals: &[Option<Value>]) -> Vec<Option<Value>> {
    let mut out: Vec<Option<Value>> = Vec::new();
    for v in vals {
        // 1 and 1.0 are the same value to DISTINCT.
        if v.is_some() && !out.iter().any(|o| num(o) == num(v)) {
            out.push(v.clone());
        }
    }
    out
}

/// Reference answers computed by hand from the generated numbers and nulls.
/// `sum` is compared by value: its type is row 158's concern.
fn reference(f: &str, vals: &[Option<Value>]) -> Option<Value> {
    let present: Vec<Option<Value>> = vals.iter().filter(|v| v.is_some()).cloned().collect();
    let nums: Vec<f64> = present.iter().filter_map(num).collect();
    let pick = |largest: bool| {
        let mut best: Option<Option<Value>> = None;
        for v in &present {
            best = Some(match best {
                None => v.clone(),
                Some(b) => {
                    let (x, y) = (num(&b).unwrap(), num(v).unwrap());
                    if (largest && x < y) || (!largest && y < x) {
                        v.clone()
                    } else {
                        b
                    }
                }
            });
        }
        best.flatten()
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

/// `sum` compared by value, whatever its numeric type.
fn as_float(v: Option<Value>) -> Option<Value> {
    num(&v).map(Value::Float).or(v)
}

fn gen_values(rng: &mut Lcg) -> Vec<Option<Value>> {
    let len = (rng.next() % 8) as usize;
    (0..len)
        .map(|_| match rng.next() % 5 {
            0 => None,
            1 | 2 => int((rng.next() % 11) as i64 - 5),
            _ => float(((rng.next() % 81) as f64 - 40.0) / 8.0),
        })
        .collect()
}

const FUNCS: [&str; 5] = ["count", "sum", "avg", "max", "min"];

/// Every aggregate, plain and DISTINCT, over a bare variable bound four ways
/// (`UNWIND`, `UNWIND … WITH x AS y`, `WITH n.v AS x`, and grouped by a key)
/// and over the property itself, against a reference computed by hand.
#[test]
fn randomized_differential_against_a_hand_reference() {
    let mut db = open("diff");
    let mut rng = Lcg(0x5eed_0157);
    for case in 0..150 {
        let vals = gen_values(&mut rng);
        let keyed: Vec<(Option<i64>, Option<Value>)> = vals
            .iter()
            .map(|v| (Some((rng.next() % 2) as i64), v.clone()))
            .collect();
        let label = format!("C{case}");
        nodes(&mut db, &label, &keyed);
        // An UNWIND list holds no null on 0.9; a null contributes nothing, so
        // leaving it out asks the same question.
        let list = vals
            .iter()
            .flatten()
            .map(literal)
            .collect::<Vec<_>>()
            .join(", ");
        for f in FUNCS {
            for dist in [false, true] {
                let d = if dist { "DISTINCT " } else { "" };
                let input = if dist { distinct(&vals) } else { vals.clone() };
                let want = reference(f, &input);
                let mut queries = vec![
                    format!("{ONE} UNWIND [{list}] AS x RETURN {f}({d}x)"),
                    format!("{ONE} UNWIND [{list}] AS x WITH x AS y RETURN {f}({d}y)"),
                ];
                // A label with no node matches no row and the aggregate sees nothing.
                if !vals.is_empty() {
                    queries.push(format!("MATCH (n:{label}) WITH n.v AS x RETURN {f}({d}x)"));
                    queries.push(format!("MATCH (n:{label}) RETURN {f}({d}n.v)"));
                }
                for q in queries {
                    let mut got = one(&db, &q);
                    if f == "sum" {
                        got = as_float(got);
                    }
                    assert!(
                        close(&got, &want),
                        "case {case}: `{q}` -> {got:?}, want {want:?}"
                    );
                }
            }
        }
        for f in FUNCS {
            let q =
                format!("MATCH (n:{label}) WITH n.k AS k, n.v AS x RETURN k, {f}(x) ORDER BY k");
            let got = rows(&db, &q);
            for k in 0..2 {
                let group: Vec<Option<Value>> = keyed
                    .iter()
                    .filter(|(kk, _)| *kk == Some(k))
                    .map(|(_, v)| v.clone())
                    .collect();
                if group.is_empty() {
                    continue;
                }
                let row = got
                    .iter()
                    .find(|r| r[0] == int(k))
                    .unwrap_or_else(|| panic!("case {case}: `{q}` has no group {k}"));
                let mut cell = row[1].clone();
                if f == "sum" {
                    cell = as_float(cell);
                }
                let want = reference(f, &group);
                assert!(
                    close(&cell, &want),
                    "case {case}: `{q}` group {k}: {cell:?}, want {want:?}"
                );
            }
        }
    }
}
