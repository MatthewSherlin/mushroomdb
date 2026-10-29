//! Hardening H-3 findings in aggregation (defect-ledger rows 168 and 169).
//!
//! Row 168: an aggregating `RETURN` straight after an aggregating `WITH` was
//! dropped. The plan was right (`GroupAggregate`, `GroupAggregate`), but the
//! executor sent it down the streaming group path, which runs the first
//! `GroupAggregate` and keeps only `ORDER BY` / `SKIP` / `LIMIT` after it, so
//! the query returned the `WITH`'s groups.
//!
//! Row 169: `SKIP` and `LIMIT` were ignored on an aggregate with no grouping
//! key. The planner's single-aggregate path returned before emitting them, on
//! the reasoning that the aggregate makes exactly one row; that one row is
//! still a row `SKIP` and `LIMIT` apply to.

use super::tests::{run, Fx};
use core_storage::Value;
use std::collections::BTreeMap;

type Rows = Vec<Vec<Option<Value>>>;

fn rows_with(fx: &Fx, q: &str, params: &BTreeMap<String, Value>) -> (Vec<String>, Rows) {
    let rs = run(&fx.view(), q, params).unwrap_or_else(|e| panic!("`{q}` failed: {e}"));
    let rows = (0..rs.len()).map(|i| rs.row(i).to_vec()).collect();
    (rs.columns().to_vec(), rows)
}

fn rows(fx: &Fx, q: &str) -> Rows {
    rows_with(fx, q, &BTreeMap::new()).1
}

fn int(n: i64) -> Option<Value> {
    Some(Value::Int(n))
}

fn s(x: &str) -> Option<Value> {
    Some(Value::Str(x.into()))
}

/// Three nodes with `s` = `'a'`, `'a'`, `'b'`, the ledger's repro graph, and
/// one `T` edge from the first to the third.
fn abb() -> Fx {
    let mut fx = Fx::new();
    let a1 = fx.add("A", "a1", vec![("s", Value::Str("a".into()))]);
    fx.add("A", "a2", vec![("s", Value::Str("a".into()))]);
    let b = fx.add("B", "b", vec![("s", Value::Str("b".into()))]);
    fx.edge("T", a1, b, vec![]);
    fx
}

const GROUPS: &str = "MATCH (a) WITH a.s AS w0, count(*) AS w1";

#[test]
fn row_168_an_aggregating_return_after_an_aggregating_with_aggregates_the_groups() {
    let fx = abb();
    let (cols, r) = rows_with(
        &fx,
        &format!("{GROUPS} RETURN count(*) AS c0"),
        &BTreeMap::new(),
    );
    assert_eq!(cols, vec!["c0".to_string()]);
    assert_eq!(r, vec![vec![int(2)]]);
    let (cols, r) = rows_with(
        &fx,
        &format!("{GROUPS} RETURN sum(w1) AS c0"),
        &BTreeMap::new(),
    );
    assert_eq!(cols, vec!["c0".to_string()]);
    assert_eq!(r, vec![vec![int(3)]]);
    assert_eq!(
        rows(&fx, "MATCH (a) WITH count(*) AS w1 RETURN count(*) AS c0"),
        vec![vec![int(1)]]
    );
    assert_eq!(
        rows(
            &fx,
            &format!("{GROUPS} RETURN count(*) AS c0, max(w1) AS c1")
        ),
        vec![vec![int(2), int(2)]]
    );
}

#[test]
fn row_168_a_grouped_or_ordered_aggregating_return_after_an_aggregating_with() {
    let fx = abb();
    assert_eq!(
        rows(
            &fx,
            &format!("{GROUPS} RETURN w1, count(*) AS c ORDER BY w1")
        ),
        vec![vec![int(1), int(1)], vec![int(2), int(1)]]
    );
    // v0.9.3 refused this one: the RETURN's alias was not a column.
    assert_eq!(
        rows(&fx, &format!("{GROUPS} RETURN count(*) AS c0 ORDER BY c0")),
        vec![vec![int(2)]]
    );
}

#[test]
fn row_168_two_aggregating_with_stages_then_an_aggregating_return() {
    let fx = abb();
    let twice = format!("{GROUPS} WITH w1, count(*) AS n");
    assert_eq!(
        rows(&fx, &format!("{twice} RETURN count(*) AS c")),
        vec![vec![int(2)]]
    );
    assert_eq!(
        rows(&fx, &format!("{twice} RETURN sum(n) AS c")),
        vec![vec![int(2)]]
    );
    assert_eq!(
        rows(&fx, &format!("{twice} RETURN w1, n ORDER BY w1")),
        vec![vec![int(1), int(1)], vec![int(2), int(1)]]
    );
}

#[test]
fn row_168_an_aggregating_with_paged_or_followed_by_a_match_then_an_aggregating_return() {
    let fx = abb();
    assert_eq!(
        rows(
            &fx,
            &format!("{GROUPS} ORDER BY w1 DESC LIMIT 1 RETURN sum(w1) AS c")
        ),
        vec![vec![int(2)]]
    );
    assert_eq!(
        rows(
            &fx,
            "MATCH (a) WITH count(*) AS n MATCH (b) RETURN n, count(*) AS c"
        ),
        vec![vec![int(3), int(3)]]
    );
}

#[test]
fn row_168_what_already_ran_is_unchanged() {
    let fx = abb();
    // A non-aggregating RETURN after an aggregating WITH.
    assert_eq!(
        rows(&fx, &format!("{GROUPS} RETURN w0 AS c0 ORDER BY c0")),
        vec![vec![s("a")], vec![s("b")]]
    );
    // An aggregating RETURN with a WHERE between it and the WITH.
    assert_eq!(
        rows(&fx, &format!("{GROUPS} WHERE w1 > 0 RETURN count(*) AS c0")),
        vec![vec![int(2)]]
    );
    // An aggregate with no WITH.
    assert_eq!(
        rows(
            &fx,
            "MATCH (a) RETURN a.s AS w0, count(*) AS w1 ORDER BY w0"
        ),
        vec![vec![s("a"), int(2)], vec![s("b"), int(1)]]
    );
}

#[test]
fn row_169_skip_and_limit_apply_to_an_ungrouped_aggregate() {
    let fx = abb();
    let none: Rows = vec![];
    assert_eq!(rows(&fx, "MATCH (a) RETURN count(*) AS c SKIP 1"), none);
    assert_eq!(rows(&fx, "MATCH (a) RETURN count(*) AS c LIMIT 0"), none);
    assert_eq!(
        rows(&fx, "MATCH (a) RETURN count(*) AS c SKIP 0"),
        vec![vec![int(3)]]
    );
    assert_eq!(
        rows(&fx, "MATCH (a) RETURN count(*) AS c SKIP 0 LIMIT 1"),
        vec![vec![int(3)]]
    );
    assert_eq!(
        rows(&fx, "MATCH (z:Missing) RETURN count(*) AS c LIMIT 0"),
        none
    );
    assert_eq!(
        rows(&fx, "MATCH (z:Missing) RETURN count(*) AS c LIMIT 1"),
        vec![vec![int(0)]]
    );
    let p = BTreeMap::from([("k".to_string(), Value::Int(1))]);
    assert_eq!(
        rows_with(&fx, "MATCH (a) RETURN count(*) AS c SKIP $k", &p).1,
        none
    );
    assert_eq!(
        rows_with(&fx, "MATCH (a) RETURN count(*) AS c LIMIT $k", &p).1,
        vec![vec![int(3)]]
    );
}

#[test]
fn row_169_skip_and_limit_on_an_ungrouped_aggregate_over_a_variable_length_path() {
    let fx = abb();
    let q = "MATCH (a)-[:T*1..2]->(b) RETURN count(*) AS c";
    assert_eq!(rows(&fx, q), vec![vec![int(1)]]);
    assert_eq!(rows(&fx, &format!("{q} SKIP 1")), Rows::new());
    assert_eq!(rows(&fx, &format!("{q} LIMIT 0")), Rows::new());
}

#[test]
fn row_169_skip_and_limit_on_several_ungrouped_aggregates_and_on_groups() {
    let fx = abb();
    assert_eq!(
        rows(&fx, "MATCH (a) RETURN count(*) AS c, max(a.s) AS m SKIP 1"),
        Rows::new()
    );
    assert_eq!(
        rows(&fx, "MATCH (a) RETURN count(*) AS c, max(a.s) AS m LIMIT 1"),
        vec![vec![int(3), s("b")]]
    );
    let grouped = "MATCH (a) RETURN a.s AS w, count(*) AS n ORDER BY w";
    assert_eq!(
        rows(&fx, &format!("{grouped} SKIP 1")),
        vec![vec![s("b"), int(1)]]
    );
    assert_eq!(
        rows(&fx, &format!("{grouped} LIMIT 1")),
        vec![vec![s("a"), int(2)]]
    );
}

#[test]
fn row_169_skip_and_limit_on_an_ungrouped_aggregate_after_a_with() {
    let fx = abb();
    let none = Rows::new();
    assert_eq!(
        rows(&fx, "MATCH (a) WITH a RETURN count(*) AS c SKIP 1"),
        none
    );
    assert_eq!(
        rows(&fx, "MATCH (a) WITH a RETURN count(*) AS c LIMIT 0"),
        none
    );
    assert_eq!(
        rows(&fx, "MATCH (a) WITH a RETURN count(*) AS c LIMIT 1"),
        vec![vec![int(3)]]
    );
    assert_eq!(
        rows(&fx, &format!("{GROUPS} RETURN count(*) AS c SKIP 1")),
        none
    );
    assert_eq!(
        rows(&fx, &format!("{GROUPS} RETURN count(*) AS c LIMIT 0")),
        none
    );
    assert_eq!(
        rows(&fx, &format!("{GROUPS} RETURN w0, w1 ORDER BY w0 SKIP 1")),
        vec![vec![s("b"), int(1)]]
    );
}
