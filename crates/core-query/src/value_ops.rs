use core_storage::Value;
use std::cmp::Ordering;

pub fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Float(x), Value::Float(y)) => x == y,
        (Value::Int(x), Value::Float(y)) => *x as f64 == *y,
        (Value::Float(x), Value::Int(y)) => *x == *y as f64,
        (Value::Str(x), Value::Str(y)) => x == y,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::List(x), Value::List(y)) => {
            x.len() == y.len() && x.iter().zip(y.iter()).all(|(l, r)| values_equal(l, r))
        }
        (Value::Map(x), Value::Map(y)) => {
            x.len() == y.len()
                && x.iter()
                    .zip(y.iter())
                    .all(|((k1, v1), (k2, v2))| k1 == k2 && values_equal(v1, v2))
        }
        (Value::Date(x), Value::Date(y)) => x == y,
        (Value::LocalDateTime(x), Value::LocalDateTime(y)) => x == y,
        // The same instant is the same DateTime whatever offset it was written
        // with: the offset is display-only.
        (Value::DateTime { micros_utc: x, .. }, Value::DateTime { micros_utc: y, .. }) => x == y,
        // Component-wise: one month is not 30 days.
        (
            Value::Duration {
                months: m1,
                days: d1,
                nanos: n1,
            },
            Value::Duration {
                months: m2,
                days: d2,
                nanos: n2,
            },
        ) => (m1, d1, n1) == (m2, d2, n2),
        (Value::Binary(x), Value::Binary(y)) => x == y,
        _ => false,
    }
}

fn class_rank(v: &Value) -> u8 {
    match v {
        Value::Int(_) | Value::Float(_) => 0,
        Value::Str(_) => 1,
        Value::Bool(_) => 2,
        Value::List(_) => 3,
        // Each temporal type is its own class: a Date never compares with a
        // DateTime by implicit coercion.
        Value::Date(_) => 4,
        Value::LocalDateTime(_) => 5,
        Value::DateTime { .. } => 6,
        Value::Duration { .. } => 7,
        Value::Binary(_) => 8,
        // Map sorts after all other types.
        Value::Map(_) => 9,
    }
}

fn as_f64(v: &Value) -> f64 {
    match v {
        Value::Int(i) => *i as f64,
        Value::Float(f) => *f,
        _ => unreachable!("as_f64 only for numeric class"),
    }
}

pub fn cmp_values(a: &Value, b: &Value) -> Ordering {
    let ra = class_rank(a);
    let rb = class_rank(b);
    if ra != rb {
        return ra.cmp(&rb);
    }
    match (a, b) {
        (Value::Int(_) | Value::Float(_), Value::Int(_) | Value::Float(_)) => {
            as_f64(a).total_cmp(&as_f64(b))
        }
        (Value::Str(x), Value::Str(y)) => x.cmp(y),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        (Value::List(x), Value::List(y)) => {
            for (l, r) in x.iter().zip(y.iter()) {
                let c = cmp_values(l, r);
                if c != Ordering::Equal {
                    return c;
                }
            }
            x.len().cmp(&y.len())
        }
        (Value::Map(x), Value::Map(y)) => {
            // BTreeMap iterates in key-sorted order; compare key then value.
            for ((k1, v1), (k2, v2)) in x.iter().zip(y.iter()) {
                let ck = k1.cmp(k2);
                if ck != Ordering::Equal {
                    return ck;
                }
                let cv = cmp_values(v1, v2);
                if cv != Ordering::Equal {
                    return cv;
                }
            }
            x.len().cmp(&y.len())
        }
        (Value::Date(x), Value::Date(y)) => x.cmp(y),
        (Value::LocalDateTime(x), Value::LocalDateTime(y)) => x.cmp(y),
        // By instant; the offset only breaks ties so a sort is stable.
        (
            Value::DateTime {
                micros_utc: x,
                offset_seconds: ox,
            },
            Value::DateTime {
                micros_utc: y,
                offset_seconds: oy,
            },
        ) => (x, ox).cmp(&(y, oy)),
        // Durations are not totally ordered, so `<` refuses them
        // (`filter::is_orderable_pair`); this component-wise order exists only
        // so ORDER BY over a duration column is deterministic.
        (
            Value::Duration {
                months: m1,
                days: d1,
                nanos: n1,
            },
            Value::Duration {
                months: m2,
                days: d2,
                nanos: n2,
            },
        ) => (m1, d1, n1).cmp(&(m2, d2, n2)),
        (Value::Binary(x), Value::Binary(y)) => x.cmp(y),
        _ => unreachable!("same class rank implies matching variants"),
    }
}

pub fn cmp_optional(a: Option<&Value>, b: Option<&Value>, descending: bool) -> Ordering {
    match (a, b) {
        (None, None) => Ordering::Equal,
        // None sorts LAST regardless of ascending/descending.
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(x), Some(y)) => {
            let c = cmp_values(x, y);
            if descending {
                c.reverse()
            } else {
                c
            }
        }
    }
}

/// One element of a list value, openCypher subscript semantics.
///
/// A negative index counts from the end. A non-list base, a non-integer
/// index, or an out-of-range index all give null — a subscript reads like a
/// property that is not there, never an error.
pub fn index_list(base: Option<Value>, index: Option<Value>) -> Option<Value> {
    let (Some(Value::List(items)), Some(idx)) = (base, index) else {
        return None;
    };
    let i = match idx {
        Value::Int(n) => n,
        Value::Float(f) if f.fract() == 0.0 && f.is_finite() => f as i64,
        _ => return None,
    };
    let len = i64::try_from(items.len()).ok()?;
    let pos = if i < 0 { len.checked_add(i)? } else { i };
    if pos < 0 || pos >= len {
        return None;
    }
    items.into_iter().nth(pos as usize)
}

#[cfg(test)]
mod tests {
    use super::{cmp_optional, cmp_values, values_equal};
    use core_storage::Value;
    use std::cmp::Ordering::*;

    #[test]
    fn int_equals_float_numerically() {
        assert!(values_equal(&Value::Int(2), &Value::Float(2.0)));
        assert!(values_equal(&Value::Float(2.0), &Value::Int(2)));
        assert!(!values_equal(&Value::Int(2), &Value::Float(2.1)));
    }

    #[test]
    fn same_variant_equality() {
        assert!(values_equal(&Value::Int(1), &Value::Int(1)));
        assert!(!values_equal(&Value::Int(1), &Value::Int(2)));
        assert!(values_equal(
            &Value::Str("a".into()),
            &Value::Str("a".into())
        ));
        assert!(!values_equal(
            &Value::Str("a".into()),
            &Value::Str("b".into())
        ));
        assert!(values_equal(&Value::Bool(true), &Value::Bool(true)));
        assert!(!values_equal(&Value::Bool(true), &Value::Bool(false)));
        assert!(values_equal(&Value::Float(1.5), &Value::Float(1.5)));
        assert!(values_equal(
            &Value::List(vec![Value::Int(1), Value::Float(2.0)]),
            &Value::List(vec![Value::Float(1.0), Value::Int(2)]),
        ));
        assert!(!values_equal(
            &Value::List(vec![Value::Int(1)]),
            &Value::List(vec![Value::Int(1), Value::Int(2)]),
        ));
    }

    #[test]
    fn cross_variant_not_equal_except_int_float() {
        assert!(!values_equal(&Value::Str("2".into()), &Value::Int(2)));
        assert!(!values_equal(&Value::Bool(true), &Value::Int(1)));
        assert!(!values_equal(&Value::List(vec![]), &Value::Str("".into())));
        assert!(!values_equal(&Value::Bool(false), &Value::Float(0.0)));
    }

    #[test]
    fn cmp_values_class_ranks() {
        // numeric < Str < Bool < List
        assert_eq!(cmp_values(&Value::Int(99), &Value::Str("a".into())), Less);
        assert_eq!(cmp_values(&Value::Float(1.0), &Value::Bool(false)), Less);
        assert_eq!(
            cmp_values(&Value::Str("z".into()), &Value::Bool(false)),
            Less
        );
        assert_eq!(cmp_values(&Value::Bool(true), &Value::List(vec![])), Less);
        assert_eq!(cmp_values(&Value::List(vec![]), &Value::Int(0)), Greater);

        // numerics via f64::total_cmp
        assert_eq!(cmp_values(&Value::Int(1), &Value::Float(1.5)), Less);
        assert_eq!(cmp_values(&Value::Int(2), &Value::Float(2.0)), Equal);
        // f64::total_cmp: -0.0 < +0.0 (unlike IEEE ==).
        assert_eq!(cmp_values(&Value::Float(-0.0), &Value::Float(0.0)), Less);

        // str lexicographic
        assert_eq!(
            cmp_values(&Value::Str("a".into()), &Value::Str("b".into())),
            Less
        );

        // bool false < true
        assert_eq!(cmp_values(&Value::Bool(false), &Value::Bool(true)), Less);

        // list elementwise, then length
        assert_eq!(
            cmp_values(
                &Value::List(vec![Value::Int(1), Value::Int(2)]),
                &Value::List(vec![Value::Int(1), Value::Int(3)]),
            ),
            Less
        );
        assert_eq!(
            cmp_values(
                &Value::List(vec![Value::Int(1)]),
                &Value::List(vec![Value::Int(1), Value::Int(2)]),
            ),
            Less
        );
        // list elements use class ranks
        assert_eq!(
            cmp_values(
                &Value::List(vec![Value::Int(1)]),
                &Value::List(vec![Value::Str("a".into())]),
            ),
            Less
        );
    }

    #[test]
    fn cmp_optional_none_last_both_directions() {
        let a = Value::Int(1);
        let b = Value::Int(2);
        assert_eq!(cmp_optional(None, Some(&a), false), Greater);
        assert_eq!(cmp_optional(Some(&a), None, false), Less);
        assert_eq!(cmp_optional(None, Some(&a), true), Greater);
        assert_eq!(cmp_optional(Some(&a), None, true), Less);
        assert_eq!(cmp_optional(None, None, false), Equal);
        assert_eq!(cmp_optional(None, None, true), Equal);
        assert_eq!(cmp_optional(Some(&a), Some(&b), false), Less);
        assert_eq!(cmp_optional(Some(&a), Some(&b), true), Greater);
        assert_eq!(cmp_optional(Some(&a), Some(&a), false), Equal);
        assert_eq!(cmp_optional(Some(&a), Some(&a), true), Equal);
    }

    fn dt(micros_utc: i64, offset_seconds: i16) -> Value {
        Value::DateTime {
            micros_utc,
            offset_seconds,
        }
    }

    fn dur(months: i32, days: i32, nanos: i64) -> Value {
        Value::Duration {
            months,
            days,
            nanos,
        }
    }

    #[test]
    fn new_types_equality() {
        assert!(values_equal(&Value::Date(5), &Value::Date(5)));
        assert!(!values_equal(&Value::Date(5), &Value::Date(6)));
        assert!(values_equal(
            &Value::LocalDateTime(7),
            &Value::LocalDateTime(7)
        ));
        // DateTime: the same instant at two offsets is equal.
        assert!(values_equal(&dt(1_000, 0), &dt(1_000, -18_000)));
        assert!(!values_equal(&dt(1_000, 0), &dt(1_001, 0)));
        // Duration: component-wise, a month is not 30 days.
        assert!(values_equal(&dur(1, 2, 3), &dur(1, 2, 3)));
        assert!(!values_equal(&dur(1, 0, 0), &dur(0, 30, 0)));
        assert!(values_equal(
            &Value::Binary(vec![1, 2]),
            &Value::Binary(vec![1, 2])
        ));
        // No implicit coercion between types.
        assert!(!values_equal(&Value::Date(0), &Value::LocalDateTime(0)));
        assert!(!values_equal(&Value::Date(0), &Value::Int(0)));
        assert!(!values_equal(&dt(0, 0), &Value::LocalDateTime(0)));
        assert!(!values_equal(
            &Value::Binary(b"a".to_vec()),
            &Value::Str("a".into())
        ));
    }

    #[test]
    fn new_types_ordering_never_panics_and_is_total() {
        let all = [
            Value::Int(0),
            Value::Str("a".into()),
            Value::Bool(true),
            Value::List(vec![]),
            Value::Date(1),
            Value::LocalDateTime(1),
            dt(1, 0),
            dur(0, 0, 1),
            Value::Binary(vec![1]),
            Value::Map(Default::default()),
        ];
        // Every pair compares without panicking; distinct types are ordered
        // by class, and the order is antisymmetric.
        for (i, a) in all.iter().enumerate() {
            for (j, b) in all.iter().enumerate() {
                assert_eq!(cmp_values(a, b), i.cmp(&j), "{a:?} vs {b:?}");
            }
        }
        assert_eq!(cmp_values(&Value::Date(-1), &Value::Date(0)), Less);
        assert_eq!(
            cmp_values(&Value::LocalDateTime(9), &Value::LocalDateTime(2)),
            Greater
        );
        // DateTime orders by instant; the offset only breaks ties.
        assert_eq!(cmp_values(&dt(1, 3_600), &dt(2, 0)), Less);
        assert_eq!(cmp_values(&dt(1, 0), &dt(1, 3_600)), Less);
        assert_eq!(
            cmp_values(&Value::Binary(vec![0, 9]), &Value::Binary(vec![1])),
            Less
        );
        assert_eq!(cmp_values(&dur(1, 0, 0), &dur(0, 99, 0)), Greater);
    }
}
