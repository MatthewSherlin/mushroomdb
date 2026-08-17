use crate::value_ops::values_equal;
use core_storage::Value;
use std::cmp::Ordering;

#[derive(Debug, Clone, PartialEq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Filter {
    Cmp {
        field: String,
        op: CmpOp,
        value: Value,
    },
    And(Vec<Filter>),
    Or(Vec<Filter>),
    Not(Box<Filter>),
}

fn is_orderable_pair(lhs: &Value, rhs: &Value) -> bool {
    matches!(
        (lhs, rhs),
        (
            Value::Int(_) | Value::Float(_),
            Value::Int(_) | Value::Float(_)
        ) | (Value::Str(_), Value::Str(_))
    )
}

pub fn eval_cmp(op: &CmpOp, lhs: &Value, rhs: &Value) -> bool {
    match op {
        CmpOp::Eq => values_equal(lhs, rhs),
        CmpOp::Ne => !values_equal(lhs, rhs),
        CmpOp::Lt | CmpOp::Le | CmpOp::Gt | CmpOp::Ge => {
            if !is_orderable_pair(lhs, rhs) {
                return false;
            }
            let c = crate::value_ops::cmp_values(lhs, rhs);
            match op {
                CmpOp::Lt => c == Ordering::Less,
                CmpOp::Le => c != Ordering::Greater,
                CmpOp::Gt => c == Ordering::Greater,
                CmpOp::Ge => c != Ordering::Less,
                _ => unreachable!(),
            }
        }
    }
}

/// Missing field or non-comparable pair → false (no 3VL in v1).
/// `Ne` on a missing field is false, not true — absence is not a differing value.
pub fn eval_filter(f: &Filter, get: &dyn Fn(&str) -> Option<Value>) -> bool {
    match f {
        Filter::Cmp { field, op, value } => match get(field) {
            // Missing → false for every op, including Ne (see comment above).
            None => false,
            Some(lhs) => eval_cmp(op, &lhs, value),
        },
        Filter::And(parts) => parts.iter().all(|p| eval_filter(p, get)),
        Filter::Or(parts) => parts.iter().any(|p| eval_filter(p, get)),
        Filter::Not(inner) => !eval_filter(inner, get),
    }
}

#[cfg(test)]
mod tests {
    use super::{eval_cmp, eval_filter, CmpOp, Filter};
    use core_storage::Value;

    fn get_ada(field: &str) -> Option<Value> {
        match field {
            "age" => Some(Value::Int(30)),
            "name" => Some(Value::Str("ada".into())),
            "score" => Some(Value::Float(2.0)),
            _ => None,
        }
    }

    #[test]
    fn str_vs_int_not_equal_and_lt_false() {
        assert!(!eval_cmp(
            &CmpOp::Eq,
            &Value::Str("2".into()),
            &Value::Int(2)
        ));
        assert!(eval_cmp(
            &CmpOp::Ne,
            &Value::Str("2".into()),
            &Value::Int(2)
        ));
        assert!(!eval_cmp(
            &CmpOp::Lt,
            &Value::Str("a".into()),
            &Value::Int(1)
        ));
        assert!(!eval_cmp(
            &CmpOp::Le,
            &Value::Str("a".into()),
            &Value::Int(1)
        ));
        assert!(!eval_cmp(
            &CmpOp::Gt,
            &Value::Str("a".into()),
            &Value::Int(1)
        ));
        assert!(!eval_cmp(
            &CmpOp::Ge,
            &Value::Str("a".into()),
            &Value::Int(1)
        ));
    }

    #[test]
    fn missing_field_is_false_for_eq_and_ne() {
        // Missing field → false for every Cmp, including Ne (not true).
        // No three-valued logic in v1: absence is not a value that "differs".
        let get = |_: &str| None;
        let eq = Filter::Cmp {
            field: "x".into(),
            op: CmpOp::Eq,
            value: Value::Int(1),
        };
        let ne = Filter::Cmp {
            field: "x".into(),
            op: CmpOp::Ne,
            value: Value::Int(1),
        };
        assert!(!eval_filter(&eq, &get));
        assert!(!eval_filter(&ne, &get));
        assert!(!eval_filter(
            &Filter::Cmp {
                field: "x".into(),
                op: CmpOp::Lt,
                value: Value::Int(1),
            },
            &get
        ));
    }

    #[test]
    fn int_float_eq_and_numeric_str_ordering() {
        assert!(eval_cmp(&CmpOp::Eq, &Value::Int(2), &Value::Float(2.0)));
        assert!(!eval_cmp(&CmpOp::Ne, &Value::Int(2), &Value::Float(2.0)));
        assert!(eval_cmp(&CmpOp::Lt, &Value::Int(1), &Value::Float(2.0)));
        assert!(eval_cmp(&CmpOp::Le, &Value::Int(2), &Value::Float(2.0)));
        assert!(eval_cmp(&CmpOp::Gt, &Value::Float(3.0), &Value::Int(2)));
        assert!(eval_cmp(&CmpOp::Ge, &Value::Int(2), &Value::Float(2.0)));
        assert!(eval_cmp(
            &CmpOp::Lt,
            &Value::Str("a".into()),
            &Value::Str("b".into())
        ));
        assert!(eval_cmp(
            &CmpOp::Ge,
            &Value::Str("b".into()),
            &Value::Str("b".into())
        ));
        // ordering ops only for numeric/numeric and str/str
        assert!(!eval_cmp(
            &CmpOp::Lt,
            &Value::Bool(false),
            &Value::Bool(true)
        ));
        assert!(!eval_cmp(
            &CmpOp::Lt,
            &Value::List(vec![]),
            &Value::List(vec![Value::Int(1)])
        ));
        assert!(!eval_cmp(
            &CmpOp::Gt,
            &Value::Bool(true),
            &Value::Str("a".into())
        ));
    }

    #[test]
    fn nested_and_or_not() {
        let age_gt = Filter::Cmp {
            field: "age".into(),
            op: CmpOp::Gt,
            value: Value::Int(18),
        };
        let name_eq = Filter::Cmp {
            field: "name".into(),
            op: CmpOp::Eq,
            value: Value::Str("ada".into()),
        };
        let name_bob = Filter::Cmp {
            field: "name".into(),
            op: CmpOp::Eq,
            value: Value::Str("bob".into()),
        };
        let score_eq = Filter::Cmp {
            field: "score".into(),
            op: CmpOp::Eq,
            value: Value::Int(2),
        };

        assert!(eval_filter(
            &Filter::And(vec![age_gt.clone(), name_eq.clone(), score_eq.clone()]),
            &get_ada
        ));
        assert!(!eval_filter(
            &Filter::And(vec![age_gt.clone(), name_bob.clone()]),
            &get_ada
        ));
        assert!(eval_filter(
            &Filter::Or(vec![name_bob.clone(), name_eq.clone()]),
            &get_ada
        ));
        assert!(!eval_filter(&Filter::Or(vec![name_bob.clone()]), &get_ada));
        assert!(!eval_filter(
            &Filter::Not(Box::new(age_gt.clone())),
            &get_ada
        ));
        assert!(eval_filter(
            &Filter::Not(Box::new(name_bob.clone())),
            &get_ada
        ));

        let nested = Filter::And(vec![
            Filter::Not(Box::new(Filter::Or(vec![
                name_bob,
                Filter::Cmp {
                    field: "missing".into(),
                    op: CmpOp::Eq,
                    value: Value::Int(1),
                },
            ]))),
            age_gt,
        ]);
        assert!(eval_filter(&nested, &get_ada));
    }

    #[test]
    fn present_ne_uses_values_equal() {
        let ne_age = Filter::Cmp {
            field: "age".into(),
            op: CmpOp::Ne,
            value: Value::Int(18),
        };
        let ne_same = Filter::Cmp {
            field: "age".into(),
            op: CmpOp::Ne,
            value: Value::Int(30),
        };
        assert!(eval_filter(&ne_age, &get_ada));
        assert!(!eval_filter(&ne_same, &get_ada));
    }
}
