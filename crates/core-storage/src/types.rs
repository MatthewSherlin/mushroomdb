use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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
    // Until the V11 format lands, a write carrying one is refused before it
    // reaches the WAL (see `v11_only_type`).
    /// Calendar date: days since 1970-01-01. JSON form `{"$date": "2024-01-15"}`.
    Date(i32),
    /// Date and time with no time zone: microseconds since 1970-01-01T00:00:00.
    /// JSON form `{"$ldt": "2024-01-15T12:34:56.789012"}`.
    LocalDateTime(i64),
    /// An instant plus the UTC offset it was written with. The offset is kept
    /// so the value prints back as written; equality and ordering in queries
    /// and in index keys use `micros_utc` only.
    /// JSON form `{"$dt": "2024-01-15T12:34:56.789012+05:30"}`.
    DateTime {
        micros_utc: i64,
        offset_seconds: i16,
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
        }
    }
}

/// The name of the first format-V11 value type inside `v`, if any.
///
/// Recurses into lists and maps: a list of dates is as V11-only as a date.
/// This is what the write path checks so that, until the V11 format lands, no
/// such value is ever appended to the WAL, where a 0.9.x reader would stop
/// replaying at the frame it cannot decode.
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
    }
}

/// The refusal a write gets when it carries a format-V11 value type before the
/// V11 format exists.
pub fn v11_type_refusal(type_name: &str) -> String {
    format!(
        "not supported in mushroomdb 0.10 yet: {type_name} values cannot be stored until the \
         V11 format lands"
    )
}

// ---------------------------------------------------------------------------
// Text forms of the V11 types: ISO 8601 and base64.
//
// Calendar arithmetic and ISO 8601 date / date-time parsing come from `time`,
// whose range is the years -9999..=9999. A value outside that range can be
// built in Rust but has no ISO text; it prints as a labelled out-of-range
// string, never a panic. ISO 8601 durations and base64 are hand-written: `time`
// has no duration text form, and base64 is not worth a dependency.
// ---------------------------------------------------------------------------

/// Julian day number of 1970-01-01.
const UNIX_EPOCH_JULIAN_DAY: i32 = 2_440_588;
const MICROS_PER_SECOND: i64 = 1_000_000;
const SECONDS_PER_DAY: i64 = 86_400;
const NANOS_PER_SECOND: i64 = 1_000_000_000;
const NANOS_PER_MINUTE: i64 = 60 * NANOS_PER_SECOND;
const NANOS_PER_HOUR: i64 = 60 * NANOS_PER_MINUTE;

fn date_from_days(days: i32) -> Option<time::Date> {
    let julian = days.checked_add(UNIX_EPOCH_JULIAN_DAY)?;
    time::Date::from_julian_day(julian).ok()
}

/// `YYYY-MM-DD`; years before 0 use the signed six-digit ISO form `-YYYYYY`.
fn format_date(date: time::Date) -> String {
    let (year, month, day) = date.to_calendar_date();
    let month = u8::from(month);
    if year < 0 {
        format!("-{:06}-{month:02}-{day:02}", -year)
    } else {
        format!("{year:04}-{month:02}-{day:02}")
    }
}

/// Days since 1970-01-01 as an ISO 8601 date, `2024-01-15`.
pub fn date_to_iso8601(days: i32) -> String {
    match date_from_days(days) {
        Some(date) => format_date(date),
        None => format!("date out of range ({days} days from 1970-01-01)"),
    }
}

/// An ISO 8601 date to days since 1970-01-01.
///
/// Refuses anything carrying a time of day: a date is only a date.
pub fn iso8601_to_date(s: &str) -> Option<i32> {
    if s.contains(['T', 't']) {
        return None;
    }
    let date =
        time::Date::parse(s, &time::format_description::well_known::Iso8601::DEFAULT).ok()?;
    Some(date.to_julian_day() - UNIX_EPOCH_JULIAN_DAY)
}

/// Microseconds since the epoch as `YYYY-MM-DDTHH:MM:SS[.ffffff]`, or `None`
/// outside the calendar range.
fn format_local_micros(micros: i64) -> Option<String> {
    let secs = micros.div_euclid(MICROS_PER_SECOND);
    let sub = micros.rem_euclid(MICROS_PER_SECOND);
    let days = i32::try_from(secs.div_euclid(SECONDS_PER_DAY)).ok()?;
    let sod = secs.rem_euclid(SECONDS_PER_DAY);
    let date = format_date(date_from_days(days)?);
    let (h, m, s) = (sod / 3600, sod / 60 % 60, sod % 60);
    Some(if sub == 0 {
        format!("{date}T{h:02}:{m:02}:{s:02}")
    } else {
        format!("{date}T{h:02}:{m:02}:{s:02}.{sub:06}")
    })
}

/// Microseconds since 1970-01-01T00:00:00 (no zone) as an ISO 8601 local
/// date-time, `2024-01-15T12:34:56.789012`. Whole seconds print without a
/// fraction.
pub fn local_datetime_to_iso8601(micros: i64) -> String {
    format_local_micros(micros)
        .unwrap_or_else(|| format!("local datetime out of range ({micros} µs from 1970-01-01)"))
}

/// An ISO 8601 date-time with no offset to microseconds since the epoch.
///
/// Refuses input that carries an offset or `Z` — that is a `DateTime`.
/// Sub-microsecond digits are truncated toward the earlier instant.
pub fn iso8601_to_local_datetime(s: &str) -> Option<i64> {
    use time::format_description::well_known::Iso8601;
    if time::OffsetDateTime::parse(s, &Iso8601::DEFAULT).is_ok() {
        return None;
    }
    let dt = time::PrimitiveDateTime::parse(s, &Iso8601::DEFAULT).ok()?;
    nanos_to_micros_floor(dt.assume_utc().unix_timestamp_nanos())
}

fn nanos_to_micros_floor(nanos: i128) -> Option<i64> {
    i64::try_from(nanos.div_euclid(1_000)).ok()
}

/// An instant plus its offset as an ISO 8601 date-time with offset,
/// `2024-01-15T12:34:56.789012+05:30`. UTC prints as `+00:00`.
pub fn datetime_to_iso8601(micros_utc: i64, offset_seconds: i16) -> String {
    let local = micros_utc.checked_add(i64::from(offset_seconds) * MICROS_PER_SECOND);
    let Some(local) = local.and_then(format_local_micros) else {
        return format!(
            "datetime out of range ({micros_utc} µs from 1970-01-01 UTC, offset {offset_seconds} s)"
        );
    };
    let sign = if offset_seconds < 0 { '-' } else { '+' };
    let off = offset_seconds.unsigned_abs();
    let (h, m, s) = (off / 3600, off / 60 % 60, off % 60);
    if s == 0 {
        format!("{local}{sign}{h:02}:{m:02}")
    } else {
        format!("{local}{sign}{h:02}:{m:02}:{s:02}")
    }
}

/// An ISO 8601 date-time with an offset (or `Z`) to
/// `(micros_utc, offset_seconds)`.
///
/// Refuses input with no offset — that is a `LocalDateTime` — and offsets that
/// do not fit `offset_seconds: i16` (beyond ±9:06:07).
pub fn iso8601_to_datetime(s: &str) -> Option<(i64, i16)> {
    let dt =
        time::OffsetDateTime::parse(s, &time::format_description::well_known::Iso8601::DEFAULT)
            .ok()?;
    let offset = i16::try_from(dt.offset().whole_seconds()).ok()?;
    Some((nanos_to_micros_floor(dt.unix_timestamp_nanos())?, offset))
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

/// The wire tag and text of a format-V11 value — `("$date", "2024-01-15")` —
/// or `None` for the types JSON carries untagged.
///
/// One place names the tags and the text forms, so every encoder (HTTP and
/// MCP JSON, export, CLI and Arrow text) writes the same thing and
/// [`value_from_tagged_text`] reads all of it back.
pub fn tagged_text(v: &Value) -> Option<(&'static str, String)> {
    match v {
        Value::Int(_)
        | Value::Float(_)
        | Value::Str(_)
        | Value::Bool(_)
        | Value::List(_)
        | Value::Map(_) => None,
        Value::Date(days) => Some((TAG_DATE, date_to_iso8601(*days))),
        Value::LocalDateTime(us) => Some((TAG_LOCAL_DATETIME, local_datetime_to_iso8601(*us))),
        Value::DateTime {
            micros_utc,
            offset_seconds,
        } => Some((
            TAG_DATETIME,
            datetime_to_iso8601(*micros_utc, *offset_seconds),
        )),
        Value::Duration {
            months,
            days,
            nanos,
        } => Some((TAG_DURATION, duration_to_iso8601(*months, *days, *nanos))),
        Value::Binary(bytes) => Some((TAG_BINARY, base64_encode(bytes))),
    }
}

/// The value a wire tag and its text name, or `None` when the tag is not one
/// of the five or the text does not parse.
pub fn value_from_tagged_text(tag: &str, text: &str) -> Option<Value> {
    match tag {
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
        _ => None,
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
        // Year 1 BCE is ISO year 0; earlier years use the signed six-digit form.
        assert_eq!(date_to_iso8601(-719_528), "0000-01-01");
        let neg = iso8601_to_date("-000001-01-01").unwrap();
        assert_eq!(date_to_iso8601(neg), "-000001-01-01");
        // Edges of the representable range roundtrip.
        for s in ["-009999-01-01", "9999-12-31"] {
            assert_eq!(date_to_iso8601(iso8601_to_date(s).unwrap()), s);
        }
    }

    #[test]
    fn date_iso8601_refuses_malformed_and_time_bearing_input() {
        for bad in [
            "",
            "not-a-date",
            "2024-13-01",
            "2024-02-30",
            "2024-01-15T00:00",
            "2024-01-15T12:00:00Z",
            " 2024-01-15",
        ] {
            assert_eq!(iso8601_to_date(bad), None, "{bad:?}");
        }
        // Out of the supported range: a defined fallback, never a panic.
        assert!(date_to_iso8601(i32::MAX).contains("out of range"));
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
        // Whole seconds print without a fraction; parsing accepts both.
        assert_eq!(local_datetime_to_iso8601(0), "1970-01-01T00:00:00");
        assert_eq!(iso8601_to_local_datetime("1970-01-01T00:00:00"), Some(0));
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
    }

    #[test]
    fn local_datetime_iso8601_refuses_offsets_and_malformed() {
        for bad in [
            "2024-01-15T12:34:56+01:00",
            "2024-01-15T12:34:56Z",
            "2024-01-15",
            "2024-01-15 12:34:56",
            "garbage",
        ] {
            assert_eq!(iso8601_to_local_datetime(bad), None, "{bad:?}");
        }
        assert!(local_datetime_to_iso8601(i64::MAX).contains("out of range"));
    }

    #[test]
    fn datetime_iso8601_roundtrip_preserves_offset() {
        let s = "2024-01-15T12:34:56.789012+05:30";
        let (micros, off) = iso8601_to_datetime(s).unwrap();
        assert_eq!(off, 19_800);
        // 12:34:56.789012 at +05:30 is 07:04:56.789012 UTC.
        assert_eq!(micros, 1_705_302_296_789_012);
        assert_eq!(datetime_to_iso8601(micros, off), s);
        // Same instant, different offsets: same micros_utc.
        let a = iso8601_to_datetime("2024-01-15T12:00:00+00:00").unwrap();
        let b = iso8601_to_datetime("2024-01-15T07:00:00-05:00").unwrap();
        assert_eq!(a.0, b.0);
        assert_ne!(a.1, b.1);
        assert_eq!(datetime_to_iso8601(b.0, b.1), "2024-01-15T07:00:00-05:00");
        // `Z` parses as +00:00 and prints as +00:00.
        let z = iso8601_to_datetime("2024-01-15T12:00:00Z").unwrap();
        assert_eq!(z, a);
        assert_eq!(datetime_to_iso8601(z.0, z.1), "2024-01-15T12:00:00+00:00");
    }

    #[test]
    fn datetime_iso8601_refuses_missing_or_unrepresentable_offset() {
        assert_eq!(iso8601_to_datetime("2024-01-15T12:00:00"), None);
        assert_eq!(iso8601_to_datetime("2024-01-15"), None);
        // offset_seconds is i16 (contract C3): ±9:06:07 is the widest it holds.
        assert_eq!(
            iso8601_to_datetime("2024-01-15T12:00:00+09:00").unwrap().1,
            32_400
        );
        assert_eq!(iso8601_to_datetime("2024-01-15T12:00:00+10:00"), None);
        assert_eq!(iso8601_to_datetime("2024-01-15T12:00:00-10:00"), None);
        assert!(datetime_to_iso8601(i64::MAX, 0).contains("out of range"));
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
        let tags = ["$date", "$ldt", "$dt", "$dur", "$binary"];
        for (v, want_tag) in vals.iter().zip(tags) {
            let (tag, text) = tagged_text(v).unwrap();
            assert_eq!(tag, want_tag);
            assert_eq!(value_from_tagged_text(tag, &text).as_ref(), Some(v));
        }
        assert_eq!(tagged_text(&Value::Str("2024-01-15".into())), None);
        assert_eq!(value_from_tagged_text("$date", "nope"), None);
        assert_eq!(value_from_tagged_text("$time", "12:00"), None);
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
