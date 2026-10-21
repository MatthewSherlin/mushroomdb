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
/// null); `too_deep` builds the error for a predicate nested past the limit.
pub fn eval_predicate<E>(
    expr: &Expr,
    resolve: &mut dyn FnMut(&Operand) -> Result<Option<Value>, E>,
    too_deep: &dyn Fn() -> E,
) -> Result<Option<bool>, E> {
    eval(expr, resolve, too_deep, 0)
}

/// How a non-boolean value reads as a predicate; the same table WHERE uses.
/// A null element is null.
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

fn eval<E>(
    expr: &Expr,
    resolve: &mut dyn FnMut(&Operand) -> Result<Option<Value>, E>,
    too_deep: &dyn Fn() -> E,
    depth: u32,
) -> Result<Option<bool>, E> {
    if depth > MAX_DEPTH {
        return Err(too_deep());
    }
    let sub = |e: &Expr, resolve: &mut dyn FnMut(&Operand) -> Result<Option<Value>, E>| {
        eval(e, resolve, too_deep, depth + 1)
    };
    Ok(match expr {
        Expr::And(l, r) => {
            let (l, r) = (sub(l, resolve)?, sub(r, resolve)?);
            match (l, r) {
                (Some(false), _) | (_, Some(false)) => Some(false),
                (Some(true), Some(true)) => Some(true),
                _ => None,
            }
        }
        Expr::Or(l, r) => {
            let (l, r) = (sub(l, resolve)?, sub(r, resolve)?);
            match (l, r) {
                (Some(true), _) | (_, Some(true)) => Some(true),
                (Some(false), Some(false)) => Some(false),
                _ => None,
            }
        }
        Expr::Xor(l, r) => {
            let (l, r) = (sub(l, resolve)?, sub(r, resolve)?);
            match (l, r) {
                (Some(a), Some(b)) => Some(a != b),
                _ => None,
            }
        }
        Expr::Not(inner) => sub(inner, resolve)?.map(|b| !b),
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

/// `needle IN list`: true on a match; otherwise null if the needle, an
/// element, or the list itself was null; otherwise false. `null IN []` is
/// false. Each list operand is a single element, or a list whose elements
/// all count (`IN $list`).
///
/// The operands are read lazily, in order, as a WHERE always read them: the
/// scan stops at the first match. A null needle against two or more written
/// elements is null without reading any of them; against one operand, that
/// operand is read to learn whether it is an empty list. So an operand the
/// answer does not depend on is never evaluated and cannot raise an error.
fn in_list<E>(
    needle: Option<&Value>,
    list: &[Operand],
    resolve: &mut dyn FnMut(&Operand) -> Result<Option<Value>, E>,
) -> Result<Option<bool>, E> {
    if needle.is_none() && list.len() > 1 {
        return Ok(None);
    }
    let mut unknown = false;
    for item_op in list {
        let value = match resolve(item_op)? {
            // `x IN null` is null.
            None => {
                if needle.is_none() {
                    return Ok(None);
                }
                unknown = true;
                continue;
            }
            Some(value) => value,
        };
        let Some(needle) = needle else {
            if matches!(&value, Value::List(items) if items.is_empty()) {
                continue;
            }
            return Ok(None);
        };
        // One element, or a list whose elements all count; no allocation.
        let items = match &value {
            Value::List(items) => items.as_slice(),
            other => std::slice::from_ref(other),
        };
        for item in items {
            match eval_cmp3(&CmpOp::Eq, needle, item) {
                Some(true) => return Ok(Some(true)),
                None => unknown = true,
                Some(false) => {}
            }
        }
    }
    Ok(if unknown { None } else { Some(false) })
}
