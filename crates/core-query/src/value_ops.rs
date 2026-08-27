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
        _ => false,
    }
}

fn class_rank(v: &Value) -> u8 {
    match v {
        Value::Int(_) | Value::Float(_) => 0,
        Value::Str(_) => 1,
        Value::Bool(_) => 2,
        Value::List(_) => 3,
        // Map sorts after all other types.
        Value::Map(_) => 4,
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
}
