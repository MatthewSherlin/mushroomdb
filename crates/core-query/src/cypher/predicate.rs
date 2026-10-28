//! Three-valued evaluation of a predicate used as a value (QL-BX).
//!
//! `RETURN n.age > 18`, `[x IN l | x > 1]` and `SET n.flag = NOT n.done`
//! carry their predicate as `Operand::Pred`. It evaluates to true, false or
//! null (`None`), following openCypher's truth tables. A WHERE, a CASE WHEN
//! and a comprehension filter evaluate with the same function and keep a row
//! only when the answer is true (`exec::eval_expr`).

use super::ast::{Expr, ListPredKind, Operand};
use crate::filter::{eval_cmp3, CmpOp};
use core_storage::Value;

/// How deep a predicate may nest before evaluation refuses it.
const MAX_DEPTH: u32 = 256;

/// Evaluate `expr` three-valued. `resolve` gives an operand's value (`None` is
/// null); `error` builds the error for a predicate nested past the limit.
pub fn eval_predicate<E>(
    expr: &Expr,
    resolve: &mut dyn FnMut(&Operand) -> Result<Option<Value>, E>,
    error: &dyn Fn(String) -> E,
) -> Result<Option<bool>, E> {
    eval(expr, resolve, error, 0)
}

/// The answer of `all`/`any`/`none`/`single` given how many of the predicate's
/// per-element answers were true, false and null. See `ListPredKind` for the
/// tables; the TCK's "lists containing nulls" scenarios are the authority.
pub fn quantify(kind: ListPredKind, trues: usize, falses: usize, nulls: usize) -> Option<bool> {
    match kind {
        ListPredKind::All if falses > 0 => Some(false),
        ListPredKind::All => (nulls == 0).then_some(true),
        ListPredKind::Any if trues > 0 => Some(true),
        ListPredKind::Any => (nulls == 0).then_some(false),
        ListPredKind::None if trues > 0 => Some(false),
        ListPredKind::None => (nulls == 0).then_some(true),
        ListPredKind::Single if trues > 1 => Some(false),
        ListPredKind::Single if nulls > 0 => None,
        ListPredKind::Single => Some(trues == 1),
    }
}

/// How a value reads as a predicate, at the root of a WHERE (`WHERE n.flag`)
/// and as an operand of `AND`, `OR`, `XOR` or `NOT` alike: a boolean is
/// itself, null is null, and any other value is true unless it is empty or
/// zero. (openCypher makes a non-boolean operand a type error; that strict
/// form is planned for a later release.)
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
    error: &dyn Fn(String) -> E,
    depth: u32,
) -> Result<Option<bool>, E> {
    if depth > MAX_DEPTH {
        return Err(error("expression nesting too deep".to_string()));
    }
    let sub = |e: &Expr, resolve: &mut dyn FnMut(&Operand) -> Result<Option<Value>, E>| {
        eval(e, resolve, error, depth + 1)
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
