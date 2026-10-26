//! QL-CL (defect row 161): the clock functions return one instant per
//! statement, `.transaction()` one per transaction, and `.realtime()` reads
//! the clock on every call.
//!
//! The clock here is a counter that moves one microsecond on every read, so
//! two reads of the clock can never agree and no test sleeps.

use super::tests::{run, Fx};
use crate::cypher::{with_test_clock, StatementClock, TransactionClock};
use core_storage::Value;
use std::cell::Cell;
use std::collections::BTreeMap;
use std::rc::Rc;

const START: i64 = 1_700_000_000_000_000;

/// A clock that reads `START`, `START + 1`, … and the count of its reads.
fn ticking() -> (impl FnMut() -> i64 + 'static, Rc<Cell<i64>>) {
    let reads = Rc::new(Cell::new(0));
    let seen = Rc::clone(&reads);
    (
        move || {
            let n = seen.get();
            seen.set(n + 1);
            START + n
        },
        reads,
    )
}

fn query(src: &str) -> Vec<Vec<Option<Value>>> {
    let mut fx = Fx::new();
    fx.add("N", "x", vec![]);
    let rs = run(&fx.view(), src, &BTreeMap::new()).unwrap_or_else(|e| panic!("`{src}`: {e}"));
    (0..rs.len()).map(|i| rs.row(i).to_vec()).collect()
}

fn one(expr: &str) -> Option<Value> {
    query(&format!("MATCH (n:N) RETURN {expr} AS r"))
        .remove(0)
        .remove(0)
}

fn truth(expr: &str) -> bool {
    one(expr) == Some(Value::Bool(true))
}

#[test]
fn every_clock_call_in_a_statement_is_one_instant() {
    let (clock, _) = ticking();
    with_test_clock(clock, || {
        assert!(truth("localdatetime() = localdatetime()"));
        assert!(truth("datetime() = datetime()"));
        assert!(truth("date() = date()"));
        assert!(truth("datetime() = datetime.statement()"));
        assert!(truth("localdatetime.statement() = localdatetime()"));
        // `datetime({timezone: …})` is the statement's instant on that clock.
        assert!(truth(
            "datetime({timezone: '+02:00'}) = datetime.statement('+02:00')"
        ));
    });
}

#[test]
fn the_instant_is_the_same_on_every_row() {
    let (clock, _) = ticking();
    with_test_clock(clock, || {
        let rows = query("UNWIND range(1, 5) AS i RETURN localdatetime() AS t");
        assert_eq!(rows.len(), 5);
        assert!(rows.iter().all(|r| r == &rows[0]), "{rows:?}");
    });
}

/// `expr` evaluated on a fresh clock that reads `START` first.
fn on_fresh_clock(expr: &str) -> Option<Value> {
    let (clock, _) = ticking();
    with_test_clock(clock, || one(expr))
}

#[test]
fn the_instant_is_in_the_functions_type_and_offset() {
    assert_eq!(
        on_fresh_clock("localdatetime()"),
        Some(Value::LocalDateTime(START))
    );
    assert_eq!(
        on_fresh_clock("datetime.statement('+02:00')"),
        Some(Value::DateTime {
            micros_utc: START,
            offset_seconds: 7200
        })
    );
    assert_eq!(
        on_fresh_clock("localdatetime.statement('+02:00')"),
        Some(Value::LocalDateTime(START + 7_200_000_000))
    );
}

#[test]
fn separate_statements_read_separate_instants() {
    let (clock, _) = ticking();
    with_test_clock(clock, || {
        let first = one("localdatetime()");
        let second = one("localdatetime()");
        assert_ne!(first, second);
    });
}

#[test]
fn realtime_reads_the_clock_on_every_call() {
    let (clock, _) = ticking();
    with_test_clock(clock, || {
        assert!(truth(
            "localdatetime.realtime() <> localdatetime.realtime()"
        ));
        assert!(truth("datetime.realtime() <> datetime.realtime()"));
        // And it is not the statement's instant.
        assert!(truth(
            "localdatetime.realtime() <> localdatetime.statement()"
        ));
    });
}

#[test]
fn a_statement_outside_a_transaction_is_its_own_transaction() {
    let (clock, _) = ticking();
    with_test_clock(clock, || {
        assert!(truth("datetime.transaction() = datetime.statement()"));
        assert!(truth("localdatetime.transaction() = localdatetime()"));
    });
}

#[test]
fn transaction_holds_its_instant_across_statements() {
    let (clock, _) = ticking();
    with_test_clock(clock, || {
        let begun = crate::cypher::clock_micros();
        let _tx = TransactionClock::enter(begun);
        let first = one("[datetime.transaction(), datetime.statement(), datetime.transaction()]");
        let second = one("[datetime.transaction(), datetime.statement()]");
        let (Some(Value::List(a)), Some(Value::List(b))) = (first, second) else {
            panic!("lists")
        };
        // `.transaction()` is BEGIN's instant in both statements.
        assert_eq!(a[0], a[2]);
        assert_eq!(a[0], b[0]);
        assert_eq!(
            a[0],
            Value::DateTime {
                micros_utc: begun,
                offset_seconds: 0
            }
        );
        // `.statement()` moves on with each statement.
        assert_ne!(a[1], b[1]);
        assert_ne!(a[1], a[0]);
    });
}

#[test]
fn a_guard_inside_a_statement_joins_it() {
    let (clock, _) = ticking();
    with_test_clock(clock, || {
        let _outer = StatementClock::enter();
        let first = one("localdatetime()");
        let second = one("localdatetime()");
        assert_eq!(first, second, "one statement, however it is executed");
    });
    // The instant does not outlive the statement.
    let (clock, _) = ticking();
    with_test_clock(clock, || {
        let first = {
            let _g = StatementClock::enter();
            one("localdatetime()")
        };
        assert_ne!(first, one("localdatetime()"));
    });
}
