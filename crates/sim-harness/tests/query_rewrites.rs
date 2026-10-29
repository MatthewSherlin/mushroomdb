//! H-2 (0.10.x hardening): rewrites of a query that must not change its answer.
//!
//! A random graph is written to a store that snapshots part-way, so reads go
//! through a snapshot base and an overlay. Then, for random predicates and the
//! fixed shapes below, two queries that must agree are both run:
//!
//! - **Ternary partition.** `Q` returns the multiset union of `Q WHERE p`,
//!   `Q WHERE NOT p` and `Q WHERE (p) IS NULL`, for any predicate `p`, over
//!   properties of mixed types and missing properties (rows 133, 160).
//! - **Aggregates against the rows.** `count`, `sum`, `min`, `max` and
//!   `collect` over a property, and over the same value bound to a variable,
//!   equal what the client computes from the plain rows; an integer `sum` is
//!   an integer (rows 157, 158).
//! - **Grouping and DISTINCT** equal a client-side grouping (row 131).
//! - **SKIP and LIMIT** under a total ORDER BY are a slice of the full order;
//!   without ORDER BY the row count is `min(limit, total)` (row 139).
//! - **A literal and the same value as a parameter** give the same rows.
//!
//! Any disagreement is an engine bug. Do not weaken an assertion or drop a
//! shape to get a run green.
//!
//! Cases: 32 per run by default; set `H2_CASES` to raise it (the nightly run).

use mushroomdb::{GraphDb, RealFs, ResultSet, Value};
use proptest::prelude::*;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// One generated node. `i` is always an integer when present; `m` is a value
/// of mixed type; `b` is a boolean; `g` is a small grouping string.
#[derive(Debug, Clone)]
struct NodeSpec {
    label: u8,
    i: Option<i8>,
    m: Option<Mixed>,
    b: Option<bool>,
    g: u8,
}

#[derive(Debug, Clone)]
enum Mixed {
    Int(i8),
    Float(i8),
    Str(u8),
    Bool(bool),
}

fn mixed_value(m: &Mixed) -> Value {
    match m {
        Mixed::Int(v) => Value::Int(i64::from(*v % 5)),
        Mixed::Float(v) => Value::Float(f64::from(*v % 5) + 0.5),
        Mixed::Str(s) => Value::Str(["a", "b", "c"][(*s as usize) % 3].into()),
        Mixed::Bool(b) => Value::Bool(*b),
    }
}

fn node_strategy() -> impl Strategy<Value = NodeSpec> {
    let mixed = prop_oneof![
        any::<i8>().prop_map(Mixed::Int),
        any::<i8>().prop_map(Mixed::Float),
        any::<u8>().prop_map(Mixed::Str),
        any::<bool>().prop_map(Mixed::Bool),
    ];
    (
        any::<u8>(),
        proptest::option::weighted(0.8, any::<i8>()),
        proptest::option::weighted(0.8, mixed),
        proptest::option::weighted(0.7, any::<bool>()),
        any::<u8>(),
    )
        .prop_map(|(label, i, m, b, g)| NodeSpec { label, i, m, b, g })
}

/// A predicate over `n`, rendered as Cypher text.
#[derive(Debug, Clone)]
enum Pred {
    Cmp(&'static str, &'static str, String),
    IsNull(&'static str),
    In(&'static str, String),
    Not(Box<Pred>),
    And(Box<Pred>, Box<Pred>),
    Or(Box<Pred>, Box<Pred>),
    Xor(Box<Pred>, Box<Pred>),
}

fn render_pred(p: &Pred) -> String {
    match p {
        Pred::Cmp(f, op, lit) => format!("n.{f} {op} {lit}"),
        Pred::IsNull(f) => format!("n.{f} IS NULL"),
        Pred::In(f, list) => format!("n.{f} IN {list}"),
        Pred::Not(a) => format!("NOT ({})", render_pred(a)),
        Pred::And(a, b) => format!("({}) AND ({})", render_pred(a), render_pred(b)),
        Pred::Or(a, b) => format!("({}) OR ({})", render_pred(a), render_pred(b)),
        Pred::Xor(a, b) => format!("({}) XOR ({})", render_pred(a), render_pred(b)),
    }
}

fn literal_strategy() -> impl Strategy<Value = String> {
    prop_oneof![
        (-2i64..6).prop_map(|v| v.to_string()),
        (0i64..5).prop_map(|v| format!("{v}.5")),
        prop_oneof![Just("'a'"), Just("'b'"), Just("'z'")].prop_map(String::from),
        prop_oneof![Just("true"), Just("false")].prop_map(String::from),
        Just("null".to_string()),
    ]
}

fn pred_strategy() -> impl Strategy<Value = Pred> {
    let field = prop_oneof![Just("i"), Just("m"), Just("b"), Just("g")];
    let op = prop_oneof![
        Just("="),
        Just("<>"),
        Just("<"),
        Just("<="),
        Just(">"),
        Just(">=")
    ];
    let leaf = prop_oneof![
        4 => (field.clone(), op, literal_strategy()).prop_map(|(f, o, l)| Pred::Cmp(f, o, l)),
        1 => field.clone().prop_map(Pred::IsNull),
        1 => (field, proptest::collection::vec(literal_strategy(), 0..3))
            .prop_map(|(f, items)| Pred::In(f, format!("[{}]", items.join(", ")))),
    ];
    leaf.prop_recursive(3, 12, 2, |inner| {
        prop_oneof![
            inner.clone().prop_map(|a| Pred::Not(Box::new(a))),
            (inner.clone(), inner.clone()).prop_map(|(a, b)| Pred::And(Box::new(a), Box::new(b))),
            (inner.clone(), inner.clone()).prop_map(|(a, b)| Pred::Or(Box::new(a), Box::new(b))),
            (inner.clone(), inner).prop_map(|(a, b)| Pred::Xor(Box::new(a), Box::new(b))),
        ]
    })
}

/// `sum` over no values. openCypher returns 0; the engine returns null and pins
/// that in `exec.rs` (`sum_numeric_prop_ignores_null_and_non_numeric`). Kept
/// as found until the owner rules on it; flipping it is this one function.
fn sum_of(values: &[i64]) -> Option<Value> {
    if values.is_empty() {
        None
    } else {
        Some(Value::Int(values.iter().sum()))
    }
}

static SEQ: AtomicU64 = AtomicU64::new(0);

fn fresh_dir() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!(
        "mdb-h2-{}-{nanos}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Write the nodes, snapshotting after `snapshot_at` of them, and change a few
/// properties after the snapshot so some values live only in the overlay.
fn build(nodes: &[NodeSpec], snapshot_at: usize) -> (PathBuf, GraphDb<RealFs>) {
    let dir = fresh_dir();
    let mut db = GraphDb::open(&dir).unwrap();
    for (idx, n) in nodes.iter().enumerate() {
        if idx == snapshot_at {
            db.snapshot().unwrap();
        }
        let mut props: Vec<(String, Value)> = vec![(
            "g".into(),
            Value::Str(["x", "y", "z"][(n.g as usize) % 3].into()),
        )];
        if let Some(i) = n.i {
            props.push(("i".into(), Value::Int(i64::from(i % 7))));
        }
        if let Some(m) = &n.m {
            props.push(("m".into(), mixed_value(m)));
        }
        if let Some(b) = n.b {
            props.push(("b".into(), Value::Bool(b)));
        }
        db.insert_node(&format!("L{}", n.label % 2), &format!("n{idx:03}"), props)
            .unwrap();
    }
    for (idx, n) in nodes.iter().enumerate().filter(|(i, _)| i % 4 == 1) {
        let _ = db.set_prop(&format!("n{idx:03}"), "i", Value::Int(i64::from(n.g % 5)));
    }
    (dir, db)
}

fn run(db: &GraphDb<RealFs>, q: &str) -> ResultSet {
    run_with(db, q, &BTreeMap::new())
}

fn run_with(db: &GraphDb<RealFs>, q: &str, params: &BTreeMap<String, Value>) -> ResultSet {
    match db.query(q, params) {
        Ok(rs) => rs,
        Err(e) => panic!("query failed: {q}\n{e}"),
    }
}

/// One column of a result as a sorted multiset of debug strings.
fn bag(rs: &ResultSet, col: &str) -> Vec<String> {
    let mut v: Vec<String> = (0..rs.len())
        .map(|i| format!("{:?}", rs.get(i, col)))
        .collect();
    v.sort();
    v
}

fn ordered(rs: &ResultSet, col: &str) -> Vec<String> {
    (0..rs.len())
        .map(|i| format!("{:?}", rs.get(i, col)))
        .collect()
}

fn single(rs: &ResultSet, col: &str) -> Option<Value> {
    assert_eq!(rs.len(), 1, "expected one row for {col}");
    rs.get(0, col).cloned()
}

fn check_partition(db: &GraphDb<RealFs>, p: &Pred) -> Result<(), TestCaseError> {
    let p = render_pred(p);
    let all = bag(&run(db, "MATCH (n) RETURN key(n) AS k"), "k");
    let mut parts = Vec::new();
    for part in [
        format!("MATCH (n) WHERE {p} RETURN key(n) AS k"),
        format!("MATCH (n) WHERE NOT ({p}) RETURN key(n) AS k"),
        format!("MATCH (n) WHERE ({p}) IS NULL RETURN key(n) AS k"),
    ] {
        parts.extend(bag(&run(db, &part), "k"));
    }
    parts.sort();
    prop_assert_eq!(&all, &parts, "ternary partition broken for WHERE {}", p);

    // The same partition, counted by the engine instead of by rows, and once
    // more through a LIMIT larger than the graph (the fused scan path).
    let count = |q: String| match single(&run(db, &q), "c") {
        Some(Value::Int(c)) => c,
        other => panic!("count is not an Int: {other:?} for {q}"),
    };
    let total = count("MATCH (n) RETURN count(*) AS c".into());
    let split = count(format!("MATCH (n) WHERE {p} RETURN count(*) AS c"))
        + count(format!("MATCH (n) WHERE NOT ({p}) RETURN count(*) AS c"))
        + count(format!(
            "MATCH (n) WHERE ({p}) IS NULL RETURN count(*) AS c"
        ));
    prop_assert_eq!(total, split, "counted partition broken for WHERE {}", p);
    let limited = run(
        db,
        &format!("MATCH (n) WHERE {p} RETURN key(n) AS k LIMIT 1000"),
    );
    let unlimited = run(db, &format!("MATCH (n) WHERE {p} RETURN key(n) AS k"));
    prop_assert_eq!(
        bag(&limited, "k"),
        bag(&unlimited, "k"),
        "LIMIT changed WHERE {}",
        p
    );
    Ok(())
}

fn check_aggregates(db: &GraphDb<RealFs>) -> Result<(), TestCaseError> {
    let rows = run(db, "MATCH (n) RETURN n.i AS i, n.b AS b");
    let ints: Vec<i64> = (0..rows.len())
        .filter_map(|r| match rows.get(r, "i") {
            Some(Value::Int(v)) => Some(*v),
            None => None,
            other => panic!("n.i is not an Int: {other:?}"),
        })
        .collect();
    let bools: Vec<bool> = (0..rows.len())
        .filter_map(|r| match rows.get(r, "b") {
            Some(Value::Bool(v)) => Some(*v),
            _ => None,
        })
        .collect();

    for q in [
        "MATCH (n) RETURN count(n.i) AS c, sum(n.i) AS s, min(n.i) AS lo, max(n.i) AS hi",
        "MATCH (n) WITH n.i AS x RETURN count(x) AS c, sum(x) AS s, min(x) AS lo, max(x) AS hi",
        "MATCH (n) UNWIND [n.i] AS x RETURN count(x) AS c, sum(x) AS s, min(x) AS lo, max(x) AS hi",
    ] {
        let rs = run(db, q);
        prop_assert_eq!(
            single(&rs, "c"),
            Some(Value::Int(ints.len() as i64)),
            "count: {}",
            q
        );
        prop_assert_eq!(single(&rs, "s"), sum_of(&ints), "sum: {}", q);
        prop_assert_eq!(
            single(&rs, "lo"),
            ints.iter().min().map(|v| Value::Int(*v)),
            "min: {}",
            q
        );
        prop_assert_eq!(
            single(&rs, "hi"),
            ints.iter().max().map(|v| Value::Int(*v)),
            "max: {}",
            q
        );
    }

    let rs = run(
        db,
        "MATCH (n) RETURN min(n.b) AS lo, max(n.b) AS hi, count(n.b) AS c",
    );
    prop_assert_eq!(
        single(&rs, "lo"),
        bools.iter().min().map(|v| Value::Bool(*v))
    );
    prop_assert_eq!(
        single(&rs, "hi"),
        bools.iter().max().map(|v| Value::Bool(*v))
    );
    prop_assert_eq!(single(&rs, "c"), Some(Value::Int(bools.len() as i64)));

    let rs = run(db, "MATCH (n) RETURN collect(n.i) AS xs");
    let mut collected: Vec<i64> = match single(&rs, "xs") {
        Some(Value::List(items)) => items
            .iter()
            .map(|v| match v {
                Value::Int(i) => *i,
                other => panic!("collect(n.i) holds {other:?}"),
            })
            .collect(),
        other => panic!("collect is not a list: {other:?}"),
    };
    let mut expected = ints.clone();
    collected.sort();
    expected.sort();
    prop_assert_eq!(
        collected,
        expected,
        "collect(n.i) skips nulls and keeps the rest"
    );
    Ok(())
}

fn check_grouping(db: &GraphDb<RealFs>) -> Result<(), TestCaseError> {
    let rows = run(db, "MATCH (n) RETURN n.g AS g, n.i AS i");
    let mut members: BTreeMap<String, (i64, Vec<i64>)> = BTreeMap::new();
    for r in 0..rows.len() {
        let e = members
            .entry(format!("{:?}", rows.get(r, "g")))
            .or_default();
        e.0 += 1;
        if let Some(Value::Int(v)) = rows.get(r, "i") {
            e.1.push(*v);
        }
    }
    let groups: BTreeMap<String, (i64, Option<Value>)> = members
        .into_iter()
        .map(|(g, (c, ints))| (g, (c, sum_of(&ints))))
        .collect();
    let rs = run(
        db,
        "MATCH (n) RETURN n.g AS g, count(*) AS c, sum(n.i) AS s",
    );
    let mut got = BTreeMap::new();
    for r in 0..rs.len() {
        let c = match rs.get(r, "c") {
            Some(Value::Int(c)) => *c,
            other => panic!("count is {other:?}"),
        };
        got.insert(
            format!("{:?}", rs.get(r, "g")),
            (c, rs.get(r, "s").cloned()),
        );
    }
    prop_assert_eq!(got, groups, "grouped count and sum");

    let distinct: BTreeSet<String> = bag(&run(db, "MATCH (n) RETURN n.i AS i"), "i")
        .into_iter()
        .collect();
    let got = bag(&run(db, "MATCH (n) RETURN DISTINCT n.i AS i"), "i");
    prop_assert_eq!(
        got,
        distinct.into_iter().collect::<Vec<_>>(),
        "DISTINCT n.i"
    );
    Ok(())
}

fn check_paging(db: &GraphDb<RealFs>, skip: usize, limit: usize) -> Result<(), TestCaseError> {
    let full = ordered(&run(db, "MATCH (n) RETURN key(n) AS k ORDER BY k"), "k");
    let page = ordered(
        &run(
            db,
            &format!("MATCH (n) RETURN key(n) AS k ORDER BY k SKIP {skip} LIMIT {limit}"),
        ),
        "k",
    );
    let want: Vec<String> = full.iter().skip(skip).take(limit).cloned().collect();
    prop_assert_eq!(page, want, "SKIP {} LIMIT {} under ORDER BY", skip, limit);

    let rs = run(
        db,
        &format!("MATCH (n) WHERE n.i >= 0 RETURN key(n) AS k LIMIT {limit}"),
    );
    let matching = run(db, "MATCH (n) WHERE n.i >= 0 RETURN count(*) AS c");
    let total = match single(&matching, "c") {
        Some(Value::Int(c)) => c as usize,
        other => panic!("count is {other:?}"),
    };
    prop_assert_eq!(
        rs.len(),
        total.min(limit),
        "unordered LIMIT {} row count",
        limit
    );
    let all: BTreeSet<String> = bag(&run(db, "MATCH (n) WHERE n.i >= 0 RETURN key(n) AS k"), "k")
        .into_iter()
        .collect();
    for k in bag(&rs, "k") {
        prop_assert!(
            all.contains(&k),
            "LIMIT returned {} which the filter does not",
            k
        );
    }
    Ok(())
}

fn check_params(db: &GraphDb<RealFs>, v: i64) -> Result<(), TestCaseError> {
    let mut params = BTreeMap::new();
    params.insert("v".to_string(), Value::Int(v));
    for op in ["=", "<>", "<", ">="] {
        let lit = bag(
            &run(
                db,
                &format!("MATCH (n) WHERE n.i {op} {v} RETURN key(n) AS k"),
            ),
            "k",
        );
        let par = bag(
            &run_with(
                db,
                &format!("MATCH (n) WHERE n.i {op} $v RETURN key(n) AS k"),
                &params,
            ),
            "k",
        );
        prop_assert_eq!(lit, par, "literal vs parameter for n.i {} {}", op, v);
    }
    Ok(())
}

fn cases() -> u32 {
    std::env::var("H2_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(32)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(), ..ProptestConfig::default() })]
    #[test]
    fn rewrites_do_not_change_the_answer(
        nodes in proptest::collection::vec(node_strategy(), 1..40),
        snapshot_slot in any::<u8>(),
        preds in proptest::collection::vec(pred_strategy(), 6),
        skip in 0usize..6,
        limit in 0usize..8,
        v in -1i64..6,
    ) {
        let (dir, db) = build(&nodes, (snapshot_slot as usize) % (nodes.len() + 1));
        let outcome = (|| {
            for p in &preds {
                check_partition(&db, p)?;
            }
            check_aggregates(&db)?;
            check_grouping(&db)?;
            check_paging(&db, skip, limit)?;
            check_params(&db, v)
        })();
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
        outcome?;
    }
}
