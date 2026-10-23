//! Temporal values as Bolt structures, read from query parameters.
//!
//! The engine's dates, times and durations travel as PackStream structures;
//! a byte blob travels as PackStream Bytes (`0xCC`/`0xCD`/`0xCE`), needing no
//! structure. Results are written by [`pack_value`](super::run::pack_value);
//! this module reads the same structures back from a `RUN`'s parameters.
//!
//! Field order, as the Bolt structure specification gives it:
//!
//! | tag | structure | fields |
//! |---|---|---|
//! | `0x44` `D` | Date | `days` since 1970-01-01 |
//! | `0x64` `d` | LocalDateTime | `seconds` since 1970-01-01T00:00:00, `nanoseconds` |
//! | `0x49` `I` | DateTime (Bolt 5) | `seconds` since the epoch in UTC, `nanoseconds`, `tz_offset_seconds` |
//! | `0x46` `F` | DateTime (Bolt 4.4) | `seconds` since the epoch in local time (UTC + offset), `nanoseconds`, `tz_offset_seconds` |
//! | `0x45` `E` | Duration | `months`, `days`, `seconds`, `nanoseconds` |
//!
//! Which DateTime form a connection uses follows the negotiated version: the
//! UTC form from Bolt 5.0, the local-seconds form before it. (A 4.4 driver
//! switches to the UTC form only when the server confirms the `utc` patch in
//! the `HELLO` reply; this server never offers it, so 4.4 stays on `0x46`.)
//! The other DateTime form is refused, as a driver at that version never
//! sends it.
//!
//! The engine keeps microseconds. A LocalDateTime or DateTime whose
//! nanoseconds are not a whole number of microseconds is refused, never
//! truncated. A Duration's nanoseconds are kept exactly. A value outside what
//! the engine's fields hold is refused.
//!
//! Not supported, and refused with an error naming the type: DateTime with a
//! named time zone (`0x69`, `0x66`), Time (`0x54`), LocalTime (`0x74`),
//! Point2D (`0x58`), Point3D (`0x59`), the graph structures, and any unknown
//! tag. A structure with the wrong number or type of fields is a malformed
//! request.

use mushroomdb::Value;

use crate::error::BoltError;
use crate::packstream::{
    PackStruct, PackValue, TAG_NODE, TAG_PATH, TAG_RELATIONSHIP, TAG_UNBOUND_RELATIONSHIP,
};

/// Date: `(days)`.
pub const TAG_DATE: u8 = 0x44;
/// LocalDateTime: `(seconds, nanoseconds)`.
pub const TAG_LOCAL_DATE_TIME: u8 = 0x64;
/// DateTime from Bolt 5.0: `(seconds_utc, nanoseconds, tz_offset_seconds)`.
pub const TAG_DATE_TIME: u8 = 0x49;
/// DateTime before Bolt 5.0: `(seconds_local, nanoseconds, tz_offset_seconds)`.
pub const TAG_LEGACY_DATE_TIME: u8 = 0x46;
/// DateTime with a named zone, from Bolt 5.0: `(seconds_utc, nanoseconds, tz_id)`.
pub const TAG_DATE_TIME_ZONE_ID: u8 = 0x69;
/// DateTime with a named zone, before Bolt 5.0.
pub const TAG_LEGACY_DATE_TIME_ZONE_ID: u8 = 0x66;
/// Duration: `(months, days, seconds, nanoseconds)`.
pub const TAG_DURATION: u8 = 0x45;
/// Time: `(nanoseconds, tz_offset_seconds)`.
pub const TAG_TIME: u8 = 0x54;
/// LocalTime: `(nanoseconds)`.
pub const TAG_LOCAL_TIME: u8 = 0x74;
/// Point2D: `(srid, x, y)`.
pub const TAG_POINT_2D: u8 = 0x58;
/// Point3D: `(srid, x, y, z)`.
pub const TAG_POINT_3D: u8 = 0x59;

const MICROS_PER_SECOND: i64 = 1_000_000;
const NANOS_PER_MICRO: i64 = 1_000;
const NANOS_PER_SECOND: i64 = 1_000_000_000;
/// The widest UTC offset a `DateTime` holds: ±18:00, as the engine's
/// `Value::DateTime` documents.
const MAX_OFFSET_SECONDS: i64 = 18 * 3600;

/// Whether `version` carries DateTime as the UTC-based structure (`0x49`)
/// rather than the local-seconds one (`0x46`).
pub fn uses_utc_date_time(version: (u8, u8)) -> bool {
    version >= (5, 0)
}

/// The engine value a parameter structure stands for. `name` is the
/// parameter, for the error message.
pub fn struct_param(name: &str, s: &PackStruct, version: (u8, u8)) -> Result<Value, BoltError> {
    let utc = uses_utc_date_time(version);
    match s.tag {
        TAG_DATE => {
            let [days] = ints(name, s, "Date")?;
            let days = i32::try_from(days)
                .map_err(|_| out_of_range(name, "Date", "the day count does not fit 32 bits"))?;
            Ok(Value::Date(days))
        }
        TAG_LOCAL_DATE_TIME => {
            let [seconds, nanos] = ints(name, s, "LocalDateTime")?;
            Ok(Value::LocalDateTime(micros(
                name,
                "LocalDateTime",
                seconds,
                nanos,
            )?))
        }
        TAG_DATE_TIME if utc => {
            let [seconds, nanos, offset] = ints(name, s, "DateTime")?;
            date_time(name, seconds, nanos, offset)
        }
        TAG_LEGACY_DATE_TIME if !utc => {
            let [local_seconds, nanos, offset] = ints(name, s, "DateTime")?;
            let offset_checked = check_offset(name, offset)?;
            let seconds = local_seconds
                .checked_sub(i64::from(offset_checked))
                .ok_or_else(|| out_of_range(name, "DateTime", "the instant overflows"))?;
            date_time(name, seconds, nanos, offset)
        }
        TAG_DATE_TIME | TAG_LEGACY_DATE_TIME => Err(BoltError::Invalid(format!(
            "parameter {name:?}: DateTime structure 0x{:02X} is not the form Bolt {}.{} uses \
             (expected 0x{:02X})",
            s.tag,
            version.0,
            version.1,
            if utc {
                TAG_DATE_TIME
            } else {
                TAG_LEGACY_DATE_TIME
            },
        ))),
        TAG_DURATION => {
            let [months, days, seconds, nanos] = ints(name, s, "Duration")?;
            let months = i32::try_from(months)
                .map_err(|_| out_of_range(name, "Duration", "months do not fit 32 bits"))?;
            let days = i32::try_from(days)
                .map_err(|_| out_of_range(name, "Duration", "days do not fit 32 bits"))?;
            // In 128 bits: the encoder floors the seconds, so the most
            // negative value only fits once the nanoseconds are added back.
            let total = i128::from(seconds) * i128::from(NANOS_PER_SECOND) + i128::from(nanos);
            let nanos = i64::try_from(total).map_err(|_| {
                out_of_range(name, "Duration", "seconds and nanoseconds overflow 64 bits")
            })?;
            Ok(Value::Duration {
                months,
                days,
                nanos,
            })
        }
        tag => Err(BoltError::Invalid(match unsupported_name(tag) {
            Some(kind) => format!("parameter {name:?}: {kind} values are not supported"),
            None => format!("parameter {name:?}: unknown structure tag 0x{tag:02X}"),
        })),
    }
}

/// The name of a structure this server knows but does not accept as a value.
fn unsupported_name(tag: u8) -> Option<&'static str> {
    Some(match tag {
        TAG_DATE_TIME_ZONE_ID | TAG_LEGACY_DATE_TIME_ZONE_ID => {
            "DateTime with a named time zone (only a UTC offset is kept)"
        }
        TAG_TIME => "Time",
        TAG_LOCAL_TIME => "LocalTime",
        TAG_POINT_2D => "Point2D",
        TAG_POINT_3D => "Point3D",
        TAG_NODE => "Node",
        TAG_RELATIONSHIP => "Relationship",
        TAG_UNBOUND_RELATIONSHIP => "UnboundRelationship",
        TAG_PATH => "Path",
        _ => return None,
    })
}

/// The fields of `s` as exactly `N` integers, or a malformed-request error.
fn ints<const N: usize>(name: &str, s: &PackStruct, kind: &str) -> Result<[i64; N], BoltError> {
    let malformed = || {
        BoltError::Invalid(format!(
            "parameter {name:?}: malformed {kind} structure: expected {N} integer field{}, got {}",
            if N == 1 { "" } else { "s" },
            describe(&s.fields),
        ))
    };
    if s.fields.len() != N {
        return Err(malformed());
    }
    let mut out = [0i64; N];
    for (slot, field) in out.iter_mut().zip(&s.fields) {
        match field {
            PackValue::Int(n) => *slot = *n,
            _ => return Err(malformed()),
        }
    }
    Ok(out)
}

fn describe(fields: &[PackValue]) -> String {
    let kinds: Vec<&str> = fields
        .iter()
        .map(|f| match f {
            PackValue::Null => "null",
            PackValue::Bool(_) => "boolean",
            PackValue::Int(_) => "integer",
            PackValue::Float(_) => "float",
            PackValue::Bytes(_) => "bytes",
            PackValue::String(_) => "string",
            PackValue::List(_) => "list",
            PackValue::Map(_) => "map",
            PackValue::Struct(_) => "structure",
        })
        .collect();
    format!("({})", kinds.join(", "))
}

fn out_of_range(name: &str, kind: &str, why: &str) -> BoltError {
    BoltError::Invalid(format!("parameter {name:?}: {kind} is out of range: {why}"))
}

/// `seconds` plus `nanos` as microseconds. `nanos` must be in
/// `0..1_000_000_000` and a whole number of microseconds.
fn micros(name: &str, kind: &str, seconds: i64, nanos: i64) -> Result<i64, BoltError> {
    if !(0..NANOS_PER_SECOND).contains(&nanos) {
        return Err(BoltError::Invalid(format!(
            "parameter {name:?}: malformed {kind} structure: nanoseconds {nanos} are outside \
             0..=999999999"
        )));
    }
    if nanos % NANOS_PER_MICRO != 0 {
        return Err(BoltError::Invalid(format!(
            "parameter {name:?}: {kind} has sub-microsecond precision ({nanos} ns); \
             mushroomdb keeps microseconds and does not round"
        )));
    }
    // In 128 bits, as for a Duration: floored seconds times a million can
    // pass i64::MIN before the microseconds bring it back.
    let total =
        i128::from(seconds) * i128::from(MICROS_PER_SECOND) + i128::from(nanos / NANOS_PER_MICRO);
    i64::try_from(total)
        .map_err(|_| out_of_range(name, kind, "the microsecond count overflows 64 bits"))
}

fn check_offset(name: &str, offset: i64) -> Result<i32, BoltError> {
    if offset.unsigned_abs() > MAX_OFFSET_SECONDS.unsigned_abs() {
        return Err(out_of_range(
            name,
            "DateTime",
            &format!("UTC offset {offset} s is beyond ±18:00"),
        ));
    }
    Ok(offset as i32)
}

fn date_time(name: &str, seconds_utc: i64, nanos: i64, offset: i64) -> Result<Value, BoltError> {
    let offset_seconds = check_offset(name, offset)?;
    Ok(Value::DateTime {
        micros_utc: micros(name, "DateTime", seconds_utc, nanos)?,
        offset_seconds,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::run::pack_value;
    use crate::packstream::{pack, unpack};

    const V5: (u8, u8) = (5, 0);
    const V44: (u8, u8) = (4, 4);
    const VERSIONS: [(u8, u8); 2] = [V5, V44];

    fn int(n: i64) -> PackValue {
        PackValue::Int(n)
    }

    fn st(tag: u8, fields: Vec<PackValue>) -> PackStruct {
        PackStruct::new(tag, fields)
    }

    /// Decodes a parameter the way `RUN` does.
    fn param(value: &PackValue, version: (u8, u8)) -> Result<Value, BoltError> {
        let mut req_fields = vec![
            PackValue::String("RETURN $p".into()),
            PackValue::Map(vec![("p".into(), value.clone())]),
            PackValue::Map(vec![]),
        ];
        let request =
            crate::messages::run::RunRequest::from_fields(std::mem::take(&mut req_fields))
                .expect("a well-formed RUN");
        request.params(version).map(|mut p| p.remove("p").unwrap())
    }

    /// Engine value → PackStream → bytes → PackStream → engine value, at
    /// `version`. Returns the wire bytes.
    fn round_trip(value: Value, version: (u8, u8)) -> Vec<u8> {
        let packed = pack_value(&value, version, 0).expect("encodes");
        let mut bytes = Vec::new();
        pack(&packed, &mut bytes);
        let (decoded, used) = unpack(&bytes).expect("decodes");
        assert_eq!(used, bytes.len());
        assert_eq!(decoded, packed);
        assert_eq!(
            param(&decoded, version).expect("reads back"),
            value,
            "{version:?}"
        );
        bytes
    }

    fn message(result: Result<Value, BoltError>) -> String {
        match result {
            Err(BoltError::Invalid(m)) => m,
            other => panic!("expected a Request.Invalid refusal, got {other:?}"),
        }
    }

    #[test]
    fn round_trip_date() {
        for version in VERSIONS {
            for days in [0, 19_737, -1, -719_162, i32::MAX, i32::MIN] {
                let bytes = round_trip(Value::Date(days), version);
                assert_eq!(bytes[..2], [0xB1, 0x44]);
            }
        }
        let refused = param(&PackValue::Struct(st(TAG_DATE, vec![int(1 << 40)])), V5);
        assert!(message(refused).contains("out of range"));
    }

    #[test]
    fn round_trip_local_datetime() {
        for version in VERSIONS {
            for micros in [0, 1, -1, 1_705_322_096_789_012, i64::MAX, i64::MIN] {
                let bytes = round_trip(Value::LocalDateTime(micros), version);
                assert_eq!(bytes[..2], [0xB2, 0x64]);
            }
        }
    }

    #[test]
    fn round_trip_datetime() {
        let cases = [
            (0, 0),
            (1_705_322_096_789_012, 19_800),
            (-1, -3600),
            (1_500_000, MAX_OFFSET_SECONDS as i32),
            (-62_135_596_800_000_000, -(MAX_OFFSET_SECONDS as i32)),
        ];
        for (micros_utc, offset_seconds) in cases {
            let value = Value::DateTime {
                micros_utc,
                offset_seconds,
            };
            assert_eq!(round_trip(value.clone(), V5)[..2], [0xB3, 0x49]);
            assert_eq!(round_trip(value, V44)[..2], [0xB3, 0x46]);
        }
    }

    #[test]
    fn datetime_wire_fields_follow_the_version() {
        // 2024-01-15T12:34:56.789012+05:30, as both versions spell it.
        let seconds_utc = 1_705_322_096;
        let value = Value::DateTime {
            micros_utc: seconds_utc * MICROS_PER_SECOND + 789_012,
            offset_seconds: 19_800,
        };
        let utc = PackValue::Struct(st(
            TAG_DATE_TIME,
            vec![int(seconds_utc), int(789_012_000), int(19_800)],
        ));
        let legacy = PackValue::Struct(st(
            TAG_LEGACY_DATE_TIME,
            vec![int(seconds_utc + 19_800), int(789_012_000), int(19_800)],
        ));
        assert_eq!(pack_value(&value, V5, 0).unwrap(), utc);
        assert_eq!(pack_value(&value, V44, 0).unwrap(), legacy);
        assert_eq!(param(&utc, V5).unwrap(), value);
        assert_eq!(param(&legacy, V44).unwrap(), value);
        // A driver at a version never sends the other form.
        assert!(message(param(&legacy, V5)).contains("0x46"));
        assert!(message(param(&utc, V44)).contains("0x49"));
    }

    #[test]
    fn round_trip_duration() {
        let cases = [
            (0, 0, 0),
            (14, 3, 14_706_789_000_000),
            (-1, -2, -3_000_000_001),
            (i32::MAX, i32::MIN, i64::MAX),
            (i32::MIN, i32::MAX, i64::MIN),
            // A Duration keeps whole nanoseconds.
            (0, 0, 1),
        ];
        for version in VERSIONS {
            for (months, days, nanos) in cases {
                let value = Value::Duration {
                    months,
                    days,
                    nanos,
                };
                assert_eq!(round_trip(value, version)[..2], [0xB4, 0x45]);
            }
        }
        // Drivers may send nanoseconds that are not normalized into a second.
        let unnormalized = st(TAG_DURATION, vec![int(0), int(0), int(1), int(-1)]);
        assert_eq!(
            param(&PackValue::Struct(unnormalized), V5).unwrap(),
            Value::Duration {
                months: 0,
                days: 0,
                nanos: 999_999_999
            }
        );
    }

    #[test]
    fn a_duration_past_the_engine_fields_is_refused() {
        let over = |fields: Vec<PackValue>| {
            message(param(&PackValue::Struct(st(TAG_DURATION, fields)), V5))
        };
        assert!(over(vec![int(1 << 31), int(0), int(0), int(0)]).contains("months"));
        assert!(over(vec![int(0), int(-(1 << 31) - 1), int(0), int(0)]).contains("days"));
        assert!(over(vec![int(0), int(0), int(i64::MAX / 1_000), int(0)]).contains("overflow"));
        assert!(over(vec![
            int(0),
            int(0),
            int(i64::MAX / NANOS_PER_SECOND),
            int(NANOS_PER_SECOND)
        ])
        .contains("overflow"));
    }

    #[test]
    fn round_trip_binary() {
        for version in VERSIONS {
            assert_eq!(round_trip(Value::Binary(vec![]), version), [0xCC, 0x00]);
            assert_eq!(
                round_trip(Value::Binary(vec![0, 255]), version),
                [0xCC, 2, 0, 255]
            );
            assert_eq!(
                round_trip(Value::Binary(vec![7; 300]), version)[..3],
                [0xCD, 0x01, 0x2C]
            );
            assert_eq!(round_trip(Value::Binary(vec![1; 70_000]), version)[0], 0xCE);
        }
    }

    #[test]
    fn temporal_values_nest_in_lists_and_maps() {
        let value = Value::List(vec![
            Value::Date(1),
            Value::Map([("d".to_string(), Value::LocalDateTime(5))].into()),
        ]);
        for version in VERSIONS {
            round_trip(value.clone(), version);
        }
    }

    #[test]
    fn sub_microsecond_nanoseconds_are_refused_never_truncated() {
        let ldt = st(TAG_LOCAL_DATE_TIME, vec![int(0), int(1_500)]);
        let m = message(param(&PackValue::Struct(ldt), V5));
        assert!(m.contains("sub-microsecond") && m.contains("1500"), "{m}");
        let dt = st(TAG_DATE_TIME, vec![int(0), int(999_999_999), int(0)]);
        assert!(message(param(&PackValue::Struct(dt), V5)).contains("sub-microsecond"));
        let legacy = st(TAG_LEGACY_DATE_TIME, vec![int(0), int(1), int(0)]);
        assert!(message(param(&PackValue::Struct(legacy), V44)).contains("sub-microsecond"));
        // Exactly a microsecond is fine.
        let ok = st(TAG_LOCAL_DATE_TIME, vec![int(0), int(1_000)]);
        assert_eq!(
            param(&PackValue::Struct(ok), V5).unwrap(),
            Value::LocalDateTime(1)
        );
    }

    #[test]
    fn instants_past_the_engine_fields_are_refused() {
        let refused =
            |tag, fields, version| message(param(&PackValue::Struct(st(tag, fields)), version));
        assert!(refused(TAG_LOCAL_DATE_TIME, vec![int(i64::MAX), int(0)], V5).contains("overflow"));
        assert!(refused(TAG_LOCAL_DATE_TIME, vec![int(0), int(-1)], V5).contains("nanoseconds"));
        assert!(
            refused(TAG_LOCAL_DATE_TIME, vec![int(0), int(NANOS_PER_SECOND)], V5)
                .contains("nanoseconds")
        );
        assert!(
            refused(TAG_DATE_TIME, vec![int(0), int(0), int(18 * 3600 + 1)], V5).contains("18:00")
        );
        assert!(refused(
            TAG_LEGACY_DATE_TIME,
            vec![int(i64::MIN), int(0), int(60)],
            V44
        )
        .contains("overflow"));
    }

    #[test]
    fn unsupported_structures_are_refused_by_name() {
        let cases: [(u8, Vec<PackValue>, &str); 9] = [
            (
                TAG_DATE_TIME_ZONE_ID,
                vec![int(0), int(0), PackValue::String("Europe/Paris".into())],
                "named time zone",
            ),
            (
                TAG_LEGACY_DATE_TIME_ZONE_ID,
                vec![int(0), int(0), PackValue::String("UTC".into())],
                "named time zone",
            ),
            (TAG_TIME, vec![int(0), int(0)], "Time"),
            (TAG_LOCAL_TIME, vec![int(0)], "LocalTime"),
            (
                TAG_POINT_2D,
                vec![int(7203), PackValue::Float(1.0), PackValue::Float(2.0)],
                "Point2D",
            ),
            (
                TAG_POINT_3D,
                vec![
                    int(9157),
                    PackValue::Float(1.0),
                    PackValue::Float(2.0),
                    PackValue::Float(3.0),
                ],
                "Point3D",
            ),
            (
                TAG_NODE,
                vec![int(1), PackValue::List(vec![]), PackValue::Map(vec![])],
                "Node",
            ),
            (TAG_PATH, vec![], "Path"),
            (0x01, vec![], "unknown structure tag 0x01"),
        ];
        for version in VERSIONS {
            for (tag, fields, needle) in &cases {
                let m = message(param(&PackValue::Struct(st(*tag, fields.clone())), version));
                assert!(m.contains(needle), "{m}");
                assert!(m.contains("\"p\""), "names the parameter: {m}");
            }
        }
    }

    #[test]
    fn a_malformed_temporal_structure_is_a_malformed_request() {
        let cases = [
            st(TAG_DATE, vec![]),
            st(TAG_DATE, vec![int(1), int(2)]),
            st(TAG_DATE, vec![PackValue::String("2024-01-15".into())]),
            st(TAG_LOCAL_DATE_TIME, vec![int(1)]),
            st(
                TAG_DATE_TIME,
                vec![int(1), int(2), PackValue::String("+01:00".into())],
            ),
            st(TAG_DURATION, vec![int(1), int(2), int(3)]),
            st(
                TAG_DURATION,
                vec![int(1), int(2), int(3), PackValue::Float(4.0)],
            ),
        ];
        for s in cases {
            let m = message(param(&PackValue::Struct(s.clone()), V5));
            assert!(m.contains("malformed"), "{s:?}: {m}");
        }
    }

    /// Small deterministic generator, so the run is reproducible.
    struct XorShift(u64);

    impl XorShift {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    fn random_field(rng: &mut XorShift) -> PackValue {
        match rng.next() % 6 {
            0 => PackValue::Null,
            1 => PackValue::Float(rng.next() as f64),
            2 => PackValue::String("x".into()),
            3 => PackValue::Struct(st(TAG_DATE, vec![int(1)])),
            // Integers twice as often, with the edges well represented.
            _ => int(match rng.next() % 4 {
                0 => i64::MAX,
                1 => i64::MIN,
                2 => (rng.next() % 2_000_000_000) as i64 - 1_000_000_000,
                _ => rng.next() as i64,
            }),
        }
    }

    /// Random tags (biased towards the temporal ones), field counts and
    /// field values: every one decodes or is refused as a client error, and
    /// none panics. Whatever decodes re-encodes and decodes to itself.
    #[test]
    fn fuzz_struct_parameters_never_panic() {
        const TAGS: [u8; 8] = [
            TAG_DATE,
            TAG_LOCAL_DATE_TIME,
            TAG_DATE_TIME,
            TAG_LEGACY_DATE_TIME,
            TAG_DURATION,
            TAG_DATE_TIME_ZONE_ID,
            TAG_TIME,
            TAG_POINT_2D,
        ];
        let mut rng = XorShift(0x2545_F491_4F6C_DD1D);
        let (mut decoded, mut refused) = (0, 0);
        for _ in 0..50_000 {
            let tag = if rng.next().is_multiple_of(4) {
                rng.next() as u8
            } else {
                TAGS[(rng.next() % TAGS.len() as u64) as usize]
            };
            let count = (rng.next() % 6) as usize;
            let fields = (0..count).map(|_| random_field(&mut rng)).collect();
            let value = PackValue::Struct(st(tag, fields));
            let version = VERSIONS[(rng.next() % 2) as usize];
            match param(&value, version) {
                Ok(engine) => {
                    decoded += 1;
                    let again = pack_value(&engine, version, 0).expect("encodes");
                    assert_eq!(param(&again, version).unwrap(), engine);
                }
                Err(BoltError::Invalid(_)) => refused += 1,
                Err(other) => panic!("a parameter fault must be a client error: {other:?}"),
            }
        }
        assert!(
            decoded > 0 && refused > 0,
            "{decoded} ok, {refused} refused"
        );
    }
}
