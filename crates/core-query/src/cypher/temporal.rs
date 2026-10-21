//! The temporal function surface (QL-13): `date`, `localdatetime`, `datetime`
//! and `duration`, plus the arithmetic between a temporal value and a
//! duration.
//!
//! The value types, their ISO 8601 text and their ordering live in
//! `core-storage` and `value_ops`; this module only builds values from query
//! arguments. An argument that is not a usable input (an unparseable string, a
//! map with an unknown key or an out-of-range field, any other type) gives
//! null, never an error. Only a wrong argument count, and `time()` /
//! `localtime()`, are errors.
//!
//! A temporal value is storable only once the V11 format lands; until then a
//! write carrying one is refused at the write path's choke point, so these
//! functions are free to produce one in a query result.

use super::ast::ArithOp;
use core_storage::types::{
    date_to_iso8601, datetime_to_iso8601, duration_to_iso8601, iso8601_to_date,
    iso8601_to_datetime, iso8601_to_duration, iso8601_to_local_datetime, local_datetime_to_iso8601,
};
use core_storage::Value;
use std::collections::BTreeMap;

const MICROS_PER_SECOND: i64 = 1_000_000;
const MICROS_PER_DAY: i64 = 86_400 * MICROS_PER_SECOND;
const NANOS_PER_MICRO: i64 = 1_000;

/// The error `time()` and `localtime()` give: time-of-day types are not in 0.10.
pub const TIME_TYPES_REFUSAL: &str = "not supported in mushroomdb 0.10: time types";

/// Evaluate a temporal constructor given its already-resolved argument, if
/// any. `arg_count` has been checked by the caller to be 0 or 1.
pub fn construct(norm: &str, arg: Option<Value>, no_arg: bool) -> Option<Value> {
    match norm {
        "date" => {
            if no_arg {
                return Some(Value::Date(now_micros().div_euclid(MICROS_PER_DAY) as i32));
            }
            date_from(arg?)
        }
        "localdatetime" => {
            if no_arg {
                return Some(Value::LocalDateTime(now_micros()));
            }
            local_datetime_from(arg?)
        }
        "datetime" => {
            if no_arg {
                return Some(Value::DateTime {
                    micros_utc: now_micros(),
                    offset_seconds: 0,
                });
            }
            datetime_from(arg?)
        }
        "duration" => {
            if no_arg {
                return None;
            }
            duration_from(arg?)
        }
        _ => None,
    }
}

fn now_micros() -> i64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_micros()).unwrap_or(i64::MAX),
        Err(e) => -i64::try_from(e.duration().as_micros()).unwrap_or(i64::MAX),
    }
}

// ---------------------------------------------------------------------------
// Constructors
// ---------------------------------------------------------------------------

fn date_from(arg: Value) -> Option<Value> {
    match arg {
        Value::Str(s) => iso8601_to_date(&s).map(Value::Date),
        Value::Map(m) => {
            let f = CivilFields::from_map(&m, &["year", "month", "day"])?;
            Some(Value::Date(i32::try_from(f.days()?).ok()?))
        }
        Value::Date(d) => Some(Value::Date(d)),
        Value::LocalDateTime(us) => date_of_micros(us),
        Value::DateTime {
            micros_utc,
            offset_seconds,
        } => date_of_micros(micros_utc.checked_add(i64::from(offset_seconds) * MICROS_PER_SECOND)?),
        _ => None,
    }
}

fn date_of_micros(local_micros: i64) -> Option<Value> {
    Some(Value::Date(
        i32::try_from(local_micros.div_euclid(MICROS_PER_DAY)).ok()?,
    ))
}

fn local_datetime_from(arg: Value) -> Option<Value> {
    match arg {
        Value::Str(s) => iso8601_to_local_datetime(&s).map(Value::LocalDateTime),
        Value::Map(m) => Some(Value::LocalDateTime(micros_from_map(&m)?)),
        Value::Date(d) => Some(Value::LocalDateTime(
            i64::from(d).checked_mul(MICROS_PER_DAY)?,
        )),
        Value::LocalDateTime(us) => Some(Value::LocalDateTime(us)),
        Value::DateTime {
            micros_utc,
            offset_seconds,
        } => Some(Value::LocalDateTime(
            micros_utc.checked_add(i64::from(offset_seconds) * MICROS_PER_SECOND)?,
        )),
        _ => None,
    }
}

/// A string with no offset is read as UTC, the openCypher default zone.
fn datetime_from(arg: Value) -> Option<Value> {
    let utc = |micros_utc| Value::DateTime {
        micros_utc,
        offset_seconds: 0,
    };
    match arg {
        Value::Str(s) => match iso8601_to_datetime(&s) {
            Some((micros_utc, offset_seconds)) => Some(Value::DateTime {
                micros_utc,
                offset_seconds,
            }),
            None => iso8601_to_local_datetime(&s).map(utc),
        },
        Value::Map(m) => Some(utc(micros_from_map(&m)?)),
        Value::Date(d) => Some(utc(i64::from(d).checked_mul(MICROS_PER_DAY)?)),
        Value::LocalDateTime(us) => Some(utc(us)),
        Value::DateTime {
            micros_utc,
            offset_seconds,
        } => Some(Value::DateTime {
            micros_utc,
            offset_seconds,
        }),
        _ => None,
    }
}

fn duration_from(arg: Value) -> Option<Value> {
    match arg {
        Value::Str(s) => iso8601_to_duration(&s).map(|(months, days, nanos)| Value::Duration {
            months,
            days,
            nanos,
        }),
        Value::Map(m) => duration_from_map(&m),
        Value::Duration { .. } => Some(arg),
        _ => None,
    }
}

/// An integer field of a constructor map: `Ok(None)` if absent, `Err` if
/// present and not an integer.
fn int_field(m: &BTreeMap<String, Value>, key: &str) -> Result<Option<i64>, ()> {
    match m.get(key) {
        None => Ok(None),
        Some(Value::Int(n)) => Ok(Some(*n)),
        Some(_) => Err(()),
    }
}

/// `year`/`month`/`day` of a map constructor.
struct CivilFields {
    year: i64,
    month: i64,
    day: i64,
}

impl CivilFields {
    /// `year` is required; `month` and `day` default to 1. A key outside
    /// `allowed` is refused.
    fn from_map(m: &BTreeMap<String, Value>, allowed: &[&str]) -> Option<CivilFields> {
        if m.keys().any(|k| !allowed.contains(&k.as_str())) {
            return None;
        }
        Some(CivilFields {
            year: int_field(m, "year").ok()??,
            month: int_field(m, "month").ok()?.unwrap_or(1),
            day: int_field(m, "day").ok()?.unwrap_or(1),
        })
    }

    fn days(&self) -> Option<i64> {
        if !(1..=12).contains(&self.month) {
            return None;
        }
        let month = self.month as u32;
        if self.day < 1 || self.day > i64::from(days_in_month(self.year, month)) {
            return None;
        }
        Some(days_from_civil(self.year, month, self.day as u32))
    }
}

fn micros_from_map(m: &BTreeMap<String, Value>) -> Option<i64> {
    const KEYS: [&str; 9] = [
        "year",
        "month",
        "day",
        "hour",
        "minute",
        "second",
        "millisecond",
        "microsecond",
        "nanosecond",
    ];
    if m.keys().any(|k| !KEYS.contains(&k.as_str())) {
        return None;
    }
    let days = CivilFields::from_map(m, &KEYS)?.days()?;
    let field = |key: &str, max: i64| -> Option<i64> {
        let n = int_field(m, key).ok()?.unwrap_or(0);
        (0..=max).contains(&n).then_some(n)
    };
    let hour = field("hour", 23)?;
    let minute = field("minute", 59)?;
    let second = field("second", 59)?;
    let milli = field("millisecond", 999)?;
    let micro = field("microsecond", 999)?;
    let nano = field("nanosecond", 999)?;
    // Sub-second fields add up the way openCypher reads them: a millisecond,
    // a microsecond and a nanosecond field are three parts of one fraction.
    let fraction_micros = milli * 1000 + micro + nano / NANOS_PER_MICRO;
    days.checked_mul(MICROS_PER_DAY)?
        .checked_add((hour * 3600 + minute * 60 + second) * MICROS_PER_SECOND + fraction_micros)
}

fn duration_from_map(m: &BTreeMap<String, Value>) -> Option<Value> {
    const KEYS: [&str; 10] = [
        "years",
        "months",
        "weeks",
        "days",
        "hours",
        "minutes",
        "seconds",
        "milliseconds",
        "microseconds",
        "nanoseconds",
    ];
    if m.keys().any(|k| !KEYS.contains(&k.as_str())) {
        return None;
    }
    // Calendar units are whole numbers; the clock units may be fractional,
    // and the fraction is carried down into nanoseconds.
    let whole = |key: &str| -> Option<i64> { Some(int_field(m, key).ok()?.unwrap_or(0)) };
    let clock_nanos = |key: &str, nanos_per_unit: f64| -> Option<i64> {
        match m.get(key) {
            None => Some(0),
            Some(Value::Int(n)) => (*n as i128 * nanos_per_unit as i128).try_into().ok(),
            Some(Value::Float(f)) => {
                let n = f * nanos_per_unit;
                (n.is_finite() && n.abs() < i64::MAX as f64).then(|| n.round() as i64)
            }
            Some(_) => None,
        }
    };
    let months = whole("years")?
        .checked_mul(12)?
        .checked_add(whole("months")?)?;
    let days = whole("weeks")?
        .checked_mul(7)?
        .checked_add(whole("days")?)?;
    let nanos = [
        clock_nanos("hours", 3_600e9)?,
        clock_nanos("minutes", 60e9)?,
        clock_nanos("seconds", 1e9)?,
        clock_nanos("milliseconds", 1e6)?,
        clock_nanos("microseconds", 1e3)?,
        clock_nanos("nanoseconds", 1.0)?,
    ]
    .into_iter()
    .try_fold(0i64, i64::checked_add)?;
    Some(Value::Duration {
        months: i32::try_from(months).ok()?,
        days: i32::try_from(days).ok()?,
        nanos,
    })
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
