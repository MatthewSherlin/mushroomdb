use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A property or query value.
///
/// # Serde tags are a storage format
///
/// bincode encodes a variant by its declaration index, and the WAL, the
/// mixed-column blob and snapshots all store `Value` that way. A variant is
/// therefore only ever **added**, never reordered or removed, and a new storable
/// variant goes **before `Null`**, which is always last. The index of every
/// variant is pinned by `types::tests::value_serde_tags_are_pinned`; changing
/// it means changing that test deliberately.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Value {
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
    List(Vec<Value>),
    /// Nested key-value map. Spills to the Mixed column path like `List`.
    /// Never promotes a homogeneous typed column. Not fulltext-indexed.
    Map(BTreeMap<String, Value>),
    // The five variants below are format V11 types (bincode tags 6–10). They
    // are appended, never inserted, so every older tag keeps its encoding.
    // The first write carrying one moves the store to the V11 snapshot format
    // before the value reaches the WAL (see `v11_only_type`).
    /// Calendar date: days since 1970-01-01. JSON form `{"$date": "2024-01-15"}`.
    Date(i32),
    /// Date and time with no time zone: microseconds since 1970-01-01T00:00:00.
    /// JSON form `{"$ldt": "2024-01-15T12:34:56.789012"}`.
    LocalDateTime(i64),
    /// An instant plus the UTC offset it was written with. The offset is kept
    /// so the value prints back as written; equality and ordering in queries
    /// and in index keys use `micros_utc` only. A valid offset is within
    /// ±18:00 ([`MAX_OFFSET_SECONDS`]).
    /// JSON form `{"$dt": "2024-01-15T12:34:56.789012+05:30"}`.
    DateTime {
        micros_utc: i64,
        offset_seconds: i32,
    },
    /// openCypher duration: months, days and nanoseconds are independent and
    /// never normalized against each other (a month is not 30 days). Not
    /// totally ordered. JSON form `{"$dur": "P1Y2M3DT4H5M6.789S"}`.
    Duration {
        months: i32,
        days: i32,
        nanos: i64,
    },
    /// Byte blob, no encoding assumed. JSON form `{"$binary": "<base64>"}`.
    Binary(Vec<u8>),
    /// Null as an element of a list or a value of a map, produced only by
    /// query evaluation (`[1, null]`, `{a: n.missing}`). A top-level query
    /// null is still `Option<Value>::None`.
    ///
    /// Query-only: it is never stored. The write path refuses a property
    /// value that holds one, and the WAL frame and column encoders refuse it
    /// again as a backstop. It serialises like any unit variant (bincode tag =
    /// its index; serde JSON `"Null"`), so a query result carrying it can be
    /// sent anywhere a `Value` can.
    ///
    /// **Must stay the last variant.** Add new variants above it.
    Null,
}

impl Value {
    /// True if this value is [`Value::Null`] or holds one at any depth. A
    /// write path refuses such a value: a stored property can never be null.
    pub fn contains_null(&self) -> bool {
        match self {
            Value::Null => true,
            Value::List(items) => items.iter().any(Value::contains_null),
            Value::Map(map) => map.values().any(Value::contains_null),
            Value::Int(_)
            | Value::Float(_)
            | Value::Str(_)
            | Value::Bool(_)
            | Value::Date(_)
            | Value::LocalDateTime(_)
            | Value::DateTime { .. }
            | Value::Duration { .. }
            | Value::Binary(_) => false,
        }
    }
}

/// Largest accepted [`Value::Binary`] payload: 1 MiB.
pub const BINARY_MAX_BYTES: usize = 1_048_576;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ValueKey {
    Int(i64),
    FloatBits(u64), // f64::to_bits — equality of representation, fine for index keys
    Str(String),
    Bool(bool),
    // Appended after the scalar keys so their encoding and order are unchanged.
    DateDays(i32),
    LocalDateTimeMicros(i64),
    /// The offset is omitted: two `DateTime`s at the same instant are one key.
    DateTimeMicrosUtc(i64),
    /// Lexicographic byte order.
    BinaryBytes(Vec<u8>),
}

impl ValueKey {
    pub fn from_value(v: &Value) -> Option<ValueKey> {
        match v {
            Value::Int(i) => Some(ValueKey::Int(*i)),
            Value::Float(f) => Some(ValueKey::FloatBits(f.to_bits())),
            Value::Str(s) => Some(ValueKey::Str(s.clone())),
            Value::Bool(b) => Some(ValueKey::Bool(*b)),
            Value::List(_) => None,
            // Maps are composite and cannot be reduced to a single index key.
            Value::Map(_) => None,
            Value::Date(d) => Some(ValueKey::DateDays(*d)),
            Value::LocalDateTime(us) => Some(ValueKey::LocalDateTimeMicros(*us)),
            Value::DateTime { micros_utc, .. } => Some(ValueKey::DateTimeMicrosUtc(*micros_utc)),
            Value::Binary(b) => Some(ValueKey::BinaryBytes(b.clone())),
            // A duration is not totally ordered (a month against 30 days is
            // indeterminate), so it has no index key.
            Value::Duration { .. } => None,
            Value::Null => None,
        }
    }
}

/// The name of the first format-V11 value type inside `v`, if any.
///
/// Recurses into lists and maps: a list of dates is as V11-only as a date.
/// This is what the write path checks so that such a value is appended to the
/// WAL only behind a V11 snapshot, which a 0.9.x reader refuses by name rather
/// than stop replaying at the frame it cannot decode.
pub fn v11_only_type(v: &Value) -> Option<&'static str> {
    match v {
        Value::Int(_) | Value::Float(_) | Value::Str(_) | Value::Bool(_) => None,
        Value::List(items) => items.iter().find_map(v11_only_type),
        Value::Map(m) => m.values().find_map(v11_only_type),
        Value::Date(_) => Some("Date"),
        Value::LocalDateTime(_) => Some("LocalDateTime"),
        Value::DateTime { .. } => Some("DateTime"),
        Value::Duration { .. } => Some("Duration"),
        Value::Binary(_) => Some("Binary"),
        Value::Null => None,
    }
}

/// The refusal a format-V11 value type gets where it still cannot be stored:
/// a role predicate, which persists to `roles.json`, a file the snapshot's
/// version stamp does not cover.
pub fn v11_type_refusal(type_name: &str) -> String {
    format!(
        "{type_name} values cannot be used in a role predicate: roles.json carries no format \
         version, so a 0.9.x reader would report it as corrupt"
    )
}

/// A declared unique constraint: no two live nodes of `label` hold equal
/// values for `field`.
///
/// Equal means equal as an equality-index key ([`ValueKey`]): two `DateTime`s
/// at the same UTC instant are equal whatever their offsets, and a value with
/// no key (a list, a map, a `Duration`) is never in conflict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UniqueConstraint {
    pub name: String,
    pub label: String,
    pub field: String,
}

// ---------------------------------------------------------------------------
// Text forms of the V11 types: ISO 8601 and base64.
//
// All hand-written, std only. Calendar arithmetic is the proleptic Gregorian
// civil-day algorithm over i64, so every representable `Date`, `LocalDateTime`
// and `DateTime` instant has text, and that text parses back to the same value:
// years 0000–9999 print as four digits, every other year in the signed
// expanded form ISO 8601 allows (`-000001`, `+010000`). The one value that has
// no text is a `DateTime` whose offset is outside ±18:00; its printer returns
// an error rather than text that would not parse back.
//
// Accepted input is the ISO 8601 extended format these printers write:
// `YYYY-MM-DD`, `THH:MM[:SS[.fffffffff]]`, and an offset `Z`, `±HH:MM` or
// `±HH:MM:SS`. Basic (`20240115`), week and ordinal forms are refused.
// ---------------------------------------------------------------------------

const MICROS_PER_SECOND: i64 = 1_000_000;
const SECONDS_PER_DAY: i64 = 86_400;
const NANOS_PER_SECOND: i64 = 1_000_000_000;
const NANOS_PER_MINUTE: i64 = 60 * NANOS_PER_SECOND;
const NANOS_PER_HOUR: i64 = 60 * NANOS_PER_MINUTE;

/// The widest UTC offset a `DateTime` may carry: ±18:00, in seconds.
pub const MAX_OFFSET_SECONDS: i32 = 18 * 3600;

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

/// Days since 1970-01-01 of a proleptic Gregorian date (H. Hinnant's
/// `days_from_civil`).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = i64::from(month);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The proleptic Gregorian date of a day count since 1970-01-01 (H. Hinnant's
/// `civil_from_days`).
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

fn format_civil(days: i64) -> String {
    let (year, month, day) = civil_from_days(days);
    let year = if (0..=9999).contains(&year) {
        format!("{year:04}")
    } else if year < 0 {
        format!("-{:06}", year.unsigned_abs())
    } else {
        format!("+{year:06}")
    };
    format!("{year}-{month:02}-{day:02}")
}

/// Exactly `n` ASCII digits at the start of `s`, and the rest.
fn fixed_digits(s: &str, n: usize) -> Option<(u32, &str)> {
    let digits = s.get(..n)?;
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((digits.parse().ok()?, &s[n..]))
}

/// `[±]YYYY…-MM-DD` at the start of `s`: days since the epoch, and the rest.
fn parse_civil(s: &str) -> Option<(i64, &str)> {
    let (sign, rest) = match s.as_bytes().first()? {
        b'+' => (1, &s[1..]),
        b'-' => (-1, &s[1..]),
        _ => (0, s),
    };
    let year_len = rest.bytes().take_while(u8::is_ascii_digit).count();
    // Unsigned years are exactly four digits; signed (expanded) ones at least
    // four. Twelve digits is far past any representable value.
    if (sign == 0 && year_len != 4) || !(4..=12).contains(&year_len) {
        return None;
    }
    let magnitude: i64 = rest[..year_len].parse().ok()?;
    let year = if sign < 0 { -magnitude } else { magnitude };
    let rest = rest[year_len..].strip_prefix('-')?;
    let (month, rest) = fixed_digits(rest, 2)?;
    let rest = rest.strip_prefix('-')?;
    let (day, rest) = fixed_digits(rest, 2)?;
    if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
        return None;
    }
    Some((days_from_civil(year, month, day), rest))
}

/// `HH:MM[:SS[.f…]]` at the start of `s`: nanoseconds since midnight, and the
/// rest.
fn parse_time_of_day(s: &str) -> Option<(i64, &str)> {
    let (hour, rest) = fixed_digits(s, 2)?;
    let rest = rest.strip_prefix(':')?;
    let (minute, mut rest) = fixed_digits(rest, 2)?;
    let mut second = 0;
    let mut frac_nanos = 0i64;
    if let Some(after) = rest.strip_prefix(':') {
        let (sec, after) = fixed_digits(after, 2)?;
        second = sec;
        rest = after;
        if let Some(after) = rest.strip_prefix(['.', ',']) {
            let len = after.bytes().take_while(u8::is_ascii_digit).count();
            if len == 0 || len > 9 {
                return None;
            }
            let digits: i64 = after[..len].parse().ok()?;
            frac_nanos = digits * 10i64.pow(9 - len as u32);
            rest = &after[len..];
        }
    }
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let secs = i64::from(hour) * 3600 + i64::from(minute) * 60 + i64::from(second);
    Some((secs * NANOS_PER_SECOND + frac_nanos, rest))
}

/// `Z`, `±HH:MM` or `±HH:MM:SS`, the whole of `s`, within ±18:00.
fn parse_offset(s: &str) -> Option<i32> {
    if s == "Z" {
        return Some(0);
    }
    let (negative, rest) = match s.as_bytes().first()? {
        b'+' => (false, &s[1..]),
        b'-' => (true, &s[1..]),
        _ => return None,
    };
    let (hours, rest) = fixed_digits(rest, 2)?;
    let rest = rest.strip_prefix(':')?;
    let (minutes, rest) = fixed_digits(rest, 2)?;
    let seconds = match rest.strip_prefix(':') {
        Some(after) => {
            let (secs, after) = fixed_digits(after, 2)?;
            if !after.is_empty() {
                return None;
            }
            secs
        }
        None if rest.is_empty() => 0,
        None => return None,
    };
    if minutes > 59 || seconds > 59 {
        return None;
    }
    let total = i32::try_from(hours * 3600 + minutes * 60 + seconds).ok()?;
    if total > MAX_OFFSET_SECONDS {
        return None;
    }
    Some(if negative { -total } else { total })
}

/// Microseconds since the epoch as `…THH:MM:SS[.ffffff]`. Total over i128, so
/// a local time past the i64 range (a `DateTime` near its limit plus its
/// offset) still prints.
fn format_local_micros(micros: i128) -> String {
    let micros_per_day = i128::from(SECONDS_PER_DAY * MICROS_PER_SECOND);
    let days = micros.div_euclid(micros_per_day) as i64;
    let of_day = micros.rem_euclid(micros_per_day) as i64;
    let sub = of_day % MICROS_PER_SECOND;
    let sod = of_day / MICROS_PER_SECOND;
    let (h, m, s) = (sod / 3600, sod / 60 % 60, sod % 60);
    let date = format_civil(days);
    if sub == 0 {
        format!("{date}T{h:02}:{m:02}:{s:02}")
    } else {
        format!("{date}T{h:02}:{m:02}:{s:02}.{sub:06}")
    }
}

/// `date T time` at the start of `s`: local microseconds since the epoch
/// (sub-microsecond digits truncated toward the earlier instant), and the rest.
fn parse_local_micros(s: &str) -> Option<(i128, &str)> {
    let (days, rest) = parse_civil(s)?;
    let rest = rest.strip_prefix('T')?;
    let (nanos_of_day, rest) = parse_time_of_day(rest)?;
    let micros = i128::from(days) * i128::from(SECONDS_PER_DAY * MICROS_PER_SECOND)
        + i128::from(nanos_of_day.div_euclid(1_000));
    Some((micros, rest))
}

/// Days since 1970-01-01 as an ISO 8601 date, `2024-01-15`.
pub fn date_to_iso8601(days: i32) -> String {
    format_civil(i64::from(days))
}

/// An ISO 8601 date to days since 1970-01-01.
///
/// Refuses anything carrying a time of day: a date is only a date.
pub fn iso8601_to_date(s: &str) -> Option<i32> {
    match parse_civil(s)? {
        (days, "") => i32::try_from(days).ok(),
        _ => None,
    }
}

/// Microseconds since 1970-01-01T00:00:00 (no zone) as an ISO 8601 local
/// date-time, `2024-01-15T12:34:56.789012`. Whole seconds print without a
/// fraction.
pub fn local_datetime_to_iso8601(micros: i64) -> String {
    format_local_micros(i128::from(micros))
}

/// An ISO 8601 date-time with no offset to microseconds since the epoch.
///
/// Refuses input that carries an offset or `Z` — that is a `DateTime`.
/// Sub-microsecond digits are truncated toward the earlier instant.
pub fn iso8601_to_local_datetime(s: &str) -> Option<i64> {
    match parse_local_micros(s)? {
        (micros, "") => i64::try_from(micros).ok(),
        _ => None,
    }
}

/// An instant plus its offset as an ISO 8601 date-time with offset,
/// `2024-01-15T12:34:56.789012+05:30`. UTC prints as `+00:00`; an offset with
/// seconds prints them, `+05:30:15`, and parses back.
///
/// An offset outside ±18:00 is not a valid `DateTime` and has no text: this
/// returns an error rather than something [`iso8601_to_datetime`] refuses.
pub fn datetime_to_iso8601(
    micros_utc: i64,
    offset_seconds: i32,
) -> std::result::Result<String, String> {
    if offset_seconds.unsigned_abs() > MAX_OFFSET_SECONDS.unsigned_abs() {
        return Err(format!(
            "DateTime offset of {offset_seconds} s is outside ±18:00"
        ));
    }
    let local = i128::from(micros_utc) + i128::from(offset_seconds) * i128::from(MICROS_PER_SECOND);
    let sign = if offset_seconds < 0 { '-' } else { '+' };
    let off = offset_seconds.unsigned_abs();
    let (h, m, s) = (off / 3600, off / 60 % 60, off % 60);
    let local = format_local_micros(local);
    Ok(if s == 0 {
        format!("{local}{sign}{h:02}:{m:02}")
    } else {
        format!("{local}{sign}{h:02}:{m:02}:{s:02}")
    })
}

/// An ISO 8601 date-time with an offset (or `Z`) to
/// `(micros_utc, offset_seconds)`.
///
/// Refuses input with no offset — that is a `LocalDateTime` — and offsets
/// outside ±18:00.
pub fn iso8601_to_datetime(s: &str) -> Option<(i64, i32)> {
    let (local, rest) = parse_local_micros(s)?;
    let offset = parse_offset(rest)?;
    let utc = local - i128::from(offset) * i128::from(MICROS_PER_SECOND);
    Some((i64::try_from(utc).ok()?, offset))
}

/// An openCypher duration as ISO 8601, `P1Y2M3DT4H5M6.789S`; zero is `PT0S`.
///
/// Months print as years and months, days as days, and nanoseconds as hours,
/// minutes and seconds — days are never folded into hours or back. A negative
/// component carries its own sign (`P-1M3D`), since the three parts are
/// independent and need not share one.
pub fn duration_to_iso8601(months: i32, days: i32, nanos: i64) -> String {
    let mut out = String::from("P");
    let month_sign = if months < 0 { "-" } else { "" };
    let (years, rest_months) = (months.unsigned_abs() / 12, months.unsigned_abs() % 12);
    if years != 0 {
        out.push_str(&format!("{month_sign}{years}Y"));
    }
    if rest_months != 0 {
        out.push_str(&format!("{month_sign}{rest_months}M"));
    }
    if days != 0 {
        out.push_str(&format!("{days}D"));
    }
    if nanos != 0 {
        let sign = if nanos < 0 { "-" } else { "" };
        let abs = nanos.unsigned_abs();
        let hours = abs / NANOS_PER_HOUR as u64;
        let minutes = abs % NANOS_PER_HOUR as u64 / NANOS_PER_MINUTE as u64;
        let seconds = abs % NANOS_PER_MINUTE as u64 / NANOS_PER_SECOND as u64;
        let frac = abs % NANOS_PER_SECOND as u64;
        out.push('T');
        if hours != 0 {
            out.push_str(&format!("{sign}{hours}H"));
        }
        if minutes != 0 {
            out.push_str(&format!("{sign}{minutes}M"));
        }
        if seconds != 0 || frac != 0 {
            out.push_str(&format!("{sign}{seconds}"));
            if frac != 0 {
                let digits = format!("{frac:09}");
                out.push('.');
                out.push_str(digits.trim_end_matches('0'));
            }
            out.push('S');
        }
    }
    if out == "P" {
        out.push_str("T0S");
    }
    out
}

/// An ISO 8601 duration to `(months, days, nanos)`.
///
/// Accepts `P[nY][nM][nW][nD][T[nH][nM][n[.f]S]]` with the components in that
/// order, at least one present, an optional leading `-` negating them all, and
/// an optional `-` on any component. Years become 12 months and weeks 7 days;
/// nothing else is normalized. Only seconds take a fraction, of 1–9 digits.
/// Refuses anything that overflows its field.
pub fn iso8601_to_duration(s: &str) -> Option<(i32, i32, i64)> {
    let (negate_all, rest) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    let body = rest.strip_prefix('P')?;
    let (date_part, time_part) = match body.split_once('T') {
        Some((d, t)) => (d, Some(t)),
        None => (body, None),
    };
    let mut months: i64 = 0;
    let mut days: i64 = 0;
    let mut nanos: i128 = 0;
    let mut any = false;

    let mut cursor = date_part;
    let mut order = 0;
    while !cursor.is_empty() {
        let (n, unit, frac, next) = duration_component(cursor)?;
        if frac.is_some() {
            return None;
        }
        let rank = match unit {
            'Y' => 1,
            'M' => 2,
            'W' => 3,
            'D' => 4,
            _ => return None,
        };
        if rank <= order {
            return None;
        }
        order = rank;
        match unit {
            'Y' => months = months.checked_add(n.checked_mul(12)?)?,
            'M' => months = months.checked_add(n)?,
            'W' => days = days.checked_add(n.checked_mul(7)?)?,
            _ => days = days.checked_add(n)?,
        }
        any = true;
        cursor = next;
    }
    if let Some(time_part) = time_part {
        if time_part.is_empty() {
            return None;
        }
        let mut cursor = time_part;
        let mut order = 0;
        while !cursor.is_empty() {
            let (n, unit, frac, next) = duration_component(cursor)?;
            let rank = match unit {
                'H' => 1,
                'M' => 2,
                'S' => 3,
                _ => return None,
            };
            if rank <= order || (frac.is_some() && unit != 'S') {
                return None;
            }
            order = rank;
            let scale = match unit {
                'H' => NANOS_PER_HOUR,
                'M' => NANOS_PER_MINUTE,
                _ => NANOS_PER_SECOND,
            };
            let mut part = i128::from(n) * i128::from(scale);
            if let Some(f) = frac {
                part += if n < 0 || cursor.starts_with('-') {
                    -f
                } else {
                    f
                };
            }
            nanos += part;
            any = true;
            cursor = next;
        }
    }
    if !any {
        return None;
    }
    if negate_all {
        months = months.checked_neg()?;
        days = days.checked_neg()?;
        nanos = nanos.checked_neg()?;
    }
    Some((
        i32::try_from(months).ok()?,
        i32::try_from(days).ok()?,
        i64::try_from(nanos).ok()?,
    ))
}

/// One `[-]digits[.digits]<unit>` component: its signed integer part, its unit,
/// its fraction in nanoseconds (unsigned; the caller applies the sign) and the
/// rest of the input.
fn duration_component(s: &str) -> Option<(i64, char, Option<i128>, &str)> {
    let (negative, rest) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    let int_len = rest.bytes().take_while(u8::is_ascii_digit).count();
    if int_len == 0 {
        return None;
    }
    let magnitude: i64 = rest[..int_len].parse().ok()?;
    let mut rest = &rest[int_len..];
    let mut frac = None;
    if let Some(after_dot) = rest.strip_prefix(['.', ',']) {
        let frac_len = after_dot.bytes().take_while(u8::is_ascii_digit).count();
        if frac_len == 0 || frac_len > 9 {
            return None;
        }
        let digits: i128 = after_dot[..frac_len].parse().ok()?;
        frac = Some(digits * 10i128.pow(9 - frac_len as u32));
        rest = &after_dot[frac_len..];
    }
    let unit = rest.chars().next()?;
    let n = if negative { -magnitude } else { magnitude };
    Some((n, unit, frac, &rest[unit.len_utf8()..]))
}

/// JSON tag of a [`Value::Date`]: `{"$date": "2024-01-15"}`.
pub const TAG_DATE: &str = "$date";
/// JSON tag of a [`Value::LocalDateTime`]: `{"$ldt": "2024-01-15T12:34:56.789012"}`.
pub const TAG_LOCAL_DATETIME: &str = "$ldt";
/// JSON tag of a [`Value::DateTime`]: `{"$dt": "2024-01-15T12:34:56.789012+05:30"}`.
pub const TAG_DATETIME: &str = "$dt";
/// JSON tag of a [`Value::Duration`]: `{"$dur": "P1Y2M3DT4H5M6.789S"}`.
pub const TAG_DURATION: &str = "$dur";
/// JSON tag of a [`Value::Binary`]: `{"$binary": "<base64>"}`.
pub const TAG_BINARY: &str = "$binary";

/// The five wire tags, in `Value` order.
pub const V11_TAGS: [&str; 5] = [
    TAG_DATE,
    TAG_LOCAL_DATETIME,
    TAG_DATETIME,
    TAG_DURATION,
    TAG_BINARY,
];

/// The wire tag and text of a format-V11 value — `Ok(Some(("$date",
/// "2024-01-15")))` — or `Ok(None)` for the types JSON carries untagged.
///
/// One place names the tags and the text forms, so every encoder (HTTP and
/// MCP JSON, export, CLI and Arrow text) writes the same thing and
/// [`value_from_tagged_text`] reads all of it back. Every representable value
/// has text except a `DateTime` whose offset is outside ±18:00, which is an
/// `Err` naming why: no encoder ever writes a tag that would not parse back.
pub fn tagged_text(v: &Value) -> std::result::Result<Option<(&'static str, String)>, String> {
    Ok(Some(match v {
        Value::Int(_)
        | Value::Float(_)
        | Value::Str(_)
        | Value::Bool(_)
        | Value::List(_)
        | Value::Map(_)
        | Value::Null => return Ok(None),
        Value::Date(days) => (TAG_DATE, date_to_iso8601(*days)),
        Value::LocalDateTime(us) => (TAG_LOCAL_DATETIME, local_datetime_to_iso8601(*us)),
        Value::DateTime {
            micros_utc,
            offset_seconds,
        } => (
            TAG_DATETIME,
            datetime_to_iso8601(*micros_utc, *offset_seconds)?,
        ),
        Value::Duration {
            months,
            days,
            nanos,
        } => (TAG_DURATION, duration_to_iso8601(*months, *days, *nanos)),
        Value::Binary(bytes) => (TAG_BINARY, base64_encode(bytes)),
    }))
}

/// The text a format-V11 value displays as — its ISO 8601 or base64 form,
/// without the tag — or `None` for the other types.
///
/// For human-facing output (CLI tables, Arrow text columns). A `DateTime` with
/// no valid text displays as `<invalid DateTime: …>`; wire encoders use
/// [`tagged_text`] instead, which reports that as an error.
pub fn v11_display_text(v: &Value) -> Option<String> {
    match tagged_text(v) {
        Ok(tagged) => tagged.map(|(_, text)| text),
        Err(reason) => Some(format!("<invalid DateTime: {reason}>")),
    }
}

/// The value a wire tag and its text name.
///
/// `Err` says why not: an unknown tag, or text that is not the tag's form.
pub fn value_from_tagged_text(tag: &str, text: &str) -> std::result::Result<Value, String> {
    let parsed = match tag {
        TAG_DATE => iso8601_to_date(text).map(Value::Date),
        TAG_LOCAL_DATETIME => iso8601_to_local_datetime(text).map(Value::LocalDateTime),
        TAG_DATETIME => {
            iso8601_to_datetime(text).map(|(micros_utc, offset_seconds)| Value::DateTime {
                micros_utc,
                offset_seconds,
            })
        }
        TAG_DURATION => iso8601_to_duration(text).map(|(months, days, nanos)| Value::Duration {
            months,
            days,
            nanos,
        }),
        TAG_BINARY => base64_decode(text).map(Value::Binary),
        _ => return Err(format!("{tag} is not a value tag")),
    };
    parsed.ok_or_else(|| format!("{tag} payload {text:?} is not {}", tag_expects(tag)))
}

/// What a tag's text must be, for error messages.
fn tag_expects(tag: &str) -> &'static str {
    match tag {
        TAG_DATE => "an ISO 8601 date (YYYY-MM-DD)",
        TAG_LOCAL_DATETIME => {
            "an ISO 8601 local date-time with no offset (YYYY-MM-DDTHH:MM:SS[.f])"
        }
        TAG_DATETIME => {
            "an ISO 8601 date-time with an offset within ±18:00 \
             (YYYY-MM-DDTHH:MM:SS[.f]±HH:MM, or Z)"
        }
        TAG_DURATION => "an ISO 8601 duration (such as P1Y2M3DT4H5M6.789S) that fits its fields",
        TAG_BINARY => "padded standard base64",
        _ => "a known form",
    }
}

const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 (RFC 4648 §4) with `=` padding.
pub fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(BASE64_ALPHABET[(n >> (18 - 6 * i) & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Decode standard padded base64 (RFC 4648 §4).
///
/// Strict: the length must be a multiple of four, padding may only end the
/// input, and unused trailing bits must be zero, so every accepted string is
/// the one [`base64_encode`] would produce. Whitespace is refused.
pub fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn sextet(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some(u32::from(c - b'A')),
            b'a'..=b'z' => Some(u32::from(c - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(c - b'0') + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let bytes = s.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    let quads = bytes.len() / 4;
    for (i, quad) in bytes.chunks(4).enumerate() {
        let pad = quad.iter().rev().take_while(|&&c| c == b'=').count();
        if pad > 2 || (pad > 0 && i + 1 != quads) {
            return None;
        }
        let mut n = 0u32;
        for &c in &quad[..4 - pad] {
            n = (n << 6) | sextet(c)?;
        }
        n <<= 6 * pad as u32;
        let produced = 3 - pad;
        // Bits the padding says are unused must be zero.
        if n & ((1u32 << (8 * pad as u32)) - 1) != 0 {
            return None;
        }
        out.extend_from_slice(&n.to_be_bytes()[1..1 + produced]);
    }
    Some(out)
}

pub fn list_tokens(v: &Value) -> Option<std::collections::BTreeSet<ValueKey>> {
    match v {
        Value::List(items) => Some(items.iter().filter_map(ValueKey::from_value).collect()),
        _ => None,
    }
}

impl GraphError {
    /// What a remote client is told for [`GraphError::Unwritable`]. The path,
    /// uid, owner and mode stay in the server's log: they describe the host.
    pub const UNWRITABLE_CLIENT_MESSAGE: &'static str =
        "the data directory is not writable by the server process; see the server log";
}

#[derive(Debug)]
pub enum GraphError {
    KeyNotFound {
        key: String,
    },
    DuplicateKey {
        key: String,
    },
    Io(std::io::Error),
    Corrupt {
        detail: String,
    },
    RuleInvalid {
        detail: String,
    },
    RuleOwned {
        detail: String,
    },
    RuleNotFound {
        name: String,
    },
    QueryError {
        detail: String,
    },
    IngestError {
        detail: String,
    },
    /// Attempted mutation on a read-only as-of instance.
    ReadOnly,
    /// Requested commit index is outside the retained range.
    ///
    /// The valid range is `floor..total`: `floor` is the oldest commit still
    /// reachable (the WAL horizon floor, `0` when nothing has been pruned) and
    /// `total` is the exclusive upper bound. A `floor > 0` means older events
    /// were pruned and are not retained; retrying at commit 0 will fail too.
    CommitOutOfRange {
        commit: u64,
        total: u64,
        floor: u64,
    },
    /// Attempted to write to a view-managed property.
    ViewPropReadOnly {
        view_name: String,
    },
    /// A compare-and-set precondition was not satisfied.
    ///
    /// `expected` is the commit seq the caller expected to see; `actual` is the
    /// commit seq recorded in the last-change map (or `u64::MAX` for
    /// `NodeAbsent` conflicts where the node unexpectedly exists).
    CasConflict {
        key: String,
        expected: u64,
        actual: u64,
    },
    /// Write statement submitted to a read-only masked query path.
    ///
    /// Returned by [`GraphDb::query_masked`] when the Cypher input is a write
    /// statement (CREATE / MERGE / MATCH…SET / DELETE). The HTTP layer maps
    /// this to 400 Bad Request with body `{"error":"masked queries are read-only"}`.
    MaskedReadOnly,
    /// An asserted commit time would move the recorded history backwards.
    ///
    /// Raised by `record_commits_at`, never by the live clock. A caller that
    /// says "this commit happened at T" is asserting a fact and can be held to
    /// it; the system clock cannot, so an NTP step backwards is stored as
    /// observed and never fails a write.
    ///
    /// Backfilling out of chronological order would make date resolution
    /// meaningless: resolution walks commit order, so a later commit carrying an
    /// earlier instant silently widens every answer after it.
    CommitTimeNotMonotonic {
        supplied_ms: i64,
        newest_ms: i64,
    },
    /// A date was given to a history surface on a store that records no commit
    /// times.
    ///
    /// Written by a release before v0.6.11, or its `commit_times.bin` sidecar
    /// was removed. The store is intact and every commit-indexed read still
    /// works; only date resolution is unavailable. Pass a commit index instead.
    ///
    /// This is deliberately not a `Corrupt`: an unreadable sidecar degrades the
    /// date surface, it does not damage the store.
    NoRecordedTime,
    /// A date predates the oldest commit time the store still records.
    ///
    /// `floor_commit` is the oldest commit the map describes and `floor_ms` is
    /// its recorded time. The shape follows [`GraphError::CommitOutOfRange`]
    /// deliberately: name the range that can be answered rather than guessing a
    /// commit. Retrying with an earlier instant will fail the same way.
    TimeBeforeFloor {
        floor_ms: i64,
        floor_commit: u64,
    },
    /// A role-scoped write was denied by the authz decision table (§3, §4.3).
    ///
    /// `reason` carries the verbatim §4.3 error body text (without the JSON
    /// envelope).  The HTTP layer maps this to 403 with body
    /// `{"error": "<reason>"}`.  Five reason patterns are defined by the spec:
    /// - `"role-bound token: label '<L>' not in write scope (<scope_field>)"`
    /// - `"role-bound token: target node not visible"` (hidden ≡ absent)
    /// - `"role-bound token: edge endpoint not visible"`
    /// - `"role-bound token: edge type '<T>' not in write scope (<scope_field>)"`
    /// - `"role-bound token: this endpoint is not permitted"`
    RoleWriteDenied {
        reason: String,
    },
    /// The store's advisory cross-process write lock could not be taken within
    /// the caller's wait budget: another process is writing.
    ///
    /// Nothing was written and in-memory state is unchanged, so retrying later
    /// is always safe. Readers never see this error — reads do not take the
    /// lock.
    ///
    /// `holder` is the process id of the lock holder when the platform makes it
    /// cheaply knowable, and `None` otherwise. It is a diagnostic hint only;
    /// never branch on it.
    Busy {
        holder: Option<u32>,
    },
    /// The store's data directory (or a file the store must create in it, such
    /// as `LOCK`) is not writable by this process: the filesystem answered
    /// `EACCES`, `EPERM` or `EROFS`.
    ///
    /// This is a deployment fault, not contention: retrying cannot help until
    /// someone changes the directory's ownership or mount. It is deliberately
    /// not [`GraphError::Busy`], which says another process holds the write
    /// lock. Nothing was written.
    ///
    /// `path` is the file or directory the filesystem refused. `detail` is the
    /// full human-readable message: the path, the effective uid, the
    /// directory's owner and mode, and what to do about it.
    Unwritable {
        path: String,
        detail: String,
        /// True when the refusal was `EROFS`: the filesystem is mounted
        /// read-only, which is a deliberate configuration a server may serve
        /// reads from, unlike an ownership mistake.
        read_only_filesystem: bool,
    },
    /// A write tried to change a node's namespace.
    ///
    /// A namespace is a tenancy boundary, set when the node is inserted and
    /// fixed for its lifetime: moving a node between tenants is a deletion from
    /// one and a creation in the other, and saying so is more honest than a
    /// property edit that silently re-homes every edge the node carries.
    NamespaceImmutable {
        key: String,
        from: String,
        to: String,
    },
    /// A user-written edge would join two nodes in different namespaces.
    ///
    /// Only a **global** rule (one with no `namespace`) may derive an edge
    /// across the boundary; a namespaced rule sees one namespace's nodes only,
    /// so its edges are intra-namespace by construction.
    CrossNamespace {
        src: String,
        src_ns: String,
        dst: String,
        dst_ns: String,
    },
    /// A write would give two live nodes of a label the same value for a
    /// property a [`UniqueConstraint`] declares unique.
    ///
    /// Deliberately says nothing about the node already holding the value — no
    /// key, no label, not whether the caller can see it — so the answer is the
    /// same for a visible node, a hidden one and another node in the same
    /// write. Nothing was written.
    ConstraintViolation {
        constraint: String,
        field: String,
    },
    /// A unique constraint cannot be declared because live nodes already
    /// violate it. `violations` lists the first 100 `(node_key, value)` in
    /// conflict, in node-id order; `total` counts all of them.
    ConstraintCreationConflict {
        violations: Vec<(String, Value)>,
        total: usize,
    },
    /// A value is larger than the store accepts: `size` bytes for property
    /// `field`, against `limit` (see [`BINARY_MAX_BYTES`]). Nothing was written.
    ValueTooLarge {
        field: String,
        size: usize,
        limit: usize,
    },
}

/// The reserved property that names a node's namespace.
///
/// Absent means [`NS_DEFAULT`]. Writing `NS_DEFAULT` explicitly stores nothing,
/// so a store that never leaves the default namespace carries no `ns` column and
/// pays nothing for the feature.
pub const NS_PROP: &str = "ns";

/// The namespace a node with no [`NS_PROP`] property is in.
pub const NS_DEFAULT: &str = "default";

/// Longest accepted namespace name, in characters.
pub const NS_MAX_LEN: usize = 64;

/// A valid namespace name: 1–[`NS_MAX_LEN`] characters of `[A-Za-z0-9_.-]`.
///
/// Deliberately narrower than a key: a namespace is an administrative label that
/// appears in `roles.json`, in a rule definition and in `stats`, so it stays
/// spellable without quoting. `/` is excluded on purpose — keys already contain
/// it (`ingest-git` keys files on their repository path) and a namespace must
/// never read as a key prefix.
pub fn valid_namespace(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= NS_MAX_LEN
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')
}

/// The namespace a node's [`NS_PROP`] value names.
///
/// Absent, or any value that is not a string, resolves to [`NS_DEFAULT`]: the
/// write path refuses a non-string `ns`, so this only decides what a store
/// hand-edited outside this binary reads as, and the default is the narrowing
/// answer for a namespaced role.
pub fn namespace_of_value(v: Option<&Value>) -> &str {
    match v {
        Some(Value::Str(s)) => s.as_str(),
        _ => NS_DEFAULT,
    }
}

impl std::fmt::Display for GraphError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GraphError::KeyNotFound { key } => write!(f, "node key not found: {key}"),
            GraphError::DuplicateKey { key } => write!(f, "duplicate node key: {key}"),
            GraphError::Io(e) => write!(f, "io error: {e}"),
            GraphError::Corrupt { detail } => write!(f, "corrupt data: {detail}"),
            GraphError::RuleInvalid { detail } => write!(f, "invalid rule: {detail}"),
            GraphError::RuleOwned { detail } => write!(f, "edge is rule-owned: {detail}"),
            GraphError::RuleNotFound { name } => write!(f, "rule not found: {name}"),
            GraphError::QueryError { detail } => write!(f, "query error: {detail}"),
            GraphError::IngestError { detail } => write!(f, "ingest error: {detail}"),
            GraphError::ReadOnly => write!(f, "as-of instances are read-only"),
            GraphError::CommitTimeNotMonotonic {
                supplied_ms,
                newest_ms,
            } => write!(
                f,
                "commit time {supplied_ms} ms is earlier than {newest_ms} ms, \
                 which this store already records; backfill in chronological \
                 order"
            ),
            GraphError::NoRecordedTime => write!(
                f,
                "this store records no commit times; pass a commit index instead \
                 of a date"
            ),
            GraphError::TimeBeforeFloor {
                floor_ms,
                floor_commit,
            } => write!(
                f,
                "no commit time is recorded that early; the oldest is commit \
                 {floor_commit} at {floor_ms} ms"
            ),
            GraphError::CommitOutOfRange {
                commit,
                total,
                floor: 0,
            } => write!(
                f,
                "commit {commit} is out of range; valid range is 0..{total}"
            ),
            GraphError::CommitOutOfRange {
                commit,
                total,
                floor,
            } => write!(
                f,
                "commit {commit} is out of range; valid range is {floor}..{total} \
                 — events before commit {floor} are not retained"
            ),
            GraphError::ViewPropReadOnly { view_name } => write!(
                f,
                "property is managed by view {:?} and cannot be written directly",
                view_name
            ),
            GraphError::CasConflict {
                key,
                expected,
                actual,
            } => write!(
                f,
                "CAS conflict on key {key:?}: expected commit {expected}, actual {actual}"
            ),
            GraphError::MaskedReadOnly => write!(f, "masked queries are read-only"),
            GraphError::RoleWriteDenied { reason } => write!(f, "{reason}"),
            GraphError::Busy { holder: Some(pid) } => {
                write!(f, "store is busy: write lock held by process {pid}")
            }
            GraphError::Busy { holder: None } => {
                write!(f, "store is busy: write lock held by another process")
            }
            GraphError::Unwritable { detail, .. } => write!(f, "{detail}"),
            GraphError::NamespaceImmutable { key, from, to } => write!(
                f,
                "node {key} is in namespace {from}; a namespace is set at insert and cannot \
                 be changed to {to} — delete and re-insert the node instead"
            ),
            GraphError::CrossNamespace {
                src,
                src_ns,
                dst,
                dst_ns,
            } => write!(
                f,
                "edge {src} → {dst} crosses a namespace boundary ({src_ns} → {dst_ns}); \
                 only a global rule may derive one"
            ),
            GraphError::ConstraintViolation { constraint, field } => write!(
                f,
                "constraint violation: property '{field}' is declared unique by constraint \
                 '{constraint}'; a node with that value already exists"
            ),
            GraphError::ConstraintCreationConflict { violations, total } => {
                write!(
                    f,
                    "cannot create the constraint: {total} existing nodes hold duplicate values"
                )?;
                if violations.len() < *total {
                    write!(f, " (first {} listed)", violations.len())?;
                }
                Ok(())
            }
            GraphError::ValueTooLarge { field, size, limit } => write!(
                f,
                "value too large: property '{field}' is {size} bytes; the limit is {limit} bytes"
            ),
        }
    }
}

impl std::error::Error for GraphError {}

impl From<std::io::Error> for GraphError {
    fn from(e: std::io::Error) -> Self {
        GraphError::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, GraphError>;

/// One value of every variant, for tests that pin or round-trip the encoding.
#[cfg(test)]
pub(crate) mod pinned {
    use super::Value;
    use std::collections::BTreeMap;

    /// The serde variant index of `v`. Exhaustive on purpose: a new variant
    /// does not compile until it is given its index here.
    pub(crate) fn index(v: &Value) -> u32 {
        match v {
            Value::Int(_) => 0,
            Value::Float(_) => 1,
            Value::Str(_) => 2,
            Value::Bool(_) => 3,
            Value::List(_) => 4,
            Value::Map(_) => 5,
            Value::Date(_) => 6,
            Value::LocalDateTime(_) => 7,
            Value::DateTime { .. } => 8,
            Value::Duration { .. } => 9,
            Value::Binary(_) => 10,
            Value::Null => 11,
        }
    }

    /// One value of every variant but `Null`, in index order: the types a
    /// store can hold (the five format-V11 types on a V11 store).
    pub(crate) fn storable_values() -> Vec<Value> {
        vec![
            Value::Int(-7),
            Value::Float(1.5),
            Value::Str("s".into()),
            Value::Bool(true),
            Value::List(vec![Value::Int(1), Value::Str("x".into())]),
            Value::Map(BTreeMap::from([("k".to_string(), Value::Int(2))])),
            Value::Date(19_737),
            Value::LocalDateTime(1_705_322_096_789_012),
            Value::DateTime {
                micros_utc: 1_705_302_296_789_012,
                offset_seconds: 19_800,
            },
            Value::Duration {
                months: 14,
                days: 3,
                nanos: 14_706_789_000_000,
            },
            Value::Binary(vec![0xDE, 0xAD, 0xBE, 0xEF]),
        ]
    }

    /// Every variant, in index order.
    pub(crate) fn all_values() -> Vec<Value> {
        let mut out = storable_values();
        out.push(Value::Null);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_roundtrips_through_bincode() {
        let vals = vec![
            Value::Int(42),
            Value::Float(1.5),
            Value::Str("hi".into()),
            Value::Bool(true),
        ];
        let bytes = bincode::serialize(&vals).unwrap();
        let back: Vec<Value> = bincode::deserialize(&bytes).unwrap();
        assert_eq!(vals, back);
    }

    #[test]
    fn errors_display_context() {
        let e = GraphError::KeyNotFound { key: "u1".into() };
        assert_eq!(e.to_string(), "node key not found: u1");
    }

    #[test]
    fn list_roundtrips_and_old_variants_keep_encoding() {
        let l = Value::List(vec![Value::Str("a".into()), Value::Int(2)]);
        let back: Value = bincode::deserialize(&bincode::serialize(&l).unwrap()).unwrap();
        assert_eq!(l, back);
        // Appending List must not change the wire tag of existing variants.
        assert_eq!(
            bincode::serialize(&Value::Int(7)).unwrap(),
            vec![0, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0]
        );
    }

    #[test]
    fn value_serde_tags_are_pinned() {
        let all = pinned::all_values();
        // Every index from 0 to the last is used exactly once, in order, and
        // Null holds the last one.
        let indices: Vec<u32> = all.iter().map(pinned::index).collect();
        assert_eq!(indices, (0..all.len() as u32).collect::<Vec<_>>());
        assert_eq!(pinned::index(&Value::Null), all.len() as u32 - 1);
        for v in &all {
            // bincode: a little-endian u32 variant index, then the payload.
            let bytes = bincode::serialize(v).unwrap();
            assert_eq!(bytes[..4], pinned::index(v).to_le_bytes(), "{v:?}");
            let back: Value = bincode::deserialize(&bytes).unwrap();
            assert_eq!(&back, v);
        }
        // The serde JSON form (external tagging, as `/subscribe` rows carry
        // it) is pinned in core-api, which depends on serde_json.
        // The exact bytes of the stored scalars did not move.
        assert_eq!(
            bincode::serialize(&Value::Bool(true)).unwrap(),
            vec![3, 0, 0, 0, 1]
        );
        assert_eq!(bincode::serialize(&Value::Null).unwrap(), vec![11, 0, 0, 0]);
        assert_eq!(
            bincode::serialize(&Value::Date(1)).unwrap(),
            vec![6, 0, 0, 0, 1, 0, 0, 0]
        );
    }

    #[test]
    fn contains_null_looks_at_every_depth() {
        assert!(Value::Null.contains_null());
        assert!(Value::List(vec![Value::Int(1), Value::Null]).contains_null());
        let inner: BTreeMap<String, Value> =
            [("b".to_string(), Value::List(vec![Value::Null]))].into();
        assert!(Value::List(vec![Value::Map(inner)]).contains_null());
        assert!(!Value::List(vec![Value::Int(1), Value::List(vec![])]).contains_null());
        assert!(!Value::Str("null".into()).contains_null());
    }

    #[test]
    fn value_keys_normalize_scalars_and_tokenize_lists() {
        assert_eq!(ValueKey::from_value(&Value::Int(3)), Some(ValueKey::Int(3)));
        assert_eq!(
            ValueKey::from_value(&Value::Float(1.5)),
            Some(ValueKey::FloatBits(1.5f64.to_bits()))
        );
        assert_eq!(ValueKey::from_value(&Value::List(vec![])), None);
        let toks = list_tokens(&Value::List(vec![
            Value::Str("x".into()),
            Value::Str("x".into()),           // dup collapses
            Value::List(vec![Value::Int(1)]), // nested list skipped
        ]))
        .unwrap();
        assert_eq!(toks.len(), 1);
        assert!(toks.contains(&ValueKey::Str("x".into())));
        assert_eq!(list_tokens(&Value::Int(1)), None);
    }

    #[test]
    fn test_value_bincode_discriminants() {
        // Existing tags must not shift.
        assert_eq!(bincode::serialize(&Value::Int(0)).unwrap()[0], 0);
        assert_eq!(bincode::serialize(&Value::Float(0.0)).unwrap()[0], 1);
        assert_eq!(bincode::serialize(&Value::Str("".into())).unwrap()[0], 2);
        assert_eq!(bincode::serialize(&Value::Bool(false)).unwrap()[0], 3);
        assert_eq!(bincode::serialize(&Value::List(vec![])).unwrap()[0], 4);
        assert_eq!(
            bincode::serialize(&Value::Map(Default::default())).unwrap()[0],
            5
        );
        // New tags.
        assert_eq!(bincode::serialize(&Value::Date(0)).unwrap()[0], 6);
        assert_eq!(bincode::serialize(&Value::LocalDateTime(0)).unwrap()[0], 7);
        assert_eq!(
            bincode::serialize(&Value::DateTime {
                micros_utc: 0,
                offset_seconds: 0
            })
            .unwrap()[0],
            8
        );
        assert_eq!(
            bincode::serialize(&Value::Duration {
                months: 0,
                days: 0,
                nanos: 0
            })
            .unwrap()[0],
            9
        );
        assert_eq!(bincode::serialize(&Value::Binary(vec![])).unwrap()[0], 10);
    }

    #[test]
    fn new_values_roundtrip_through_bincode() {
        let vals = vec![
            Value::Date(-719_528),
            Value::LocalDateTime(1_705_322_096_789_012),
            Value::DateTime {
                micros_utc: -1,
                offset_seconds: -18_000,
            },
            Value::Duration {
                months: -14,
                days: 3,
                nanos: 14_706_789_000_000,
            },
            Value::Binary(vec![0, 255, 7]),
        ];
        let back: Vec<Value> = bincode::deserialize(&bincode::serialize(&vals).unwrap()).unwrap();
        assert_eq!(vals, back);
    }

    #[test]
    fn test_value_key_ordering() {
        assert!(ValueKey::DateDays(-1) < ValueKey::DateDays(0));
        assert!(ValueKey::DateDays(0) < ValueKey::DateDays(1));
        // UTC equality: same micros_utc, different offset → same key.
        assert_eq!(
            ValueKey::from_value(&Value::DateTime {
                micros_utc: 1_000_000,
                offset_seconds: 0
            }),
            ValueKey::from_value(&Value::DateTime {
                micros_utc: 1_000_000,
                offset_seconds: 3600
            })
        );
        // BinaryBytes: lexicographic.
        assert!(ValueKey::BinaryBytes(vec![0]) < ValueKey::BinaryBytes(vec![1]));
    }

    #[test]
    fn value_key_from_new_values() {
        assert_eq!(
            ValueKey::from_value(&Value::Date(19_737)),
            Some(ValueKey::DateDays(19_737))
        );
        assert_eq!(
            ValueKey::from_value(&Value::LocalDateTime(-5)),
            Some(ValueKey::LocalDateTimeMicros(-5))
        );
        assert_eq!(
            ValueKey::from_value(&Value::DateTime {
                micros_utc: 9,
                offset_seconds: -60
            }),
            Some(ValueKey::DateTimeMicrosUtc(9))
        );
        assert_eq!(
            ValueKey::from_value(&Value::Binary(vec![1, 2])),
            Some(ValueKey::BinaryBytes(vec![1, 2]))
        );
        // Duration is not totally ordered, so it has no key.
        assert_eq!(
            ValueKey::from_value(&Value::Duration {
                months: 1,
                days: 0,
                nanos: 0
            }),
            None
        );
        // The old key variants keep their encoding: new ones were appended.
        assert_eq!(bincode::serialize(&ValueKey::Bool(true)).unwrap()[0], 3);
        assert_eq!(bincode::serialize(&ValueKey::DateDays(0)).unwrap()[0], 4);
        assert_eq!(
            bincode::serialize(&ValueKey::BinaryBytes(vec![])).unwrap()[0],
            7
        );
    }

    #[test]
    fn binary_max_bytes_is_one_mebibyte() {
        assert_eq!(BINARY_MAX_BYTES, 1_048_576);
    }

    #[test]
    fn date_iso8601_roundtrip() {
        // 2024-01-15 = days 19737 since 1970-01-01.
        assert_eq!(date_to_iso8601(19_737), "2024-01-15");
        assert_eq!(iso8601_to_date("2024-01-15"), Some(19_737));
        assert_eq!(date_to_iso8601(0), "1970-01-01");
        assert_eq!(date_to_iso8601(-1), "1969-12-31");
        assert_eq!(iso8601_to_date("1969-12-31"), Some(-1));
        assert_eq!(iso8601_to_date("2024-02-29"), Some(19_782));
        assert_eq!(iso8601_to_date("2000-02-29"), Some(11_016));
        // Year 1 BCE is ISO year 0; years outside 0000–9999 use the signed
        // expanded form.
        assert_eq!(date_to_iso8601(-719_528), "0000-01-01");
        let neg = iso8601_to_date("-000001-01-01").unwrap();
        assert_eq!(date_to_iso8601(neg), "-000001-01-01");
        assert_eq!(date_to_iso8601(2_932_897), "+010000-01-01");
        // A signed four-digit year parses too; it prints in the canonical form.
        assert_eq!(iso8601_to_date("+2024-01-15"), Some(19_737));
    }

    #[test]
    fn every_date_prints_and_parses_back() {
        for days in [
            i32::MIN,
            i32::MIN + 1,
            -719_529,
            -1,
            0,
            19_737,
            2_932_896,
            i32::MAX - 1,
            i32::MAX,
        ] {
            let text = date_to_iso8601(days);
            assert_eq!(iso8601_to_date(&text), Some(days), "{text}");
        }
        assert_eq!(date_to_iso8601(i32::MAX), "+5881580-07-11");
        assert_eq!(date_to_iso8601(i32::MIN), "-5877641-06-23");
        // One day past the field's range does not fit and is refused.
        assert_eq!(iso8601_to_date("+5881580-07-12"), None);
        assert_eq!(iso8601_to_date("-5877641-06-22"), None);
    }

    #[test]
    fn date_iso8601_refuses_malformed_and_time_bearing_input() {
        for bad in [
            "",
            "not-a-date",
            "2024-13-01",
            "2024-00-10",
            "2024-02-30",
            "2023-02-29",
            "1900-02-29",
            "2024-1-5",
            "20240115",
            "02024-01-15",
            "2024-01-15T00:00",
            "2024-01-15T12:00:00Z",
            " 2024-01-15",
            "2024-01-15 ",
        ] {
            assert_eq!(iso8601_to_date(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn local_datetime_iso8601_roundtrip() {
        let micros = 1_705_322_096_789_012; // 2024-01-15T12:34:56.789012
        assert_eq!(
            local_datetime_to_iso8601(micros),
            "2024-01-15T12:34:56.789012"
        );
        assert_eq!(
            iso8601_to_local_datetime("2024-01-15T12:34:56.789012"),
            Some(micros)
        );
        // Whole seconds print without a fraction; parsing accepts both, and
        // a time without seconds.
        assert_eq!(local_datetime_to_iso8601(0), "1970-01-01T00:00:00");
        assert_eq!(iso8601_to_local_datetime("1970-01-01T00:00:00"), Some(0));
        assert_eq!(
            iso8601_to_local_datetime("1970-01-01T00:01"),
            Some(60_000_000)
        );
        // Before the epoch: floor, so the fraction stays non-negative.
        assert_eq!(local_datetime_to_iso8601(-1), "1969-12-31T23:59:59.999999");
        assert_eq!(
            iso8601_to_local_datetime("1969-12-31T23:59:59.999999"),
            Some(-1)
        );
        // Sub-microsecond digits are truncated toward the earlier instant.
        assert_eq!(
            iso8601_to_local_datetime("1969-12-31T23:59:59.9999995"),
            Some(-1)
        );
        for micros in [i64::MIN, i64::MIN + 1, -1, 0, 1, i64::MAX - 1, i64::MAX] {
            let text = local_datetime_to_iso8601(micros);
            assert_eq!(iso8601_to_local_datetime(&text), Some(micros), "{text}");
        }
        assert_eq!(
            local_datetime_to_iso8601(i64::MAX),
            "+294247-01-10T04:00:54.775807"
        );
    }

    #[test]
    fn local_datetime_iso8601_refuses_offsets_and_malformed() {
        for bad in [
            "2024-01-15T12:34:56+01:00",
            "2024-01-15T12:34:56Z",
            "2024-01-15",
            "2024-01-15 12:34:56",
            "2024-01-15T24:00:00",
            "2024-01-15T12:60:00",
            "2024-01-15T12:00:60",
            "2024-01-15T12:00:00.",
            "2024-01-15T12:00:00.1234567890",
            "2024-01-15T12",
            "+294247-01-10T04:00:54.775808",
            "garbage",
        ] {
            assert_eq!(iso8601_to_local_datetime(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn datetime_iso8601_roundtrip_preserves_offset() {
        let s = "2024-01-15T12:34:56.789012+05:30";
        let (micros, off) = iso8601_to_datetime(s).unwrap();
        assert_eq!(off, 19_800);
        // 12:34:56.789012 at +05:30 is 07:04:56.789012 UTC.
        assert_eq!(micros, 1_705_302_296_789_012);
        assert_eq!(datetime_to_iso8601(micros, off).unwrap(), s);
        // Same instant, different offsets: same micros_utc.
        let a = iso8601_to_datetime("2024-01-15T12:00:00+00:00").unwrap();
        let b = iso8601_to_datetime("2024-01-15T07:00:00-05:00").unwrap();
        assert_eq!(a.0, b.0);
        assert_ne!(a.1, b.1);
        assert_eq!(
            datetime_to_iso8601(b.0, b.1).unwrap(),
            "2024-01-15T07:00:00-05:00"
        );
        // `Z` parses as +00:00 and prints as +00:00.
        let z = iso8601_to_datetime("2024-01-15T12:00:00Z").unwrap();
        assert_eq!(z, a);
        assert_eq!(
            datetime_to_iso8601(z.0, z.1).unwrap(),
            "2024-01-15T12:00:00+00:00"
        );
    }

    #[test]
    fn datetime_offsets_cover_every_real_zone_up_to_eighteen_hours() {
        for (text, secs) in [
            ("+10:00", 36_000),
            ("+12:45", 45_900),
            ("+14:00", 50_400),
            ("-10:00", -36_000),
            ("-12:00", -43_200),
            ("+18:00", 64_800),
            ("-18:00", -64_800),
        ] {
            let s = format!("2024-01-15T12:00:00{text}");
            let (micros, off) = iso8601_to_datetime(&s).unwrap();
            assert_eq!(off, secs, "{s}");
            assert_eq!(datetime_to_iso8601(micros, off).unwrap(), s);
        }
        for bad in [
            "+18:01",
            "-18:00:01",
            "+19:00",
            "+05:60",
            "+05",
            "+0530",
            "+05:30:60",
        ] {
            let s = format!("2024-01-15T12:00:00{bad}");
            assert_eq!(iso8601_to_datetime(&s), None, "{s}");
        }
        assert_eq!(iso8601_to_datetime("2024-01-15T12:00:00"), None);
        assert_eq!(iso8601_to_datetime("2024-01-15"), None);
    }

    #[test]
    fn datetime_offsets_with_seconds_print_and_parse_back() {
        let s = "2024-01-15T12:00:00+05:30:15";
        let (micros, off) = iso8601_to_datetime(s).unwrap();
        assert_eq!(off, 19_815);
        assert_eq!(datetime_to_iso8601(micros, off).unwrap(), s);
        // Every valid offset, at a step that covers odd seconds, round-trips.
        for off in (-MAX_OFFSET_SECONDS..=MAX_OFFSET_SECONDS).step_by(997) {
            let text = datetime_to_iso8601(1_000_000_007, off).unwrap();
            assert_eq!(
                iso8601_to_datetime(&text),
                Some((1_000_000_007, off)),
                "{text}"
            );
        }
    }

    #[test]
    fn every_valid_datetime_prints_and_parses_back() {
        // The local time of an instant near the i64 limit, shifted by its
        // offset, is past the i64 range; it still prints, and parses back.
        for (micros, off) in [
            (i64::MAX, MAX_OFFSET_SECONDS),
            (i64::MIN, -MAX_OFFSET_SECONDS),
            (i64::MAX, -MAX_OFFSET_SECONDS),
            (i64::MIN, 0),
            (-1, 1),
        ] {
            let text = datetime_to_iso8601(micros, off).unwrap();
            assert_eq!(iso8601_to_datetime(&text), Some((micros, off)), "{text}");
        }
        // An offset outside ±18:00 has no text: an error, never a bad tag.
        let err = datetime_to_iso8601(0, MAX_OFFSET_SECONDS + 1).unwrap_err();
        assert!(err.contains("outside ±18:00"), "{err}");
        assert!(datetime_to_iso8601(0, i32::MIN).is_err());
    }

    #[test]
    fn duration_iso8601_roundtrip() {
        let (months, days, nanos) = iso8601_to_duration("P1Y2M3DT4H5M6.789S").unwrap();
        assert_eq!((months, days), (14, 3));
        assert_eq!(nanos, ((4 * 60 + 5) * 60 + 6) * 1_000_000_000 + 789_000_000);
        assert_eq!(
            duration_to_iso8601(months, days, nanos),
            "P1Y2M3DT4H5M6.789S"
        );
        assert_eq!(iso8601_to_duration("PT0S"), Some((0, 0, 0)));
        assert_eq!(duration_to_iso8601(0, 0, 0), "PT0S");
        // Weeks are days; days and hours are never folded into each other.
        assert_eq!(iso8601_to_duration("P2W"), Some((0, 14, 0)));
        assert_eq!(
            iso8601_to_duration("PT25H"),
            Some((0, 0, 25 * 3_600_000_000_000))
        );
        assert_eq!(duration_to_iso8601(0, 0, 25 * 3_600_000_000_000), "PT25H");
        assert_eq!(duration_to_iso8601(0, 0, 1), "PT0.000000001S");
    }

    #[test]
    fn duration_iso8601_negative_components() {
        // A leading sign negates every component.
        assert_eq!(
            iso8601_to_duration("-P1Y2DT3S"),
            Some((-12, -2, -3_000_000_000))
        );
        // Components may carry their own sign, as openCypher durations can
        // mix signs; that is the form they print in.
        assert_eq!(iso8601_to_duration("P-1M3D"), Some((-1, 3, 0)));
        assert_eq!(duration_to_iso8601(-1, 3, 0), "P-1M3D");
        assert_eq!(duration_to_iso8601(-12, -2, -3_000_000_000), "P-1Y-2DT-3S");
        assert_eq!(duration_to_iso8601(0, 0, -500_000_000), "PT-0.5S");
        for (m, d, n) in [
            (-14, -3, -14_706_789_000_000),
            (i32::MIN, i32::MAX, i64::MIN),
            (i32::MAX, i32::MIN, i64::MAX),
        ] {
            let s = duration_to_iso8601(m, d, n);
            assert_eq!(iso8601_to_duration(&s), Some((m, d, n)), "{s}");
        }
    }

    #[test]
    fn duration_iso8601_refuses_malformed() {
        for bad in [
            "",
            "P",
            "PT",
            "1Y",
            "P1",
            "P1D2M",           // out of order
            "P1.5D",           // fraction only on seconds
            "PT1.S",           // empty fraction
            "PT1.0000000001S", // more than nanosecond precision
            "P1Y1Y",
            "PT1H1D",
            "P--1D",
            "P99999999999Y", // overflows months
            "PT1S ",
        ] {
            assert_eq!(iso8601_to_duration(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn base64_rfc4648_vectors() {
        let vectors: [(&[u8], &str); 7] = [
            (b"", ""),
            (b"f", "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg=="),
            (b"fooba", "Zm9vYmE="),
            (b"foobar", "Zm9vYmFy"),
        ];
        for (raw, enc) in vectors {
            assert_eq!(base64_encode(raw), enc);
            assert_eq!(base64_decode(enc).as_deref(), Some(raw));
        }
        let all: Vec<u8> = (0..=255).collect();
        assert_eq!(base64_decode(&base64_encode(&all)), Some(all));
        assert_eq!(base64_encode(&[0xDE, 0xAD, 0xBE, 0xEF]), "3q2+7w==");
    }

    #[test]
    fn base64_refuses_non_canonical_input() {
        for bad in [
            "Zg",
            "Zg=",
            "Z===",
            "Zh==",
            "Zm9v YmFy",
            "Zm9-",
            "====",
            "Zg==Zg==",
        ] {
            assert_eq!(base64_decode(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn tagged_text_roundtrips_every_new_type() {
        let vals = [
            Value::Date(19_737),
            Value::LocalDateTime(1_705_322_096_789_012),
            Value::DateTime {
                micros_utc: 1_705_302_296_789_012,
                offset_seconds: 19_800,
            },
            Value::Duration {
                months: 14,
                days: 3,
                nanos: 14_706_789_000_000,
            },
            Value::Binary(vec![0xDE, 0xAD, 0xBE, 0xEF]),
        ];
        for (v, want_tag) in vals.iter().zip(V11_TAGS) {
            let (tag, text) = tagged_text(v).unwrap().unwrap();
            assert_eq!(tag, want_tag);
            assert_eq!(value_from_tagged_text(tag, &text).as_ref(), Ok(v));
        }
        assert_eq!(tagged_text(&Value::Str("2024-01-15".into())), Ok(None));
        let err = value_from_tagged_text("$date", "2024-1-5").unwrap_err();
        assert_eq!(
            err,
            r#"$date payload "2024-1-5" is not an ISO 8601 date (YYYY-MM-DD)"#
        );
        for tag in V11_TAGS {
            assert!(value_from_tagged_text(tag, "nope!").is_err(), "{tag}");
        }
        assert!(value_from_tagged_text("$time", "12:00").is_err());
        // A DateTime with no valid text is an error, not a tag.
        let bad = Value::DateTime {
            micros_utc: 0,
            offset_seconds: MAX_OFFSET_SECONDS + 1,
        };
        assert!(tagged_text(&bad).is_err());
    }

    #[test]
    fn v11_only_type_names_new_values_at_any_depth() {
        assert_eq!(v11_only_type(&Value::Int(1)), None);
        assert_eq!(v11_only_type(&Value::Date(0)), Some("Date"));
        assert_eq!(
            v11_only_type(&Value::LocalDateTime(0)),
            Some("LocalDateTime")
        );
        assert_eq!(
            v11_only_type(&Value::DateTime {
                micros_utc: 0,
                offset_seconds: 0
            }),
            Some("DateTime")
        );
        assert_eq!(
            v11_only_type(&Value::Duration {
                months: 0,
                days: 0,
                nanos: 0
            }),
            Some("Duration")
        );
        assert_eq!(v11_only_type(&Value::Binary(vec![])), Some("Binary"));
        let nested = Value::List(vec![
            Value::Str("a".into()),
            Value::Map(BTreeMap::from([("b".to_string(), Value::Binary(vec![1]))])),
        ]);
        assert_eq!(v11_only_type(&nested), Some("Binary"));
        let plain = Value::Map(BTreeMap::from([("x".to_string(), Value::List(vec![]))]));
        assert_eq!(v11_only_type(&plain), None);
    }
}
