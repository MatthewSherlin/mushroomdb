//! Three-valued evaluation of a predicate used as a value (QL-BX).
//!
//! `RETURN n.age > 18`, `[x IN l | x > 1]` and `SET n.flag = NOT n.done`
//! carry their predicate as `Operand::Pred`. It evaluates to true, false or
//! null (`None`), following openCypher's truth tables. A WHERE, a CASE WHEN
//! and a comprehension filter evaluate with the same function and keep a row
//! only when the answer is true (`exec::eval_expr`).

use super::ast::{Expr, Operand};
use crate::filter::{eval_cmp3, CmpOp};
use core_storage::Value;

/// How deep a predicate may nest before evaluation refuses it.
const MAX_DEPTH: u32 = 256;

/// Evaluate `expr` three-valued. `resolve` gives an operand's value (`None` is
/// null); `error` builds the error for a predicate nested past the limit and
/// for a non-boolean operand of `AND`, `OR`, `XOR` or `NOT`.
pub fn eval_predicate<E>(
    expr: &Expr,
    resolve: &mut dyn FnMut(&Operand) -> Result<Option<Value>, E>,
    error: &dyn Fn(String) -> E,
) -> Result<Option<bool>, E> {
    eval(expr, resolve, error, 0)
}

/// How a value reads as a whole predicate (the root of a WHERE, a CASE WHEN
/// or a comprehension filter, `WHERE n.flag`): a boolean is itself, null is
/// null, and any other value is true unless it is empty or zero. An operand of
/// `AND`, `OR`, `XOR` or `NOT` is not read this way; it must be a boolean.
pub fn truthy(value: &Value) -> Option<bool> {
    Some(match value {
        Value::Null => return None,
        Value::Bool(b) => *b,
        Value::Int(n) => *n != 0,
        Value::Float(f) => *f != 0.0,
        Value::Str(s) => !s.is_empty(),
        Value::List(v) => !v.is_empty(),
        Value::Map(m) => !m.is_empty(),
        Value::Binary(b) => !b.is_empty(),
        // A temporal value is a present value, never "empty".
        Value::Date(_)
        | Value::LocalDateTime(_)
        | Value::DateTime { .. }
        | Value::Duration { .. } => true,
    })
}

/// The name of a value's type, for a type error.
fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Int(_) => "an integer",
        Value::Float(_) => "a float",
        Value::Str(_) => "a string",
        Value::List(_) => "a list",
        Value::Map(_) => "a map",
        Value::Binary(_) => "binary",
        Value::Date(_) => "a date",
        Value::LocalDateTime(_) => "a local datetime",
        Value::DateTime { .. } => "a datetime",
        Value::Duration { .. } => "a duration",
    }
}

fn eval<E>(
    expr: &Expr,
    resolve: &mut dyn FnMut(&Operand) -> Result<Option<Value>, E>,
    error: &dyn Fn(String) -> E,
    depth: u32,
) -> Result<Option<bool>, E> {
    if depth > MAX_DEPTH {
        return Err(error("expression nesting too deep".to_string()));
    }
    Ok(match expr {
        Expr::And(l, r) => {
            let (l, r) = (
                bool_operand(l, "AND", resolve, error, depth)?,
                bool_operand(r, "AND", resolve, error, depth)?,
            );
            match (l, r) {
                (Some(false), _) | (_, Some(false)) => Some(false),
                (Some(true), Some(true)) => Some(true),
                _ => None,
            }
        }
        Expr::Or(l, r) => {
            let (l, r) = (
                bool_operand(l, "OR", resolve, error, depth)?,
                bool_operand(r, "OR", resolve, error, depth)?,
            );
            match (l, r) {
                (Some(true), _) | (_, Some(true)) => Some(true),
                (Some(false), Some(false)) => Some(false),
                _ => None,
            }
        }
        Expr::Xor(l, r) => {
            let (l, r) = (
                bool_operand(l, "XOR", resolve, error, depth)?,
                bool_operand(r, "XOR", resolve, error, depth)?,
            );
            match (l, r) {
                (Some(a), Some(b)) => Some(a != b),
                _ => None,
            }
        }
        Expr::Not(inner) => bool_operand(inner, "NOT", resolve, error, depth)?.map(|b| !b),
        Expr::Cmp { lhs, op, rhs } => {
            let (l, r) = (resolve(lhs)?, resolve(rhs)?);
            match (l, r) {
                (Some(a), Some(b)) => eval_cmp3(op, &a, &b),
                _ => None,
            }
        }
        Expr::Truthy(op) => resolve(op)?.as_ref().and_then(truthy),
        Expr::IsNull(op) => Some(resolve(op)?.is_none()),
        Expr::IsNotNull(op) => Some(resolve(op)?.is_some()),
        Expr::In { expr, list } => {
            let needle = resolve(expr)?;
            in_list(needle.as_ref(), list, resolve)?
        }
    })
}

/// An operand of `AND`, `OR`, `XOR` or `NOT`: a predicate, or a value that
/// must be a boolean or null (openCypher raises a type error for anything
/// else).
#[inline]
fn bool_operand<E>(
    expr: &Expr,
    op: &str,
    resolve: &mut dyn FnMut(&Operand) -> Result<Option<Value>, E>,
    error: &dyn Fn(String) -> E,
    depth: u32,
) -> Result<Option<bool>, E> {
    match expr {
        Expr::Truthy(o) => match resolve(o)? {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Bool(b)) => Ok(Some(b)),
            Some(other) => Err(error(not_a_boolean(op, &other))),
        },
        other => eval(other, resolve, error, depth + 1),
    }
}

/// The type error for a non-boolean operand of `op`.
#[cold]
fn not_a_boolean(op: &str, value: &Value) -> String {
    format!(
        "type error: {op} expects boolean operands, got {} ({value:?})",
        type_name(value)
    )
}

/// `needle IN list`: true when an element equals the needle (a list element
/// is compared whole: `[1, 2] IN [[1, 2]]` is true, `1 IN [[1]]` is false);
/// otherwise null if the needle, an element or the list is null; otherwise
/// false. `null IN []` is false and `null IN [[]]` is null. A `list` that is
/// not a list is compared as one element.
///
/// A written list (`[a, n.x]`) is read lazily, as a WHERE always read it: the
/// scan stops at the first match, and a null needle reads none of it (the
/// list is not empty, so the answer is null). An element the answer does not
/// depend on is never evaluated and cannot raise an error. A constant list is
/// borrowed, not copied.
fn in_list<E>(
    needle: Option<&Value>,
    list: &Operand,
    resolve: &mut dyn FnMut(&Operand) -> Result<Option<Value>, E>,
) -> Result<Option<bool>, E> {
    if let Operand::ListLit(items) = list {
        let Some(needle) = needle else {
            return Ok(if items.is_empty() { Some(false) } else { None });
        };
        let mut unknown = false;
        for item in items {
            match resolve(item)? {
                None => unknown = true,
                Some(value) => match eval_cmp3(&CmpOp::Eq, needle, &value) {
                    Some(true) => return Ok(Some(true)),
                    None => unknown = true,
                    Some(false) => {}
                },
            }
        }
        return Ok(if unknown { None } else { Some(false) });
    }
    let resolved;
    let value = match list {
        Operand::Lit(Value::Null) => return Ok(None),
        Operand::Lit(value) => value,
        other => match resolve(other)? {
            // `x IN null` is null.
            None => return Ok(None),
            Some(value) => {
                resolved = value;
                &resolved
            }
        },
    };
    let items = match value {
        Value::List(items) => items.as_slice(),
        other => std::slice::from_ref(other),
    };
    Ok(in_values(needle, items))
}

/// [`in_list`] over values already in hand.
fn in_values(needle: Option<&Value>, items: &[Value]) -> Option<bool> {
    let needle = match needle {
        None if items.is_empty() => return Some(false),
        None => return None,
        Some(needle) => needle,
    };
    let mut unknown = false;
    for item in items {
        match eval_cmp3(&CmpOp::Eq, needle, item) {
            Some(true) => return Some(true),
            None => unknown = true,
            Some(false) => {}
        }
    }
    if unknown {
        None
    } else {
        Some(false)
    }
}
