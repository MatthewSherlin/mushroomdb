//! Built-in procedures (QL-12, cross-plan contract C1).
//!
//! A procedure is a pure function over a [`ProcCatalog`] — what the caller's
//! view of the store holds — so `CALL` answers the same over HTTP, embedded
//! and Bolt, and nothing here depends on a protocol crate. [`call_procedure`]
//! dispatches by name; [`execute_call`] runs a parsed `CALL` statement:
//! it evaluates the arguments, calls the procedure, and runs the `YIELD`,
//! `WHERE` and `RETURN` over the rows it returns with the query executor.

use super::ast::{ProcCallStmt, Query, RetItem, RetVal, UnwindClause, UnwindExpr, WithStage};
use super::exec::{eval_operand, execute, Params};
use super::plan::plan;
use crate::result::ResultSet;
use crate::view::GraphView;
use core_storage::Value;
use std::collections::BTreeMap;

/// What a procedure returns: named columns and rows of values in their order.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
}

/// One row of `SHOW INDEXES` (`name`, `type`, `label`, `property`, `state`)
/// or `SHOW CONSTRAINTS` (`name`, `type`, `label`, `property`), in that
/// column order: the rows the schema statements answer with (W2 TY-9).
pub type SchemaRow = Vec<Value>;

/// The store as one caller sees it. A view under a mask (a role, an access
/// key, a namespace) lists only what that mask lets the caller see.
pub trait ProcCatalog {
    /// The labels of the nodes the view can see, sorted.
    fn labels(&self) -> Vec<String>;
    /// The types of the relationships between nodes the view can see, sorted.
    fn relationship_types(&self) -> Vec<String>;
    /// The property keys set on the nodes and relationships the view can see,
    /// sorted.
    fn property_keys(&self) -> Vec<String>;
    /// The rows `SHOW INDEXES` gives the same caller.
    fn indexes(&self) -> Vec<SchemaRow>;
    /// The rows `SHOW CONSTRAINTS` gives the same caller.
    fn constraints(&self) -> Vec<SchemaRow>;
}

/// Run the procedure `ns.name` with `args`. Names are case-sensitive.
pub fn call_procedure(
    cat: &dyn ProcCatalog,
    ns: &str,
    name: &str,
    args: &[Value],
) -> Result<ProcResult, String> {
    // `db.ping` reads nothing; the catalog procedures (TX-15) read `cat`.
    let _ = cat;
    match (ns, name) {
        ("db", "ping") => {
            no_args(ns, name, args)?;
            Ok(ProcResult {
                columns: vec!["success".to_string()],
                rows: vec![vec![Value::Bool(true)]],
            })
        }
        _ => Err(format!("unknown procedure: {ns}.{name}")),
    }
}

fn no_args(ns: &str, name: &str, args: &[Value]) -> Result<(), String> {
    if args.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{ns}.{name} takes no arguments, got {}",
            args.len()
        ))
    }
}

/// The variable the procedure's rows are unwound into. The space keeps it
/// apart from any name a query writes without backticks.
const ROW: &str = " call row";

/// Run a parsed `CALL` statement against `view`: evaluate its arguments (a
/// `$param` from `params`), call the procedure over `cat`, and return the
/// yielded columns, filtered by `WHERE` and projected by `RETURN` when the
/// statement has them.
pub fn execute_call(
    view: &GraphView,
    stmt: &ProcCallStmt,
    cat: &dyn ProcCatalog,
    params: &Params,
) -> Result<ResultSet, String> {
    let args = stmt
        .args
        .iter()
        .map(|arg| eval_operand(view, arg, params, &[]).map(|v| v.unwrap_or(Value::Null)))
        .collect::<Result<Vec<_>, _>>()?;
    let result = call_procedure(cat, &stmt.ns, &stmt.name, &args)?;
    let query = call_query(stmt, result)?;
    execute(view, &plan(&query)?, params)
}

/// The read query that runs a `CALL`'s `YIELD`, `WHERE` and `RETURN` over the
/// procedure's rows: `UNWIND <rows> AS row WITH row.f AS a, … [WHERE …]
/// RETURN …`, where a missing `RETURN` returns the yielded names.
fn call_query(stmt: &ProcCallStmt, result: ProcResult) -> Result<Query, String> {
    let bound: Vec<(String, String)> = match &stmt.yields {
        None => result
            .columns
            .iter()
            .map(|c| (c.clone(), c.clone()))
            .collect(),
        Some(items) => items
            .iter()
            .map(|item| {
                if result.columns.contains(&item.field) {
                    Ok((item.field.clone(), item.name().to_string()))
                } else {
                    Err(format!(
                        "YIELD `{}`: {}.{} returns no such column; it returns: {}",
                        item.field,
                        stmt.ns,
                        stmt.name,
                        result.columns.join(", ")
                    ))
                }
            })
            .collect::<Result<_, _>>()?,
    };
    let rows = result
        .rows
        .into_iter()
        .map(|row| {
            let entries: BTreeMap<String, Value> =
                result.columns.iter().cloned().zip(row).collect();
            Value::Map(entries)
        })
        .collect();
    let yielded = bound
        .iter()
        .map(|(field, name)| RetItem {
            value: RetVal::Prop {
                var: ROW.to_string(),
                field: field.clone(),
            },
            alias: Some(name.clone()),
        })
        .collect();
    let (distinct, returns, order_by, skip, limit) = match &stmt.ret {
        Some(ret) => (
            ret.distinct,
            ret.items.clone(),
            ret.order_by.clone(),
            ret.skip.clone(),
            ret.limit.clone(),
        ),
        None => (
            false,
            bound
                .iter()
                .map(|(_, name)| RetItem {
                    value: RetVal::Var(name.clone()),
                    alias: None,
                })
                .collect(),
            Vec::new(),
            None,
            None,
        ),
    };
    Ok(Query {
        matches: Vec::new(),
        optional_clauses: Vec::new(),
        where_expr: None,
        unwinds: vec![UnwindClause {
            list: UnwindExpr::Lit(rows),
            alias: ROW.to_string(),
        }],
        post_unwind_where: None,
        stages: vec![WithStage {
            items: yielded,
            where_expr: stmt.where_expr.clone(),
            order_by: Vec::new(),
            skip: None,
            limit: None,
            matches: Vec::new(),
            optional_clauses: Vec::new(),
            unwinds: Vec::new(),
            post_where: None,
        }],
        returns,
        distinct,
        order_by,
        skip,
        limit,
    })
}
