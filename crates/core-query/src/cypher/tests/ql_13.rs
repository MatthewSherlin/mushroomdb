//! QL-13: the temporal function surface, evaluated through the full
//! lex/parse/plan/execute path.

use super::tests::{eval, eval_err, run, Fx};
use core_storage::Value;
use std::collections::BTreeMap;

fn date(y: i64, m: u32, d: u32) -> Value {
    // 719_468 is the day count from 0000-03-01 to 1970-01-01.
    let (y, mi) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * i64::from(mi) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Value::Date((era * 146_097 + doe - 719_468) as i32)
}

#[test]
fn constructors_build_the_value_types() {
    assert_eq!(eval("date(\"2024-03-15\")"), Some(date(2024, 3, 15)));
    assert_eq!(eval("date(\"1970-01-01\")"), Some(Value::Date(0)));
    assert_eq!(
        eval("date({year: 2024, month: 3, day: 15})"),
        Some(date(2024, 3, 15))
    );
    assert_eq!(eval("date({year: 2024})"), Some(date(2024, 1, 1)));
    assert_eq!(
        eval("datetime(\"2024-03-15T10:30:00Z\")"),
        Some(Value::DateTime {
            micros_utc: 1_710_498_600_000_000,
            offset_seconds: 0
        })
    );
    // The offset is kept; the instant is UTC.
    assert_eq!(
        eval("datetime(\"2024-03-15T10:30:00+05:30\")"),
        Some(Value::DateTime {
            micros_utc: 1_710_498_600_000_000 - 19_800 * 1_000_000,
            offset_seconds: 19_800
        })
    );
    // No offset: read as UTC.
    assert_eq!(
        eval("datetime(\"2024-03-15T10:30:00\")"),
        eval("datetime(\"2024-03-15T10:30:00Z\")")
    );
    assert_eq!(
        eval("localdatetime(\"2024-03-15T10:30:00\")"),
        Some(Value::LocalDateTime(1_710_498_600_000_000))
    );
    assert_eq!(
        eval("localdatetime({year: 2024, month: 3, day: 15, hour: 10, minute: 30})"),
        Some(Value::LocalDateTime(1_710_498_600_000_000))
    );
    assert_eq!(
        eval("duration({days: 3, hours: 2})"),
        Some(Value::Duration {
            months: 0,
            days: 3,
            nanos: 7_200_000_000_000
        })
    );
    assert_eq!(
        eval("duration({years: 1, months: 2, weeks: 1, seconds: 1.5})"),
        Some(Value::Duration {
            months: 14,
            days: 7,
            nanos: 1_500_000_000
        })
    );
    assert_eq!(
        eval("duration(\"P1Y2M3DT4H\")"),
        Some(Value::Duration {
            months: 14,
            days: 3,
            nanos: 14_400_000_000_000
        })
    );
}

#[test]
fn no_argument_is_the_current_instant() {
    assert!(matches!(eval("date()"), Some(Value::Date(d)) if d > 20_000));
    assert!(matches!(eval("datetime()"), Some(Value::DateTime { .. })));
    assert!(matches!(
        eval("localdatetime()"),
        Some(Value::LocalDateTime(_))
    ));
    // A duration has no "now": it needs its argument (QL-TF: an error, not null).
    assert!(eval_err("duration()").contains("duration() requires"));
}

#[test]
fn conversions_between_temporal_types() {
    assert_eq!(
        eval("date(datetime(\"2024-03-15T23:30:00-05:00\"))"),
        Some(date(2024, 3, 15))
    );
    assert_eq!(
        eval("date(localdatetime(\"2024-03-15T23:30:00\"))"),
        Some(date(2024, 3, 15))
    );
    assert_eq!(
        eval("localdatetime(date(\"2024-03-15\"))"),
        eval("localdatetime(\"2024-03-15T00:00:00\")")
    );
}

/// QL-TF: an input that cannot be read is an error naming the function, not
/// null (QL-13 gave null).
#[test]
fn invalid_input_is_an_error_not_null() {
    for q in [
        "date(\"not-a-date\")",
        "date(\"2024-02-30\")",
        "date(\"2024-13-01\")",
        "date(\"2024-03-15T10:00:00\")",
        "datetime(\"nope\")",
        "datetime(\"2024-03-15T10:30:00+19:00\")",
        "localdatetime(\"2024-03-15\")",
        "duration(\"nope\")",
        "date({month: 3})",
        "date({year: 2024, month: 13})",
        "date({year: 2024, day: 31, month: 4})",
        "date({year: 2024, bogus: 1})",
        "date({year: \"2024\"})",
        "duration({bogus: 1})",
        "duration({days: \"3\"})",
        "duration({days: 3000000000})",
        "localdatetime({year: 2024, hour: 24})",
    ] {
        let err = eval_err(q);
        let f = &q[..q.find('(').unwrap()];
        assert!(err.contains(&format!("{f}()")), "{q}: {err}");
    }
}

/// Null in, null out; a wrong type is an error naming the function (QL-TF).
#[test]
fn null_arguments_are_null_and_wrong_types_are_errors() {
    for f in ["date", "datetime", "localdatetime", "duration"] {
        assert_eq!(eval(&format!("{f}(n.nope)")), None, "{f}(n.nope)");
        for arg in ["42", "true", "$l"] {
            let err = eval_err(&format!("{f}({arg})"));
            assert!(err.contains(&format!("{f}() expects")), "{f}({arg}): {err}");
        }
    }
    assert!(eval_err("duration(date(\"2024-01-01\"))").contains("duration() expects"));
}

#[test]
fn wrong_arity_is_an_error() {
    for f in ["date", "datetime", "localdatetime", "duration"] {
        let err = eval_err(&format!("{f}(\"a\", \"b\")"));
        assert!(err.contains(&format!("{f}() requires")), "{err}");
    }
}

#[test]
fn time_types_are_refused_with_the_named_error() {
    for q in [
        "time(\"10:00:00\")",
        "localtime(\"10:00:00\")",
        "time()",
        "localtime()",
    ] {
        assert_eq!(
            eval_err(q),
            "not supported in mushroomdb 0.10: time types",
            "{q}"
        );
    }
}

/// Whether `MATCH (n:N) WHERE <cond>` keeps the one node: a comparison in a
/// WHERE is the only place the grammar evaluates one.
fn holds(cond: &str) -> bool {
    let mut fx = Fx::new();
    fx.add("N", "x", vec![]);
    let rs = run(
        &fx.view(),
        &format!("MATCH (n:N) WHERE {cond} RETURN n.key AS k"),
        &BTreeMap::new(),
    )
    .unwrap_or_else(|e| panic!("`{cond}` failed: {e}"));
    rs.len() == 1
}

#[test]
fn comparison_and_equality() {
    assert!(holds("date(\"2024-01-01\") < date(\"2024-12-31\")"));
    assert!(!holds("date(\"2024-12-31\") < date(\"2024-01-01\")"));
    assert!(holds("date(\"2024-01-01\") = date(\"2024-01-01\")"));
    assert!(holds("date(\"2024-01-01\") >= date(\"2024-01-01\")"));
    assert!(holds(
        "localdatetime(\"2024-01-01T00:00:01\") > localdatetime(\"2024-01-01T00:00:00\")"
    ));
    // The same instant at two offsets is equal and neither is less.
    assert!(holds(
        "datetime(\"2024-01-01T12:00:00+02:00\") = datetime(\"2024-01-01T10:00:00Z\")"
    ));
    assert!(!holds(
        "datetime(\"2024-01-01T12:00:00+02:00\") < datetime(\"2024-01-01T10:00:00Z\")"
    ));
    // Types do not coerce into each other.
    assert!(!holds(
        "date(\"2024-01-01\") = localdatetime(\"2024-01-01T00:00:00\")"
    ));
    assert!(!holds(
        "date(\"2024-01-01\") < localdatetime(\"2024-12-01T00:00:00\")"
    ));
    // A month is not 30 days; durations are equal component-wise and unordered.
    assert!(!holds("duration({months: 1}) = duration({days: 30})"));
    assert!(!holds("duration({days: 1}) = duration({hours: 24})"));
    assert!(holds("duration({days: 1}) = duration({days: 1})"));
    assert!(!holds("duration({days: 1}) < duration({days: 2})"));
    // Null on either side drops the row.
    assert!(!holds("date(null) < date(\"2024-01-01\")"));
    assert!(!holds("date(\"2024-01-01\") <> date(null)"));
}

#[test]
fn where_and_order_by_over_stored_temporal_values() {
    let mut fx = Fx::new();
    fx.add("N", "c", vec![("d", Value::Date(300))]);
    fx.add("N", "a", vec![("d", Value::Date(100))]);
    fx.add("N", "b", vec![("d", Value::Date(200))]);
    fx.add("N", "none", vec![]);
    let keys = |q: &str| -> Vec<Option<Value>> {
        let rs = run(&fx.view(), q, &BTreeMap::new()).unwrap();
        (0..rs.len()).map(|i| rs.get(i, "k").cloned()).collect()
    };
    let k = |s: &str| Some(Value::Str(s.into()));
    assert_eq!(
        keys("MATCH (n:N) WHERE n.d < date(\"1970-08-01\") RETURN n.key AS k ORDER BY n.d"),
        vec![k("a"), k("b")]
    );
    assert_eq!(
        keys("MATCH (n:N) RETURN n.key AS k ORDER BY n.d"),
        vec![k("a"), k("b"), k("c"), k("none")]
    );
    // Null is the largest value, so DESC puts it first (QL-OB).
    assert_eq!(
        keys("MATCH (n:N) RETURN n.key AS k ORDER BY n.d DESC"),
        vec![k("none"), k("c"), k("b"), k("a")]
    );
    // ORDER BY a computed temporal expression sorts the same way.
    assert_eq!(
        keys("MATCH (n:N) RETURN n.key AS k ORDER BY n.d + duration({days: 1}) DESC"),
        vec![k("none"), k("c"), k("b"), k("a")]
    );
}

#[test]
fn arithmetic_with_durations() {
    assert_eq!(
        eval("date(\"2024-01-01\") + duration({days: 1})"),
        Some(date(2024, 1, 2))
    );
    assert_eq!(
        eval("duration({days: 1}) + date(\"2024-01-01\")"),
        Some(date(2024, 1, 2))
    );
    assert_eq!(
        eval("date(\"2024-03-01\") - duration({days: 1})"),
        Some(date(2024, 2, 29))
    );
    // A month lands on the last day of a shorter month.
    assert_eq!(
        eval("date(\"2024-01-31\") + duration({months: 1})"),
        Some(date(2024, 2, 29))
    );
    assert_eq!(
        eval("date(\"2023-01-31\") + duration({years: 1, months: 1})"),
        Some(date(2024, 2, 29))
    );
    assert_eq!(
        eval("date(\"2024-03-31\") - duration({months: 1})"),
        Some(date(2024, 2, 29))
    );
    assert_eq!(
        eval("date(\"2024-01-01\") - duration({months: 2})"),
        Some(date(2023, 11, 1))
    );
    // A date drops the time part of the duration.
    assert_eq!(
        eval("date(\"2024-01-01\") + duration({hours: 30})"),
        Some(date(2024, 1, 1))
    );
    assert_eq!(
        eval("localdatetime(\"2024-01-01T23:00:00\") + duration({hours: 2})"),
        eval("localdatetime(\"2024-01-02T01:00:00\")")
    );
    assert_eq!(
        eval("localdatetime(\"2024-01-31T10:00:00\") + duration({months: 1, hours: 1})"),
        eval("localdatetime(\"2024-02-29T11:00:00\")")
    );
    // A DateTime moves on its own wall clock and keeps its offset.
    assert_eq!(
        eval("datetime(\"2024-01-31T23:30:00+05:30\") + duration({hours: 1})"),
        eval("datetime(\"2024-02-01T00:30:00+05:30\")")
    );
    assert_eq!(
        eval("toString(datetime(\"2024-01-31T23:30:00+05:30\") + duration({hours: 1}))"),
        Some(Value::Str("2024-02-01T00:30:00+05:30".into()))
    );
    assert_eq!(
        eval("duration({days: 1}) + duration({days: 2, hours: 1})"),
        eval("duration({days: 3, hours: 1})")
    );
    assert_eq!(
        eval("duration({days: 3}) - duration({days: 1, months: 1})"),
        eval("duration({days: 2, months: -1})")
    );
    assert_eq!(
        eval("duration({days: 1, hours: 2}) * 3"),
        eval("duration({days: 3, hours: 6})")
    );
    assert_eq!(eval("2 * duration({days: 1})"), eval("duration({days: 2})"));
    assert_eq!(
        eval("duration({hours: 4}) / 2"),
        eval("duration({hours: 2})")
    );
}

#[test]
fn arithmetic_null_and_range() {
    assert_eq!(eval("date(\"2024-01-01\") + n.nope"), None);
    assert_eq!(eval("date(null) + duration({days: 1})"), None);
    // Out of range is null, not a panic.
    assert_eq!(
        eval("date(\"2024-01-01\") + duration({years: 100000000})"),
        None
    );
    assert_eq!(eval("duration({days: 1}) / 0"), None);
    // A temporal value is not a number.
    assert!(eval_err("date(\"2024-01-01\") + 1").contains("must be numeric"));
    assert!(eval_err("date(\"2024-01-01\") * 2").contains("must be numeric"));
}

#[test]
fn to_string_is_the_iso_text() {
    assert_eq!(
        eval("toString(date(\"2024-03-15\"))"),
        Some(Value::Str("2024-03-15".into()))
    );
    assert_eq!(
        eval("toString(localdatetime(\"2024-03-15T10:30:00\"))"),
        Some(Value::Str("2024-03-15T10:30:00".into()))
    );
    assert_eq!(
        eval("toString(datetime(\"2024-03-15T10:30:00Z\"))"),
        Some(Value::Str("2024-03-15T10:30:00+00:00".into()))
    );
    assert_eq!(
        eval("toString(duration({days: 3, hours: 2}))"),
        Some(Value::Str("P3DT2H".into()))
    );
    assert_eq!(eval("toString(date(null))"), None);
}

/// QL-TF: out of range is an error naming the function (QL-13 gave null);
/// never a panic or a wrap.
#[test]
fn extreme_years_and_fields_are_errors_not_a_panic_or_a_wrap() {
    for q in [
        "date({year: 9223372036854775807})",
        "date({year: -9223372036854775807})",
        "date({year: 9223372036854775807, month: 12, day: 31})",
        "datetime({year: 9223372036854775807})",
        "localdatetime({year: 9223372036854775807})",
        "localdatetime({year: -9223372036854775807})",
        "date({year: 2024, month: 9223372036854775807})",
        "date({year: 2024, day: 9223372036854775807})",
        "date({year: 2024, month: -9223372036854775807})",
        "localdatetime({year: 2024, hour: 9223372036854775807})",
        "localdatetime({year: 2024, second: 9223372036854775807})",
        "localdatetime({year: 2024, nanosecond: 9223372036854775807})",
        "duration({years: 9223372036854775807})",
        "duration({days: 9223372036854775807})",
        "duration({weeks: 9223372036854775807})",
        "duration({hours: 9223372036854775807})",
        "duration({seconds: 9223372036854775807})",
        "duration({nanoseconds: 9223372036854775807, seconds: 9223372036854775807})",
        "duration({seconds: 1000000000000000000000000000000.0})",
        "duration({seconds: 9300000000000000000.0})",
        "duration({days: -9223372036854775807})",
        "duration({nanoseconds: -9223372036854775807, hours: -9223372036854775807})",
        // just outside the Date range (i32 days: 5881580-07-11 is the last day)
        "date({year: 5881580, month: 7, day: 12})",
        "date({year: 5881581})",
        "date({year: -5877641, month: 6, day: 22})",
        "date({year: -5877642})",
        // just outside the LocalDateTime range (i64 microseconds)
        "localdatetime({year: 294248})",
        "localdatetime({year: -290308})",
        "datetime({year: 294248})",
    ] {
        let err = eval_err(q);
        let f = &q[..q.find('(').unwrap()];
        assert!(err.contains(&format!("{f}()")), "{q}: {err}");
    }
}

#[test]
fn the_edges_of_the_ranges_are_values() {
    assert_eq!(
        eval("date({year: 5881580, month: 7, day: 11})"),
        Some(Value::Date(i32::MAX))
    );
    assert_eq!(
        eval("date({year: -5877641, month: 6, day: 23})"),
        Some(Value::Date(i32::MIN))
    );
    assert!(matches!(
        eval("localdatetime({year: 294247, month: 1, day: 1})"),
        Some(Value::LocalDateTime(_))
    ));
    assert!(matches!(
        eval("localdatetime({year: -290307, month: 1, day: 1})"),
        Some(Value::LocalDateTime(_))
    ));
}

#[test]
fn arithmetic_at_the_edges_is_null_not_a_panic() {
    for q in [
        "date({year: 5881580, month: 7, day: 11}) + duration({days: 1})",
        "date({year: 5881580, month: 7, day: 11}) + duration({months: 1})",
        "date({year: -5877641, month: 6, day: 23}) - duration({days: 1})",
        "date(\"2024-01-01\") + duration({days: 2147483647})",
        "date(\"2024-01-01\") - duration({days: 2147483647, months: 2147483647})",
        "date(\"2024-01-01\") + duration({months: 2147483647})",
        "date(\"2024-01-01\") - duration({months: -2147483647})",
        "localdatetime({year: 294247, month: 1, day: 1}) + duration({years: 1000})",
        "localdatetime(\"2024-01-01T00:00:00\") + duration({days: 2147483647})",
        "localdatetime(\"2024-01-01T00:00:00\") - duration({nanoseconds: 9223372036854775807, days: 2147483647})",
        "datetime({year: 294247, month: 1, day: 1}) + duration({years: 1000})",
        "duration({days: 2147483647}) + duration({days: 1})",
        "duration({months: -2147483647}) - duration({months: 2147483647})",
        "duration({nanoseconds: 9223372036854775807}) + duration({nanoseconds: 1})",
        "duration({days: 2147483647}) * 2",
        "duration({days: 1}) * 1000000000000000000000000000000.0",
        "duration({days: 1}) * 9223372036854775807",
        "duration({nanoseconds: 9223372036854775807}) * -2",
        "duration({days: 2147483647}) / 0.0000001",
    ] {
        assert_eq!(eval(q), None, "{q}");
    }
}
