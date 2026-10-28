//! `EXPLAIN` (QL-11): the plan of a statement as JSON, without running it.
//!
//! [`explain_prefix`] finds the leading `EXPLAIN` (or the refused `PROFILE`);
//! [`explain_statement`] parses what follows as the statement it is — a read, a
//! write, a `CALL`, a schema statement — plans it and renders the plan with
//! [`explain_plan_with`]. Nothing here reads or writes the store: the only
//! thing the caller supplies about it is which indexes are declared
//! ([`IndexNames`]), so a plan can say which index a lookup reads.
//!
//! The JSON is a diagnostic: an array of operators in execution order, each an
//! object with an `"op"` and the fields that operator has. Its shape may change
//! between releases; read it, do not build on it.

use super::ast::{
    operand_label, ret_val_label, AggArg, CallReturn, CreateStmt, DdlStatement, Expr, LimitSkip,
    Operand, OrderItem, OrderTarget, Query, RelDir, RetItem, RetVal, RowMerge, RowMergeNode,
    RowWrite, SetClause, UnwindExpr, WriteStatement,
};
use super::lexer::Tok;
use super::parser::{
    is_call_tokens, is_ddl_tokens, is_write_tokens, kw, parse_call, parse_ddl, parse_read,
    parse_write,
};
use super::plan::{plan, PlanOp};
use core_storage::Value;
use std::fmt::Write as _;

/// The words that open a statement, which `EXPLAIN` / `PROFILE` may precede.
const STATEMENT_STARTS: [&str; 10] = [
    "match", "optional", "unwind", "create", "merge", "call", "drop", "show", "with", "return",
];

/// What a statement begins with.
#[derive(Debug, PartialEq)]
pub enum ExplainPrefix<'a> {
    /// Neither: an ordinary statement.
    None,
    /// `EXPLAIN`, then these tokens: the statement to plan.
    Explain(&'a [Tok]),
    /// `PROFILE`: refused by name, as it would run the query.
    Profile,
}

/// Whether `tokens` open with `EXPLAIN` or `PROFILE` followed by a statement.
/// Both are ordinary names elsewhere (`MATCH (explain) …`), so a bare word, or
/// one followed by anything that does not start a statement, is not a prefix.
pub fn explain_prefix(tokens: &[Tok]) -> ExplainPrefix<'_> {
    let starts_statement = match tokens.get(1) {
        Some(Tok::Match | Tok::Create | Tok::Merge | Tok::Unwind | Tok::Return | Tok::With) => true,
        next => STATEMENT_STARTS.iter().any(|w| kw(next, w)),
    };
    if !starts_statement {
        return ExplainPrefix::None;
    }
    if kw(tokens.first(), "explain") {
        ExplainPrefix::Explain(&tokens[1..])
    } else if kw(tokens.first(), "profile") {
        ExplainPrefix::Profile
    } else {
        ExplainPrefix::None
    }
}

/// The refusal for `PROFILE`.
pub const PROFILE_REFUSAL: &str = "not supported in mushroomdb 0.10: PROFILE (it would run the \
     query; EXPLAIN shows the plan without running it)";

/// Which indexes the store declares, as far as a plan names them. `None` means
/// the lookup scans instead.
pub trait IndexNames {
    /// The index an equality lookup on `(label, field)` reads.
    fn equality(&self, label: &str, field: &str) -> Option<String>;
    /// The range index on `(label, field)`.
    fn range(&self, label: &str, field: &str) -> Option<String>;
}

/// No index is declared.
pub struct NoIndexes;

impl IndexNames for NoIndexes {
    fn equality(&self, _: &str, _: &str) -> Option<String> {
        None
    }
    fn range(&self, _: &str, _: &str) -> Option<String> {
        None
    }
}

/// A JSON value, built by hand: this crate has no JSON dependency.
enum J {
    Null,
    Bool(bool),
    Int(i64),
    Str(String),
    Arr(Vec<J>),
    Obj(Vec<(&'static str, J)>),
}

impl J {
    fn str(s: impl Into<String>) -> J {
        J::Str(s.into())
    }

    fn opt_str(s: Option<&str>) -> J {
        s.map_or(J::Null, J::str)
    }

    fn strs<'a>(items: impl IntoIterator<Item = &'a String>) -> J {
        J::Arr(items.into_iter().map(|s| J::str(s.as_str())).collect())
    }

    fn render(&self, out: &mut String) {
        match self {
            J::Null => out.push_str("null"),
            J::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            J::Int(i) => {
                let _ = write!(out, "{i}");
            }
            J::Str(s) => escape_into(s, out),
            J::Arr(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.render(out);
                }
                out.push(']');
            }
            J::Obj(fields) => {
                out.push('{');
                for (i, (key, value)) in fields.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    escape_into(key, out);
                    out.push(':');
                    value.render(out);
                }
                out.push('}');
            }
        }
    }
}

fn escape_into(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn op(name: &'static str, mut fields: Vec<(&'static str, J)>) -> J {
    fields.insert(0, ("op", J::str(name)));
    J::Obj(fields)
}

fn render(ops: Vec<J>) -> String {
    let mut out = String::new();
    J::Arr(ops).render(&mut out);
    out
}

/// A literal, parameter or expression, as short text.
fn operand_text(operand: &Operand) -> String {
    match operand {
        Operand::Lit(Value::Str(s)) => format!("{s:?}"),
        Operand::Lit(v) => format!("{v:?}"),
        other => operand_label(other),
    }
}

fn limit_text(limit: &LimitSkip) -> String {
    match limit {
        LimitSkip::Exact(n) => n.to_string(),
        LimitSkip::Param(p) => format!("${p}"),
    }
}

fn props_json(props: &[(String, Operand)]) -> J {
    J::Arr(
        props
            .iter()
            .map(|(field, value)| {
                J::Obj(vec![
                    ("field", J::str(field.as_str())),
                    ("value", J::str(operand_text(value))),
                ])
            })
            .collect(),
    )
}

fn item_name(item: &RetItem) -> String {
    item.alias
        .clone()
        .or_else(|| ret_val_label(&item.value))
        .unwrap_or_else(|| match &item.value {
            RetVal::Agg { func, arg } => format!("{func:?}({})", agg_arg_text(arg)),
            _ => "<expr>".to_string(),
        })
}

fn agg_arg_text(arg: &AggArg) -> String {
    match arg {
        AggArg::Star => "*".to_string(),
        AggArg::Var(v) => v.clone(),
        AggArg::Prop { var, field } => format!("{var}.{field}"),
        AggArg::Distinct(inner) => format!("DISTINCT {}", agg_arg_text(inner)),
    }
}

fn order_json(items: &[OrderItem]) -> J {
    J::Arr(
        items
            .iter()
            .map(|item| {
                let target = match &item.target {
                    OrderTarget::Alias(a) | OrderTarget::Var(a) => a.clone(),
                    OrderTarget::Prop { var, field } => format!("{var}.{field}"),
                    OrderTarget::Expr(e) => operand_label(e),
                };
                J::Obj(vec![
                    ("target", J::str(target)),
                    ("descending", J::Bool(item.descending)),
                ])
            })
            .collect(),
    )
}

fn dir_text(dir: RelDir) -> &'static str {
    match dir {
        RelDir::Right => "out",
        RelDir::Left => "in",
        RelDir::Undirected => "both",
    }
}

fn expr_text(expr: &Expr) -> String {
    format!("{expr:?}")
}

fn bound_json(bound: &Option<super::plan::RangeBound>) -> J {
    match bound {
        None => J::Null,
        Some(b) => J::Obj(vec![
            ("value", J::str(operand_text(&b.value))),
            ("inclusive", J::Bool(b.inclusive)),
        ]),
    }
}

/// The index a lookup on `(label, field)` reads. `id` and `key` are served by
/// the key map, not an index.
fn equality_index(indexes: &dyn IndexNames, label: Option<&str>, field: &str) -> J {
    if field == "id" || field == "key" {
        return J::str("key");
    }
    let name = label.and_then(|l| indexes.equality(l, field));
    J::opt_str(name.as_deref())
}

/// `ops` as a JSON array in execution order, with no index named (every lookup
/// reads `"index":null` where an index would answer it).
pub fn explain_plan(ops: &[PlanOp]) -> String {
    explain_plan_with(ops, &NoIndexes)
}

/// `ops` as a JSON array in execution order; a lookup names the index
/// `indexes` says it reads, or `null` when it scans.
pub fn explain_plan_with(ops: &[PlanOp], indexes: &dyn IndexNames) -> String {
    render(ops_json(ops, indexes))
}

fn ops_json(ops: &[PlanOp], indexes: &dyn IndexNames) -> Vec<J> {
    ops.iter()
        .enumerate()
        .map(|(i, o)| op_json(o, ops.get(i + 1), indexes))
        .collect()
}

/// The field a `ScanLabel`'s following `Filter` is a plain comparison on, which
/// the executor folds into the scan when the query is bounded.
fn fused_filter_field<'a>(var: &str, next: Option<&'a PlanOp>) -> Option<&'a str> {
    let Some(PlanOp::Filter {
        expr:
            Expr::Cmp {
                lhs: Operand::Prop { var: v, field },
                rhs: Operand::Lit(_),
                ..
            },
    }) = next
    else {
        return None;
    };
    (v == var && !matches!(field.as_str(), "key" | "id" | "label")).then_some(field.as_str())
}

fn op_json(plan_op: &PlanOp, next: Option<&PlanOp>, indexes: &dyn IndexNames) -> J {
    match plan_op {
        PlanOp::ScanLabel { var, label } => {
            let mut fields = vec![
                ("var", J::str(var.as_str())),
                ("label", J::opt_str(label.as_deref())),
            ];
            if let Some(field) = fused_filter_field(var, next) {
                fields.push(("fused_filter", J::str(field)));
            }
            op("ScanLabel", fields)
        }
        PlanOp::ScanKey { var, key, label } => op(
            "ScanKey",
            vec![
                ("var", J::str(var.as_str())),
                ("label", J::opt_str(label.as_deref())),
                ("key", J::str(operand_text(key))),
                ("index", J::str("key")),
            ],
        ),
        PlanOp::IndexScan {
            var,
            label,
            field,
            value,
        } => op(
            "IndexScan",
            vec![
                ("var", J::str(var.as_str())),
                ("label", J::opt_str(label.as_deref())),
                ("field", J::str(field.as_str())),
                ("value", J::str(operand_text(value))),
                ("index", equality_index(indexes, label.as_deref(), field)),
            ],
        ),
        PlanOp::IndexIntersect {
            var,
            label,
            equalities,
        } => op(
            "IndexIntersect",
            vec![
                ("var", J::str(var.as_str())),
                ("label", J::opt_str(label.as_deref())),
                (
                    "equalities",
                    J::Arr(
                        equalities
                            .iter()
                            .map(|(field, value)| {
                                J::Obj(vec![
                                    ("field", J::str(field.as_str())),
                                    ("value", J::str(operand_text(value))),
                                    ("index", equality_index(indexes, label.as_deref(), field)),
                                ])
                            })
                            .collect(),
                    ),
                ),
            ],
        ),
        PlanOp::IndexRangeScan {
            var,
            label,
            field,
            lo,
            hi,
            prefix,
            order,
        } => op(
            "IndexRangeScan",
            vec![
                ("var", J::str(var.as_str())),
                ("label", J::str(label.as_str())),
                ("field", J::str(field.as_str())),
                ("lo", bound_json(lo)),
                ("hi", bound_json(hi)),
                (
                    "prefix",
                    prefix.as_ref().map_or(J::Null, |p| J::str(operand_text(p))),
                ),
                (
                    "order",
                    order.as_ref().map_or(J::Null, |o| {
                        J::Obj(vec![
                            ("descending", J::Bool(o.descending)),
                            (
                                "skip",
                                o.skip.as_ref().map_or(J::Null, |s| J::str(limit_text(s))),
                            ),
                            ("limit", J::str(limit_text(&o.limit))),
                        ])
                    }),
                ),
                ("index", J::opt_str(indexes.range(label, field).as_deref())),
            ],
        ),
        PlanOp::LookupProps { var, props } => op(
            "LookupProps",
            vec![("var", J::str(var.as_str())), ("props", props_json(props))],
        ),
        PlanOp::Expand {
            from,
            rel_var,
            etypes,
            dir,
            to,
            to_label,
            to_props,
            distinct_from,
        } => op(
            "Expand",
            vec![
                ("from", J::str(from.as_str())),
                ("rel_var", J::opt_str(rel_var.as_deref())),
                ("etypes", J::strs(etypes)),
                ("dir", J::str(dir_text(*dir))),
                ("to", J::str(to.as_str())),
                ("to_label", J::opt_str(to_label.as_deref())),
                ("to_props", props_json(to_props)),
                ("distinct_from", J::strs(distinct_from)),
            ],
        ),
        PlanOp::JoinBound { var, label, props } => op(
            "JoinBound",
            vec![
                ("var", J::str(var.as_str())),
                ("label", J::opt_str(label.as_deref())),
                ("props", props_json(props)),
            ],
        ),
        PlanOp::Filter { expr } => op("Filter", vec![("expr", J::str(expr_text(expr)))]),
        PlanOp::Project { items } => op(
            "Project",
            vec![(
                "items",
                J::Arr(items.iter().map(|i| J::str(item_name(i))).collect()),
            )],
        ),
        PlanOp::Distinct => op("Distinct", vec![]),
        PlanOp::OrderBy {
            items,
            visible_columns,
        } => op(
            "OrderBy",
            vec![
                ("items", order_json(items)),
                (
                    "visible_columns",
                    visible_columns.map_or(J::Null, |n| J::Int(n as i64)),
                ),
            ],
        ),
        PlanOp::Skip(n) => op("Skip", vec![("count", J::str(limit_text(n)))]),
        PlanOp::Limit(n) => op("Limit", vec![("count", J::str(limit_text(n)))]),
        PlanOp::Aggregate { func, arg, column } => op(
            "Aggregate",
            vec![
                ("func", J::str(format!("{func:?}"))),
                ("arg", J::str(agg_arg_text(arg))),
                ("column", J::str(column.as_str())),
            ],
        ),
        PlanOp::GroupAggregate { keys, aggs } => op(
            "GroupAggregate",
            vec![
                (
                    "keys",
                    J::Arr(keys.iter().map(|(c, _)| J::str(c.as_str())).collect()),
                ),
                (
                    "aggs",
                    J::Arr(
                        aggs.iter()
                            .map(|(func, arg, column)| {
                                J::Obj(vec![
                                    ("func", J::str(format!("{func:?}"))),
                                    ("arg", J::str(agg_arg_text(arg))),
                                    ("column", J::str(column.as_str())),
                                ])
                            })
                            .collect(),
                    ),
                ),
            ],
        ),
        PlanOp::VarExpand {
            from,
            rel_var,
            etypes,
            dir,
            to,
            to_label,
            to_props,
            min,
            max,
            trail_var,
            distinct_from,
        } => op(
            "VarExpand",
            vec![
                ("from", J::str(from.as_str())),
                ("rel_var", J::opt_str(rel_var.as_deref())),
                ("etypes", J::strs(etypes)),
                ("dir", J::str(dir_text(*dir))),
                ("to", J::str(to.as_str())),
                ("to_label", J::opt_str(to_label.as_deref())),
                ("to_props", props_json(to_props)),
                ("min", J::Int(i64::from(*min))),
                ("max", J::Int(i64::from(*max))),
                ("trail_var", J::opt_str(trail_var.as_deref())),
                ("distinct_from", J::strs(distinct_from)),
            ],
        ),
        PlanOp::ShortestPath {
            from,
            rel_var,
            etypes,
            dir,
            to,
            max_hops,
            trail_var,
            distinct_from,
        } => op(
            "ShortestPath",
            vec![
                ("from", J::str(from.as_str())),
                ("rel_var", J::opt_str(rel_var.as_deref())),
                ("etypes", J::strs(etypes)),
                ("dir", J::str(dir_text(*dir))),
                ("to", J::str(to.as_str())),
                ("max_hops", J::Int(i64::from(*max_hops))),
                ("trail_var", J::opt_str(trail_var.as_deref())),
                ("distinct_from", J::strs(distinct_from)),
            ],
        ),
        PlanOp::BindPath {
            path_var,
            start_var,
            hops,
        } => op(
            "BindPath",
            vec![
                ("path_var", J::str(path_var.as_str())),
                ("start_var", J::str(start_var.as_str())),
                (
                    "hops",
                    J::Arr(hops.iter().map(|h| J::str(h.to_var.as_str())).collect()),
                ),
            ],
        ),
        PlanOp::With {
            items,
            where_expr,
            order_by,
            skip,
            limit,
        } => op(
            "With",
            vec![
                (
                    "items",
                    J::Arr(items.iter().map(|i| J::str(item_name(i))).collect()),
                ),
                (
                    "where",
                    where_expr
                        .as_ref()
                        .map_or(J::Null, |e| J::str(expr_text(e))),
                ),
                ("order_by", order_json(order_by)),
                (
                    "skip",
                    skip.as_ref().map_or(J::Null, |s| J::str(limit_text(s))),
                ),
                (
                    "limit",
                    limit.as_ref().map_or(J::Null, |s| J::str(limit_text(s))),
                ),
            ],
        ),
        PlanOp::Unwind { expr, alias } => op(
            "Unwind",
            vec![
                ("alias", J::str(alias.as_str())),
                ("list", J::str(unwind_text(expr))),
            ],
        ),
        PlanOp::LeftOuterApply {
            inner,
            optional_vars,
        } => op(
            "LeftOuterApply",
            vec![
                ("optional_vars", J::strs(optional_vars)),
                ("inner", J::Arr(ops_json(inner, indexes))),
            ],
        ),
    }
}

fn unwind_text(expr: &UnwindExpr) -> String {
    match expr {
        UnwindExpr::Lit(items) => format!("{items:?}"),
        UnwindExpr::Prop { var, field } => format!("{var}.{field}"),
        UnwindExpr::Var(v) => v.clone(),
        UnwindExpr::Expr(o) => operand_text(o),
    }
}

fn plan_error(e: String) -> String {
    format!("plan: {e}")
}

fn parse_error(e: String) -> String {
    format!("parse: {e}")
}

/// The plan of the read part of a write statement: its `MATCH`es and `WHERE`,
/// with nothing projected.
fn read_part(matches: &[super::ast::Pattern], where_expr: &Option<Expr>) -> Query {
    Query {
        matches: matches.to_vec(),
        optional_clauses: vec![],
        where_expr: where_expr.clone(),
        unwinds: vec![],
        post_unwind_where: None,
        stages: vec![],
        returns: vec![],
        distinct: false,
        order_by: vec![],
        skip: None,
        limit: None,
    }
}

/// The ops of a read part with no `RETURN`: the planner's trailing empty
/// `Project` is dropped.
fn read_part_ops(query: &Query, indexes: &dyn IndexNames) -> Result<Vec<J>, String> {
    let mut ops = plan(query).map_err(plan_error)?;
    if matches!(ops.last(), Some(PlanOp::Project { items }) if items.is_empty()) {
        ops.pop();
    }
    Ok(ops_json(&ops, indexes))
}

fn returns_op(returns: &Option<Vec<RetItem>>) -> Option<J> {
    returns.as_ref().map(|items| {
        op(
            "Project",
            vec![(
                "items",
                J::Arr(items.iter().map(|i| J::str(item_name(i))).collect()),
            )],
        )
    })
}

fn set_clause_json(sc: &SetClause) -> J {
    J::Obj(vec![
        ("var", J::str(sc.var.as_str())),
        ("field", J::str(sc.field.as_str())),
        ("value", J::str(operand_text(&sc.value))),
    ])
}

fn create_op(stmt: &CreateStmt) -> J {
    op(
        "Create",
        vec![
            (
                "nodes",
                J::Arr(
                    stmt.nodes
                        .iter()
                        .map(|n| {
                            J::Obj(vec![
                                ("var", J::opt_str(n.var.as_deref())),
                                ("label", J::str(n.label.as_str())),
                            ])
                        })
                        .collect(),
                ),
            ),
            (
                "edges",
                J::Arr(
                    stmt.edges
                        .iter()
                        .map(|e| {
                            J::Obj(vec![
                                ("from", J::str(e.src_var.as_str())),
                                ("etype", J::str(e.etype.as_str())),
                                ("to", J::str(e.dst_var.as_str())),
                            ])
                        })
                        .collect(),
                ),
            ),
        ],
    )
}

fn merge_node_json(node: &RowMergeNode) -> J {
    J::Obj(vec![
        ("var", J::str(node.var.as_str())),
        ("label", J::str(node.label.as_str())),
        ("key_field", J::str(node.key_field.as_str())),
        ("bound", J::Bool(node.bound)),
        ("index", J::str("key")),
    ])
}

fn row_merge_op(m: &RowMerge) -> J {
    op(
        "Merge",
        vec![
            ("start", merge_node_json(&m.start)),
            (
                "edge",
                m.edge.as_ref().map_or(J::Null, |e| {
                    J::Obj(vec![
                        ("etype", J::str(e.etype.as_str())),
                        ("dir", J::str(dir_text(e.dir))),
                        ("end", merge_node_json(&e.end)),
                    ])
                }),
            ),
            ("on_create", J::Int(m.on_create.len() as i64)),
            ("on_match", J::Int(m.on_match.len() as i64)),
        ],
    )
}

fn row_write_op(w: &RowWrite) -> J {
    match w {
        RowWrite::Create { nodes, edges } => op(
            "Create",
            vec![
                (
                    "nodes",
                    J::Arr(
                        nodes
                            .iter()
                            .map(|n| {
                                J::Obj(vec![
                                    ("var", J::str(n.var.as_str())),
                                    ("label", J::str(n.label.as_str())),
                                ])
                            })
                            .collect(),
                    ),
                ),
                (
                    "edges",
                    J::Arr(
                        edges
                            .iter()
                            .map(|e| {
                                J::Obj(vec![
                                    ("from", J::str(e.src_var.as_str())),
                                    ("etype", J::str(e.etype.as_str())),
                                    ("to", J::str(e.dst_var.as_str())),
                                ])
                            })
                            .collect(),
                    ),
                ),
            ],
        ),
        RowWrite::Merge(m) => row_merge_op(m),
        RowWrite::Set(sets) => op(
            "SetProperty",
            vec![("sets", J::Arr(sets.iter().map(set_clause_json).collect()))],
        ),
        RowWrite::SetMap { var, merge, value } => op(
            "SetProperties",
            vec![
                ("var", J::str(var.as_str())),
                ("merge", J::Bool(*merge)),
                ("value", J::str(operand_text(value))),
            ],
        ),
        RowWrite::Remove(removals) => remove_op(removals),
        RowWrite::Delete { vars, rels, detach } => {
            let mut fields = vec![("vars", J::strs(vars)), ("detach", J::Bool(*detach))];
            if !rels.is_empty() {
                fields.push(("relationships", J::strs(rels)));
            }
            op("DeleteNode", fields)
        }
    }
}

fn remove_op(removals: &[(String, String)]) -> J {
    op(
        "RemoveProperty",
        vec![(
            "removals",
            J::Arr(
                removals
                    .iter()
                    .map(|(var, field)| {
                        J::Obj(vec![
                            ("var", J::str(var.as_str())),
                            ("field", J::str(field.as_str())),
                        ])
                    })
                    .collect(),
            ),
        )],
    )
}

fn write_ops(stmt: &WriteStatement, indexes: &dyn IndexNames) -> Result<Vec<J>, String> {
    let mut out = Vec::new();
    match stmt {
        WriteStatement::Create(s) => {
            out.push(create_op(s));
            out.extend(returns_op(&s.returns));
        }
        WriteStatement::MatchSet(s) => {
            out.extend(read_part_ops(
                &read_part(&s.matches, &s.where_expr),
                indexes,
            )?);
            out.push(op(
                "SetProperty",
                vec![("sets", J::Arr(s.sets.iter().map(set_clause_json).collect()))],
            ));
            out.extend(returns_op(&s.returns));
        }
        WriteStatement::MatchSetMap(s) => {
            out.extend(read_part_ops(
                &read_part(&s.matches, &s.where_expr),
                indexes,
            )?);
            out.push(op(
                "SetProperties",
                vec![
                    ("var", J::str(s.var.as_str())),
                    ("merge", J::Bool(s.merge)),
                    (
                        "value",
                        match &s.param {
                            Some(p) => J::str(format!("${p}")),
                            None => J::str(format!("{} keys", s.map.len())),
                        },
                    ),
                ],
            ));
            out.extend(returns_op(&s.returns));
        }
        WriteStatement::MatchRemove(s) => {
            out.extend(read_part_ops(
                &read_part(&s.matches, &s.where_expr),
                indexes,
            )?);
            out.push(remove_op(&s.removals));
            out.extend(returns_op(&s.returns));
        }
        WriteStatement::MatchDelete(s) => {
            out.extend(read_part_ops(
                &read_part(&s.matches, &s.where_expr),
                indexes,
            )?);
            out.push(op(
                "DeleteEdge",
                vec![(
                    "edges",
                    J::Arr(
                        s.deletes
                            .iter()
                            .map(|d| {
                                J::Obj(vec![
                                    ("rel_var", J::str(d.rel_var.as_str())),
                                    ("etype", J::str(d.etype.as_str())),
                                ])
                            })
                            .collect(),
                    ),
                )],
            ));
        }
        WriteStatement::MatchDeleteNode(s) => {
            out.extend(read_part_ops(
                &read_part(&s.matches, &s.where_expr),
                indexes,
            )?);
            out.push(op(
                "DeleteNode",
                vec![
                    ("vars", J::strs(&s.node_vars)),
                    ("detach", J::Bool(s.detach)),
                ],
            ));
        }
        WriteStatement::Merge(s) => {
            let mut fields = vec![
                ("label", J::str(s.label.as_str())),
                ("key_field", J::str(s.key_field.as_str())),
                ("index", J::str("key")),
                ("on_create", J::Int(s.on_create.len() as i64)),
                ("on_match", J::Int(s.on_match.len() as i64)),
            ];
            if let Some(edge) = &s.edge {
                fields.push((
                    "edge",
                    J::Obj(vec![
                        ("etype", J::str(edge.etype.as_str())),
                        ("dir", J::str(dir_text(edge.dir))),
                        ("end_label", J::str(edge.end_label.as_str())),
                    ]),
                ));
            }
            out.push(op("Merge", fields));
            out.extend(returns_op(&s.returns));
        }
        WriteStatement::UnwindWrite(s) => {
            out.extend(read_part_ops(&s.read, indexes)?);
            out.extend(s.writes.iter().map(row_write_op));
            if let Some(r) = &s.returns {
                out.push(op(
                    "Project",
                    vec![(
                        "items",
                        J::Arr(r.items.iter().map(|i| J::str(item_name(i))).collect()),
                    )],
                ));
            }
        }
    }
    Ok(out)
}

fn call_ops(tokens: &[Tok]) -> Result<Vec<J>, String> {
    let stmt = parse_call(tokens).map_err(parse_error)?;
    let mut out = vec![op(
        "ProcedureCall",
        vec![
            ("procedure", J::str(format!("{}.{}", stmt.ns, stmt.name))),
            (
                "args",
                J::Arr(stmt.args.iter().map(|a| J::str(operand_text(a))).collect()),
            ),
            (
                "yield",
                stmt.yields.as_ref().map_or(J::Null, |ys| {
                    J::Arr(ys.iter().map(|y| J::str(y.name())).collect())
                }),
            ),
        ],
    )];
    if let Some(w) = &stmt.where_expr {
        out.push(op("Filter", vec![("expr", J::str(expr_text(w)))]));
    }
    if let Some(CallReturn { items, .. }) = &stmt.ret {
        out.push(op(
            "Project",
            vec![(
                "items",
                J::Arr(items.iter().map(|i| J::str(item_name(i))).collect()),
            )],
        ));
    }
    Ok(out)
}

fn ddl_op(stmt: &DdlStatement) -> J {
    op("Schema", vec![("statement", J::str(format!("{stmt:?}")))])
}

/// The plan of the statement in `tokens` (the statement after `EXPLAIN`) as
/// JSON, without executing it. Read, write, `CALL` and schema statements all
/// have one; a statement that does not parse or plan is that error, prefixed
/// `parse:` or `plan:` as the entry points prefix them.
pub fn explain_statement(tokens: &[Tok], indexes: &dyn IndexNames) -> Result<String, String> {
    let ops = if is_ddl_tokens(tokens) {
        vec![ddl_op(&parse_ddl(tokens).map_err(parse_error)?)]
    } else if is_call_tokens(tokens) {
        call_ops(tokens)?
    } else if is_write_tokens(tokens) {
        write_ops(&parse_write(tokens).map_err(parse_error)?, indexes)?
    } else {
        let union = parse_read(tokens).map_err(parse_error)?;
        let mut ops = Vec::new();
        for (i, part) in union.parts.iter().enumerate() {
            let part_ops = plan(part).map_err(plan_error)?;
            if union.parts.len() == 1 {
                ops = ops_json(&part_ops, indexes);
            } else {
                ops.push(op(
                    "Union",
                    vec![
                        ("part", J::Int(i as i64)),
                        (
                            "all",
                            J::Bool(i > 0 && union.all_flags.get(i - 1).copied().unwrap_or(false)),
                        ),
                        ("plan", J::Arr(ops_json(&part_ops, indexes))),
                    ],
                ));
            }
        }
        ops
    };
    Ok(render(ops))
}
