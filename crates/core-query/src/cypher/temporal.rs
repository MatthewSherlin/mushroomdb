//! The temporal function surface (QL-13, QL-TF): `date`, `localdatetime`,
//! `datetime` and `duration`, the namespaced temporal functions
//! (`date.truncate`, `datetime.fromepoch`, `duration.between`, the clocks),
//! and the arithmetic between a temporal value and a duration.
//!
//! The value types, their ISO 8601 text and their ordering live in
//! `core-storage` and `value_ops`; this module builds values from query
//! arguments. A constructor or function gives a value or an error naming the
//! function, never null for a bad input (QL-TF): an unreadable string, an
//! unknown, conflicting or out-of-range map field and a wrong argument type
//! are errors. A null argument, or a null map field, is null. `time()`,
//! `localtime()` and their namespaces, and named time zones, are refused by
//! name.
//!
//! Precision: values hold microseconds. A constructor truncates nanosecond
//! digits toward the earlier instant, as the ISO 8601 parser does (QL-13's
//! rule, kept here); it never rounds. Durations keep nanoseconds.
//!
//! Storing a temporal value moves the store to the V11 snapshot format, at
//! the write path's choke point; a query result carrying one changes nothing
//! on disk.

use super::ast::ArithOp;
use core_storage::types::{
    date_to_iso8601, datetime_to_iso8601, duration_to_iso8601, iso8601_to_date,
    iso8601_to_datetime, iso8601_to_duration, iso8601_to_local_datetime, local_datetime_to_iso8601,
};
use core_storage::Value;
use std::collections::BTreeMap;
use std::ops::RangeInclusive;

const MICROS_PER_SECOND: i64 = 1_000_000;
const MICROS_PER_DAY: i64 = 86_400 * MICROS_PER_SECOND;
const NANOS_PER_MICRO: i64 = 1_000;
const NANOS_PER_SECOND: i64 = 1_000_000_000;
const NANOS_PER_DAY: i64 = 86_400 * NANOS_PER_SECOND;
/// The mean Gregorian month, which a fractional month of a duration becomes.
const DAYS_PER_MEAN_MONTH: f64 = 365.2425 / 12.0;
/// A `DateTime` offset is within ±18:00.
const MAX_OFFSET_SECONDS: i64 = 18 * 3600;

/// The largest year magnitude calendar math is attempted for. A `Date` holds
/// an `i32` day count, about +-5.88 million years, so anything past this is
/// out of range anyway; the bound keeps `days_from_civil`'s `i64` arithmetic
/// from overflowing on a user-supplied year such as `i64::MAX`.
const MAX_YEAR: u64 = 100_000_000;

/// The error `time()` and `localtime()` give: time-of-day types are not in 0.10.
pub const TIME_TYPES_REFUSAL: &str = "not supported in mushroomdb 0.10: time types";

/// The error a named time zone (`Europe/Paris`, `[Europe/Paris]`) gives: 0.10
/// has UTC offsets only.
pub const ZONE_ID_REFUSAL: &str = "not supported in mushroomdb 0.10: named time zones";

type Fallible<T> = Result<T, String>;

/// `<fname>(): <message>`.
fn fail(fname: &str, message: impl std::fmt::Display) -> String {
    format!("{fname}(): {message}")
}

/// A value's type, as an error names it.
fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Int(_) => "an integer",
        Value::Float(_) => "a float",
        Value::Str(_) => "a string",
        Value::List(_) => "a list",
        Value::Map(_) => "a map",
        Value::Date(_) => "a Date",
        Value::LocalDateTime(_) => "a LocalDateTime",
        Value::DateTime { .. } => "a DateTime",
        Value::Duration { .. } => "a Duration",
        Value::Binary(_) => "binary data",
    }
}

/// The three temporal types a constructor builds.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Date,
    LocalDateTime,
    DateTime,
}

impl Kind {
    fn of(norm: &str) -> Option<Kind> {
        match norm {
            "date" => Some(Kind::Date),
            "localdatetime" => Some(Kind::LocalDateTime),
            "datetime" => Some(Kind::DateTime),
            _ => None,
        }
    }

    fn has_time(self) -> bool {
        self != Kind::Date
    }

    fn type_name(self) -> &'static str {
        match self {
            Kind::Date => "Date",
            Kind::LocalDateTime => "LocalDateTime",
            Kind::DateTime => "DateTime",
        }
    }
}

/// A temporal value taken apart on its own wall clock: the local day, the
/// local time of day in nanoseconds, and the UTC offset when it has one.
#[derive(Clone, Copy)]
struct Parts {
    days: i64,
    nanos: i64,
    offset: Option<i32>,
}

impl Parts {
    fn of(v: &Value) -> Option<Parts> {
        match *v {
            Value::Date(d) => Some(Parts {
                days: i64::from(d),
                nanos: 0,
                offset: None,
            }),
            Value::LocalDateTime(us) => Some(Parts::local(i128::from(us), None)),
            Value::DateTime {
                micros_utc,
                offset_seconds,
            } => Some(Parts::local(
                i128::from(micros_utc) + i128::from(offset_seconds) * i128::from(MICROS_PER_SECOND),
                Some(offset_seconds),
            )),
            _ => None,
        }
    }

    /// Local microseconds since the epoch, split.
    fn local(micros: i128, offset: Option<i32>) -> Parts {
        let per_day = i128::from(MICROS_PER_DAY);
        Parts {
            days: micros.div_euclid(per_day) as i64,
            nanos: micros.rem_euclid(per_day) as i64 * NANOS_PER_MICRO,
            offset,
        }
    }

    /// Local microseconds since the epoch; sub-microsecond nanoseconds are
    /// truncated (`nanos` is never negative).
    fn local_micros(&self) -> i128 {
        i128::from(self.days) * i128::from(MICROS_PER_DAY)
            + i128::from(self.nanos / NANOS_PER_MICRO)
    }

    fn into_value(self, kind: Kind, fname: &str) -> Fallible<Value> {
        match kind {
            Kind::Date => i32::try_from(self.days)
                .map(Value::Date)
                .map_err(|_| fail(fname, "the date is out of range")),
            Kind::LocalDateTime => i64::try_from(self.local_micros())
                .map(Value::LocalDateTime)
                .map_err(|_| fail(fname, "the date-time is out of range")),
            Kind::DateTime => {
                let offset_seconds = self.offset.unwrap_or(0);
                let utc = self.local_micros()
                    - i128::from(offset_seconds) * i128::from(MICROS_PER_SECOND);
                let micros_utc =
                    i64::try_from(utc).map_err(|_| fail(fname, "the date-time is out of range"))?;
                Ok(Value::DateTime {
                    micros_utc,
                    offset_seconds,
                })
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Constructors
// ---------------------------------------------------------------------------

/// Evaluate `date(x)`, `localdatetime(x)`, `datetime(x)` or `duration(x)`.
/// `arg` is `None` when the call has no argument; the caller has checked the
/// argument count and turned a null argument into null. `Ok(None)`: a map
/// field is null.
pub fn construct(norm: &str, arg: Option<Value>) -> Fallible<Option<Value>> {
    if norm == "duration" {
        return match arg {
            Some(v) => duration_from(v),
            None => Err("duration() requires exactly 1 argument(s), got 0".to_string()),
        };
    }
    let Some(kind) = Kind::of(norm) else {
        return Err(format!("unknown function `{norm}`"));
    };
    match arg {
        None => now(kind, 0, Scope::Statement).map(Some),
        Some(v) => temporal_from(kind, norm, v),
    }
}

// ---------------------------------------------------------------------------
// The clock: one instant per statement, one per transaction
// ---------------------------------------------------------------------------

/// Which instant a clock call reads. `date()`, `datetime()`, `localdatetime()`
/// and the `.statement()` forms read [`Scope::Statement`]; `.transaction()`
/// reads [`Scope::Transaction`]; `.realtime()` reads the clock itself.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    Statement,
    Transaction,
    Realtime,
}

#[derive(Default)]
struct ClockState {
    /// Nesting depth of [`StatementClock`] guards on this thread.
    depth: usize,
    /// The statement's instant, read on its first use.
    statement: Option<i64>,
    /// The enclosing transaction's instant, set by [`TransactionClock`].
    transaction: Option<i64>,
    /// A test's clock, in place of the system's.
    source: Option<Box<dyn FnMut() -> i64>>,
}

thread_local! {
    static CLOCK: std::cell::RefCell<ClockState> = std::cell::RefCell::new(ClockState::default());
}

/// Holds one instant for every non-realtime clock call made while it lives.
/// Take one at the top of every statement; a guard taken inside another (a
/// write statement running its read part) joins the outer statement's instant.
/// Outside any guard each call reads the clock, as a library caller of the
/// evaluator expects.
pub struct StatementClock {
    outermost: bool,
}

impl StatementClock {
    pub fn enter() -> Self {
        let outermost = CLOCK.with(|c| {
            let mut c = c.borrow_mut();
            c.depth += 1;
            c.depth == 1
        });
        Self { outermost }
    }
}

impl Drop for StatementClock {
    fn drop(&mut self) {
        CLOCK.with(|c| {
            let mut c = c.borrow_mut();
            c.depth -= 1;
            if self.outermost {
                c.statement = None;
            }
        });
    }
}

/// Makes `instant` (microseconds since the epoch, from [`clock_micros`] at
/// BEGIN) the instant `.transaction()` returns while it lives.
pub struct TransactionClock {
    previous: Option<i64>,
}

impl TransactionClock {
    pub fn enter(instant: i64) -> Self {
        let previous = CLOCK.with(|c| c.borrow_mut().transaction.replace(instant));
        Self { previous }
    }
}

impl Drop for TransactionClock {
    fn drop(&mut self) {
        CLOCK.with(|c| c.borrow_mut().transaction = self.previous);
    }
}

/// The clock now, in microseconds since the epoch: what BEGIN records as a
/// transaction's instant.
pub fn clock_micros() -> i64 {
    CLOCK.with(|c| match c.borrow_mut().source.as_mut() {
        Some(source) => source(),
        None => system_micros(),
    })
}

/// Run `body` with `source` as this thread's clock. For tests: it makes the
/// instants deterministic without a sleep.
#[doc(hidden)]
pub fn with_test_clock<R>(source: impl FnMut() -> i64 + 'static, body: impl FnOnce() -> R) -> R {
    struct Restore(Option<Box<dyn FnMut() -> i64>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            CLOCK.with(|c| c.borrow_mut().source = self.0.take());
        }
    }
    let previous = CLOCK.with(|c| c.borrow_mut().source.replace(Box::new(source)));
    let _restore = Restore(previous);
    body()
}

fn system_micros() -> i64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_micros()).unwrap_or(i64::MAX),
        Err(e) => -i64::try_from(e.duration().as_micros()).unwrap_or(i64::MAX),
    }
}

/// The instant `scope` names. Inside a statement the first read fixes it.
fn instant(scope: Scope) -> i64 {
    match scope {
        Scope::Realtime => clock_micros(),
        Scope::Statement => {
            let held = CLOCK.with(|c| {
                let c = c.borrow();
                (c.depth > 0).then_some(c.statement)
            });
            match held {
                None => clock_micros(),
                Some(Some(t)) => t,
                Some(None) => {
                    let t = clock_micros();
                    CLOCK.with(|c| c.borrow_mut().statement = Some(t));
                    t
                }
            }
        }
        // A statement outside a transaction is its own transaction.
        Scope::Transaction => match CLOCK.with(|c| c.borrow().transaction) {
            Some(t) => t,
            None => instant(Scope::Statement),
        },
    }
}

/// The instant `scope` names, as `kind`, on the clock of `offset_seconds`.
fn now(kind: Kind, offset_seconds: i32, scope: Scope) -> Fallible<Value> {
    let local =
        i128::from(instant(scope)) + i128::from(offset_seconds) * i128::from(MICROS_PER_SECOND);
    Parts::local(local, Some(offset_seconds)).into_value(kind, kind_fn(kind))
}

fn kind_fn(kind: Kind) -> &'static str {
    match kind {
        Kind::Date => "date",
        Kind::LocalDateTime => "localdatetime",
        Kind::DateTime => "datetime",
    }
}

fn temporal_from(kind: Kind, fname: &str, arg: Value) -> Fallible<Option<Value>> {
    match arg {
        Value::Str(s) => parse_text(kind, fname, &s).map(Some),
        Value::Map(m) => from_map(kind, fname, &m, None),
        other => match Parts::of(&other) {
            // Another temporal value: its date, its wall clock, or (for a
            // `DateTime` from a local value) UTC.
            Some(parts) => parts.into_value(kind, fname).map(Some),
            None => Err(format!(
                "{fname}() expects a string, a map or a temporal value, got {}",
                type_name(&other)
            )),
        },
    }
}

/// A string with no offset is read as UTC by `datetime`, the openCypher
/// default zone. An offset may omit its colon (`+0100`) or its minutes (`+01`).
fn parse_text(kind: Kind, fname: &str, s: &str) -> Fallible<Value> {
    if s.contains('[') {
        return Err(ZONE_ID_REFUSAL.to_string());
    }
    let parsed = match kind {
        Kind::Date => iso8601_to_date(s).map(Value::Date),
        Kind::LocalDateTime => iso8601_to_local_datetime(s).map(Value::LocalDateTime),
        Kind::DateTime => {
            let text = with_extended_offset(s);
            match iso8601_to_datetime(&text) {
                Some((micros_utc, offset_seconds)) => Some(Value::DateTime {
                    micros_utc,
                    offset_seconds,
                }),
                None => iso8601_to_local_datetime(&text).map(|micros_utc| Value::DateTime {
                    micros_utc,
                    offset_seconds: 0,
                }),
            }
        }
    };
    parsed.ok_or_else(|| {
        fail(
            fname,
            format!("cannot read '{s}' as a {}", kind.type_name()),
        )
    })
}

/// `s` with a trailing basic-format offset (`+0100`, `+01`) rewritten to the
/// extended form the ISO 8601 parser reads (`+01:00`).
fn with_extended_offset(s: &str) -> String {
    if let Some(t) = s.find('T') {
        let tail = &s[t..];
        if let Some(i) = tail.rfind(['+', '-']) {
            if let Some(seconds) = offset_seconds_of(&tail[i..]) {
                return format!("{}{}", &s[..t + i], offset_text(seconds));
            }
        }
    }
    s.to_string()
}

/// `Z`, or `±HH`, `±HHMM`, `±HH:MM`, `±HHMMSS`, `±HH:MM:SS` within ±18:00.
fn offset_seconds_of(s: &str) -> Option<i32> {
    if s == "Z" {
        return Some(0);
    }
    let (negative, body) = match s.as_bytes().first()? {
        b'+' => (false, &s[1..]),
        b'-' => (true, &s[1..]),
        _ => return None,
    };
    let groups: Vec<&str> = if body.contains(':') {
        body.split(':').collect()
    } else if body.len() % 2 == 0 {
        (0..body.len())
            .step_by(2)
            .map(|i| body.get(i..i + 2).unwrap_or(""))
            .collect()
    } else {
        return None;
    };
    if !(1..=3).contains(&groups.len())
        || groups
            .iter()
            .any(|g| g.len() != 2 || !g.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    let part = |i: usize| -> i64 { groups.get(i).map_or(0, |g| g.parse().unwrap_or(99)) };
    let (h, m, sec) = (part(0), part(1), part(2));
    let total = h * 3600 + m * 60 + sec;
    if m > 59 || sec > 59 || total > MAX_OFFSET_SECONDS {
        return None;
    }
    let total = i32::try_from(total).ok()?;
    Some(if negative { -total } else { total })
}

fn offset_text(seconds: i32) -> String {
    let sign = if seconds < 0 { '-' } else { '+' };
    let abs = seconds.unsigned_abs();
    let (h, m, s) = (abs / 3600, abs / 60 % 60, abs % 60);
    if s == 0 {
        format!("{sign}{h:02}:{m:02}")
    } else {
        format!("{sign}{h:02}:{m:02}:{s:02}")
    }
}

/// A `timezone` value: an offset string. A named zone is refused by name.
fn parse_timezone(fname: &str, v: &Value) -> Fallible<i32> {
    let Value::Str(s) = v else {
        return Err(fail(
            fname,
            format!("`timezone` must be a string, got {}", type_name(v)),
        ));
    };
    if let Some(seconds) = offset_seconds_of(s) {
        return Ok(seconds);
    }
    if s.chars().any(|c| c.is_ascii_alphabetic()) {
        return Err(ZONE_ID_REFUSAL.to_string());
    }
    Err(fail(
        fname,
        format!("`timezone` '{s}' is not an offset within ±18:00"),
    ))
}

/// An integer field of a constructor map: `Ok(None)` if absent.
fn int_field(fname: &str, m: &BTreeMap<String, Value>, key: &str) -> Fallible<Option<i64>> {
    match m.get(key) {
        None => Ok(None),
        Some(Value::Int(n)) => Ok(Some(*n)),
        Some(other) => Err(fail(
            fname,
            format!("`{key}` must be an integer, got {}", type_name(other)),
        )),
    }
}

fn check_range(fname: &str, key: &str, v: i64, range: RangeInclusive<i64>) -> Fallible<i64> {
    if range.contains(&v) {
        Ok(v)
    } else {
        Err(fail(
            fname,
            format!("`{key}` {v} is outside {}..={}", range.start(), range.end()),
        ))
    }
}

const DATE_FIELDS: [&str; 8] = [
    "year",
    "month",
    "day",
    "week",
    "dayOfWeek",
    "quarter",
    "dayOfQuarter",
    "ordinalDay",
];
const TIME_FIELDS: [&str; 6] = [
    "hour",
    "minute",
    "second",
    "millisecond",
    "microsecond",
    "nanosecond",
];
const EPOCH_FIELDS: [&str; 2] = ["epochSeconds", "epochMillis"];

/// Whether a `kind` map takes `key`. A truncation's map has no selecting
/// keys: the truncated value is its base.
fn takes_field(kind: Kind, key: &str, truncating: bool) -> bool {
    DATE_FIELDS.contains(&key)
        || (!truncating && key == "date")
        || (kind.has_time()
            && (TIME_FIELDS.contains(&key)
                || (!truncating && (key == "time" || key == "datetime"))))
        || (kind == Kind::DateTime
            && (key == "timezone" || (!truncating && EPOCH_FIELDS.contains(&key))))
}

/// A map constructor, or (with `truncated`) a truncation's field overrides.
///
/// The base is the value a `date`, `time` or `datetime` key selects (or the
/// truncated value); fields override it. Without a base, `year` is required
/// and a field needs the one above it (`day` needs `month`). A `datetime`
/// base given a `timezone` keeps its instant, a `time` base its time of day
/// on the new clock; a local base, and a truncation, keep their wall clock.
fn from_map(
    kind: Kind,
    fname: &str,
    m: &BTreeMap<String, Value>,
    truncated: Option<Parts>,
) -> Fallible<Option<Value>> {
    let truncating = truncated.is_some();
    // `{timezone: …}` alone is the current instant on that clock.
    if !truncating && m.len() == 1 {
        if let Some(tz) = m.get("timezone") {
            if matches!(tz, Value::Null) {
                return Ok(None);
            }
            return now(kind, parse_timezone(fname, tz)?, Scope::Statement).map(Some);
        }
    }
    if let Some(key) = m.keys().find(|k| !takes_field(kind, k, truncating)) {
        return Err(fail(fname, format!("unknown field `{key}`")));
    }
    if m.values().any(|v| matches!(v, Value::Null)) {
        return Ok(None);
    }
    let tz = m
        .get("timezone")
        .map(|v| parse_timezone(fname, v))
        .transpose()?;
    if EPOCH_FIELDS.iter().any(|k| m.contains_key(*k)) {
        return from_epoch_fields(fname, m, tz).map(Some);
    }
    let (date_base, time_base) = match truncated {
        Some(p) => (Some(p.days), Some((p.nanos, p.offset))),
        None => selected_bases(fname, m, tz)?,
    };
    let days = resolve_date(fname, m, date_base)?;
    let (nanos, base_offset) = if kind.has_time() {
        let nanos = resolve_time(fname, m, time_base.map(|(n, _)| n))?;
        (nanos, time_base.and_then(|(_, o)| o))
    } else {
        (0, None)
    };
    Parts {
        days,
        nanos,
        offset: tz.or(base_offset),
    }
    .into_value(kind, fname)
    .map(Some)
}

/// A time base: the time of day in nanoseconds, and the offset it has.
type TimeBase = (i64, Option<i32>);

/// The date base and the time base a constructor map's `datetime`, `date`
/// and `time` keys select.
fn selected_bases(
    fname: &str,
    m: &BTreeMap<String, Value>,
    tz: Option<i32>,
) -> Fallible<(Option<i64>, Option<TimeBase>)> {
    let temporal = |key: &str, v: &Value| -> Fallible<Parts> {
        Parts::of(v).ok_or_else(|| {
            fail(
                fname,
                format!("`{key}` must be a temporal value, got {}", type_name(v)),
            )
        })
    };
    if let Some(v) = m.get("datetime") {
        if let Some(other) = ["date", "time"].into_iter().find(|k| m.contains_key(*k)) {
            return Err(fail(
                fname,
                format!("`datetime` cannot be combined with `{other}`"),
            ));
        }
        let mut p = temporal("datetime", v)?;
        if let (Some(tz), Some(o)) = (tz, p.offset) {
            let shift = i128::from(tz - o) * i128::from(MICROS_PER_SECOND);
            p = Parts::local(p.local_micros() + shift, Some(tz));
        }
        return Ok((Some(p.days), Some((p.nanos, p.offset))));
    }
    let date_base = match m.get("date") {
        Some(v) => Some(temporal("date", v)?.days),
        None => None,
    };
    let time_base = match m.get("time") {
        None => None,
        Some(v @ (Value::LocalDateTime(_) | Value::DateTime { .. })) => {
            let mut p = temporal("time", v)?;
            if let (Some(tz), Some(o)) = (tz, p.offset) {
                p.nanos =
                    (p.nanos + i64::from(tz - o) * NANOS_PER_SECOND).rem_euclid(NANOS_PER_DAY);
                p.offset = Some(tz);
            }
            Some((p.nanos, p.offset))
        }
        Some(v) => {
            return Err(fail(
                fname,
                format!(
                    "`time` must be a value with a time of day, got {}",
                    type_name(v)
                ),
            ))
        }
    };
    Ok((date_base, time_base))
}

/// `datetime({epochSeconds | epochMillis, [sub-second fields], [timezone]})`.
fn from_epoch_fields(fname: &str, m: &BTreeMap<String, Value>, tz: Option<i32>) -> Fallible<Value> {
    let epoch = if m.contains_key("epochSeconds") {
        "epochSeconds"
    } else {
        "epochMillis"
    };
    const WITH_EPOCH: [&str; 4] = ["millisecond", "microsecond", "nanosecond", "timezone"];
    if let Some(other) = m
        .keys()
        .find(|k| k.as_str() != epoch && !WITH_EPOCH.contains(&k.as_str()))
    {
        return Err(fail(
            fname,
            format!("`{epoch}` cannot be combined with `{other}`"),
        ));
    }
    let base = int_field(fname, m, epoch)?.unwrap_or(0);
    let scale = if epoch == "epochSeconds" {
        MICROS_PER_SECOND
    } else {
        1_000
    };
    let mut nanos = 0i128;
    for (key, per) in [
        ("millisecond", 1_000_000i128),
        ("microsecond", 1_000),
        ("nanosecond", 1),
    ] {
        nanos += i128::from(int_field(fname, m, key)?.unwrap_or(0)) * per;
    }
    let utc = i128::from(base) * i128::from(scale) + nanos.div_euclid(i128::from(NANOS_PER_MICRO));
    let offset = tz.unwrap_or(0);
    Parts::local(
        utc + i128::from(offset) * i128::from(MICROS_PER_SECOND),
        Some(offset),
    )
    .into_value(Kind::DateTime, fname)
}

/// Which way a map names its date: calendar (`month`/`day`), ISO week
/// (`week`/`dayOfWeek`), quarter (`quarter`/`dayOfQuarter`) or ordinal day.
const DATE_GROUPS: [[&str; 2]; 4] = [
    ["month", "day"],
    ["week", "dayOfWeek"],
    ["quarter", "dayOfQuarter"],
    ["ordinalDay", "ordinalDay"],
];

/// The day a map's date fields name, over `base` (days since the epoch).
fn resolve_date(fname: &str, m: &BTreeMap<String, Value>, base: Option<i64>) -> Fallible<i64> {
    let field = |key: &str| int_field(fname, m, key);
    for key in DATE_FIELDS {
        field(key)?;
    }
    let present: Vec<(usize, &str)> = DATE_GROUPS
        .iter()
        .enumerate()
        .filter_map(|(g, keys)| keys.iter().find(|k| m.contains_key(**k)).map(|k| (g, *k)))
        .collect();
    if let [(_, first), (_, second), ..] = present.as_slice() {
        return Err(fail(
            fname,
            format!("`{second}` cannot be combined with `{first}`"),
        ));
    }
    let year = field("year")?;
    if base.is_none() && year.is_none() {
        return Err(fail(fname, "`year` is required"));
    }
    if year.is_some_and(|y| y.unsigned_abs() > MAX_YEAR) {
        return Err(fail(fname, "the date is out of range"));
    }
    let needs = |lower: &str, upper: &str| -> Fallible<()> {
        if base.is_none() && m.contains_key(lower) && !m.contains_key(upper) {
            return Err(fail(fname, format!("`{lower}` needs `{upper}`")));
        }
        Ok(())
    };
    let base_civil = base.map(civil_from_days);
    match present.first().map(|(g, _)| *g) {
        None | Some(0) => {
            needs("day", "month")?;
            let (by, bm, bd) = base_civil.unwrap_or((year.unwrap_or(0), 1, 1));
            let y = year.unwrap_or(by);
            let month = check_range(
                fname,
                "month",
                field("month")?.unwrap_or(i64::from(bm)),
                1..=12,
            )?;
            let max_day = i64::from(days_in_month(y, month as u32));
            let day = check_range(
                fname,
                "day",
                field("day")?.unwrap_or(i64::from(bd)),
                1..=max_day,
            )?;
            Ok(days_from_civil(y, month as u32, day as u32))
        }
        Some(1) => {
            needs("dayOfWeek", "week")?;
            let (bwy, bw, bdow) = match base {
                Some(d) => iso_week(d),
                None => (year.unwrap_or(0), 1, 1),
            };
            let wy = year.unwrap_or(bwy);
            let max_week = weeks_in_week_year(wy);
            let week = check_range(fname, "week", field("week")?.unwrap_or(bw), 1..=max_week)?;
            let dow = check_range(
                fname,
                "dayOfWeek",
                field("dayOfWeek")?.unwrap_or(bdow),
                1..=7,
            )?;
            Ok(week_year_start(wy) + (week - 1) * 7 + (dow - 1))
        }
        Some(2) => {
            needs("dayOfQuarter", "quarter")?;
            let (bq, bdoq, by) = match (base, base_civil) {
                (Some(d), Some((y, month, _))) => {
                    let q = i64::from((month - 1) / 3 + 1);
                    (q, d - quarter_start(y, q) + 1, y)
                }
                _ => (1, 1, year.unwrap_or(0)),
            };
            let y = year.unwrap_or(by);
            let q = check_range(fname, "quarter", field("quarter")?.unwrap_or(bq), 1..=4)?;
            let start = quarter_start(y, q);
            let len = if q == 4 {
                days_from_civil(y + 1, 1, 1) - start
            } else {
                quarter_start(y, q + 1) - start
            };
            let doq = check_range(
                fname,
                "dayOfQuarter",
                field("dayOfQuarter")?.unwrap_or(bdoq),
                1..=len,
            )?;
            Ok(start + doq - 1)
        }
        Some(_) => {
            let y = year.unwrap_or_else(|| base_civil.map_or(0, |(y, _, _)| y));
            let len = if is_leap_year(y) { 366 } else { 365 };
            let ordinal = check_range(
                fname,
                "ordinalDay",
                field("ordinalDay")?.unwrap_or(1),
                1..=len,
            )?;
            Ok(days_from_civil(y, 1, 1) + ordinal - 1)
        }
    }
}

/// The time of day (nanoseconds) a map's time fields name, over `base`.
///
/// `millisecond`, `microsecond` and `nanosecond` are parts of one fraction:
/// each is 0–999 under a larger one given beside it, and otherwise spans the
/// whole fraction (`microsecond: 645876`, `nanosecond: 645876123`). Over a
/// base, a part not given keeps the base's digits.
fn resolve_time(fname: &str, m: &BTreeMap<String, Value>, base: Option<i64>) -> Fallible<i64> {
    let field = |key: &str| int_field(fname, m, key);
    if base.is_none() {
        for (lower, upper) in [("minute", "hour"), ("second", "minute")] {
            if m.contains_key(lower) && !m.contains_key(upper) {
                return Err(fail(fname, format!("`{lower}` needs `{upper}`")));
            }
        }
        if let Some(sub) = ["millisecond", "microsecond", "nanosecond"]
            .into_iter()
            .find(|k| m.contains_key(*k))
        {
            if !m.contains_key("second") {
                return Err(fail(fname, format!("`{sub}` needs `second`")));
            }
        }
    }
    let base = base.unwrap_or(0);
    let base_seconds = base / NANOS_PER_SECOND;
    let base_fraction = base % NANOS_PER_SECOND;
    let hour = check_range(
        fname,
        "hour",
        field("hour")?.unwrap_or(base_seconds / 3600),
        0..=23,
    )?;
    let minute = check_range(
        fname,
        "minute",
        field("minute")?.unwrap_or(base_seconds / 60 % 60),
        0..=59,
    )?;
    let second = check_range(
        fname,
        "second",
        field("second")?.unwrap_or(base_seconds % 60),
        0..=59,
    )?;
    let (milli, micro, nano) = (
        field("millisecond")?,
        field("microsecond")?,
        field("nanosecond")?,
    );
    let fraction = if milli.is_none() && micro.is_none() && nano.is_none() {
        base_fraction
    } else {
        let milli = match milli {
            Some(v) => check_range(fname, "millisecond", v, 0..=999)?,
            None => base_fraction / 1_000_000,
        };
        let micro_max = if m.contains_key("millisecond") {
            999
        } else {
            999_999
        };
        let micro = match micro {
            Some(v) => check_range(fname, "microsecond", v, 0..=micro_max)?,
            None => base_fraction / 1_000 % 1_000,
        };
        let nano_max = if m.contains_key("microsecond") {
            999
        } else if m.contains_key("millisecond") {
            999_999
        } else {
            999_999_999
        };
        let nano = match nano {
            Some(v) => check_range(fname, "nanosecond", v, 0..=nano_max)?,
            None => base_fraction % 1_000,
        };
        let fraction = milli * 1_000_000 + micro * 1_000 + nano;
        if fraction >= NANOS_PER_SECOND {
            return Err(fail(
                fname,
                "the sub-second fields add up to a second or more",
            ));
        }
        fraction
    };
    Ok((hour * 3600 + minute * 60 + second) * NANOS_PER_SECOND + fraction)
}

fn duration_from(arg: Value) -> Fallible<Option<Value>> {
    match arg {
        Value::Str(s) => iso8601_to_duration(&s)
            .map(|(months, days, nanos)| {
                Some(Value::Duration {
                    months,
                    days,
                    nanos,
                })
            })
            .ok_or_else(|| fail("duration", format!("cannot read '{s}' as a Duration"))),
        Value::Map(m) => duration_from_map(&m),
        Value::Duration { .. } => Ok(Some(arg)),
        other => Err(format!(
            "duration() expects a string or a map, got {}",
            type_name(&other)
        )),
    }
}

/// One tier of a duration (months, days or nanoseconds): a whole count and
/// the fraction carried down from fractional fields.
#[derive(Default)]
struct Tier {
    whole: i128,
    fraction: f64,
}

impl Tier {
    /// Add `n` of a unit worth `per` of this tier. `Err`: not finite, or far
    /// past any field's range.
    fn add(&mut self, n: &Value, per: i128) -> Result<(), ()> {
        match n {
            Value::Int(i) => self.whole += i128::from(*i) * per,
            Value::Float(f) => self.add_float(f * per as f64)?,
            _ => return Err(()),
        }
        Ok(())
    }

    fn add_float(&mut self, x: f64) -> Result<(), ()> {
        if !x.is_finite() || x.abs() > 1e30 {
            return Err(());
        }
        let whole = x.trunc();
        self.whole += whole as i128;
        self.fraction += x - whole;
        Ok(())
    }
}

/// `duration({…})`. Every field may be fractional; a fraction carries down,
/// a month as the mean Gregorian month (30.436875 days) and a day as 24 hours.
fn duration_from_map(m: &BTreeMap<String, Value>) -> Fallible<Option<Value>> {
    const KEYS: [(&str, usize, i128); 10] = [
        ("years", 0, 12),
        ("months", 0, 1),
        ("weeks", 1, 7),
        ("days", 1, 1),
        ("hours", 2, 3_600 * NANOS_PER_SECOND as i128),
        ("minutes", 2, 60 * NANOS_PER_SECOND as i128),
        ("seconds", 2, NANOS_PER_SECOND as i128),
        ("milliseconds", 2, 1_000_000),
        ("microseconds", 2, 1_000),
        ("nanoseconds", 2, 1),
    ];
    if let Some(key) = m.keys().find(|k| !KEYS.iter().any(|(n, _, _)| n == k)) {
        return Err(fail("duration", format!("unknown field `{key}`")));
    }
    if m.values().any(|v| matches!(v, Value::Null)) {
        return Ok(None);
    }
    let out_of_range = || fail("duration", "the duration is out of range");
    let mut tiers: [Tier; 3] = Default::default();
    for (key, tier, per) in KEYS {
        if let Some(v) = m.get(key) {
            if !matches!(v, Value::Int(_) | Value::Float(_)) {
                return Err(fail(
                    "duration",
                    format!("`{key}` must be a number, got {}", type_name(v)),
                ));
            }
            tiers[tier].add(v, per).map_err(|_| out_of_range())?;
        }
    }
    let month_fraction = tiers[0].fraction;
    tiers[1]
        .add_float(month_fraction * DAYS_PER_MEAN_MONTH)
        .map_err(|_| out_of_range())?;
    let day_fraction = tiers[1].fraction;
    tiers[2]
        .add_float(day_fraction * NANOS_PER_DAY as f64)
        .map_err(|_| out_of_range())?;
    let nanos = tiers[2].whole + tiers[2].fraction.round() as i128;
    Ok(Some(Value::Duration {
        months: i32::try_from(tiers[0].whole).map_err(|_| out_of_range())?,
        days: i32::try_from(tiers[1].whole).map_err(|_| out_of_range())?,
        nanos: i64::try_from(nanos).map_err(|_| out_of_range())?,
    }))
}

// ---------------------------------------------------------------------------
// Namespaced functions
// ---------------------------------------------------------------------------

/// The namespaced temporal functions: canonical name and argument counts.
const NAMESPACED: [(&str, usize, usize); 18] = [
    ("date.truncate", 2, 3),
    ("localdatetime.truncate", 2, 3),
    ("datetime.truncate", 2, 3),
    ("date.statement", 0, 1),
    ("date.transaction", 0, 1),
    ("date.realtime", 0, 1),
    ("localdatetime.statement", 0, 1),
    ("localdatetime.transaction", 0, 1),
    ("localdatetime.realtime", 0, 1),
    ("datetime.statement", 0, 1),
    ("datetime.transaction", 0, 1),
    ("datetime.realtime", 0, 1),
    ("datetime.fromepoch", 2, 2),
    ("datetime.fromepochmillis", 1, 1),
    ("duration.between", 2, 2),
    ("duration.inMonths", 2, 2),
    ("duration.inDays", 2, 2),
    ("duration.inSeconds", 2, 2),
];

/// The canonical names of the namespaced temporal functions.
pub fn namespaced_names() -> impl Iterator<Item = &'static str> {
    NAMESPACED.iter().map(|(n, _, _)| *n)
}

/// A namespaced temporal function's canonical name and argument counts, for a
/// name in any case.
pub fn namespaced(name: &str) -> Option<(&'static str, RangeInclusive<usize>)> {
    NAMESPACED
        .iter()
        .find(|(n, _, _)| n.eq_ignore_ascii_case(name))
        .map(|(n, lo, hi)| (*n, *lo..=*hi))
}

/// Whether `norm` (lower case) is in the `time` or `localtime` namespace,
/// which 0.10 refuses with [`TIME_TYPES_REFUSAL`].
pub fn is_time_type_namespace(norm: &str) -> bool {
    norm.starts_with("time.") || norm.starts_with("localtime.")
}

/// Call the namespaced function `canonical` (from [`namespaced`]) on its
/// first `n` resolved arguments; a null argument is null.
pub fn call_namespaced(
    canonical: &str,
    args: [Option<Value>; 3],
    n: usize,
) -> Fallible<Option<Value>> {
    if args.iter().take(n).any(Option::is_none) {
        return Ok(None);
    }
    let [a, b, c] = args;
    let (a, b) = (a.unwrap_or(Value::Null), b.unwrap_or(Value::Null));
    let namespace = canonical.split('.').next().unwrap_or(canonical);
    match canonical {
        "date.truncate" | "localdatetime.truncate" | "datetime.truncate" => {
            let kind = Kind::of(namespace).unwrap_or(Kind::Date);
            truncate(canonical, kind, a, b, c)
        }
        "datetime.fromepoch" => from_epoch(canonical, a, b, NANOS_PER_SECOND).map(Some),
        "datetime.fromepochmillis" => from_epoch(canonical, a, Value::Int(0), 1_000_000).map(Some),
        "duration.between" | "duration.inMonths" | "duration.inDays" | "duration.inSeconds" => {
            between(canonical, a, b).map(Some)
        }
        "date.statement"
        | "date.transaction"
        | "date.realtime"
        | "localdatetime.statement"
        | "localdatetime.transaction"
        | "localdatetime.realtime"
        | "datetime.statement"
        | "datetime.transaction"
        | "datetime.realtime" => {
            let kind = Kind::of(namespace).unwrap_or(Kind::Date);
            let scope = match canonical.rsplit('.').next() {
                Some("transaction") => Scope::Transaction,
                Some("realtime") => Scope::Realtime,
                _ => Scope::Statement,
            };
            clock(canonical, kind, scope, (n == 1).then_some(a)).map(Some)
        }
        // `namespaced` lists every name above; a name it does not list is
        // never dispatched here, and must not read the clock if it is.
        other => Err(format!("unknown function `{other}`")),
    }
}

/// `<type>.statement/transaction/realtime([timezone])`: the current instant,
/// as `<type>()` gives it, on the clock of `timezone`: the statement's instant,
/// the transaction's, or (`realtime`) the clock itself.
fn clock(fname: &str, kind: Kind, scope: Scope, arg: Option<Value>) -> Fallible<Value> {
    let offset = match arg {
        None => 0,
        Some(Value::Map(m)) if m.len() == 1 && m.contains_key("timezone") => {
            parse_timezone(fname, &m["timezone"])?
        }
        Some(v @ Value::Str(_)) => parse_timezone(fname, &v)?,
        Some(other) => {
            return Err(format!(
                "{fname}() expects a timezone string or map, got {}",
                type_name(&other)
            ))
        }
    };
    now(kind, offset, scope)
}

/// `datetime.fromepoch(seconds, nanoseconds)` (`nanos_per_unit` 10^9) and
/// `datetime.fromepochmillis(milliseconds)` (10^6): a UTC `DateTime`.
fn from_epoch(fname: &str, a: Value, b: Value, nanos_per_unit: i64) -> Fallible<Value> {
    let (Value::Int(count), Value::Int(extra_nanos)) = (a, b) else {
        return Err(if nanos_per_unit == NANOS_PER_SECOND {
            format!("{fname}() expects integers")
        } else {
            format!("{fname}() expects an integer")
        });
    };
    let nanos = i128::from(count) * i128::from(nanos_per_unit) + i128::from(extra_nanos);
    Parts::local(nanos.div_euclid(i128::from(NANOS_PER_MICRO)), Some(0))
        .into_value(Kind::DateTime, fname)
}

/// `<type>.truncate(unit, value[, fields])`: `value` cut down to `unit` on
/// its own wall clock (a `DateTime` keeps its offset), then `fields` set the
/// parts below the unit, as a map constructor's fields override its base.
fn truncate(
    fname: &str,
    kind: Kind,
    unit: Value,
    value: Value,
    fields: Option<Value>,
) -> Fallible<Option<Value>> {
    let Value::Str(unit) = unit else {
        return Err(format!(
            "{fname}() expects a unit string, got {}",
            type_name(&unit)
        ));
    };
    let Some(p) = Parts::of(&value) else {
        return Err(format!(
            "{fname}() expects a temporal value, got {}",
            type_name(&value)
        ));
    };
    let fields = match fields {
        None => BTreeMap::new(),
        Some(Value::Map(m)) => m,
        Some(other) => {
            return Err(format!(
                "{fname}() expects a map of fields, got {}",
                type_name(&other)
            ))
        }
    };
    let u = unit.to_ascii_lowercase();
    let time_unit = match u.as_str() {
        "millennium" | "century" | "decade" | "year" | "weekyear" | "quarter" | "month"
        | "week" | "day" => false,
        "hour" | "minute" | "second" | "millisecond" | "microsecond" | "nanosecond" => true,
        _ => return Err(fail(fname, format!("unknown unit '{unit}'"))),
    };
    if time_unit && (kind == Kind::Date || matches!(value, Value::Date(_))) {
        return Err(fail(fname, format!("cannot truncate a Date to `{unit}`")));
    }
    let (y, m, _) = civil_from_days(p.days);
    let days = match u.as_str() {
        "millennium" => days_from_civil(y.div_euclid(1000) * 1000, 1, 1),
        "century" => days_from_civil(y.div_euclid(100) * 100, 1, 1),
        "decade" => days_from_civil(y.div_euclid(10) * 10, 1, 1),
        "year" => days_from_civil(y, 1, 1),
        "weekyear" => week_year_start(iso_week(p.days).0),
        "quarter" => days_from_civil(y, (m - 1) / 3 * 3 + 1, 1),
        "month" => days_from_civil(y, m, 1),
        "week" => p.days - (day_of_week(p.days) - 1),
        _ => p.days,
    };
    let step = match u.as_str() {
        "hour" => 3_600 * NANOS_PER_SECOND,
        "minute" => 60 * NANOS_PER_SECOND,
        "second" => NANOS_PER_SECOND,
        "millisecond" => 1_000_000,
        "microsecond" => 1_000,
        "nanosecond" => 1,
        _ => NANOS_PER_DAY,
    };
    let truncated = Parts {
        days,
        nanos: p.nanos - p.nanos % step,
        offset: p.offset,
    };
    from_map(kind, fname, &fields, Some(truncated))
}

/// `duration.between(a, b)` and `duration.inMonths/inDays/inSeconds(a, b)`.
///
/// Two values with offsets are compared as instants (`b` moved onto `a`'s
/// clock); otherwise each is read on its own wall clock, a date at midnight.
/// Months, then days, count only whole units from `a` towards `b` (a part day
/// or month does not count), and the rest is the time.
fn between(fname: &str, a: Value, b: Value) -> Fallible<Value> {
    let parts = |v: &Value| {
        Parts::of(v)
            .ok_or_else(|| format!("{fname}() expects temporal values, got {}", type_name(v)))
    };
    let (pa, pb) = (parts(&a)?, parts(&b)?);
    let from = pa.local_micros();
    let mut to = pb.local_micros();
    if let (Some(oa), Some(ob)) = (pa.offset, pb.offset) {
        to += i128::from(oa - ob) * i128::from(MICROS_PER_SECOND);
    }
    let out_of_range = || fail(fname, "the duration is out of range");
    let (months, days, micros) = match fname {
        "duration.between" => {
            let months = until(from, to, Unit::Months);
            let moved = i64::try_from(from)
                .ok()
                .zip(i32::try_from(months).ok())
                .and_then(|(f, mo)| shift_micros(f, mo, 0, 0))
                .ok_or_else(out_of_range)?;
            let moved = i128::from(moved);
            let days = until(moved, to, Unit::Days);
            (months, days, to - moved - days * i128::from(MICROS_PER_DAY))
        }
        "duration.inMonths" => (until(from, to, Unit::Months), 0, 0),
        "duration.inDays" => (0, until(from, to, Unit::Days), 0),
        _ => (0, 0, to - from),
    };
    Ok(Value::Duration {
        months: i32::try_from(months).map_err(|_| out_of_range())?,
        days: i32::try_from(days).map_err(|_| out_of_range())?,
        nanos: i64::try_from(micros * i128::from(NANOS_PER_MICRO)).map_err(|_| out_of_range())?,
    })
}

enum Unit {
    Months,
    Days,
}

/// Whole months or days from local `from` to local `to` (microseconds),
/// truncated toward zero: the end day steps back (or forward) one when its
/// time of day has not yet reached `from`'s.
fn until(from: i128, to: i128, unit: Unit) -> i128 {
    let per_day = i128::from(MICROS_PER_DAY);
    let (from_day, from_time) = (from.div_euclid(per_day), from.rem_euclid(per_day));
    let (to_day, to_time) = (to.div_euclid(per_day), to.rem_euclid(per_day));
    let end = if to_day > from_day && to_time < from_time {
        to_day - 1
    } else if to_day < from_day && to_time > from_time {
        to_day + 1
    } else {
        to_day
    };
    match unit {
        Unit::Days => end - from_day,
        Unit::Months => {
            let packed = |day: i128| {
                let (y, m, d) = civil_from_days(day as i64);
                (i128::from(y) * 12 + i128::from(m) - 1) * 32 + i128::from(d)
            };
            (packed(end) - packed(from_day)) / 32
        }
    }
}

// ---------------------------------------------------------------------------
// ISO weeks and quarters
// ---------------------------------------------------------------------------

/// ISO day of the week, Monday 1 to Sunday 7 (1970-01-01 was a Thursday).
fn day_of_week(days: i64) -> i64 {
    (days + 3).rem_euclid(7) + 1
}

/// The Monday that starts ISO week-year `year`: the week holding 4 January.
fn week_year_start(year: i64) -> i64 {
    let jan4 = days_from_civil(year, 1, 4);
    jan4 - (day_of_week(jan4) - 1)
}

fn weeks_in_week_year(year: i64) -> i64 {
    (week_year_start(year + 1) - week_year_start(year)) / 7
}

/// `(week-year, week, day of week)` of a day.
fn iso_week(days: i64) -> (i64, i64, i64) {
    let (year, _, _) = civil_from_days(days);
    let week_year = if days >= week_year_start(year + 1) {
        year + 1
    } else if days < week_year_start(year) {
        year - 1
    } else {
        year
    };
    (
        week_year,
        (days - week_year_start(week_year)) / 7 + 1,
        day_of_week(days),
    )
}

/// The first day of quarter `q` (1–4) of `year`.
fn quarter_start(year: i64, q: i64) -> i64 {
    days_from_civil(year, (q as u32 - 1) * 3 + 1, 1)
}

// ---------------------------------------------------------------------------
// toString
// ---------------------------------------------------------------------------

/// The ISO 8601 text of a temporal value, or `None` for any other value.
pub fn to_text(v: &Value) -> Option<String> {
    match v {
        Value::Date(d) => Some(date_to_iso8601(*d)),
        Value::LocalDateTime(us) => Some(local_datetime_to_iso8601(*us)),
        Value::DateTime {
            micros_utc,
            offset_seconds,
        } => datetime_to_iso8601(*micros_utc, *offset_seconds).ok(),
        Value::Duration {
            months,
            days,
            nanos,
        } => Some(duration_to_iso8601(*months, *days, *nanos)),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Arithmetic
// ---------------------------------------------------------------------------

/// `lhs op rhs` where at least one side is a duration or a temporal value.
///
/// `None`: not a temporal pair, the caller's numeric path applies.
/// `Some(None)`: a temporal pair whose result is out of range, null.
///
/// Supported: temporal `+`/`-` duration, duration `+` temporal, duration
/// `+`/`-` duration, duration `*` number, number `*` duration, duration `/`
/// number. A date takes the calendar part of a duration (months, then days);
/// its time part is dropped. A month added to a day past the end of the target
/// month lands on the month's last day.
pub fn arith(op: &ArithOp, lhs: &Value, rhs: &Value) -> Option<Option<Value>> {
    use Value::{Date, DateTime, Duration, LocalDateTime};
    match (op, lhs, rhs) {
        (
            ArithOp::Add | ArithOp::Sub,
            temporal @ (Date(_) | LocalDateTime(_) | DateTime { .. }),
            Duration {
                months,
                days,
                nanos,
            },
        ) => {
            let sign = if matches!(op, ArithOp::Sub) { -1 } else { 1 };
            Some(shift(temporal, sign, *months, *days, *nanos))
        }
        (
            ArithOp::Add,
            Duration {
                months,
                days,
                nanos,
            },
            temporal @ (Date(_) | LocalDateTime(_) | DateTime { .. }),
        ) => Some(shift(temporal, 1, *months, *days, *nanos)),
        (
            ArithOp::Add | ArithOp::Sub,
            Duration {
                months: m1,
                days: d1,
                nanos: n1,
            },
            Duration {
                months: m2,
                days: d2,
                nanos: n2,
            },
        ) => {
            let sign = if matches!(op, ArithOp::Sub) { -1 } else { 1 };
            Some((|| {
                Some(Duration {
                    months: m1.checked_add(m2.checked_mul(sign)?)?,
                    days: d1.checked_add(d2.checked_mul(sign)?)?,
                    nanos: n1.checked_add(n2.checked_mul(i64::from(sign))?)?,
                })
            })())
        }
        (ArithOp::Mul, dur @ Duration { .. }, factor @ (Value::Int(_) | Value::Float(_)))
        | (ArithOp::Mul, factor @ (Value::Int(_) | Value::Float(_)), dur @ Duration { .. }) => {
            Some(scale(dur, number_of(factor)?))
        }
        (ArithOp::Div, dur @ Duration { .. }, divisor @ (Value::Int(_) | Value::Float(_))) => {
            let d = number_of(divisor)?;
            Some(if d == 0.0 { None } else { scale(dur, 1.0 / d) })
        }
        _ => None,
    }
}

fn number_of(v: &Value) -> Option<f64> {
    match v {
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) => Some(*f),
        _ => None,
    }
}

fn scale(dur: &Value, factor: f64) -> Option<Value> {
    let Value::Duration {
        months,
        days,
        nanos,
    } = dur
    else {
        return None;
    };
    let part = |n: f64| -> Option<f64> {
        let x = n * factor;
        (x.is_finite() && x.abs() < i64::MAX as f64).then(|| x.round())
    };
    Some(Value::Duration {
        months: i32::try_from(part(f64::from(*months))? as i64).ok()?,
        days: i32::try_from(part(f64::from(*days))? as i64).ok()?,
        nanos: part(*nanos as f64)? as i64,
    })
}

/// A temporal value moved by `sign` times a duration. A `DateTime` is shifted
/// on its own wall clock, then keeps its offset.
fn shift(temporal: &Value, sign: i32, months: i32, days: i32, nanos: i64) -> Option<Value> {
    let months = months.checked_mul(sign)?;
    let days = days.checked_mul(sign)?;
    let micros = nanos.checked_mul(i64::from(sign))? / NANOS_PER_MICRO;
    match temporal {
        Value::Date(d) => {
            let moved = shift_days(i64::from(*d), months, days)?;
            Some(Value::Date(i32::try_from(moved).ok()?))
        }
        Value::LocalDateTime(us) => Some(Value::LocalDateTime(shift_micros(
            *us, months, days, micros,
        )?)),
        Value::DateTime {
            micros_utc,
            offset_seconds,
        } => {
            let offset = i64::from(*offset_seconds) * MICROS_PER_SECOND;
            let local = shift_micros(micros_utc.checked_add(offset)?, months, days, micros)?;
            Some(Value::DateTime {
                micros_utc: local.checked_sub(offset)?,
                offset_seconds: *offset_seconds,
            })
        }
        _ => None,
    }
}

fn shift_days(days_since_epoch: i64, months: i32, days: i32) -> Option<i64> {
    let (year, month, day) = civil_from_days(days_since_epoch);
    let total = year
        .checked_mul(12)?
        .checked_add(i64::from(month) - 1)?
        .checked_add(i64::from(months))?;
    let (year, month) = (total.div_euclid(12), (total.rem_euclid(12) + 1) as u32);
    if year.unsigned_abs() > MAX_YEAR {
        return None;
    }
    let day = day.min(days_in_month(year, month));
    days_from_civil(year, month, day).checked_add(i64::from(days))
}

fn shift_micros(local: i64, months: i32, days: i32, micros: i64) -> Option<i64> {
    let day = local.div_euclid(MICROS_PER_DAY);
    let time_of_day = local.rem_euclid(MICROS_PER_DAY);
    shift_days(day, months, days)?
        .checked_mul(MICROS_PER_DAY)?
        .checked_add(time_of_day)?
        .checked_add(micros)
}

// ---------------------------------------------------------------------------
// Proleptic Gregorian calendar (H. Hinnant's civil-day algorithms)
// ---------------------------------------------------------------------------

fn is_leap_year(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        2 if is_leap_year(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = i64::from(month);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}
