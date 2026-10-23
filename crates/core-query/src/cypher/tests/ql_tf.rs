//! QL-TF: temporal map constructors that give a value or an error (never
//! null for a bad input), and the namespaced temporal functions
//! (`date.truncate`, `duration.between`, `datetime.fromepoch`, the clocks),
//! evaluated through the full lex/parse/plan/execute path.
//!
//! Expected values are the openCypher TCK's (Temporal1, 3, 9, 10) where the
//! TCK has one, written as ISO 8601 text.

use super::tests::{eval, eval_err, run, Fx};
use crate::cypher::temporal::to_text;
use core_storage::Value;
use std::collections::BTreeMap;

/// The ISO 8601 text of a temporal expression's value.
fn text(expr: &str) -> String {
    match eval(expr) {
        Some(v) => to_text(&v).unwrap_or_else(|| panic!("`{expr}` is not temporal: {v:?}")),
        None => panic!("`{expr}` is null"),
    }
}

fn assert_texts(cases: &[(&str, &str)]) {
    for (expr, want) in cases {
        assert_eq!(text(expr), *want, "{expr}");
    }
}

// ---------------------------------------------------------------------------
// Map constructors
// ---------------------------------------------------------------------------

#[test]
fn week_dates_follow_iso_weeks() {
    assert_texts(&[
        ("date({year: 1816, week: 1})", "1816-01-01"),
        ("date({year: 1816, week: 52})", "1816-12-23"),
        ("date({year: 1817, week: 1})", "1816-12-30"),
        ("date({year: 1817, week: 10})", "1817-03-03"),
        ("date({year: 1818, week: 53})", "1818-12-28"),
        ("date({year: 1819, week: 1})", "1819-01-04"),
        ("date({dayOfWeek: 2, year: 1817, week: 1})", "1816-12-31"),
        (
            "date({date: date('1816-12-30'), week: 2, dayOfWeek: 3})",
            "1817-01-08",
        ),
        ("date({date: date('1816-12-31'), week: 2})", "1817-01-07"),
        (
            "date({date: date('1816-12-31'), year: 1817, week: 2})",
            "1817-01-07",
        ),
        (
            "localdatetime({year: 1817, week: 10})",
            "1817-03-03T00:00:00",
        ),
        (
            "datetime({year: 1817, week: 10})",
            "1817-03-03T00:00:00+00:00",
        ),
    ]);
}

#[test]
fn ordinal_and_quarter_dates() {
    assert_texts(&[
        ("date({year: 1984, month: 10})", "1984-10-01"),
        ("date({year: 1984, week: 10, dayOfWeek: 3})", "1984-03-07"),
        ("date({year: 1984, week: 10})", "1984-03-05"),
        ("date({year: 1984, ordinalDay: 202})", "1984-07-20"),
        (
            "date({year: 1984, quarter: 3, dayOfQuarter: 45})",
            "1984-08-14",
        ),
        ("date({year: 1984, quarter: 3})", "1984-07-01"),
        ("date({year: 1984, ordinalDay: 366})", "1984-12-31"),
    ]);
}

#[test]
fn time_fields_combine_and_sub_microsecond_digits_are_truncated() {
    // QL-13's rule, kept: a constructor truncates below the microsecond
    // (as ISO 8601 text does), it never rounds.
    assert_texts(&[
        (
            "localdatetime({year: 1984, month: 10, day: 11, hour: 12, minute: 31, second: 14, nanosecond: 789, millisecond: 123, microsecond: 456})",
            "1984-10-11T12:31:14.123456",
        ),
        (
            "localdatetime({year: 1984, month: 10, day: 11, hour: 12, minute: 31, second: 14, nanosecond: 645876123})",
            "1984-10-11T12:31:14.645876",
        ),
        (
            "localdatetime({year: 1984, month: 10, day: 11, hour: 12, minute: 31, second: 14, microsecond: 645876})",
            "1984-10-11T12:31:14.645876",
        ),
        (
            "localdatetime({year: 1984, month: 10, day: 11, hour: 12, minute: 31, second: 14, millisecond: 645})",
            "1984-10-11T12:31:14.645000",
        ),
        (
            "localdatetime({year: 1984, quarter: 3, dayOfQuarter: 45, hour: 12})",
            "1984-08-14T12:00:00",
        ),
    ]);
}

#[test]
fn a_timezone_offset_is_kept_and_the_instant_is_utc() {
    assert_texts(&[
        (
            "datetime({year: 1984, month: 10, day: 11, hour: 12, minute: 31, second: 14, millisecond: 645, timezone: '+01:00'})",
            "1984-10-11T12:31:14.645000+01:00",
        ),
        (
            "datetime({year: 1984, month: 10, day: 11, hour: 12, minute: 34, second: 56, timezone: '+02:05:59'})",
            "1984-10-11T12:34:56+02:05:59",
        ),
        ("datetime({year: 1984, timezone: 'Z'})", "1984-01-01T00:00:00+00:00"),
        ("datetime({year: 1984, timezone: '-0530'})", "1984-01-01T00:00:00-05:30"),
        ("datetime({year: 1984, timezone: '+05'})", "1984-01-01T00:00:00+05:00"),
    ]);
    assert_eq!(
        eval("datetime({year: 1970, hour: 1, timezone: '+01:00'})"),
        Some(Value::DateTime {
            micros_utc: 0,
            offset_seconds: 3600
        })
    );
    assert_texts(&[
        (
            "datetime({epochSeconds: 416779, nanosecond: 999999999})",
            "1970-01-05T19:46:19.999999+00:00",
        ),
        (
            "datetime({epochMillis: 237821673987})",
            "1977-07-15T13:34:33.987000+00:00",
        ),
    ]);
}

#[test]
fn selecting_from_another_temporal_with_overrides() {
    let d = "date({year: 1984, month: 11, day: 11})";
    let ldt = "localdatetime({year: 1984, month: 11, day: 11, hour: 12, minute: 31, second: 14, nanosecond: 645876123})";
    let dt = "datetime({year: 1984, month: 11, day: 11, hour: 12, timezone: '+01:00'})";
    for other in [d, ldt, dt] {
        assert_texts(&[
            (&format!("date({other})"), "1984-11-11"),
            (&format!("date({{date: {other}}})"), "1984-11-11"),
            (&format!("date({{date: {other}, year: 28}})"), "0028-11-11"),
            (&format!("date({{date: {other}, day: 28}})"), "1984-11-28"),
            (&format!("date({{date: {other}, week: 1}})"), "1984-01-08"),
            (
                &format!("date({{date: {other}, ordinalDay: 28}})"),
                "1984-01-28",
            ),
            (
                &format!("date({{date: {other}, quarter: 3}})"),
                "1984-08-11",
            ),
        ]);
    }
    let ldt2 = "localdatetime({year: 1984, week: 10, dayOfWeek: 3, hour: 12, minute: 31, second: 14, millisecond: 645})";
    let dt2 = "datetime({year: 1984, month: 10, day: 11, hour: 12, timezone: '+01:00'})";
    assert_texts(&[
        (
            &format!("localdatetime({{date: {d}, hour: 10, minute: 10, second: 10}})"),
            "1984-11-11T10:10:10",
        ),
        (
            &format!("localdatetime({{year: 1984, month: 10, day: 11, time: {ldt2}, second: 42}})"),
            "1984-10-11T12:31:42.645000",
        ),
        (
            &format!("localdatetime({{year: 1984, month: 10, day: 11, time: {dt2}}})"),
            "1984-10-11T12:00:00",
        ),
        (
            &format!("localdatetime({{date: {ldt2}, time: {dt2}, day: 28, second: 42}})"),
            "1984-03-28T12:00:42",
        ),
        (
            &format!("localdatetime({{datetime: {dt2}, day: 28, second: 42}})"),
            "1984-10-28T12:00:42",
        ),
        // A date taken from a datetime does not bring its zone.
        (
            &format!("datetime({{date: {dt2}, hour: 10, minute: 10, second: 10}})"),
            "1984-10-11T10:10:10+00:00",
        ),
        (
            &format!("datetime({{date: {d}, hour: 10, timezone: '+05:00'}})"),
            "1984-11-11T10:00:00+05:00",
        ),
        // A time or datetime with a zone, given a new zone, keeps its instant.
        (
            &format!("datetime({{date: {d}, time: {dt2}, timezone: '+05:00'}})"),
            "1984-11-11T16:00:00+05:00",
        ),
        (
            &format!("datetime({{datetime: {dt2}, timezone: '+05:00'}})"),
            "1984-10-11T16:00:00+05:00",
        ),
        (
            &format!("datetime({{datetime: {dt2}, day: 28, second: 42, timezone: '-10:00'}})"),
            "1984-10-28T01:00:42-10:00",
        ),
        // A local one, given a zone, keeps its wall clock.
        (
            &format!("datetime({{datetime: {ldt2}, timezone: '+05:00'}})"),
            "1984-03-07T12:31:14.645000+05:00",
        ),
        (
            &format!("datetime({{date: {d}, time: {dt2}}})"),
            "1984-11-11T12:00:00+01:00",
        ),
    ]);
}

#[test]
fn invalid_maps_are_errors_naming_the_function_never_null() {
    for (expr, needle) in [
        ("date({month: 3})", "date(): `year` is required"),
        ("date({year: 2024, month: 13})", "date(): `month` 13 is outside 1..=12"),
        ("date({year: 2024, month: 4, day: 31})", "date(): `day` 31 is outside 1..=30"),
        ("date({year: 2023, month: 2, day: 29})", "date(): `day` 29 is outside 1..=28"),
        ("date({year: 2024, week: 53})", "date(): `week` 53 is outside 1..=52"),
        ("date({year: 2024, ordinalDay: 367})", "date(): `ordinalDay` 367 is outside 1..=366"),
        ("date({year: 2024, quarter: 5})", "date(): `quarter` 5 is outside 1..=4"),
        ("date({year: 2024, bogus: 1})", "date(): unknown field `bogus`"),
        ("date({year: 2024, hour: 1})", "date(): unknown field `hour`"),
        ("date({year: \"2024\"})", "date(): `year` must be an integer, got a string"),
        ("date({year: 2024, month: 1, week: 1})", "date(): `week` cannot be combined with `month`"),
        ("date({year: 2024, ordinalDay: 1, quarter: 1})", "cannot be combined"),
        ("date({year: 2024, day: 3})", "date(): `day` needs `month`"),
        ("date({year: 2024, dayOfWeek: 3})", "date(): `dayOfWeek` needs `week`"),
        ("date({year: 2024, dayOfQuarter: 3})", "date(): `dayOfQuarter` needs `quarter`"),
        ("date({})", "date(): `year` is required"),
        ("localdatetime({year: 2024, hour: 24})", "localdatetime(): `hour` 24 is outside 0..=23"),
        ("localdatetime({year: 2024, minute: 3})", "localdatetime(): `minute` needs `hour`"),
        ("localdatetime({year: 2024, hour: 1, second: 3})", "localdatetime(): `second` needs `minute`"),
        (
            "localdatetime({year: 2024, hour: 1, minute: 1, second: 1, millisecond: 1000})",
            "localdatetime(): `millisecond` 1000 is outside 0..=999",
        ),
        (
            "localdatetime({year: 2024, hour: 1, minute: 1, second: 1, millisecond: 1, microsecond: 1000})",
            "localdatetime(): `microsecond` 1000 is outside 0..=999",
        ),
        ("localdatetime({year: 2024, timezone: '+01:00'})", "localdatetime(): unknown field `timezone`"),
        (
            "localdatetime({datetime: date('2024-01-01'), date: date('2024-01-01')})",
            "localdatetime(): `datetime` cannot be combined with `date`",
        ),
        ("date({date: 3})", "date(): `date` must be a temporal value, got an integer"),
        ("localdatetime({year: 2024, time: date('2024-01-01')})", "localdatetime(): `time` must be a value with a time of day, got a Date"),
        ("datetime({year: 2024, timezone: '+19:00'})", "datetime(): `timezone` '+19:00' is not an offset within ±18:00"),
        ("datetime({year: 2024, timezone: 3})", "datetime(): `timezone` must be a string, got an integer"),
        ("datetime({epochSeconds: 1, year: 2024})", "datetime(): `epochSeconds` cannot be combined with `year`"),
        ("date({year: 5881581})", "date(): the date is out of range"),
        ("date({year: 9223372036854775807})", "date(): the date is out of range"),
        ("localdatetime({year: 294248})", "localdatetime(): the date-time is out of range"),
        ("datetime({year: -290308})", "datetime(): the date-time is out of range"),
    ] {
        let err = eval_err(expr);
        assert!(err.contains(needle), "{expr}: {err}");
    }
}

#[test]
fn named_zones_and_time_types_are_refused_by_name() {
    for expr in [
        "datetime({year: 1984, timezone: 'Europe/Stockholm'})",
        "datetime('2024-03-15T10:30:00+01:00[Europe/Paris]')",
        "datetime.truncate('day', date('2024-01-01'), {timezone: 'Europe/Paris'})",
        "datetime.realtime('Europe/Paris')",
    ] {
        let err = eval_err(expr);
        assert!(
            err.contains("not supported in mushroomdb 0.10: named time zones"),
            "{expr}: {err}"
        );
    }
    for expr in [
        "time.truncate('hour', localdatetime('2024-01-01T10:00'))",
        "localtime.statement()",
        "time.realtime()",
    ] {
        assert_eq!(
            eval_err(expr),
            "not supported in mushroomdb 0.10: time types",
            "{expr}"
        );
    }
}

#[test]
fn null_in_is_null_out_and_wrong_types_name_the_function() {
    for expr in [
        "date(null)",
        "datetime(n.nope)",
        "date({year: null})",
        "date({year: 2024, month: n.nope})",
        "date.truncate('year', null)",
        "date.truncate(null, date('2024-01-01'))",
        "datetime.fromepoch(null, 0)",
        "datetime.fromepochmillis(null)",
        "duration.between(null, date('2024-01-01'))",
        "duration.inMonths(date('2024-01-01'), null)",
        "duration.inDays(null, null)",
        "duration.inSeconds(null, null)",
        "date.statement(null)",
        "datetime.transaction(null)",
        "localdatetime.realtime(null)",
    ] {
        assert_eq!(eval(expr), None, "{expr}");
    }
    for (expr, needle) in [
        (
            "date(42)",
            "date() expects a string, a map or a temporal value, got an integer",
        ),
        (
            "localdatetime(true)",
            "localdatetime() expects a string, a map or a temporal value, got a boolean",
        ),
        (
            "duration(date('2024-01-01'))",
            "duration() expects a string or a map, got a Date",
        ),
        (
            "duration($l)",
            "duration() expects a string or a map, got a list",
        ),
        (
            "date.truncate(1, date('2024-01-01'))",
            "date.truncate() expects a unit string, got an integer",
        ),
        (
            "date.truncate('year', 'x')",
            "date.truncate() expects a temporal value, got a string",
        ),
        (
            "date.truncate('year', date('2024-01-01'), 3)",
            "date.truncate() expects a map of fields, got an integer",
        ),
        (
            "datetime.fromepoch('1', 0)",
            "datetime.fromepoch() expects integers",
        ),
        (
            "datetime.fromepochmillis(1.5)",
            "datetime.fromepochmillis() expects an integer",
        ),
        (
            "duration.between(1, date('2024-01-01'))",
            "duration.between() expects temporal values, got an integer",
        ),
        (
            "duration.inDays(date('2024-01-01'), duration('P1D'))",
            "duration.inDays() expects temporal values, got a Duration",
        ),
        (
            "date.statement(3)",
            "date.statement() expects a timezone string or map, got an integer",
        ),
    ] {
        let err = eval_err(expr);
        assert!(err.contains(needle), "{expr}: {err}");
    }
}

#[test]
fn invalid_strings_are_errors_and_offsets_may_omit_the_colon() {
    for (expr, needle) in [
        (
            "date('not-a-date')",
            "date(): cannot read 'not-a-date' as a Date",
        ),
        ("date('2024-02-30')", "date(): cannot read"),
        (
            "localdatetime('2024-03-15')",
            "localdatetime(): cannot read",
        ),
        ("datetime('nope')", "datetime(): cannot read"),
        (
            "duration('nope')",
            "duration(): cannot read 'nope' as a Duration",
        ),
    ] {
        let err = eval_err(expr);
        assert!(err.contains(needle), "{expr}: {err}");
    }
    assert_eq!(
        text("datetime('2015-07-21T21:40:32.142+0100')"),
        "2015-07-21T21:40:32.142000+01:00"
    );
    assert_eq!(
        text("datetime('2015-07-21T21:40+01')"),
        "2015-07-21T21:40:00+01:00"
    );
}

#[test]
fn duration_maps_take_fractions_and_carry_them_down() {
    assert_texts(&[
        (
            "duration({days: 14, hours: 16, minutes: 12})",
            "P14DT16H12M",
        ),
        ("duration({months: 5, days: 1.5})", "P5M1DT12H"),
        ("duration({months: 0.75})", "P22DT19H51M49.5S"),
        ("duration({weeks: 2.5})", "P17DT12H"),
        (
            "duration({years: 12, months: 5, days: 14, hours: 16, minutes: 12, seconds: 70})",
            "P12Y5M14DT16H13M10S",
        ),
        (
            "duration({days: 14, seconds: 70, nanoseconds: 1})",
            "P14DT1M10.000000001S",
        ),
        ("duration({minutes: 1.5, seconds: 1})", "PT1M31S"),
        ("duration({years: 0.5})", "P6M"),
        ("duration({days: -1.5})", "P-1DT-12H"),
    ]);
    for (expr, needle) in [
        ("duration({bogus: 1})", "duration(): unknown field `bogus`"),
        (
            "duration({days: '3'})",
            "duration(): `days` must be a number, got a string",
        ),
        (
            "duration({days: 3000000000})",
            "duration(): the duration is out of range",
        ),
        (
            "duration({seconds: 1000000000000000000000000000000.0})",
            "duration(): the duration is out of range",
        ),
        ("duration()", "duration() requires"),
    ] {
        let err = eval_err(expr);
        assert!(err.contains(needle), "{expr}: {err}");
    }
}

// ---------------------------------------------------------------------------
// Namespaced functions
// ---------------------------------------------------------------------------

const D84: &str = "date({year: 1984, month: 10, day: 11})";
const LDT84: &str =
    "localdatetime({year: 1984, month: 10, day: 11, hour: 12, minute: 31, second: 14, nanosecond: 645876123})";
const DT84: &str = "datetime({year: 1984, month: 10, day: 11, hour: 12, minute: 31, second: 14, nanosecond: 645876123, timezone: '+01:00'})";

#[test]
fn date_truncate_over_every_date_unit() {
    for other in [D84, LDT84, DT84] {
        assert_texts(&[
            (
                &format!("date.truncate('millennium', {other})"),
                "1000-01-01",
            ),
            (
                &format!("date.truncate('century', {other}, {{day: 2}})"),
                "1900-01-02",
            ),
            (&format!("date.truncate('decade', {other})"), "1980-01-01"),
            (
                &format!("date.truncate('year', {other}, {{day: 2}})"),
                "1984-01-02",
            ),
            (&format!("date.truncate('quarter', {other})"), "1984-10-01"),
            (
                &format!("date.truncate('month', {other}, {{day: 2}})"),
                "1984-10-02",
            ),
            (&format!("date.truncate('week', {other})"), "1984-10-08"),
            (
                &format!("date.truncate('week', {other}, {{dayOfWeek: 2}})"),
                "1984-10-09",
            ),
            (&format!("date.truncate('day', {other})"), "1984-10-11"),
            (&format!("date.truncate('weekYear', {other})"), "1984-01-02"),
        ]);
    }
    assert_texts(&[
        (
            "date.truncate('millennium', date('2017-10-11'))",
            "2000-01-01",
        ),
        (
            "date.truncate('weekYear', date('1984-01-01'), {day: 5})",
            "1983-01-05",
        ),
        (
            "date.truncate('WEEKYEAR', date('1984-01-01'))",
            "1983-01-03",
        ),
    ]);
    for (expr, needle) in [
        (
            "date.truncate('hour', date('2024-01-01'))",
            "date.truncate(): cannot truncate a Date to `hour`",
        ),
        (
            "date.truncate('fortnight', date('2024-01-01'))",
            "date.truncate(): unknown unit 'fortnight'",
        ),
        (
            "date.truncate('year', date('2024-01-01'), {hour: 1})",
            "date.truncate(): unknown field `hour`",
        ),
        ("date.truncate('year')", "date.truncate() requires"),
    ] {
        let err = eval_err(expr);
        assert!(err.contains(needle), "{expr}: {err}");
    }
}

#[test]
fn datetime_and_localdatetime_truncate() {
    assert_texts(&[
        (
            &format!("datetime.truncate('year', {DT84})"),
            "1984-01-01T00:00:00+01:00",
        ),
        (
            &format!("datetime.truncate('year', {D84})"),
            "1984-01-01T00:00:00+00:00",
        ),
        (
            &format!("datetime.truncate('year', {LDT84}, {{day: 2}})"),
            "1984-01-02T00:00:00+00:00",
        ),
        (
            &format!("datetime.truncate('hour', {DT84})"),
            "1984-10-11T12:00:00+01:00",
        ),
        (
            &format!("datetime.truncate('minute', {DT84})"),
            "1984-10-11T12:31:00+01:00",
        ),
        (
            &format!("datetime.truncate('second', {DT84})"),
            "1984-10-11T12:31:14+01:00",
        ),
        (
            &format!("datetime.truncate('millisecond', {DT84})"),
            "1984-10-11T12:31:14.645000+01:00",
        ),
        (
            &format!("datetime.truncate('microsecond', {DT84})"),
            "1984-10-11T12:31:14.645876+01:00",
        ),
        (
            &format!("datetime.truncate('millisecond', {DT84}, {{microsecond: 2}})"),
            "1984-10-11T12:31:14.645002+01:00",
        ),
        // A zone in the map is the wall clock's new zone, not a conversion.
        (
            &format!("datetime.truncate('hour', {DT84}, {{timezone: '-01:00'}})"),
            "1984-10-11T12:00:00-01:00",
        ),
        (
            &format!("localdatetime.truncate('day', {DT84})"),
            "1984-10-11T00:00:00",
        ),
        (
            &format!("localdatetime.truncate('hour', {LDT84}, {{minute: 5}})"),
            "1984-10-11T12:05:00",
        ),
        (
            &format!("localdatetime.truncate('week', {D84})"),
            "1984-10-08T00:00:00",
        ),
        (
            &format!("localdatetime.truncate('second', {LDT84}, {{nanosecond: 2}})"),
            "1984-10-11T12:31:14",
        ),
    ]);
    let err = eval_err(&format!("datetime.truncate('hour', {D84})"));
    assert!(
        err.contains("datetime.truncate(): cannot truncate a Date to `hour`"),
        "{err}"
    );
}

#[test]
fn clocks_are_the_current_instant() {
    for f in ["statement", "transaction", "realtime"] {
        assert!(
            matches!(eval(&format!("date.{f}()")), Some(Value::Date(d)) if d > 20_000),
            "date.{f}"
        );
        assert!(
            matches!(
                eval(&format!("localdatetime.{f}()")),
                Some(Value::LocalDateTime(_))
            ),
            "localdatetime.{f}"
        );
        assert!(
            matches!(
                eval(&format!("datetime.{f}('+02:00')")),
                Some(Value::DateTime {
                    offset_seconds: 7200,
                    ..
                })
            ),
            "datetime.{f}"
        );
    }
    assert!(matches!(
        eval("datetime({timezone: '-05:00'})"),
        Some(Value::DateTime {
            offset_seconds: -18_000,
            ..
        })
    ));
    assert!(matches!(
        eval("date.realtime({timezone: '+14:00'})"),
        Some(Value::Date(_))
    ));
}

#[test]
fn from_epoch() {
    assert_texts(&[
        (
            "datetime.fromepoch(416779, 999999999)",
            "1970-01-05T19:46:19.999999+00:00",
        ),
        (
            "datetime.fromepochmillis(237821673987)",
            "1977-07-15T13:34:33.987000+00:00",
        ),
        ("datetime.fromepoch(-1, 0)", "1969-12-31T23:59:59+00:00"),
    ]);
    let err = eval_err("datetime.fromepoch(9223372036854775807, 0)");
    assert!(
        err.contains("datetime.fromepoch(): the date-time is out of range"),
        "{err}"
    );
}

#[test]
fn duration_between_and_its_units() {
    let cases: [(&str, &str, [&str; 4]); 8] = [
        (
            "date('1984-10-11')",
            "date('2015-06-24')",
            ["P30Y8M13D", "P30Y8M", "P11213D", "PT269112H"],
        ),
        (
            "date('1984-10-11')",
            "localdatetime('2016-07-21T21:45:22.142')",
            [
                "P31Y9M10DT21H45M22.142S",
                "P31Y9M",
                "P11606D",
                "PT278565H45M22.142S",
            ],
        ),
        (
            "date('1984-10-11')",
            "datetime('2015-07-21T21:40:32.142+0100')",
            [
                "P30Y9M10DT21H40M32.142S",
                "P30Y9M",
                "P11240D",
                "PT269781H40M32.142S",
            ],
        ),
        (
            "localdatetime('2015-07-21T21:40:32.142')",
            "date('2015-06-24')",
            [
                "P-27DT-21H-40M-32.142S",
                "PT0S",
                "P-27D",
                "PT-669H-40M-32.142S",
            ],
        ),
        (
            "localdatetime('2015-07-21T21:40:32.142')",
            "datetime('2015-07-21T21:40:32.142+0100')",
            ["PT0S", "PT0S", "PT0S", "PT0S"],
        ),
        (
            "datetime('2014-07-21T21:40:36.143+0200')",
            "date('2015-06-24')",
            ["P11M2DT2H19M23.857S", "P11M", "P337D", "PT8090H19M23.857S"],
        ),
        (
            "datetime('2014-07-21T21:40:36.143+0200')",
            "datetime('2015-07-21T21:40:32.142+0100')",
            ["P1YT59M55.999S", "P1Y", "P365D", "PT8760H59M55.999S"],
        ),
        (
            "localdatetime('2018-01-02T10:00:00.1')",
            "localdatetime('2018-01-01T10:00:00.2')",
            ["PT-23H-59M-59.9S", "PT0S", "PT0S", "PT-23H-59M-59.9S"],
        ),
    ];
    for (lhs, rhs, want) in cases {
        for (f, w) in ["between", "inMonths", "inDays", "inSeconds"]
            .iter()
            .zip(want)
        {
            assert_eq!(
                text(&format!("duration.{f}({lhs}, {rhs})")),
                w,
                "duration.{f}({lhs}, {rhs})"
            );
        }
    }
    assert_texts(&[
        ("duration.inMonths(date('2018-03-11'), date('2016-06-24'))", "P-1Y-8M"),
        (
            "duration.inMonths(datetime('2018-07-21T21:40:36.143+0500'), datetime('1984-07-21T22:40:36.143+0200'))",
            "P-33Y-11M",
        ),
        (
            "duration.inSeconds(localdatetime('2014-07-21T21:40:36.143'), localdatetime('2014-07-21T21:40:36.142'))",
            "PT-0.001S",
        ),
        ("duration.inSeconds(date(), date())", "PT0S"),
    ]);
    let err = eval_err("duration.inSeconds(date('-5000000-01-01'), date('+5000000-01-01'))");
    assert!(
        err.contains("duration.inSeconds(): the duration is out of range"),
        "{err}"
    );
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

#[test]
fn a_dotted_call_is_a_namespaced_function_and_property_access_is_unchanged() {
    let mut fx = Fx::new();
    fx.add(
        "N",
        "x",
        vec![
            ("date", Value::Str("p".into())),
            ("truncate", Value::Int(7)),
        ],
    );
    let params = BTreeMap::new();
    let rs = run(
        &fx.view(),
        "MATCH (n:N) WHERE n.truncate IN [7] RETURN n.date AS d, n.truncate AS t, \
         date.truncate('year', date('2024-05-05')) AS y",
        &params,
    )
    .unwrap();
    assert_eq!(rs.get(0, "d"), Some(&Value::Str("p".into())));
    assert_eq!(rs.get(0, "t"), Some(&Value::Int(7)));
    assert_eq!(
        rs.get(0, "y").and_then(to_text).as_deref(),
        Some("2024-01-01")
    );
    // Case-insensitive, like every function name.
    assert_eq!(
        text("DATE.Truncate('year', date('2024-05-05'))"),
        "2024-01-01"
    );
    let err = eval_err("foo.bar(1)");
    assert!(err.contains("unknown function `foo.bar`"), "{err}");
    let err = eval_err("date.bogus()");
    assert!(err.contains("unknown function `date.bogus`"), "{err}");
}
