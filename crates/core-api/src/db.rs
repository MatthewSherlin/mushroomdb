use crate::ingest::{IngestOptions, IngestReport};
use crate::roles::{RoleDef, RolesFile};
use crate::subscription::{
    event_matches, DbEvent, SubEntry, SubFilter, SubInner, Subscription, DEFAULT_SUB_CAPACITY,
};
use core_query::cypher::ast::ArithOp;
use core_query::cypher::{
    execute, is_subscribable, is_write_tokens, lex, parse, parse_write, plan, MatchDeleteNodeStmt,
    NodePat, Operand, Params, Pattern, PlanOp, Query, RetItem, RetVal, WriteStatement,
};
use core_query::{eval_filter, expand, neighborhood, Dir, Filter, GraphView, ResultSet};
use core_rules::{
    decode_rule_def, evaluate, EngineEdgeDelta, GraphMut, NodeView, Predicate, RuleDef, RuleEngine,
    RuleIvfExport, ViewDef, ViewStore,
};
use core_storage::fs::{FileId, Fs, FsIntrospect, RealFs};
use core_storage::fulltext::FulltextIndex;
use core_storage::v8::encode::{
    archived_edge_props_to_owned, archived_hnsw_to_owned, archived_provenance_to_owned,
    archived_rules_meta_to_owned, archived_to_idmap, archived_to_interner, archived_views_to_owned,
    decode_meta, encode_v8, V8Meta,
};
use core_storage::v8::seam::TopologyView;
use core_storage::wal::{decode_all, encode_record, WalRecord};
use core_storage::{
    ColumnStore, Direction, EdgeProps, GraphError, IdMap, Interner, Result, Topology, Value,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

// Test-only: counts how many times `pending_deltas_since().to_vec()` actually
// executes (i.e., at least one view is defined). Used to verify the fast-path
// guard skips the allocation when `view_store.is_empty()`.
#[cfg(test)]
thread_local! {
    static DELTA_COPY_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Internal state for a single `subscribe_query` subscription.
///
/// On every commit, `distribute_events` re-executes `ops` against the current
/// graph state, diffs the result against `prev_rows`, and pushes
/// `DbEvent::QueryRowAdded` / `QueryRowRemoved` events to `inner`.
///
/// **Full re-run per commit; use LIMIT to bound execution cost.**
/// (Differential evaluation is roadmap / Phase 5.)
pub(crate) struct QuerySubEntry {
    /// Compiled plan for the subscribed Cypher query.
    ops: Vec<PlanOp>,
    /// Column names from the first execution (fixed for the subscription lifetime).
    columns: Vec<String>,
    /// Serialized (JSON) row key → row data, representing the result set at
    /// the end of the last commit. Used to diff against the new result.
    prev_row_map: std::collections::HashMap<String, Vec<Option<Value>>>,
    /// Weak pointer to the subscriber queue; dead Weak → subscription dropped.
    inner: std::sync::Weak<SubInner>,
}

/// A post-commit mutation notification.
///
/// Emitted from `log_then_apply` after the WAL append, fsync, and
/// in-memory `apply` all succeed. Never emitted for rejected operations
/// (validation errors, [`GraphError::RuleOwned`], duplicate keys, no-op
/// deletes/removes). Event payloads carry user keys and rule names, never
/// internal ids.
///
/// **Replay:** [`GraphDb::open`] / [`GraphDb::open_with`] replay the WAL via
/// `apply` only. Emission lives exclusively in `log_then_apply`, so
/// recovery is silent even if a sink were installed (it cannot be: the
/// sink is in-memory and set after open).
///
/// **Ordering:** a `Batch` WAL frame emits one event per inner record, then
/// [`MutationEvent::BatchApplied`]. An ingest commit emits those same inner
/// events, then [`MutationEvent::Ingested`] (not `BatchApplied`). An empty
/// or all-noop batch writes no WAL and emits nothing (including no summary).
///
/// **Derived edges:** rule-created or retracted edges are not individually
/// evented — they are recoverable from the triggering mutation plus the live
/// rule set. Only the triggering record is emitted.
///
/// **Wire form:** externally tagged snake_case JSON
/// (`{"node_inserted":{"label":"A","key":"k"}}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MutationEvent {
    NodeInserted {
        label: String,
        key: String,
    },
    PropSet {
        key: String,
        field: String,
    },
    PropRemoved {
        key: String,
        field: String,
    },
    EdgeInserted {
        edge_type: String,
        src: String,
        dst: String,
    },
    EdgeDeleted {
        edge_type: String,
        src: String,
        dst: String,
    },
    NodeDeleted {
        key: String,
    },
    RuleCreated {
        name: String,
    },
    RuleDeleted {
        name: String,
    },
    RuleRebuilt {
        name: String,
    },
    BatchApplied {
        ops: usize,
    },
    Ingested {
        label: String,
        inserted: usize,
    },
}

fn event_from_record(rec: &WalRecord, intern: &Interner, ids: &IdMap) -> Option<MutationEvent> {
    match rec {
        WalRecord::InsertNode { label, key, .. } => Some(MutationEvent::NodeInserted {
            label: label.clone(),
            key: key.clone(),
        }),
        WalRecord::InsertNodeId { label, key, .. } => Some(MutationEvent::NodeInserted {
            label: intern.resolve(*label)?.to_string(),
            key: key.clone(),
        }),
        WalRecord::SetProp { key, field, .. } => Some(MutationEvent::PropSet {
            key: key.clone(),
            field: field.clone(),
        }),
        WalRecord::SetPropId { id, field, .. } => Some(MutationEvent::PropSet {
            key: ids.key_of(*id)?.to_string(),
            field: intern.resolve(*field)?.to_string(),
        }),
        WalRecord::RemoveProp { key, field } => Some(MutationEvent::PropRemoved {
            key: key.clone(),
            field: field.clone(),
        }),
        WalRecord::InsertEdge {
            edge_type,
            src_key,
            dst_key,
        } => Some(MutationEvent::EdgeInserted {
            edge_type: edge_type.clone(),
            src: src_key.clone(),
            dst: dst_key.clone(),
        }),
        WalRecord::InsertEdgeId { etype, src, dst } => Some(MutationEvent::EdgeInserted {
            edge_type: intern.resolve(*etype)?.to_string(),
            src: ids.key_of(*src)?.to_string(),
            dst: ids.key_of(*dst)?.to_string(),
        }),
        WalRecord::DeleteEdge {
            edge_type,
            src_key,
            dst_key,
        } => Some(MutationEvent::EdgeDeleted {
            edge_type: edge_type.clone(),
            src: src_key.clone(),
            dst: dst_key.clone(),
        }),
        WalRecord::DeleteNode { key } => Some(MutationEvent::NodeDeleted { key: key.clone() }),
        WalRecord::CreateRule { def_bytes } => {
            let def: RuleDef = decode_rule_def(def_bytes).ok()?;
            Some(MutationEvent::RuleCreated { name: def.name })
        }
        WalRecord::DeleteRule { name } => Some(MutationEvent::RuleDeleted { name: name.clone() }),
        WalRecord::RebuildRule { name } => Some(MutationEvent::RuleRebuilt { name: name.clone() }),
        WalRecord::Batch(_)
        | WalRecord::CreateView { .. }
        | WalRecord::DeleteView { .. }
        | WalRecord::EnableFulltext { .. }
        | WalRecord::DisableFulltext { .. }
        | WalRecord::Intern { .. } => None,
    }
}

/// Database-wide counters plus per-rule budget/fire stats.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Stats {
    pub nodes_live: usize,
    pub nodes_tombstoned: usize,
    pub edges: u64,
    pub rules: Vec<RuleStats>,
}

/// One rule's provenance size, trip latch, and fire counter.
///
/// `tripped` is a one-way latch: once set, the engine adds no new edges for
/// that rule until [`GraphDb::rebuild_rule`] (and only if the full desired
/// set then fits). `fires` counts `on_node_changed` evaluations plus
/// backfill/rebuild participant ticks (rebuild counts even when it is a
/// provenance no-op).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuleStats {
    pub name: String,
    pub edges: u64,
    pub tripped: bool,
    pub fires: u64,
    /// Whether this rule uses the approximate IVF-Flat candidate path.
    pub approximate: bool,
}

/// Wire summary of a [`Predicate`]. JSON only — `Explanation` is never
/// bincode-persisted (WAL/snapshots store `RuleDef` bytes, not this type).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PredicateSummary {
    pub kind: String,
    pub fields: Vec<String>,
    pub min: Option<f64>,
    pub tolerance: Option<f64>,
    pub km: Option<f64>,
    pub parts: Option<Vec<PredicateSummary>>,
    /// True when the owning rule has `approximate=true` (IVF-Flat candidate path).
    /// Always false for predicates reported without rule context (sub-predicates in `parts`).
    #[serde(default)]
    pub approximate: bool,
}

impl From<&Predicate> for PredicateSummary {
    fn from(p: &Predicate) -> Self {
        match p {
            Predicate::KeyMatch { field } => PredicateSummary {
                kind: "key_match".into(),
                fields: vec![field.clone()],
                min: None,
                tolerance: None,
                km: None,
                parts: None,
                approximate: false,
            },
            Predicate::FieldEqual { field } => PredicateSummary {
                kind: "field_equal".into(),
                fields: vec![field.clone()],
                min: None,
                tolerance: None,
                km: None,
                parts: None,
                approximate: false,
            },
            Predicate::Overlap { field, min } => PredicateSummary {
                kind: "overlap".into(),
                fields: vec![field.clone()],
                min: Some(*min),
                tolerance: None,
                km: None,
                parts: None,
                approximate: false,
            },
            Predicate::NumericWithin { field, tolerance } => PredicateSummary {
                kind: "numeric_within".into(),
                fields: vec![field.clone()],
                min: None,
                tolerance: Some(*tolerance),
                km: None,
                parts: None,
                approximate: false,
            },
            Predicate::GeoRadius { field, km } => PredicateSummary {
                kind: "geo_radius".into(),
                fields: vec![field.clone()],
                min: None,
                tolerance: None,
                km: Some(*km),
                parts: None,
                approximate: false,
            },
            Predicate::VectorSimilar { field, min } => PredicateSummary {
                kind: "vector_similar".into(),
                fields: vec![field.clone()],
                min: Some(*min),
                tolerance: None,
                km: None,
                parts: None,
                approximate: false,
            },
            Predicate::All(inner) => {
                let parts: Vec<PredicateSummary> = inner.iter().map(Self::from).collect();
                let mut fields = Vec::new();
                for part in &parts {
                    for f in &part.fields {
                        if !fields.contains(f) {
                            fields.push(f.clone());
                        }
                    }
                }
                PredicateSummary {
                    kind: "all".into(),
                    fields,
                    min: None,
                    tolerance: None,
                    km: None,
                    parts: Some(parts),
                    approximate: false,
                }
            }
            Predicate::Any(inner) => {
                let parts: Vec<PredicateSummary> = inner.iter().map(Self::from).collect();
                let mut fields = Vec::new();
                for part in &parts {
                    for f in &part.fields {
                        if !fields.contains(f) {
                            fields.push(f.clone());
                        }
                    }
                }
                PredicateSummary {
                    kind: "any".into(),
                    fields,
                    min: None,
                    tolerance: None,
                    km: None,
                    parts: Some(parts),
                    approximate: false,
                }
            }
        }
    }
}

/// Snapshot of a live node's key, label, and columnar properties.
///
/// `props` is a [`BTreeMap`] so field order is deterministic (sorted by name)
/// regardless of insert order or the columnar store's `HashMap` iteration.
///
/// Deliberately does not derive `Serialize`: `Value`'s serde form is
/// internally tagged. Wire JSON is built by `value_to_json` in the server.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeInfo {
    pub key: String,
    pub label: String,
    pub props: BTreeMap<String, Value>,
}

/// Counts returned by [`GraphDb::delete_node`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DeleteReport {
    /// Number of manual (user-inserted) edges removed.
    pub manual_edges: u64,
    /// Number of derived (rule-owned) edges retracted.
    pub derived_edges: u64,
}

/// One directed edge incident on a node, with provenance membership.
///
/// `derived` is true iff `(edge_type, src, dst)` is in the rule engine's
/// Plan-8 `by_node` provenance index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EdgeInfo {
    pub edge_type: String,
    pub src_key: String,
    pub dst_key: String,
    pub derived: bool,
}

/// One rule-owned edge between two nodes, with the rule name, edge type,
/// direction (src_key → dst_key), and weight if the rule stores one.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Explanation {
    pub rule: String,
    pub edge_type: String,
    pub src_key: String,
    pub dst_key: String,
    pub weight: Option<f64>,
    pub predicate: PredicateSummary,
}

/// Construct the standard write-query result set (columns: created, properties_set, deleted).
fn write_result_set() -> ResultSet {
    ResultSet::new(vec![
        "created".into(),
        "properties_set".into(),
        "deleted".into(),
    ])
}

fn resolve_merge_set_value(op: &Operand, params: &BTreeMap<String, Value>) -> Result<Value> {
    match op {
        Operand::Lit(v) => Ok(v.clone()),
        Operand::Param(name) => params
            .get(name)
            .cloned()
            .ok_or_else(|| GraphError::QueryError {
                detail: format!("missing parameter `{name}`"),
            }),
        _ => Err(GraphError::QueryError {
            detail: "ON CREATE/ON MATCH SET value must be a literal or $parameter".into(),
        }),
    }
}

fn operand_node_vars(op: &Operand, out: &mut Vec<String>) {
    match op {
        Operand::Prop { var, .. } | Operand::Var(var) => {
            if !out.contains(var) {
                out.push(var.clone());
            }
        }
        Operand::FuncCall { args, .. } => {
            for arg in args {
                operand_node_vars(arg, out);
            }
        }
        Operand::BinArith { left, right, .. } => {
            operand_node_vars(left, out);
            operand_node_vars(right, out);
        }
        Operand::Lit(_) | Operand::Param(_) => {}
    }
}

fn ret_node_vars(items: &[RetItem]) -> Vec<String> {
    let mut out = Vec::new();
    for item in items {
        match &item.value {
            RetVal::Var(v) | RetVal::Prop { var: v, .. } => {
                if !out.contains(v) {
                    out.push(v.clone());
                }
            }
            RetVal::FuncCall { args, .. } => {
                for arg in args {
                    operand_node_vars(arg, &mut out);
                }
            }
            RetVal::ScalarExpr(op) => operand_node_vars(op, &mut out),
            RetVal::Agg { .. } => {}
        }
    }
    out
}

fn add_var(out: &mut Vec<String>, v: &str) {
    if !out.iter().any(|x| x == v) {
        out.push(v.to_string());
    }
}

fn pattern_node_vars(pats: &[Pattern]) -> Vec<String> {
    let mut out = Vec::new();
    for p in pats {
        if let Some(v) = &p.start.var {
            add_var(&mut out, v);
        }
        for (_, dest) in &p.chain {
            if let Some(v) = &dest.var {
                add_var(&mut out, v);
            }
        }
    }
    out
}

fn pattern_rel_vars(pats: &[Pattern]) -> Vec<String> {
    let mut out = Vec::new();
    for p in pats {
        for (rel, _) in &p.chain {
            if rel.hops.is_none() {
                if let Some(v) = &rel.var {
                    add_var(&mut out, v);
                }
            }
        }
    }
    out
}

fn rel_type_alias(var: &str) -> String {
    format!("__rt_{var}")
}

fn ret_column_name(item: &RetItem) -> String {
    if let Some(alias) = &item.alias {
        return alias.clone();
    }
    match &item.value {
        RetVal::Var(v) => v.clone(),
        RetVal::Prop { var, field } => format!("{var}.{field}"),
        RetVal::FuncCall { name, args } => {
            let arg_strs: Vec<String> = args
                .iter()
                .map(|a| match a {
                    Operand::Var(v) => v.clone(),
                    Operand::Prop { var, field } => format!("{var}.{field}"),
                    Operand::Lit(_) => "<lit>".to_string(),
                    Operand::Param(p) => format!("${p}"),
                    Operand::FuncCall { name: n, .. } => format!("{n}(...)"),
                    Operand::BinArith { .. } => "<arith>".to_string(),
                })
                .collect();
            format!("{name}({})", arg_strs.join(", "))
        }
        RetVal::ScalarExpr(_) => "<expr>".to_string(),
        RetVal::Agg { .. } => "<agg>".to_string(),
    }
}

fn eval_set_return_operand<F: Fs>(
    db: &GraphDb<F>,
    match_rs: &ResultSet,
    row: usize,
    rel_vars: &[String],
    op: &Operand,
    params: &BTreeMap<String, Value>,
) -> Result<Option<Value>> {
    match op {
        Operand::Lit(v) => Ok(Some(v.clone())),
        Operand::Param(name) => params.get(name).cloned().ok_or_else(|| GraphError::QueryError {
            detail: format!("missing parameter `{name}`"),
        }).map(Some),
        Operand::Var(name) if rel_vars.iter().any(|r| r == name) => Err(GraphError::QueryError {
            detail: format!(
                "cannot return relationship variable '{name}' bare; return its properties ({name}.field) instead"
            ),
        }),
        Operand::Var(name) => Ok(match_rs.get(row, name).cloned()),
        Operand::Prop { var, field } => {
            if rel_vars.iter().any(|r| r == var) {
                return Ok(None);
            }
            let Some(Value::Str(key)) = match_rs.get(row, var) else {
                return Ok(None);
            };
            Ok(db.get_prop(key, field))
        }
        Operand::FuncCall { name, args } => {
            eval_set_return_func(db, match_rs, row, rel_vars, name, args, params)
        }
        Operand::BinArith { op, left, right } => {
            let lv = eval_set_return_operand(db, match_rs, row, rel_vars, left, params)?;
            let rv = eval_set_return_operand(db, match_rs, row, rel_vars, right, params)?;
            eval_set_return_arith(op, lv, rv)
        }
    }
}

fn eval_set_return_arith(
    op: &ArithOp,
    lv: Option<Value>,
    rv: Option<Value>,
) -> Result<Option<Value>> {
    match (lv, rv) {
        (None, _) | (_, None) => Ok(None),
        (Some(Value::Int(a)), Some(Value::Int(b))) => {
            let result = match op {
                ArithOp::Sub => a.saturating_sub(b),
                ArithOp::Mul => a.saturating_mul(b),
                ArithOp::Add => a.saturating_add(b),
                ArithOp::Div => {
                    if b == 0 {
                        return Err(GraphError::QueryError {
                            detail: "division by zero".into(),
                        });
                    }
                    a.checked_div(b).unwrap_or(i64::MAX)
                }
            };
            Ok(Some(Value::Int(result)))
        }
        (Some(lv), Some(rv)) => {
            let a = match &lv {
                Value::Float(f) => *f,
                Value::Int(i) => *i as f64,
                _ => {
                    return Err(GraphError::QueryError {
                        detail: format!("arithmetic operand must be numeric, got {lv:?}"),
                    })
                }
            };
            let b = match &rv {
                Value::Float(f) => *f,
                Value::Int(i) => *i as f64,
                _ => {
                    return Err(GraphError::QueryError {
                        detail: format!("arithmetic operand must be numeric, got {rv:?}"),
                    })
                }
            };
            let result = match op {
                ArithOp::Sub => a - b,
                ArithOp::Mul => a * b,
                ArithOp::Add => a + b,
                ArithOp::Div => {
                    if b == 0.0 {
                        return Err(GraphError::QueryError {
                            detail: "division by zero".into(),
                        });
                    }
                    a / b
                }
            };
            Ok(Some(Value::Float(result)))
        }
    }
}

fn eval_set_return_func<F: Fs>(
    db: &GraphDb<F>,
    match_rs: &ResultSet,
    row: usize,
    rel_vars: &[String],
    name: &str,
    args: &[Operand],
    params: &BTreeMap<String, Value>,
) -> Result<Option<Value>> {
    let norm = name.to_ascii_lowercase();
    if norm == "type" {
        if args.len() != 1 {
            return Err(GraphError::QueryError {
                detail: format!("type() requires exactly 1 argument, got {}", args.len()),
            });
        }
        let Operand::Var(rel) = &args[0] else {
            return Err(GraphError::QueryError {
                detail: "type() argument must be a relationship variable (e.g. type(r))".into(),
            });
        };
        return Ok(match_rs.get(row, &rel_type_alias(rel)).cloned());
    }
    let mut vals = Vec::with_capacity(args.len());
    for arg in args {
        vals.push(eval_set_return_operand(
            db, match_rs, row, rel_vars, arg, params,
        )?);
    }
    match norm.as_str() {
        "tolower" => {
            if vals.len() != 1 {
                return Err(GraphError::QueryError {
                    detail: format!("toLower() requires exactly 1 argument, got {}", vals.len()),
                });
            }
            Ok(vals[0].clone().map(|val| match val {
                Value::Str(s) => Value::Str(s.to_ascii_lowercase()),
                other => other,
            }))
        }
        "toupper" => {
            if vals.len() != 1 {
                return Err(GraphError::QueryError {
                    detail: format!("toUpper() requires exactly 1 argument, got {}", vals.len()),
                });
            }
            Ok(vals[0].clone().map(|val| match val {
                Value::Str(s) => Value::Str(s.to_ascii_uppercase()),
                other => other,
            }))
        }
        "size" => match vals.first().cloned().flatten() {
            None => Ok(None),
            Some(Value::Str(s)) => Ok(Some(Value::Int(s.len() as i64))),
            Some(Value::List(items)) => Ok(Some(Value::Int(items.len() as i64))),
            Some(_) => Ok(None),
        },
        "coalesce" => Ok(vals.into_iter().flatten().next()),
        "abs" => match vals.first().cloned().flatten() {
            None => Ok(None),
            Some(Value::Int(n)) => Ok(Some(Value::Int(n.saturating_abs()))),
            Some(Value::Float(f)) => Ok(Some(Value::Float(f.abs()))),
            Some(_) => Ok(None),
        },
        "round" => match vals.first().cloned().flatten() {
            None => Ok(None),
            Some(Value::Float(f)) => Ok(Some(Value::Float(f.round()))),
            Some(Value::Int(n)) => Ok(Some(Value::Int(n))),
            Some(_) => Ok(None),
        },
        _ => Err(GraphError::QueryError {
            detail: format!(
                "unknown function `{name}`; supported: toLower, toUpper, size, coalesce, type, abs, round, textMatches"
            ),
        }),
    }
}

fn eval_set_return_item<F: Fs>(
    db: &GraphDb<F>,
    match_rs: &ResultSet,
    row: usize,
    rel_vars: &[String],
    item: &RetItem,
    params: &BTreeMap<String, Value>,
) -> Result<Option<Value>> {
    match &item.value {
        RetVal::Var(v) => eval_set_return_operand(
            db,
            match_rs,
            row,
            rel_vars,
            &Operand::Var(v.clone()),
            params,
        ),
        RetVal::Prop { var, field } => eval_set_return_operand(
            db,
            match_rs,
            row,
            rel_vars,
            &Operand::Prop {
                var: var.clone(),
                field: field.clone(),
            },
            params,
        ),
        RetVal::FuncCall { name, args } => {
            eval_set_return_func(db, match_rs, row, rel_vars, name, args, params)
        }
        RetVal::ScalarExpr(op) => eval_set_return_operand(db, match_rs, row, rel_vars, op, params),
        RetVal::Agg { .. } => Err(GraphError::QueryError {
            detail: "aggregates are not supported in MATCH … SET … RETURN".into(),
        }),
    }
}

/// Project user RETURN from original MATCH rows after SET. No rematch.
fn project_set_return_rows<F: Fs>(
    db: &GraphDb<F>,
    rel_vars: &[String],
    match_rs: &ResultSet,
    returns: &[RetItem],
    params: &BTreeMap<String, Value>,
) -> Result<ResultSet> {
    let columns: Vec<String> = returns.iter().map(ret_column_name).collect();
    let mut out = ResultSet::new(columns);
    for row in 0..match_rs.len() {
        let mut cells = Vec::with_capacity(returns.len());
        for item in returns {
            cells.push(eval_set_return_item(
                db, match_rs, row, rel_vars, item, params,
            )?);
        }
        out.push_row(cells);
    }
    Ok(out)
}

/// Single construction point for a `GraphMut` view over the split-borrowed graph fields.
/// Callers use `std::mem::take` on the engine before calling this, then restore it after.
/// Extract a `Vec<f64>` from a `Value::List` whose items are all numeric.
/// Returns `None` for non-list values or lists with non-numeric elements.
fn value_as_float_list(v: &Value) -> Option<Vec<f64>> {
    match v {
        Value::List(items) => items
            .iter()
            .map(|item| match item {
                Value::Float(f) => Some(*f),
                Value::Int(i) => Some(*i as f64),
                _ => None,
            })
            .collect(),
        _ => None,
    }
}

fn make_graph_mut<'a>(
    ids: &'a IdMap,
    syms: &'a mut Interner,
    labels: &'a [u32],
    props: core_storage::v8::seam::ColumnsView<'a>,
    topo: &'a mut Topology,
    edge_props: &'a mut EdgeProps,
) -> GraphMut<'a> {
    GraphMut {
        ids,
        syms,
        labels,
        props,
        topo,
        edge_props,
    }
}

/// Build a `ColumnsView` from the disjoint `props` overlay and optional V8 base.
///
/// Takes explicit field references rather than `&self` so the caller can hold
/// simultaneous mutable borrows of other fields (e.g. `syms`, `topo`).
fn build_props_view<'a>(
    props: &'a ColumnStore,
    base: &'a Option<std::sync::Arc<core_storage::v8::MappedBase>>,
) -> core_storage::v8::seam::ColumnsView<'a> {
    match base {
        None => core_storage::v8::seam::ColumnsView::owned(props),
        Some(b) => {
            let archived = b
                .columns()
                .expect("base columns CRC already verified at open");
            core_storage::v8::seam::ColumnsView::with_base(props, archived)
        }
    }
}

fn build_topo_view<'a>(
    overlay: &'a Topology,
    base: &'a Option<std::sync::Arc<core_storage::v8::MappedBase>>,
) -> core_storage::v8::seam::TopologyView<'a> {
    match base {
        None => core_storage::v8::seam::TopologyView::owned(overlay),
        Some(b) => {
            let archived_csr = b
                .topology()
                .expect("base topology CRC already verified at open");
            core_storage::v8::seam::TopologyView::with_base(overlay, archived_csr)
        }
    }
}

/// When [`GraphDb`] calls `Fs::sync` after a WAL append.
///
/// Default is [`Strict`](FsyncPolicy::Strict): every `log_then_apply_with`
/// fsyncs (single `insert_node` / `set_prop`). Ingest and `write_batch`
/// emit one `WalRecord::Batch` and fsync once at that frame (Batched).
/// [`Relaxed`](FsyncPolicy::Relaxed) skips WAL sync; [`GraphDb::snapshot`]
/// is still durable via `write_atomic`. Crash-recovery DST stays Strict.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum FsyncPolicy {
    /// Every WAL commit calls `fs.sync` (today's behavior).
    #[default]
    Strict,
    /// Sync only at a `Batch` frame end. Single-op path stays Strict unless
    /// this policy is set on the database.
    Batched,
    /// Never call `fs.sync`. [`GraphDb::snapshot`] still syncs via `write_atomic`.
    Relaxed,
}

pub struct GraphDb<F: Fs> {
    fs: F,
    ids: IdMap,
    syms: Interner,
    topo: Topology,
    props: ColumnStore,
    labels: Vec<u32>, // node id -> label symbol
    edge_props: EdgeProps,
    engine: RuleEngine,
    view_store: ViewStore,
    /// Incremental inverted index for full-text-lite search.
    /// Rebuild-on-open: populated from WAL replay + rebuild_all at open end.
    fulltext: FulltextIndex,
    event_sink: Option<Box<dyn Fn(MutationEvent) + Send + Sync>>,
    /// WAL fsync cadence. Default [`FsyncPolicy::Strict`].
    fsync: FsyncPolicy,
    /// Monotonically increasing per-commit counter.  A single `log_then_apply_with`
    /// call increments this once; all events emitted from that call share the same
    /// `commit_seq` value.
    commit_seq: u64,
    /// RBAC role definitions loaded from `roles.json` at open.
    ///
    /// `Some(roles)` — loaded successfully (may be empty when no roles are defined).
    /// `None` — `roles.json` was present but corrupt; `mask_for_role` returns
    /// `Err` for any request (fail-loud, never silently grant empty visibility).
    roles: Option<Vec<RoleDef>>,
    /// Live subscriptions.  Entries with a dead `Weak` are pruned on the next
    /// distribute_events call.
    subscriptions: Vec<SubEntry>,
    /// Live query subscriptions. Re-executed on every commit when non-empty.
    /// Dead `Weak` entries are pruned inside `distribute_events`.
    query_subscriptions: Vec<QuerySubEntry>,
    /// Queue capacity for new subscriptions created by this db.  Default is
    /// [`DEFAULT_SUB_CAPACITY`]; can be overridden via [`set_sub_capacity`]
    /// to test Lagged behaviour with small queues.
    sub_capacity: usize,
    /// True for as-of instances opened via [`GraphDb::open_at`].
    /// Every mutation method and `snapshot()` returns [`GraphError::ReadOnly`]
    /// when this flag is set.
    read_only: bool,
    /// Total WAL commit count at the time [`open_at`] was called.
    /// 0 for normal (non-as-of) instances.
    total_wal_commits: u64,
    /// Immutable mmap-backed base snapshot (V8).  When `Some`, `self.topo` is
    /// the WAL-replay overlay (empty at open time, populated by apply()) and
    /// reads go through a merged `TopologyView`.  `self.props` is always
    /// fully materialized (base + WAL replay) for HNSW/IVF and view compat.
    base: Option<Arc<core_storage::v8::MappedBase>>,
}

/// Options for [`GraphDb::open_with_options`].
#[derive(Clone, Copy, Debug)]
pub struct OpenOptions {
    /// Rewrite an old-format snapshot to the current VERSION after a
    /// successful load (default `true`). The old snapshot is kept as
    /// `snapshot.bin.bak` until the next clean open at the current version,
    /// at which point the `.bak` is deleted.
    ///
    /// Set to `false` to open a store without touching any on-disk files
    /// (useful for read-only inspection of a store at an older format).
    pub auto_migrate: bool,
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self { auto_migrate: true }
    }
}

/// Write `bytes` to `snapshot.bin.bak` atomically with full fsync.
///
/// Uses [`RealFs::write_atomic`] which applies `F_FULLFSYNC` on macOS and
/// `sync_all` on other platforms, then renames the `.tmp` file into place and
/// syncs the directory entry. This is the only correct path for writing the
/// `.bak` — plain `std::fs::write + sync_all` misses both `F_FULLFSYNC` and
/// the directory sync.
pub fn write_snapshot_bak(dir: &std::path::Path, bytes: &[u8]) -> crate::Result<()> {
    use core_storage::fs::{FileId, Fs as _};
    RealFs::new(dir)
        .map_err(core_storage::GraphError::Io)?
        .write_atomic(FileId::SnapshotBak, bytes)
        .map_err(core_storage::GraphError::Io)
}

/// Return the on-disk snapshot format version without decoding the full snapshot.
///
/// Reads only the 6-byte header (magic + version LE). Returns `None` when no
/// snapshot file exists (WAL-only store). Returns an error if the header is
/// malformed.
pub fn snapshot_version_at(dir: &std::path::Path) -> crate::Result<Option<u16>> {
    use std::io::Read as _;
    let path = dir.join("snapshot.bin");
    let mut header = [0u8; 6];
    let n = match std::fs::File::open(&path) {
        Ok(mut f) => f.read(&mut header).map_err(core_storage::GraphError::Io)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(core_storage::GraphError::Io(e)),
    };
    core_storage::snapshot::peek_version(&header[..n])
}

/// Options for [`GraphDb::snapshot_with`].
#[derive(Debug, Clone, Default)]
pub struct SnapshotOptions {
    /// When `true`, the WAL is preserved after the snapshot write.
    /// Pre-snapshot commits remain reachable via [`GraphDb::open_at`].
    /// When `false` (the default), the WAL is truncated to a minimal
    /// baseline so cold-start replay stays fast.
    pub keep_wal: bool,
}

impl GraphDb<RealFs> {
    /// Open the database at `dir` with default options.
    ///
    /// Equivalent to `open_with_options(dir, OpenOptions::default())`.
    /// Old-format snapshots (V5, V6) are automatically migrated to the
    /// current version on a successful load (see [`OpenOptions::auto_migrate`]).
    pub fn open(dir: &std::path::Path) -> Result<Self> {
        Self::open_with_options(dir, OpenOptions::default())
    }

    /// Open the database at `dir` with explicit options.
    ///
    /// When `opts.auto_migrate` is `true` (the default) and the on-disk
    /// snapshot is an older format version, this function:
    ///   1. Copies the current `snapshot.bin` to `snapshot.bin.bak` (atomic
    ///      + fsynced) before any modification.
    ///   2. Rewrites `snapshot.bin` at the current format version via
    ///      [`GraphDb::snapshot_with`] with `keep_wal: true` (WAL preserved).
    ///
    /// If migration fails the error is returned and the original files are
    /// intact (the `.bak` was written before the new snapshot was attempted).
    ///
    /// A clean open that finds the snapshot already at the current version
    /// deletes any leftover `.bak` file.
    ///
    /// WAL-only stores (no snapshot) are never auto-migrated on open.
    pub fn open_with_options(dir: &std::path::Path, opts: OpenOptions) -> Result<Self> {
        // Header-only peek — 6 bytes, no full decode.
        let snap_version = snapshot_version_at(dir)?;

        // Full load: decode snapshot + replay WAL + rebuild indexes.
        let mut db = Self::open_with(RealFs::new(dir)?)?;

        if opts.auto_migrate {
            match snap_version {
                Some(ver) if ver < core_storage::snapshot::VERSION => {
                    // Read the full snapshot bytes for the backup.
                    let snap_bytes = std::fs::read(dir.join("snapshot.bin"))
                        .map_err(core_storage::GraphError::Io)?;
                    // Write .bak atomically (fsynced) BEFORE touching snapshot.bin.
                    db.fs
                        .write_atomic(FileId::SnapshotBak, &snap_bytes)
                        .map_err(core_storage::GraphError::Io)?;
                    // Rewrite snapshot at current version; keep WAL intact.
                    db.snapshot_with(SnapshotOptions { keep_wal: true })?;
                }
                Some(_) => {
                    // Already current version: remove any leftover .bak.
                    let bak = dir.join("snapshot.bin.bak");
                    if bak.exists() {
                        std::fs::remove_file(&bak).map_err(core_storage::GraphError::Io)?;
                    }
                }
                None => {
                    // WAL-only store — nothing to migrate on open.
                }
            }
        }

        Ok(db)
    }

    /// Open a read-only view of the database as it existed after `commit`.
    ///
    /// Commit indices are 0-based over the current WAL: commit 0 is the state
    /// after the first WAL frame, commit N-1 is the state after the N-th (most
    /// recent) frame.  Call [`GraphDb::open`] to read the full current state.
    ///
    /// **Replay base.** [`GraphDb::snapshot`] truncates the WAL when it runs,
    /// so as-of can only reach commits recorded in the current WAL (those
    /// written after the most recent snapshot, or all commits if no snapshot
    /// was ever taken).  Commit 0 in `open_at` always refers to the first
    /// frame in the WAL that exists on disk, not the first ever write to the
    /// database.  When the on-disk snapshot recorded that it truncated the
    /// WAL (V7, default `keep_wal: false`), it is loaded as the base state
    /// before frame replay, so the as-of view includes all pre-snapshot data.
    /// Snapshots written with `keep_wal: true` (and legacy V5/V6 snapshots)
    /// are ignored and replay is WAL-only, as before.
    ///
    /// **Read-only.** Every mutation method and `snapshot()` on the returned
    /// instance returns [`GraphError::ReadOnly`].  Queries, `explain()`, and
    /// `stats()` work normally.
    ///
    /// # Errors
    /// - [`GraphError::CommitOutOfRange`] if `commit >= wal_commit_count` (including
    ///   when the WAL is empty after a snapshot).
    pub fn open_at(dir: &std::path::Path, commit: u64) -> Result<Self> {
        Self::open_at_with(RealFs::new(dir)?, commit)
    }
}

impl<F: Fs> GraphDb<F> {
    pub fn open_with(fs: F) -> Result<Self> {
        let mut db = Self {
            fs,
            ids: IdMap::new(),
            syms: Interner::new(),
            topo: Topology::new(),
            props: ColumnStore::new(),
            labels: Vec::new(),
            edge_props: EdgeProps::new(),
            engine: RuleEngine::new(),
            view_store: ViewStore::new(),
            fulltext: FulltextIndex::new(),
            event_sink: None,
            fsync: FsyncPolicy::Strict,
            commit_seq: 0,
            roles: Some(vec![]),
            subscriptions: Vec::new(),
            query_subscriptions: Vec::new(),
            sub_capacity: DEFAULT_SUB_CAPACITY,
            read_only: false,
            total_wal_commits: 0,
            base: None,
        };
        let snap_bytes = db.fs.read(FileId::Snapshot)?;
        // V8 path: keep MappedBase alive for zero-copy topology reads.
        // Non-V8 paths (fresh / V5-V7) fall through to the legacy decode path.
        if snap_bytes.len() >= 6
            && &snap_bytes[0..4] == b"GDB1"
            && u16::from_le_bytes([snap_bytes[4], snap_bytes[5]])
                == core_storage::snapshot::VERSION_8
        {
            // C2: use file mmap on RealFs (zero-copy, no heap copy); fall back to
            // from_bytes on SimFs (in-memory, path unavailable).
            let mapped = Arc::new(
                if let Some(snap_path) = db.fs.snapshot_path() {
                    core_storage::v8::MappedBase::map(&snap_path)
                } else {
                    core_storage::v8::MappedBase::from_bytes(snap_bytes)
                }
                .map_err(|e| GraphError::Corrupt {
                    detail: format!("v8: mmap open: {e:?}"),
                })?,
            );
            db.restore_v8_base(Arc::clone(&mapped))?;
            db.base = Some(mapped);
        } else if let Some(state) = core_storage::snapshot::decode(&snap_bytes)? {
            db.restore_snapshot_state(state)?;
        }
        let bytes = db.fs.read(FileId::Wal)?;
        let (records, valid_len) = decode_all(&bytes);
        if valid_len < bytes.len() {
            db.fs.write_atomic(FileId::Wal, &bytes[..valid_len])?;
        }
        // WAL-present path: build indexes eagerly BEFORE replay so that the
        // first replayed record does not trigger the lazy-init guard (which
        // would call reindex_all_load_ivf on an empty graph, defeating the
        // point of restoring IVF/HNSW blobs from the snapshot).
        if !records.is_empty() {
            db.engine.consume_retained_state_eager(
                &db.ids,
                &db.syms,
                &db.labels,
                build_props_view(&db.props, &db.base),
            );
        }
        for rec in records {
            db.apply(&rec)?;
            // Drain per-frame to keep pending_deltas O(1) during replay (I-2).
            // No subscriber exists yet; discard is correct.
            let _ = db.engine.drain_deltas();
        }
        // Enforce I-2: if the per-frame drain above is ever removed or skipped,
        // this assert catches the regression in debug builds immediately.
        debug_assert_eq!(
            db.engine.pending_delta_count(),
            0,
            "pending_deltas non-empty after replay — \
             per-frame drain must run inside the loop to keep memory O(1)"
        );
        // T2 note: the per-frame drain IS the suppression seam for replay.
        // Any future as-of replay path (Plan-15 T2) must drain here to feed
        // replaying subscribers; the mechanism is already in place.
        let _ = db.engine.drain_deltas(); // belt-and-braces no-op after loop drain
                                          // Rebuild view values after WAL replay only when there is no V8 base.
                                          // With a V8 base, view values are correct in the snapshot and are updated
                                          // incrementally during WAL replay (on_edge_changed / on_prop_changed).
                                          // A full rebuild would read overlay-only props (empty after restore_v8_base)
                                          // and overwrite correct base values with wrong results (e.g. NeighborAgg
                                          // Sum reads no "score" in overlay → writes 0.0, shadowing the correct
                                          // base value).
        if db.base.is_none() {
            let topo_view = TopologyView::owned(&db.topo);
            db.view_store
                .rebuild_all(&mut db.props, &topo_view, &db.ids, &db.syms, &db.labels);
        }
        // Rebuild full-text index after WAL replay.  Corrects drift from
        // per-record incremental apply during replay.
        db.fulltext.rebuild_all(
            &db.ids,
            &db.labels,
            &db.syms,
            build_props_view(&db.props, &db.base),
        );
        // Load roles sidecar. Missing file = no roles (Some(vec![])).
        // Corrupt/unparseable = poisoned (None); mask_for_role will fail-loud.
        db.roles = Self::load_roles_from_fs(&db.fs)?;
        Ok(db)
    }

    /// As-of replay for [`GraphDb::open_at`]: snapshot base (only when the
    /// snapshot truncated the WAL) plus the first `commit + 1` WAL frames;
    /// see [`GraphDb::open_at`] for the semantics.  The per-frame drain
    /// mirrors `open_with` exactly so pending_delta_count is 0 on exit.
    /// Restore all persisted state from a decoded snapshot. Shared by
    /// `open_with` and (when the snapshot truncated the WAL) `open_at_with`.
    fn restore_snapshot_state(
        &mut self,
        state: core_storage::snapshot::SnapshotState,
    ) -> Result<()> {
        self.ids = state.ids;
        self.syms = state.syms;
        self.topo = state.topo;
        self.props = state.props;
        self.labels = state.labels;
        self.edge_props = state.edge_props;
        let defs: Vec<RuleDef> = state
            .rule_defs
            .iter()
            .map(|b| {
                decode_rule_def(b).map_err(|e| GraphError::Corrupt {
                    detail: format!("snapshot rule_def deserialize: {e}"),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        self.engine =
            RuleEngine::from_persist(defs, state.provenance, state.rule_tripped, state.rule_fires);
        // Candidate indexes are rebuilt lazily on the first mutation (see
        // RuleEngine::on_node_changed).  HNSW blobs and IVF centroids from the
        // snapshot are retained without deserializing so that:
        //   - clean-open (empty WAL): indexes stay empty; blobs load on first
        //     ANN query via ensure_hnsw_loaded, or on first mutation via the
        //     lazy-init guard which calls reindex_all_load_ivf + load_hnsw_state.
        //   - WAL-present: open_with calls consume_retained_state_eager before
        //     replay so HNSW/IVF are live before any record fires the hooks.
        let ivf_state: BTreeMap<String, RuleIvfExport> = state
            .ivf_state
            .into_iter()
            .map(|(name, ps)| {
                (
                    name,
                    (
                        (ps.src.centroids, ps.src.clusters, ps.src.drift),
                        (ps.dst.centroids, ps.dst.clusters, ps.dst.drift),
                    ),
                )
            })
            .collect();
        // Store blobs without eagerly deserializing them.
        self.engine
            .store_snapshot_state(state.hnsw_state, ivf_state);
        // Restore view defs from snapshot (V5).
        // The ColumnStore already contains view values from the snapshot;
        // use restore_view (no collision check, no backfill) so the store
        // is aware of the definitions.  rebuild_all runs after WAL replay.
        for def_bytes in &state.view_defs {
            let def: ViewDef =
                bincode::deserialize(def_bytes).map_err(|e| GraphError::Corrupt {
                    detail: format!("snapshot view_def deserialize: {e}"),
                })?;
            self.view_store
                .restore_view(def)
                .map_err(|e| GraphError::Corrupt {
                    detail: format!("snapshot view restore: {e}"),
                })?;
        }
        Ok(())
    }

    /// Restore all persisted state from a V8 `MappedBase` snapshot, **except**
    /// topology (`self.topo` stays empty and serves as the WAL-replay overlay).
    ///
    /// `self.props` IS fully materialised from the base so that HNSW/IVF blob
    /// deserialization and view rebuild have access to all column data.
    fn restore_v8_base(&mut self, mapped: Arc<core_storage::v8::MappedBase>) -> Result<()> {
        self.ids = archived_to_idmap(mapped.ids().map_err(|e| GraphError::Corrupt {
            detail: format!("v8: ids section: {e:?}"),
        })?);
        self.syms = archived_to_interner(mapped.syms().map_err(|e| GraphError::Corrupt {
            detail: format!("v8: syms section: {e:?}"),
        })?);

        // C1: self.props is left as an empty overlay. Column reads go through
        // props_view() (ColumnsView::with_base), which consults the archived base
        // section zero-copy. This avoids the O(columns) heap copy at every open.

        // self.topo deliberately left as Topology::new() — overlay path.

        let meta = decode_meta(mapped.meta_bytes().map_err(|e| GraphError::Corrupt {
            detail: format!("v8: meta section: {e:?}"),
        })?)
        .map_err(|e| GraphError::Corrupt {
            detail: format!("v8: meta decode: {e:?}"),
        })?;
        self.labels = meta.labels;
        self.edge_props =
            archived_edge_props_to_owned(mapped.edge_props_section().map_err(|e| {
                GraphError::Corrupt {
                    detail: format!("v8: edge_props section: {e:?}"),
                }
            })?);

        // Restore rule engine.
        let (rule_def_bytes, rule_tripped, rule_fires) =
            archived_rules_meta_to_owned(mapped.rules_meta_section().map_err(|e| {
                GraphError::Corrupt {
                    detail: format!("v8: rules_meta section: {e:?}"),
                }
            })?);
        let defs: Vec<RuleDef> = rule_def_bytes
            .iter()
            .map(|b| {
                decode_rule_def(b).map_err(|e| GraphError::Corrupt {
                    detail: format!("v8: rule_def deserialize: {e}"),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let provenance =
            archived_provenance_to_owned(mapped.provenance_section().map_err(|e| {
                GraphError::Corrupt {
                    detail: format!("v8: provenance section: {e:?}"),
                }
            })?);
        self.engine = RuleEngine::from_persist(defs, provenance, rule_tripped, rule_fires);

        // Retain HNSW/IVF blobs without eagerly deserialising (same semantics
        // as restore_snapshot_state: lazy on clean open, eager before WAL replay).
        let hnsw_state =
            archived_hnsw_to_owned(mapped.hnsw_section().map_err(|e| GraphError::Corrupt {
                detail: format!("v8: hnsw section: {e:?}"),
            })?);
        let ivf_state: BTreeMap<String, RuleIvfExport> = meta
            .ivf_state
            .into_iter()
            .map(|(name, ps)| {
                (
                    name,
                    (
                        (ps.src.centroids, ps.src.clusters, ps.src.drift),
                        (ps.dst.centroids, ps.dst.clusters, ps.dst.drift),
                    ),
                )
            })
            .collect();
        self.engine.store_snapshot_state(hnsw_state, ivf_state);

        // Restore view definitions.
        let view_defs =
            archived_views_to_owned(mapped.views_section().map_err(|e| GraphError::Corrupt {
                detail: format!("v8: views section: {e:?}"),
            })?);
        for def_bytes in &view_defs {
            let def: ViewDef =
                bincode::deserialize(def_bytes).map_err(|e| GraphError::Corrupt {
                    detail: format!("v8: view_def deserialize: {e}"),
                })?;
            self.view_store
                .restore_view(def)
                .map_err(|e| GraphError::Corrupt {
                    detail: format!("v8: view restore: {e}"),
                })?;
        }
        Ok(())
    }

    /// Return a `TopologyView` that merges the mmap'd base (when present) with
    /// the in-memory WAL overlay.  Used by all read paths in db.rs that need
    /// the full merged topology without going through `self.view()`.
    fn topo_view(&self) -> TopologyView<'_> {
        match self.base {
            None => TopologyView::owned(&self.topo),
            Some(ref base) => {
                // SAFETY: base lives as long as self; CRC was verified at open.
                let archived = base
                    .topology()
                    .expect("base topology CRC already verified at open");
                TopologyView::with_base(&self.topo, archived)
            }
        }
    }

    /// Return a `ColumnsView` that merges the mmap'd base columns (when a V8
    /// snapshot is open) with the in-memory WAL overlay.  Reads consult the
    /// overlay first, then fall through to the archived base section zero-copy.
    fn props_view(&self) -> core_storage::v8::seam::ColumnsView<'_> {
        match self.base {
            None => core_storage::v8::seam::ColumnsView::owned(&self.props),
            Some(ref base) => {
                let archived = base
                    .columns()
                    .expect("base columns CRC already verified at open");
                core_storage::v8::seam::ColumnsView::with_base(&self.props, archived)
            }
        }
    }

    fn open_at_with(fs: F, commit: u64) -> Result<Self> {
        let mut db = Self {
            fs,
            ids: IdMap::new(),
            syms: Interner::new(),
            topo: Topology::new(),
            props: ColumnStore::new(),
            labels: Vec::new(),
            edge_props: EdgeProps::new(),
            engine: RuleEngine::new(),
            view_store: ViewStore::new(),
            fulltext: FulltextIndex::new(),
            event_sink: None,
            fsync: FsyncPolicy::Strict,
            commit_seq: 0,
            roles: Some(vec![]),
            subscriptions: Vec::new(),
            query_subscriptions: Vec::new(),
            sub_capacity: DEFAULT_SUB_CAPACITY,
            read_only: false, // set to true after replay
            total_wal_commits: 0,
            base: None,
        };
        // Base state: a truncating snapshot compacts all pre-truncation
        // commits, so the on-disk WAL head coincides with the snapshot and
        // frame replay must start from it — dense-id records (`Intern`,
        // `*Id`) embed the live intern/id numbering, which only a snapshot
        // base reproduces. `keep_wal` and legacy V5/V6 snapshots leave a WAL
        // that reaches further back; for those the historical WAL-only
        // replay applies (`wal_truncated` defaults to false on decode).
        let snap_bytes = db.fs.read(FileId::Snapshot)?;
        if let Some(state) = core_storage::snapshot::decode(&snap_bytes)? {
            if state.wal_truncated {
                db.restore_snapshot_state(state)?;
            }
        }
        let bytes = db.fs.read(FileId::Wal)?;
        let (records, _valid_len) = decode_all(&bytes);
        let total = records.len() as u64;
        if commit >= total {
            return Err(GraphError::CommitOutOfRange { commit, total });
        }
        // Replay frames 0..=commit — identical drain pattern to open_with so
        // the pending_delta_count == 0 invariant holds.
        for rec in records.into_iter().take((commit + 1) as usize) {
            db.apply(&rec)?;
            // Drain per-frame: no subscriber exists; discard is correct.
            // This keeps memory O(1) and mirrors the open_with seam exactly.
            let _ = db.engine.drain_deltas();
        }
        // Pin: pending_delta_count must be 0 after as-of replay, mirroring T1's
        // post-loop assert in open_with.
        debug_assert_eq!(
            db.engine.pending_delta_count(),
            0,
            "pending_deltas non-empty after open_at replay — \
             per-frame drain must run inside the loop to keep memory O(1)"
        );
        let _ = db.engine.drain_deltas(); // belt-and-braces no-op
                                          // Rebuild view values after WAL replay so derived-edge-driven views
                                          // reflect the as-of state.  open_at always uses the legacy path (no V8
                                          // base), so topo_view is always owned.
        {
            let topo_view = TopologyView::owned(&db.topo);
            db.view_store
                .rebuild_all(&mut db.props, &topo_view, &db.ids, &db.syms, &db.labels);
        }
        // Rebuild full-text index for as-of view (mirrors open_with pattern).
        db.fulltext.rebuild_all(
            &db.ids,
            &db.labels,
            &db.syms,
            build_props_view(&db.props, &db.base),
        );
        // Load roles sidecar (current roles, not point-in-time).
        db.roles = Self::load_roles_from_fs(&db.fs)?;
        db.read_only = true;
        db.total_wal_commits = total;
        Ok(db)
    }

    /// Whether this instance is a read-only as-of view.
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Total number of WAL commits at the time [`open_at`] was called.
    /// Returns 0 for normal (non-as-of) instances.
    pub fn total_wal_commits(&self) -> u64 {
        self.total_wal_commits
    }

    /// Apply a record to in-memory state. Used by both live writes and replay,
    /// so replay is definitionally identical to the original execution.
    fn apply(&mut self, rec: &WalRecord) -> Result<()> {
        match rec {
            WalRecord::InsertNode { label, key, props } => {
                let id = self.ids.try_insert(key)?;
                let sym = self.syms.intern(label);
                if self.labels.len() <= id as usize {
                    // gap slots are sentinels, never valid label symbols
                    self.labels.resize(id as usize + 1, u32::MAX);
                }
                self.labels[id as usize] = sym;
                for (field, value) in props {
                    self.props.set(id, field, value.clone());
                }
                // Initialize view values for the new node before the engine runs so
                // delta-based increments start from a known zero baseline.
                self.view_store
                    .init_node_views(id, &mut self.props, &self.syms, &self.labels);
                // Fire rules for the newly inserted node.
                let cursor = self.engine.pending_delta_count();
                let mut eng = std::mem::take(&mut self.engine);
                {
                    let mut gm = make_graph_mut(
                        &self.ids,
                        &mut self.syms,
                        &self.labels,
                        build_props_view(&self.props, &self.base),
                        &mut self.topo,
                        &mut self.edge_props,
                    );
                    eng.on_node_changed(id, None, &mut gm);
                }
                self.engine = eng;
                // Process derived-edge deltas for view maintenance.
                // Fast path: skip the O(delta_count) allocation when no views exist.
                if !self.view_store.is_empty() {
                    #[cfg(test)]
                    DELTA_COPY_COUNT.with(|c| c.set(c.get() + 1));
                    let new_deltas: Vec<_> = self.engine.pending_deltas_since(cursor).to_vec();
                    for d in &new_deltas {
                        self.view_store.on_edge_changed(
                            d.etype_sym,
                            d.src_id,
                            d.dst_id,
                            d.fired,
                            &mut self.props,
                            &build_topo_view(&self.topo, &self.base),
                            &self.ids,
                            &self.syms,
                            &self.labels,
                            self.base.as_ref().map(|b| {
                                b.columns()
                                    .expect("base columns CRC already verified at open")
                            }),
                        );
                    }
                }
                // Full-text index maintenance: index enabled fields for this label.
                if self.fulltext.has_label(label) {
                    for (field, value) in props {
                        if self.fulltext.is_enabled(label, field) {
                            self.fulltext.add_tokens(id, field, value);
                        }
                    }
                }
            }
            WalRecord::InsertEdge {
                edge_type,
                src_key,
                dst_key,
            } => {
                let src = self.ids.get(src_key).ok_or_else(|| GraphError::Corrupt {
                    detail: format!("wal replay references unknown key {src_key}"),
                })?;
                let dst = self.ids.get(dst_key).ok_or_else(|| GraphError::Corrupt {
                    detail: format!("wal replay references unknown key {dst_key}"),
                })?;
                let etype = self.syms.intern(edge_type);
                // Skip if the edge is already visible in the merged base+overlay
                // view.  This keeps WAL replay idempotent when the WAL contains
                // pre-snapshot records that are already encoded in a V8 base
                // (keep_wal=true opens and crash-before-truncation scenarios).
                if self.base.is_some()
                    && self
                        .topo_view()
                        .neighbors(etype, Direction::Out, src)
                        .contains(&dst)
                {
                    return Ok(());
                }
                self.topo.add_edge(etype, src, dst);
                // View maintenance for manual edge insert.
                self.view_store.on_edge_changed(
                    etype,
                    src,
                    dst,
                    true,
                    &mut self.props,
                    &build_topo_view(&self.topo, &self.base),
                    &self.ids,
                    &self.syms,
                    &self.labels,
                    self.base.as_ref().map(|b| {
                        b.columns()
                            .expect("base columns CRC already verified at open")
                    }),
                );
                // Rule engine: via-hop rules must update when user edges change.
                let cursor = self.engine.pending_delta_count();
                let mut eng = std::mem::take(&mut self.engine);
                {
                    let mut gm = make_graph_mut(
                        &self.ids,
                        &mut self.syms,
                        &self.labels,
                        build_props_view(&self.props, &self.base),
                        &mut self.topo,
                        &mut self.edge_props,
                    );
                    eng.on_edge_changed(edge_type, src, dst, &mut gm);
                }
                self.engine = eng;
                if !self.view_store.is_empty() {
                    let new_deltas: Vec<_> = self.engine.pending_deltas_since(cursor).to_vec();
                    for d in &new_deltas {
                        self.view_store.on_edge_changed(
                            d.etype_sym,
                            d.src_id,
                            d.dst_id,
                            d.fired,
                            &mut self.props,
                            &build_topo_view(&self.topo, &self.base),
                            &self.ids,
                            &self.syms,
                            &self.labels,
                            self.base.as_ref().map(|b| {
                                b.columns()
                                    .expect("base columns CRC already verified at open")
                            }),
                        );
                    }
                }
            }
            WalRecord::SetProp { key, field, value } => {
                let id = self.ids.get(key).ok_or_else(|| GraphError::Corrupt {
                    detail: format!("wal replay references unknown key {key}"),
                })?;
                let old_value = build_props_view(&self.props, &self.base)
                    .get(id, field)
                    .map(|vr| vr.into_value());
                self.props.set(id, field, value.clone());
                // Fire rules for the changed field.
                let cursor = self.engine.pending_delta_count();
                let mut eng = std::mem::take(&mut self.engine);
                {
                    let mut gm = make_graph_mut(
                        &self.ids,
                        &mut self.syms,
                        &self.labels,
                        build_props_view(&self.props, &self.base),
                        &mut self.topo,
                        &mut self.edge_props,
                    );
                    eng.on_node_changed(id, Some((field, old_value)), &mut gm);
                }
                self.engine = eng;
                // Derived-edge deltas → view updates.
                if !self.view_store.is_empty() {
                    #[cfg(test)]
                    DELTA_COPY_COUNT.with(|c| c.set(c.get() + 1));
                    let new_deltas: Vec<_> = self.engine.pending_deltas_since(cursor).to_vec();
                    for d in &new_deltas {
                        self.view_store.on_edge_changed(
                            d.etype_sym,
                            d.src_id,
                            d.dst_id,
                            d.fired,
                            &mut self.props,
                            &build_topo_view(&self.topo, &self.base),
                            &self.ids,
                            &self.syms,
                            &self.labels,
                            self.base.as_ref().map(|b| {
                                b.columns()
                                    .expect("base columns CRC already verified at open")
                            }),
                        );
                    }
                }
                // Neighbor-aggregate views that read `field` must also update.
                self.view_store.on_prop_changed(
                    id,
                    field,
                    &mut self.props,
                    &build_topo_view(&self.topo, &self.base),
                    &self.ids,
                    &self.syms,
                    &self.labels,
                    self.base.as_ref().map(|b| {
                        b.columns()
                            .expect("base columns CRC already verified at open")
                    }),
                );
                // Full-text index maintenance: update tokens for this field if indexed.
                if self.fulltext.field_indexed(field) {
                    let label_opt = self.labels.get(id as usize).and_then(|&sym| {
                        if sym == u32::MAX {
                            None
                        } else {
                            self.syms.resolve(sym)
                        }
                    });
                    if let Some(label) = label_opt {
                        if self.fulltext.is_enabled(label, field) {
                            self.fulltext.remove_node_field(id, field);
                            self.fulltext.add_tokens(id, field, value);
                        }
                    }
                }
            }
            WalRecord::Intern { id, text } => {
                if let Some(existing) = self.syms.get(text) {
                    if existing != *id {
                        return Err(GraphError::Corrupt {
                            detail: format!(
                                "wal intern mismatch for {text:?}: have {existing}, record {id}"
                            ),
                        });
                    }
                } else {
                    let got = self.syms.intern(text);
                    if got != *id {
                        return Err(GraphError::Corrupt {
                            detail: format!(
                                "wal intern assigned {got} for {text:?}, record wanted {id}"
                            ),
                        });
                    }
                }
            }
            WalRecord::InsertNodeId { label, key, props } => {
                let id = self.ids.try_insert(key)?;
                if self.labels.len() <= id as usize {
                    self.labels.resize(id as usize + 1, u32::MAX);
                }
                self.labels[id as usize] = *label;
                let label_str = self
                    .syms
                    .resolve(*label)
                    .ok_or_else(|| GraphError::Corrupt {
                        detail: format!("wal InsertNodeId unknown label intern {label}"),
                    })?
                    .to_string();
                for (field_sym, value) in props {
                    let field =
                        self.syms
                            .resolve(*field_sym)
                            .ok_or_else(|| GraphError::Corrupt {
                                detail: format!(
                                    "wal InsertNodeId unknown field intern {field_sym}"
                                ),
                            })?;
                    self.props.set(id, field, value.clone());
                }
                self.view_store
                    .init_node_views(id, &mut self.props, &self.syms, &self.labels);
                let cursor = self.engine.pending_delta_count();
                let mut eng = std::mem::take(&mut self.engine);
                {
                    let mut gm = make_graph_mut(
                        &self.ids,
                        &mut self.syms,
                        &self.labels,
                        build_props_view(&self.props, &self.base),
                        &mut self.topo,
                        &mut self.edge_props,
                    );
                    eng.on_node_changed(id, None, &mut gm);
                }
                self.engine = eng;
                if !self.view_store.is_empty() {
                    #[cfg(test)]
                    DELTA_COPY_COUNT.with(|c| c.set(c.get() + 1));
                    let new_deltas: Vec<_> = self.engine.pending_deltas_since(cursor).to_vec();
                    for d in &new_deltas {
                        self.view_store.on_edge_changed(
                            d.etype_sym,
                            d.src_id,
                            d.dst_id,
                            d.fired,
                            &mut self.props,
                            &build_topo_view(&self.topo, &self.base),
                            &self.ids,
                            &self.syms,
                            &self.labels,
                            self.base.as_ref().map(|b| {
                                b.columns()
                                    .expect("base columns CRC already verified at open")
                            }),
                        );
                    }
                }
                if self.fulltext.has_label(&label_str) {
                    for (field_sym, value) in props {
                        let Some(field) = self.syms.resolve(*field_sym) else {
                            continue;
                        };
                        if self.fulltext.is_enabled(&label_str, field) {
                            self.fulltext.add_tokens(id, field, value);
                        }
                    }
                }
            }
            WalRecord::InsertEdgeId { etype, src, dst } => {
                // Replay-over-snapshot: dense ids in the pre-snapshot WAL may
                // already be tombstoned. Skip rather than attaching edges to
                // dead ids (DeleteNode keys the live re-insert, not the old id).
                if self.ids.is_tombstoned(*src)
                    || self.ids.is_tombstoned(*dst)
                    || self.ids.key_of(*src).is_none()
                    || self.ids.key_of(*dst).is_none()
                {
                    return Ok(());
                }
                // Skip if already visible in the merged view (same idempotency
                // guard as InsertEdge above: prevents double-counting when
                // pre-snapshot WAL records are replayed over a V8 base).
                if self.base.is_some()
                    && self
                        .topo_view()
                        .neighbors(*etype, Direction::Out, *src)
                        .contains(dst)
                {
                    return Ok(());
                }
                self.topo.add_edge(*etype, *src, *dst);
                self.view_store.on_edge_changed(
                    *etype,
                    *src,
                    *dst,
                    true,
                    &mut self.props,
                    &build_topo_view(&self.topo, &self.base),
                    &self.ids,
                    &self.syms,
                    &self.labels,
                    self.base.as_ref().map(|b| {
                        b.columns()
                            .expect("base columns CRC already verified at open")
                    }),
                );
                // Rule engine: via-hop rules fire when user via-edges are inserted.
                // Resolve etype back to string so on_edge_changed can match rules by name.
                if let Some(etype_str) = self.syms.resolve(*etype).map(|s| s.to_string()) {
                    let cursor = self.engine.pending_delta_count();
                    let mut eng = std::mem::take(&mut self.engine);
                    {
                        let mut gm = make_graph_mut(
                            &self.ids,
                            &mut self.syms,
                            &self.labels,
                            build_props_view(&self.props, &self.base),
                            &mut self.topo,
                            &mut self.edge_props,
                        );
                        eng.on_edge_changed(&etype_str, *src, *dst, &mut gm);
                    }
                    self.engine = eng;
                    if !self.view_store.is_empty() {
                        let new_deltas: Vec<_> = self.engine.pending_deltas_since(cursor).to_vec();
                        for d in &new_deltas {
                            self.view_store.on_edge_changed(
                                d.etype_sym,
                                d.src_id,
                                d.dst_id,
                                d.fired,
                                &mut self.props,
                                &build_topo_view(&self.topo, &self.base),
                                &self.ids,
                                &self.syms,
                                &self.labels,
                                self.base.as_ref().map(|b| {
                                    b.columns()
                                        .expect("base columns CRC already verified at open")
                                }),
                            );
                        }
                    }
                }
            }
            WalRecord::SetPropId { id, field, value } => {
                if self.ids.is_tombstoned(*id) || self.ids.key_of(*id).is_none() {
                    return Ok(());
                }
                let field_str = self
                    .syms
                    .resolve(*field)
                    .ok_or_else(|| GraphError::Corrupt {
                        detail: format!("wal SetPropId unknown field intern {field}"),
                    })?
                    .to_string();
                let old_value = build_props_view(&self.props, &self.base)
                    .get(*id, &field_str)
                    .map(|vr| vr.into_value());
                self.props.set(*id, &field_str, value.clone());
                let cursor = self.engine.pending_delta_count();
                let mut eng = std::mem::take(&mut self.engine);
                {
                    let mut gm = make_graph_mut(
                        &self.ids,
                        &mut self.syms,
                        &self.labels,
                        build_props_view(&self.props, &self.base),
                        &mut self.topo,
                        &mut self.edge_props,
                    );
                    eng.on_node_changed(*id, Some((field_str.as_str(), old_value)), &mut gm);
                }
                self.engine = eng;
                if !self.view_store.is_empty() {
                    #[cfg(test)]
                    DELTA_COPY_COUNT.with(|c| c.set(c.get() + 1));
                    let new_deltas: Vec<_> = self.engine.pending_deltas_since(cursor).to_vec();
                    for d in &new_deltas {
                        self.view_store.on_edge_changed(
                            d.etype_sym,
                            d.src_id,
                            d.dst_id,
                            d.fired,
                            &mut self.props,
                            &build_topo_view(&self.topo, &self.base),
                            &self.ids,
                            &self.syms,
                            &self.labels,
                            self.base.as_ref().map(|b| {
                                b.columns()
                                    .expect("base columns CRC already verified at open")
                            }),
                        );
                    }
                }
                self.view_store.on_prop_changed(
                    *id,
                    &field_str,
                    &mut self.props,
                    &build_topo_view(&self.topo, &self.base),
                    &self.ids,
                    &self.syms,
                    &self.labels,
                    self.base.as_ref().map(|b| {
                        b.columns()
                            .expect("base columns CRC already verified at open")
                    }),
                );
                if self.fulltext.field_indexed(&field_str) {
                    let label_opt = self.labels.get(*id as usize).and_then(|&sym| {
                        if sym == u32::MAX {
                            None
                        } else {
                            self.syms.resolve(sym)
                        }
                    });
                    if let Some(label) = label_opt {
                        if self.fulltext.is_enabled(label, &field_str) {
                            self.fulltext.remove_node_field(*id, &field_str);
                            self.fulltext.add_tokens(*id, &field_str, value);
                        }
                    }
                }
            }
            WalRecord::CreateRule { def_bytes } => {
                let def: RuleDef = decode_rule_def(def_bytes).map_err(|e| GraphError::Corrupt {
                    detail: format!("CreateRule def_bytes deserialize failed: {e}"),
                })?;
                // Replay-over-snapshot idempotency: the rule was captured in the snapshot
                // so the engine already has it; silently skip to avoid a spurious
                // RuleInvalid error in the crash window between snapshot write and WAL
                // truncation.
                if self.engine.rules().any(|r| r.name == def.name) {
                    return Ok(());
                }
                let cursor = self.engine.pending_delta_count();
                let mut eng = std::mem::take(&mut self.engine);
                let result = {
                    let mut gm = make_graph_mut(
                        &self.ids,
                        &mut self.syms,
                        &self.labels,
                        build_props_view(&self.props, &self.base),
                        &mut self.topo,
                        &mut self.edge_props,
                    );
                    eng.create_rule(def, &mut gm)
                };
                self.engine = eng;
                result.map_err(|e| GraphError::RuleInvalid { detail: e })?;
                // Derived-edge fires from backfill → view updates.
                // Fast path: skip O(edge_count) allocation when no views exist.
                if !self.view_store.is_empty() {
                    #[cfg(test)]
                    DELTA_COPY_COUNT.with(|c| c.set(c.get() + 1));
                    let new_deltas: Vec<_> = self.engine.pending_deltas_since(cursor).to_vec();
                    for d in &new_deltas {
                        self.view_store.on_edge_changed(
                            d.etype_sym,
                            d.src_id,
                            d.dst_id,
                            d.fired,
                            &mut self.props,
                            &build_topo_view(&self.topo, &self.base),
                            &self.ids,
                            &self.syms,
                            &self.labels,
                            self.base.as_ref().map(|b| {
                                b.columns()
                                    .expect("base columns CRC already verified at open")
                            }),
                        );
                    }
                }
            }
            WalRecord::DeleteRule { name } => {
                // Replay-over-snapshot idempotency: the snapshot already captured the
                // post-delete state so the rule is absent; silently skip to avoid a
                // spurious RuleNotFound error in the crash window between snapshot write
                // and WAL truncation.
                if !self.engine.rules().any(|r| r.name == *name) {
                    return Ok(());
                }
                let cursor = self.engine.pending_delta_count();
                let mut eng = std::mem::take(&mut self.engine);
                let result = {
                    let mut gm = make_graph_mut(
                        &self.ids,
                        &mut self.syms,
                        &self.labels,
                        build_props_view(&self.props, &self.base),
                        &mut self.topo,
                        &mut self.edge_props,
                    );
                    eng.delete_rule(name, &mut gm)
                };
                self.engine = eng;
                result.map_err(|_| GraphError::RuleNotFound { name: name.clone() })?;
                // Derived-edge retractions → view updates.
                if !self.view_store.is_empty() {
                    #[cfg(test)]
                    DELTA_COPY_COUNT.with(|c| c.set(c.get() + 1));
                    let new_deltas: Vec<_> = self.engine.pending_deltas_since(cursor).to_vec();
                    for d in &new_deltas {
                        self.view_store.on_edge_changed(
                            d.etype_sym,
                            d.src_id,
                            d.dst_id,
                            d.fired,
                            &mut self.props,
                            &build_topo_view(&self.topo, &self.base),
                            &self.ids,
                            &self.syms,
                            &self.labels,
                            self.base.as_ref().map(|b| {
                                b.columns()
                                    .expect("base columns CRC already verified at open")
                            }),
                        );
                    }
                }
            }
            WalRecord::RemoveProp { key, field } => {
                // Recovery-safe: unknown key or already-absent field is a
                // clean no-op. Crash-window replay over a snapshot that
                // already applied this record must not Err.
                let Some(id) = self.ids.get(key) else {
                    return Ok(());
                };
                // Read old value through the seam for rule retraction.
                let old = build_props_view(&self.props, &self.base)
                    .get(id, field)
                    .map(|vr| vr.into_value());
                self.props.remove(id, field);
                // If the base still supplies the value after the overlay removal,
                // record a tombstone so ColumnsView::get does not resurrect it.
                // This covers both the base-only case AND the both-resident case:
                //   base-only (in_overlay=false): old prop was only in base, remove
                //     is a no-op on overlay, base still visible → tombstone needed.
                //   both-resident (in_overlay=true): overlay had v2, base has v1;
                //     removing overlay uncovers v1 → tombstone needed.
                // Idempotent on double-replay: second pass sees the tombstone →
                // get() returns None → condition is false → no duplicate tombstone.
                if build_props_view(&self.props, &self.base)
                    .get(id, field)
                    .is_some()
                {
                    self.props.record_prop_tombstone(id, field);
                }
                let cursor = self.engine.pending_delta_count();
                let mut eng = std::mem::take(&mut self.engine);
                {
                    let mut gm = make_graph_mut(
                        &self.ids,
                        &mut self.syms,
                        &self.labels,
                        build_props_view(&self.props, &self.base),
                        &mut self.topo,
                        &mut self.edge_props,
                    );
                    eng.on_node_changed(id, Some((field, old)), &mut gm);
                }
                self.engine = eng;
                // Derived-edge deltas → view updates.
                if !self.view_store.is_empty() {
                    #[cfg(test)]
                    DELTA_COPY_COUNT.with(|c| c.set(c.get() + 1));
                    let new_deltas: Vec<_> = self.engine.pending_deltas_since(cursor).to_vec();
                    for d in &new_deltas {
                        self.view_store.on_edge_changed(
                            d.etype_sym,
                            d.src_id,
                            d.dst_id,
                            d.fired,
                            &mut self.props,
                            &build_topo_view(&self.topo, &self.base),
                            &self.ids,
                            &self.syms,
                            &self.labels,
                            self.base.as_ref().map(|b| {
                                b.columns()
                                    .expect("base columns CRC already verified at open")
                            }),
                        );
                    }
                }
                // Neighbor-aggregate views that read `field` must also update.
                self.view_store.on_prop_changed(
                    id,
                    field,
                    &mut self.props,
                    &build_topo_view(&self.topo, &self.base),
                    &self.ids,
                    &self.syms,
                    &self.labels,
                    self.base.as_ref().map(|b| {
                        b.columns()
                            .expect("base columns CRC already verified at open")
                    }),
                );
                // Full-text index maintenance: remove tokens for this field.
                if self.fulltext.field_indexed(field) {
                    self.fulltext.remove_node_field(id, field);
                }
            }
            WalRecord::DeleteEdge {
                edge_type,
                src_key,
                dst_key,
            } => {
                // Recovery-safe: unknown keys, unknown etype, or already-
                // absent edge is a clean no-op (remove_edge returns false).
                let Some(src) = self.ids.get(src_key) else {
                    return Ok(());
                };
                let Some(dst) = self.ids.get(dst_key) else {
                    return Ok(());
                };
                let Some(etype) = self.syms.get(edge_type) else {
                    return Ok(());
                };
                // I3: phantom-tombstone guard.  When a V8 base is present, a
                // DeleteEdge WAL record for an edge that was already absorbed into
                // the new base (i.e. neither in overlay nor in base) must be skipped.
                // Without this guard, remove_edge records a tombstone for an edge
                // that no longer exists, incorrectly understating edge_count.
                if self.base.is_some()
                    && !self
                        .topo_view()
                        .neighbors(etype, core_storage::topology::Direction::Out, src)
                        .contains(&dst)
                {
                    return Ok(());
                }
                self.topo.remove_edge(etype, src, dst);
                self.edge_props.remove_edge(etype, src, dst);
                // View maintenance for manual edge delete (topo already updated above).
                self.view_store.on_edge_changed(
                    etype,
                    src,
                    dst,
                    false,
                    &mut self.props,
                    &build_topo_view(&self.topo, &self.base),
                    &self.ids,
                    &self.syms,
                    &self.labels,
                    self.base.as_ref().map(|b| {
                        b.columns()
                            .expect("base columns CRC already verified at open")
                    }),
                );
                // Rule engine: via-hop rules must retract when user via-edges are deleted.
                let cursor = self.engine.pending_delta_count();
                let mut eng = std::mem::take(&mut self.engine);
                {
                    let mut gm = make_graph_mut(
                        &self.ids,
                        &mut self.syms,
                        &self.labels,
                        build_props_view(&self.props, &self.base),
                        &mut self.topo,
                        &mut self.edge_props,
                    );
                    eng.on_edge_changed(edge_type, src, dst, &mut gm);
                }
                self.engine = eng;
                if !self.view_store.is_empty() {
                    let new_deltas: Vec<_> = self.engine.pending_deltas_since(cursor).to_vec();
                    for d in &new_deltas {
                        self.view_store.on_edge_changed(
                            d.etype_sym,
                            d.src_id,
                            d.dst_id,
                            d.fired,
                            &mut self.props,
                            &build_topo_view(&self.topo, &self.base),
                            &self.ids,
                            &self.syms,
                            &self.labels,
                            self.base.as_ref().map(|b| {
                                b.columns()
                                    .expect("base columns CRC already verified at open")
                            }),
                        );
                    }
                }
            }
            WalRecord::DeleteNode { key } => {
                // Recovery-safe: already-tombstoned / unknown key is a clean
                // no-op. Crash-window replay over a snapshot that already
                // applied this record cannot recover the retired id from the
                // key (`IdMap::get` is None), so every subsequent step is
                // skipped. Each step is independently idempotent if invoked
                // twice on a still-live id: retraction is a no-op on empty
                // provenance, `remove_edge` returns false, `remove_all` is a
                // no-op, `ids.delete` returns None, label sentinel is sticky.
                let Some(n) = self.ids.get(key) else {
                    return Ok(());
                };

                // (1) Retract derived edges + de-index while props/labels live.
                let cursor = self.engine.pending_delta_count();
                let mut eng = std::mem::take(&mut self.engine);
                {
                    let mut gm = make_graph_mut(
                        &self.ids,
                        &mut self.syms,
                        &self.labels,
                        build_props_view(&self.props, &self.base),
                        &mut self.topo,
                        &mut self.edge_props,
                    );
                    eng.on_node_removed(n, &mut gm);
                }
                self.engine = eng;
                // Derived-edge retractions → view updates for neighbors.
                if !self.view_store.is_empty() {
                    #[cfg(test)]
                    DELTA_COPY_COUNT.with(|c| c.set(c.get() + 1));
                    let new_deltas: Vec<_> = self.engine.pending_deltas_since(cursor).to_vec();
                    for d in &new_deltas {
                        self.view_store.on_edge_changed(
                            d.etype_sym,
                            d.src_id,
                            d.dst_id,
                            d.fired,
                            &mut self.props,
                            &build_topo_view(&self.topo, &self.base),
                            &self.ids,
                            &self.syms,
                            &self.labels,
                            self.base.as_ref().map(|b| {
                                b.columns()
                                    .expect("base columns CRC already verified at open")
                            }),
                        );
                    }
                }

                // (2) Sweep remaining user edges touching n, both directions,
                // every etype. Collect then remove so neighbor slices stay valid.
                // Remove from topo first, then call view maintenance so Avg/Min/Max
                // recompute sees the correct (reduced) neighbor set.
                let etypes: Vec<u32> = self.topo.etypes().collect();
                let mut doomed = Vec::new();
                for et in &etypes {
                    for &dst in self.topo.neighbors(*et, Direction::Out, n).as_ref() {
                        doomed.push((*et, n, dst));
                    }
                    for &src in self.topo.neighbors(*et, Direction::In, n).as_ref() {
                        doomed.push((*et, src, n));
                    }
                }
                for (et, s, d) in doomed {
                    self.topo.remove_edge(et, s, d);
                    self.edge_props.remove_edge(et, s, d);
                    // View maintenance: n's own view values will be cleared by
                    // remove_all below; only update surviving neighbors.
                    self.view_store.on_edge_changed(
                        et,
                        s,
                        d,
                        false,
                        &mut self.props,
                        &build_topo_view(&self.topo, &self.base),
                        &self.ids,
                        &self.syms,
                        &self.labels,
                        self.base.as_ref().map(|b| {
                            b.columns()
                                .expect("base columns CRC already verified at open")
                        }),
                    );
                }

                // (3) Drop every remaining prop (`ColumnStore::remove_all`).
                self.props.remove_all(n);
                // Full-text index maintenance: remove all tokens for this node.
                self.fulltext.remove_node(n);

                // (4) Retire the dense id and stamp the label sentinel.
                self.ids.delete(key);
                if let Some(slot) = self.labels.get_mut(n as usize) {
                    *slot = u32::MAX;
                }
            }
            WalRecord::Batch(inner) => {
                // Apply each inner record in order through the same apply path.
                // Inner records are validated free of nested Batch by encode_record.
                for rec in inner {
                    self.apply(rec)?;
                }
            }
            WalRecord::RebuildRule { name } => {
                // Replay-over-snapshot idempotency: the snapshot may already
                // reflect a later delete_rule, so the rule is absent; skip.
                if !self.engine.rules().any(|r| r.name == *name) {
                    return Ok(());
                }
                let cursor = self.engine.pending_delta_count();
                let mut eng = std::mem::take(&mut self.engine);
                let result = {
                    let mut gm = make_graph_mut(
                        &self.ids,
                        &mut self.syms,
                        &self.labels,
                        build_props_view(&self.props, &self.base),
                        &mut self.topo,
                        &mut self.edge_props,
                    );
                    eng.rebuild(name, &mut gm)
                };
                self.engine = eng;
                result.map_err(|_| GraphError::RuleNotFound { name: name.clone() })?;
                // Derived-edge delta changes → view updates.
                if !self.view_store.is_empty() {
                    #[cfg(test)]
                    DELTA_COPY_COUNT.with(|c| c.set(c.get() + 1));
                    let new_deltas: Vec<_> = self.engine.pending_deltas_since(cursor).to_vec();
                    for d in &new_deltas {
                        self.view_store.on_edge_changed(
                            d.etype_sym,
                            d.src_id,
                            d.dst_id,
                            d.fired,
                            &mut self.props,
                            &build_topo_view(&self.topo, &self.base),
                            &self.ids,
                            &self.syms,
                            &self.labels,
                            self.base.as_ref().map(|b| {
                                b.columns()
                                    .expect("base columns CRC already verified at open")
                            }),
                        );
                    }
                }
            }
            WalRecord::CreateView { def_bytes } => {
                let def: ViewDef =
                    bincode::deserialize(def_bytes).map_err(|e| GraphError::Corrupt {
                        detail: format!("CreateView def_bytes deserialize failed: {e}"),
                    })?;
                // Replay-over-snapshot idempotency: view already present → skip.
                if self.view_store.has_view(&def.name) {
                    return Ok(());
                }
                self.view_store
                    .create_view(
                        def,
                        &mut self.props,
                        &build_topo_view(&self.topo, &self.base),
                        &self.ids,
                        &self.syms,
                        &self.labels,
                    )
                    .map_err(|e| GraphError::RuleInvalid { detail: e })?;
            }
            WalRecord::DeleteView { name } => {
                // Replay-over-snapshot idempotency: view already absent → skip.
                if !self.view_store.has_view(name) {
                    return Ok(());
                }
                self.view_store
                    .delete_view(name, &mut self.props, &self.ids, &self.labels, &self.syms)
                    .map_err(|_| GraphError::RuleNotFound { name: name.clone() })?;
            }
            WalRecord::EnableFulltext { label, field } => {
                // Replay-over-snapshot idempotency: already enabled → skip.
                if self.fulltext.is_enabled(label, field) {
                    return Ok(());
                }
                self.fulltext.enable(label, field);
                // Backfill: index all live nodes of this label that have the field.
                let n = self.ids.len() as u32;
                for id in 0..n {
                    let Some(&sym) = self.labels.get(id as usize) else {
                        continue;
                    };
                    if sym == u32::MAX {
                        continue; // tombstoned
                    }
                    let Some(lbl) = self.syms.resolve(sym) else {
                        continue;
                    };
                    if lbl != label {
                        continue;
                    }
                    if let Some(value) = build_props_view(&self.props, &self.base)
                        .get(id, field)
                        .map(|vr| vr.into_value())
                    {
                        self.fulltext.add_tokens(id, field, &value);
                    }
                }
            }
            WalRecord::DisableFulltext { label, field } => {
                // Replay-over-snapshot idempotency: already disabled → skip.
                if !self.fulltext.is_enabled(label, field) {
                    return Ok(());
                }
                // If another label still indexes this field, the postings column
                // is kept — but it must not contain node_ids from the now-disabled
                // label.  Remove them before calling disable() so the field_indexed
                // guard inside disable() sees the correct post-removal state.
                if self.fulltext.field_indexed_by_other(label, field) {
                    if let Some(label_sym) = self.syms.get(label) {
                        for (node_id, &lsym) in self.labels.iter().enumerate() {
                            if lsym == label_sym {
                                self.fulltext.remove_node_field(node_id as u32, field);
                            }
                        }
                    }
                }
                self.fulltext.disable(label, field);
            }
        }
        Ok(())
    }

    /// Intern `s` in `syms` and emit a WAL `Intern` record so `*Id` records
    /// replay on WAL-only `open_at` (no snapshot intern table). Apply is
    /// idempotent when the string is already bound. Always emit: after
    /// `snapshot()` the WAL is truncated and live intern is not on disk.
    fn intern_wal(&mut self, s: &str) -> (u32, WalRecord) {
        let id = if let Some(id) = self.syms.get(s) {
            id
        } else {
            self.syms.intern(s)
        };
        (
            id,
            WalRecord::Intern {
                id,
                text: s.to_string(),
            },
        )
    }

    /// Rewrite user-facing records into dense-id records. On `Err`, no live
    /// state is left mutated: speculative interns made while building the
    /// output are rolled back, so a later successful mutation cannot log an
    /// `Intern` record whose id replay would never reproduce.
    fn rewrite_wal_dense(&mut self, recs: Vec<WalRecord>) -> Result<Vec<WalRecord>> {
        let syms_checkpoint = self.syms.len();
        let result = self.rewrite_wal_dense_inner(recs);
        if result.is_err() {
            self.syms.truncate(syms_checkpoint);
        }
        result
    }

    fn rewrite_wal_dense_inner(&mut self, recs: Vec<WalRecord>) -> Result<Vec<WalRecord>> {
        let mut out = Vec::with_capacity(recs.len());
        // Node ids allocated by later apply(InsertNodeId) in this same batch.
        let mut pending: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
        let mut interned = std::collections::HashSet::<u32>::new();
        let mut next = u32::try_from(self.ids.len()).map_err(|_| GraphError::Corrupt {
            detail: "id space exhausted".into(),
        })?;
        let lookup = |ids: &IdMap,
                      pending: &std::collections::HashMap<String, u32>,
                      key: &str|
         -> Option<u32> { ids.get(key).or_else(|| pending.get(key).copied()) };
        for rec in recs {
            match rec {
                WalRecord::InsertNode { label, key, props } => {
                    let (label_id, intern) = self.intern_wal(&label);
                    if interned.insert(label_id) {
                        out.push(intern);
                    }
                    let mut props_id = Vec::with_capacity(props.len());
                    for (field, value) in props {
                        let (field_id, intern) = self.intern_wal(&field);
                        if interned.insert(field_id) {
                            out.push(intern);
                        }
                        props_id.push((field_id, value));
                    }
                    if lookup(&self.ids, &pending, &key).is_none() {
                        pending.insert(key.clone(), next);
                        next = next.checked_add(1).ok_or_else(|| GraphError::Corrupt {
                            detail: "id space exhausted".into(),
                        })?;
                    }
                    out.push(WalRecord::InsertNodeId {
                        label: label_id,
                        key,
                        props: props_id,
                    });
                }
                WalRecord::SetProp { key, field, value } => {
                    let id =
                        lookup(&self.ids, &pending, &key).ok_or_else(|| GraphError::Corrupt {
                            detail: format!("dense WAL rewrite missing key {key}"),
                        })?;
                    let (field_id, intern) = self.intern_wal(&field);
                    if interned.insert(field_id) {
                        out.push(intern);
                    }
                    out.push(WalRecord::SetPropId {
                        id,
                        field: field_id,
                        value,
                    });
                }
                WalRecord::InsertEdge {
                    edge_type,
                    src_key,
                    dst_key,
                } => {
                    let (etype, intern) = self.intern_wal(&edge_type);
                    if interned.insert(etype) {
                        out.push(intern);
                    }
                    let src = lookup(&self.ids, &pending, &src_key).ok_or_else(|| {
                        GraphError::Corrupt {
                            detail: format!("dense WAL rewrite missing src {src_key}"),
                        }
                    })?;
                    let dst = lookup(&self.ids, &pending, &dst_key).ok_or_else(|| {
                        GraphError::Corrupt {
                            detail: format!("dense WAL rewrite missing dst {dst_key}"),
                        }
                    })?;
                    out.push(WalRecord::InsertEdgeId { etype, src, dst });
                }
                other => out.push(other),
            }
        }
        Ok(out)
    }

    fn log_dense(&mut self, recs: Vec<WalRecord>) -> Result<()> {
        let recs = self.rewrite_wal_dense(recs)?;
        match recs.len() {
            0 => Ok(()),
            1 => self.log_then_apply(recs.into_iter().next().unwrap()),
            _ => self.log_then_apply(WalRecord::Batch(recs)),
        }
    }

    /// Durable write, then notify the event sink. Replay (`apply` during
    /// `open`) never enters this function, so it is the replay-silent seam.
    fn log_then_apply(&mut self, rec: WalRecord) -> Result<()> {
        self.log_then_apply_with(rec, None, self.fsync)
    }

    /// Whether this frame must fsync under `policy`.
    ///
    /// Batched contract: user-visible batches (>1 mutation) fsync; single
    /// mutations do not. The dense rewrite wraps a single mutation in a
    /// `Batch([Intern.., <one *Id record>])`, so `Intern` records are excluded
    /// from the count — removing that filter would make every single-op write
    /// fsync under Batched (or, if the threshold were raised instead, skip a
    /// needed fsync for real two-op batches).
    fn wal_needs_sync(policy: FsyncPolicy, rec: &WalRecord) -> bool {
        match policy {
            FsyncPolicy::Relaxed => false,
            FsyncPolicy::Strict => true,
            FsyncPolicy::Batched => match rec {
                // Intern + one mutation is the single-op rewrite, not a user batch.
                WalRecord::Batch(inner) => {
                    inner
                        .iter()
                        .filter(|r| !matches!(r, WalRecord::Intern { .. }))
                        .count()
                        > 1
                }
                _ => false,
            },
        }
    }

    /// # Apply-infallibility invariant (load-bearing)
    ///
    /// The ordering is: WAL append → fsync → apply. If `apply` returned `Err`
    /// for a `Batch` frame after a successful WAL write, the WAL would contain
    /// the full frame while in-memory state would reflect only the ops before
    /// the failure. On reopen, WAL replay would then apply the entire batch —
    /// diverging permanently from what the pre-crash process had in memory.
    ///
    /// For `Batch` frames this situation cannot arise because:
    /// - All validation runs via `commit_logged_batch`/`MutPreview` **before**
    ///   the WAL write. `MutPreview` uses the same `&mut self` that apply will
    ///   use, with no concurrent mutation between validation exit and apply entry.
    /// - Every `apply` arm for a validated op is either infallible by construction
    ///   (`InsertNode`, `RemoveProp`, `DeleteEdge`, `DeleteNode`), has idempotency
    ///   guards that return `Ok(())` (`CreateRule`, `DeleteRule`), or is
    ///   guaranteed-present by validation (`InsertEdge`/`SetProp` key lookups).
    /// - `on_node_changed` and `on_node_removed` return `()` — never `Err`.
    ///
    /// A `debug_assert!` below fires in debug builds if `apply` ever returns
    /// `Err` for a `Batch` frame, making any future regression immediately visible
    /// in tests rather than silently diverging crash-recovery behaviour.
    fn log_then_apply_with(
        &mut self,
        rec: WalRecord,
        ingest: Option<(String, usize)>,
        policy: FsyncPolicy,
    ) -> Result<()> {
        // Read-only guard: as-of instances must never write the WAL.
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        // Invariant (I-1): no stale deltas may enter from a previous apply.
        // If any engine method ever accumulates deltas before erroring, they would
        // contaminate the *next* commit's event stream. This assert fires in debug
        // builds, making any future regression visible at the earliest point.
        debug_assert_eq!(
            self.engine.pending_delta_count(),
            0,
            "stale engine deltas at log_then_apply_with entry — \
             a previous apply arm may have accumulated deltas before erroring; \
             the caller must drain_deltas() on any error path before returning"
        );
        self.fs.append(FileId::Wal, &encode_record(&rec))?;
        if Self::wal_needs_sync(policy, &rec) {
            self.fs.sync(FileId::Wal)?;
        }
        let apply_result = self.apply(&rec);
        // For Batch frames, post-validation apply must be infallible (see above).
        // A debug_assert here catches any future change that makes apply fallible
        // before the caller notices via silent WAL/memory divergence.
        if matches!(&rec, WalRecord::Batch(_)) {
            debug_assert!(
                apply_result.is_ok(),
                "Batch apply returned Err after successful WAL write — \
                 the validate-then-apply invariant has been violated; \
                 see log_then_apply_with invariant doc"
            );
        }
        if apply_result.is_err() {
            // Discard any partial deltas accumulated by the failed apply.
            // They must not ride the next commit's event stream (I-1).
            let _ = self.engine.drain_deltas();
            let _ = self.engine.take_rebuild_needed();
            apply_result?;
        }
        self.commit_seq += 1;
        let seq = self.commit_seq;
        // Drain engine deltas and distribute to subscribers before the existing
        // MutationEvent sink fires — both happen post-fsync, post-apply.
        let engine_deltas = self.engine.drain_deltas();
        self.distribute_events(&rec, &engine_deltas, seq);
        self.emit_committed(&rec, ingest);
        // Drift is only known after apply, so auto-rebuild cannot join the
        // triggering op's WAL frame. Issue RebuildRule as a second commit.
        // Skip when `rec` is itself RebuildRule: rebuild resets drift, so a
        // retrigger loop is impossible if the fit succeeded, but we still
        // drain the flag so a leftover cannot re-enter.
        let rebuilds = self.engine.take_rebuild_needed();
        if !matches!(&rec, WalRecord::RebuildRule { .. }) {
            let mut failed = Vec::new();
            for name in rebuilds {
                if self.engine.rules().any(|r| r.name == name) {
                    // User op is already durable. A failed second commit must
                    // not surface as the caller's error.
                    if let Err(e) =
                        self.log_then_apply(WalRecord::RebuildRule { name: name.clone() })
                    {
                        eprintln!(
                            "auto-rebuild of rule {name:?} failed after durable user commit: {e}"
                        );
                        failed.push(name);
                    }
                }
            }
            for name in failed {
                self.engine.queue_rebuild_needed(name);
            }
        }
        Ok(())
    }

    /// Install a post-commit hook. Replaces any previous sink.
    ///
    /// The sink runs inside `log_then_apply` after a successful
    /// durable commit, while the caller still holds `&mut self`. When this
    /// database is behind a [`crate::SharedDb`], that means the **write
    /// guard is held**. The sink must never call `read` / `write` (or any
    /// other method) on the same `SharedDb` — the `RwLock` is not
    /// re-entrant and doing so deadlocks. The sink is `Send + Sync`;
    /// `std::sync::mpsc::Sender` is not `Sync` and will not type-check.
    /// Intended examples: `std::sync::mpsc::SyncSender`,
    /// `tokio::sync::mpsc::Sender`, `tokio::sync::broadcast::Sender`
    /// (non-blocking `send`), or `Arc<Mutex<Vec<MutationEvent>>>`.
    pub fn set_event_sink(&mut self, sink: Box<dyn Fn(MutationEvent) + Send + Sync>) {
        self.event_sink = Some(sink);
    }

    /// Whether a post-commit event sink is currently installed.
    pub fn has_event_sink(&self) -> bool {
        self.event_sink.is_some()
    }

    /// Set WAL fsync cadence. Default [`FsyncPolicy::Strict`].
    pub fn set_fsync_policy(&mut self, p: FsyncPolicy) {
        self.fsync = p;
    }

    fn emit(&self, ev: MutationEvent) {
        if let Some(sink) = &self.event_sink {
            sink(ev);
        }
    }

    fn emit_committed(&self, rec: &WalRecord, ingest: Option<(String, usize)>) {
        match rec {
            WalRecord::Batch(inner) => {
                for r in inner {
                    if let Some(ev) = event_from_record(r, &self.syms, &self.ids) {
                        self.emit(ev);
                    }
                }
                match ingest {
                    Some((label, inserted)) => {
                        self.emit(MutationEvent::Ingested { label, inserted })
                    }
                    None => {
                        let ops = inner
                            .iter()
                            .filter(|r| !matches!(r, WalRecord::Intern { .. }))
                            .count();
                        if ops > 1 {
                            self.emit(MutationEvent::BatchApplied { ops });
                        }
                    }
                }
            }
            other => {
                if let Some(ev) = event_from_record(other, &self.syms, &self.ids) {
                    self.emit(ev);
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Subscription API
    // -----------------------------------------------------------------------

    /// Distribute post-commit events to all live subscribers.
    ///
    /// Called from `log_then_apply_with` after apply + fsync, before the
    /// legacy MutationEvent sink. Prunes dead `Weak` entries in-place.
    ///
    /// Query subscriptions (subscribe_query) re-execute their plan on every
    /// call and diff the result against the previous run. Zero overhead when
    /// no query subscriptions are active.
    fn distribute_events(&mut self, rec: &WalRecord, engine_deltas: &[EngineEdgeDelta], seq: u64) {
        if self.subscriptions.is_empty() && self.query_subscriptions.is_empty() {
            return;
        }

        if !self.subscriptions.is_empty() {
            // Build write events from the WAL record.
            let write_events: Vec<DbEvent> =
                Self::write_events_from_record(rec, seq, &self.syms, &self.ids);

            // Build edge events from engine deltas.  Weight is looked up from
            // edge_props at distribution time (after apply), so it's always fresh.
            let edge_events: Vec<DbEvent> = engine_deltas
                .iter()
                .map(|d| {
                    if d.fired {
                        let weight = self
                            .edge_props
                            .get(d.etype_sym, d.src_id, d.dst_id, "weight")
                            .and_then(|v| {
                                if let core_storage::Value::Float(f) = v {
                                    Some(*f)
                                } else {
                                    None
                                }
                            });
                        DbEvent::EdgeFired {
                            rule: d.rule.clone(),
                            src_key: d.src_key.clone(),
                            dst_key: d.dst_key.clone(),
                            edge_type: d.edge_type.clone(),
                            weight,
                            commit_seq: seq,
                        }
                    } else {
                        DbEvent::EdgeRetracted {
                            rule: d.rule.clone(),
                            src_key: d.src_key.clone(),
                            dst_key: d.dst_key.clone(),
                            edge_type: d.edge_type.clone(),
                            commit_seq: seq,
                        }
                    }
                })
                .collect();

            // Prune dead entries; push matching events to live ones.
            self.subscriptions.retain(|entry| {
                let Some(inner) = entry.inner.upgrade() else {
                    return false;
                };
                for ev in &write_events {
                    if event_matches(ev, &entry.filter) {
                        inner.push(ev.clone());
                    }
                }
                for ev in &edge_events {
                    if event_matches(ev, &entry.filter) {
                        inner.push(ev.clone());
                    }
                }
                true
            });

            // Turn off delta accumulation if all subscribers dropped and no views remain.
            if self.subscriptions.is_empty() && self.view_store.is_empty() {
                self.engine.set_emit_deltas(false);
            }
        }

        // Query subscriptions: full re-run per commit, then diff rows.
        // IMPORTANT: full re-execution on every commit — use LIMIT to bound cost.
        // Differential evaluation is roadmap / Phase 5.
        if !self.query_subscriptions.is_empty() {
            // Take the list out so we can call self.view() without borrow conflict.
            let mut query_subs = std::mem::take(&mut self.query_subscriptions);
            let empty_params = BTreeMap::new();
            query_subs.retain_mut(|entry| {
                let Some(inner) = entry.inner.upgrade() else {
                    return false; // subscriber dropped — prune
                };
                let result = match execute(&self.view(), &entry.ops, &Params(&empty_params)) {
                    Ok(r) => r,
                    Err(_) => return true, // keep entry; skip diff on transient error
                };
                // Build new row map: serialized-key → row data.
                let new_row_map: std::collections::HashMap<String, Vec<Option<Value>>> = (0
                    ..result.len())
                    .map(|i| {
                        let row = result.row(i).to_vec();
                        let key =
                            serde_json::to_string(&row).unwrap_or_else(|_| format!("{row:?}"));
                        (key, row)
                    })
                    .collect();
                // Removed rows: in prev but not in new.
                for (key, row) in &entry.prev_row_map {
                    if !new_row_map.contains_key(key) {
                        inner.push(DbEvent::QueryRowRemoved {
                            columns: entry.columns.clone(),
                            row: row.clone(),
                        });
                    }
                }
                // Added rows: in new but not in prev.
                for (key, row) in &new_row_map {
                    if !entry.prev_row_map.contains_key(key) {
                        inner.push(DbEvent::QueryRowAdded {
                            columns: entry.columns.clone(),
                            row: row.clone(),
                        });
                    }
                }
                entry.prev_row_map = new_row_map;
                true
            });
            self.query_subscriptions = query_subs;
        }
    }

    /// Returns `true` if any live subscriber or view definition requires delta
    /// accumulation. Used to set `engine.emit_deltas` on subscribe/view DDL.
    fn needs_emit_deltas(&self) -> bool {
        !self.view_store.is_empty()
            || self
                .subscriptions
                .iter()
                .any(|e| e.inner.upgrade().is_some())
    }

    /// Convert a WAL record into `DbEvent` write events with the given seq.
    fn write_events_from_record(
        rec: &WalRecord,
        seq: u64,
        intern: &Interner,
        ids: &IdMap,
    ) -> Vec<DbEvent> {
        match rec {
            WalRecord::InsertNode { label, key, .. } => vec![DbEvent::NodeInserted {
                label: label.clone(),
                key: key.clone(),
                commit_seq: seq,
            }],
            // *Id arms run after a successful apply, so resolution can only
            // fail on a programming error. Skip the event rather than emit a
            // fabricated "" that clients can't tell from a real empty value
            // (mirrors event_from_record returning None).
            WalRecord::InsertNodeId { label, key, .. } => intern
                .resolve(*label)
                .map(|label| DbEvent::NodeInserted {
                    label: label.to_string(),
                    key: key.clone(),
                    commit_seq: seq,
                })
                .into_iter()
                .collect(),
            WalRecord::SetProp { key, field, .. } => vec![DbEvent::PropSet {
                key: key.clone(),
                field: field.clone(),
                commit_seq: seq,
            }],
            WalRecord::SetPropId { id, field, .. } => ids
                .key_of(*id)
                .zip(intern.resolve(*field))
                .map(|(key, field)| DbEvent::PropSet {
                    key: key.to_string(),
                    field: field.to_string(),
                    commit_seq: seq,
                })
                .into_iter()
                .collect(),
            WalRecord::RemoveProp { key, field } => vec![DbEvent::PropRemoved {
                key: key.clone(),
                field: field.clone(),
                commit_seq: seq,
            }],
            WalRecord::InsertEdge {
                edge_type,
                src_key,
                dst_key,
            } => vec![DbEvent::EdgeInserted {
                edge_type: edge_type.clone(),
                src: src_key.clone(),
                dst: dst_key.clone(),
                commit_seq: seq,
            }],
            WalRecord::InsertEdgeId { etype, src, dst } => (|| {
                Some(DbEvent::EdgeInserted {
                    edge_type: intern.resolve(*etype)?.to_string(),
                    src: ids.key_of(*src)?.to_string(),
                    dst: ids.key_of(*dst)?.to_string(),
                    commit_seq: seq,
                })
            })()
            .into_iter()
            .collect(),
            WalRecord::DeleteEdge {
                edge_type,
                src_key,
                dst_key,
            } => vec![DbEvent::EdgeDeleted {
                edge_type: edge_type.clone(),
                src: src_key.clone(),
                dst: dst_key.clone(),
                commit_seq: seq,
            }],
            WalRecord::DeleteNode { key } => vec![DbEvent::NodeDeleted {
                key: key.clone(),
                commit_seq: seq,
            }],
            WalRecord::Batch(inner) => inner
                .iter()
                .flat_map(|r| Self::write_events_from_record(r, seq, intern, ids))
                .collect(),
            WalRecord::CreateRule { .. }
            | WalRecord::DeleteRule { .. }
            | WalRecord::RebuildRule { .. }
            | WalRecord::CreateView { .. }
            | WalRecord::DeleteView { .. }
            | WalRecord::EnableFulltext { .. }
            | WalRecord::DisableFulltext { .. }
            | WalRecord::Intern { .. } => vec![],
        }
    }

    /// Subscribe to edge-fire and edge-retract events for one named rule.
    ///
    /// Returns `Err(GraphError::RuleNotFound)` if `rule_name` is not
    /// currently registered. Dropping the returned [`Subscription`] handle
    /// unregisters the subscriber — no further events are queued, no
    /// resources leak.
    pub fn subscribe_rule(&mut self, rule_name: &str) -> core_storage::Result<Subscription> {
        if self.read_only {
            return Err(core_storage::GraphError::ReadOnly);
        }
        if !self.engine.rules().any(|r| r.name == rule_name) {
            return Err(core_storage::GraphError::RuleNotFound {
                name: rule_name.to_string(),
            });
        }
        let inner = SubInner::new(self.sub_capacity());
        self.subscriptions.push(SubEntry {
            filter: SubFilter::Rule(rule_name.to_string()),
            inner: std::sync::Arc::downgrade(&inner),
        });
        self.engine.set_emit_deltas(true);
        Ok(Subscription(inner))
    }

    /// Subscribe to edge-fire and edge-retract events for **all** rules.
    ///
    /// Returns `Err(GraphError::ReadOnly)` if called on an as-of instance —
    /// as-of instances never commit, so `distribute_events` never runs and the
    /// subscription would never deliver events.
    pub fn subscribe_all_rules(&mut self) -> core_storage::Result<Subscription> {
        if self.read_only {
            return Err(core_storage::GraphError::ReadOnly);
        }
        let inner = SubInner::new(self.sub_capacity());
        self.subscriptions.push(SubEntry {
            filter: SubFilter::AllRules,
            inner: std::sync::Arc::downgrade(&inner),
        });
        self.engine.set_emit_deltas(true);
        Ok(Subscription(inner))
    }

    /// Subscribe to write events: node insert/delete, prop set/remove.
    ///
    /// Does not include edge-fire / edge-retract (rule-derived edge events).
    ///
    /// Returns `Err(GraphError::ReadOnly)` if called on an as-of instance —
    /// as-of instances never commit, so `distribute_events` never runs and the
    /// subscription would never deliver events.
    pub fn subscribe_writes(&mut self) -> core_storage::Result<Subscription> {
        if self.read_only {
            return Err(core_storage::GraphError::ReadOnly);
        }
        let inner = SubInner::new(self.sub_capacity());
        self.subscriptions.push(SubEntry {
            filter: SubFilter::Writes,
            inner: std::sync::Arc::downgrade(&inner),
        });
        self.engine.set_emit_deltas(true);
        Ok(Subscription(inner))
    }

    /// Subscribe to incremental Cypher query results.
    ///
    /// Parses and plans `cypher`; rejects the query if the plan is not in the
    /// allowlisted subset (see [`core_query::cypher::is_subscribable`]):
    ///   - `MATCH (n:Label) WHERE … RETURN … [LIMIT n]`
    ///   - `MATCH (a)-[r:TYPE]->(b) RETURN … [LIMIT n]`  (exactly one hop)
    ///
    /// SKIP is not supported — it shifts the result window on every commit,
    /// causing spurious Added/Removed churn for rows whose data never changed.
    /// Multi-hop Expand chains are not supported; each additional MATCH clause
    /// widens scope beyond the documented single-scan / single-hop subset.
    ///
    /// After each successful commit, the plan is **fully re-executed** and the
    /// result is diffed against the previous run. Added rows produce
    /// [`DbEvent::QueryRowAdded`]; removed rows produce
    /// [`DbEvent::QueryRowRemoved`].
    ///
    /// **Full re-run per commit; use LIMIT to bound execution cost.**
    /// The existing 1 M intermediate-row cap applies. Differential evaluation
    /// is roadmap / Phase 5.
    ///
    /// Returns `Err(GraphError::ReadOnly)` if called on an as-of instance —
    /// as-of instances never commit, so `distribute_events` never runs and the
    /// subscription would never deliver events.
    ///
    /// Returns `Err(GraphError::QueryError)` if the query fails to parse, plan,
    /// or if the plan shape is not in the allowlist.
    pub fn subscribe_query(&mut self, cypher: &str) -> Result<Subscription> {
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        let tokens = lex(cypher).map_err(|e| GraphError::QueryError {
            detail: format!("lex: {e}"),
        })?;
        let ast = parse(&tokens).map_err(|e| GraphError::QueryError {
            detail: format!("parse: {e}"),
        })?;
        let ops = plan(&ast).map_err(|e| GraphError::QueryError {
            detail: format!("plan: {e}"),
        })?;
        if !is_subscribable(&ops) {
            return Err(GraphError::QueryError {
                detail: "subscribe_query only supports allowlisted plan shapes: \
                         MATCH (n:Label) WHERE … RETURN … [LIMIT n] or \
                         MATCH (a)-[r:TYPE]->(b) RETURN … [LIMIT n] (exactly one hop). \
                         Not supported: multi-hop Expand chains, SKIP (creates \
                         unstable offset windows), ORDER BY, DISTINCT, aggregates, \
                         variable-length paths, OPTIONAL MATCH, WITH, UNWIND. \
                         Use LIMIT to bound re-execution cost."
                    .to_string(),
            });
        }
        // Execute once to capture initial state (initial rows are not emitted as
        // events — the subscriber learns the baseline via the first query call).
        let empty_params = BTreeMap::new();
        let initial = execute(&self.view(), &ops, &Params(&empty_params)).map_err(|e| {
            GraphError::QueryError {
                detail: format!("execute: {e}"),
            }
        })?;
        let columns = initial.columns().to_vec();
        let prev_row_map: std::collections::HashMap<String, Vec<Option<Value>>> = (0..initial
            .len())
            .map(|i| {
                let row = initial.row(i).to_vec();
                let key = serde_json::to_string(&row).unwrap_or_else(|_| format!("{row:?}"));
                (key, row)
            })
            .collect();
        let inner = SubInner::new(self.sub_capacity());
        self.query_subscriptions.push(QuerySubEntry {
            ops,
            columns,
            prev_row_map,
            inner: std::sync::Arc::downgrade(&inner),
        });
        Ok(Subscription(inner))
    }

    /// Queue capacity used for new subscriptions.
    fn sub_capacity(&self) -> usize {
        self.sub_capacity
    }

    /// Override per-subscriber queue capacity for subsequently created
    /// subscriptions on this db instance.
    ///
    /// Default is [`DEFAULT_SUB_CAPACITY`] (65,536 events). Use a smaller
    /// value in tests to exercise the [`DbEvent::Lagged`] path without
    /// generating tens of thousands of events.
    ///
    /// This is a test-support escape hatch. Calling it in production reduces
    /// subscriber reliability (more Lagged events). It is hidden from rustdoc
    /// to discourage accidental production use.
    #[doc(hidden)]
    pub fn set_sub_capacity(&mut self, capacity: usize) {
        self.sub_capacity = capacity;
    }

    // -----------------------------------------------------------------------

    /// Start an atomic batch.
    ///
    /// The returned [`BatchBuilder`] borrows `self` mutably until
    /// [`BatchBuilder::commit`]. Builder methods queue ops only — no
    /// validation, no WAL I/O. `commit` validates every queued op against
    /// live state plus preceding ops in this batch (duplicate key inside
    /// the batch is `Err`; an edge between two nodes created earlier in
    /// the batch is valid; `delete_node` then insert of the same key is a
    /// fresh identity). Validation never mutates the database. Any failure
    /// leaves WAL bytes and in-memory state identical to before `commit`.
    /// On success, one `WalRecord::Batch` frame is appended (one fsync)
    /// and each inner record is applied in order so rules fire per record.
    /// An empty batch, or a batch of only no-ops, writes zero WAL bytes.
    ///
    /// **Rule-window limitation:** batch validation cannot see edges that a
    /// rule created earlier in the *same* batch will derive at apply time, so
    /// a `delete_edge` / `insert_edge` in that window is silently no-oped
    /// where sequential calls would return `Err(RuleOwned)`. State integrity
    /// is unaffected (idempotent apply, provenance intact). Create rules in
    /// their own batch, or sequentially, when later ops may touch derived
    /// edges.
    pub fn batch(&mut self) -> BatchBuilder<'_, F> {
        BatchBuilder {
            db: self,
            ops: Vec::new(),
        }
    }

    /// Closure-style atomic write batch.
    ///
    /// Equivalent to calling [`GraphDb::batch`], invoking `build` to queue ops,
    /// then committing. All ops queued inside `build` are validated in order and
    /// committed as a single `WalRecord::Batch` frame (one fsync). Rules fire
    /// once per inner record, in order, after commit — semantically identical to
    /// sequential single-op writes.
    ///
    /// **Error semantics — validate-then-apply.** `build` queues ops without
    /// touching the database. [`BatchBuilder::commit`] validates every op against
    /// live state plus earlier ops in this batch before writing anything. If op N
    /// fails validation (duplicate key, unknown key, rule-owned edge, …) the
    /// entire batch is rejected: no WAL bytes are written and no in-memory state
    /// changes. The database is identical to its state before `write_batch` was
    /// called.
    ///
    /// **Atomicity is crash-level, NOT isolation-level.** On replay after a crash,
    /// a partial (torn) `Batch` frame applies NONE of its ops — the frame is
    /// either fully applied or not at all. However, while applying a committed
    /// batch, concurrent readers may observe intermediate states as ops are applied
    /// sequentially in memory. There is no interactive transaction isolation in v1.
    /// This is documented as "crash-atomic write batches; no interactive
    /// transactions or read isolation."
    ///
    /// **Returns** `(nodes_inserted, edges_inserted)`. An empty or all-noop batch
    /// writes zero WAL bytes and returns `(0, 0)`.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let (nodes, edges) = db.write_batch(|b| {
    ///     b.insert_node("Person", "alice", vec![("age".into(), Value::Int(30))]);
    ///     b.insert_node("Person", "bob", vec![]);
    ///     b.insert_edge("KNOWS", "alice", "bob");
    ///     b.set_prop("alice", "role", Value::Str("admin".into()));
    ///     b.delete_node("old_key");
    /// })?;
    /// // One fsync; on crash replay: all five ops land or none do.
    /// ```
    pub fn write_batch<C>(&mut self, build: C) -> Result<(usize, usize)>
    where
        C: FnOnce(&mut BatchBuilder<'_, F>),
    {
        let mut b = self.batch();
        build(&mut b);
        b.commit()
    }

    /// Insert `rows` as nodes of `label`. One call is one atomic batch:
    /// auto-declared KeyMatch rules (if any) first, then the accepted node
    /// inserts, so incremental fire sees the new rules. Per-row key problems
    /// are collected in [`IngestReport::row_errors`] and skipped; a commit
    /// `Err` means nothing was applied.
    ///
    /// Auto-FK rule names are `auto_fk_<src_label_lowercase>_<field>` so
    /// distinct source labels sharing an FK field each get their own rule.
    pub fn ingest(
        &mut self,
        label: &str,
        rows: Vec<BTreeMap<String, Value>>,
        opts: &IngestOptions,
    ) -> Result<IngestReport> {
        self.ingest_with_edges(label, rows, opts, &[])
    }

    /// [`ingest`] plus user edges in the **same** previewed WAL batch.
    /// A failing edge rejects the whole request; nothing is applied.
    pub fn ingest_with_edges(
        &mut self,
        label: &str,
        rows: Vec<BTreeMap<String, Value>>,
        opts: &IngestOptions,
        edges: &[(String, String, String)],
    ) -> Result<IngestReport> {
        crate::ingest::run(self, label, rows, opts, edges)
    }

    /// Parse `json` as an array of objects and ingest via [`GraphDb::ingest`].
    ///
    /// JSON `null` fields are silently omitted (not stored, not a row error).
    /// Nested objects and arrays-of-objects are a per-row error (row skipped).
    /// Parse failures and a top-level value that is not an array of objects
    /// return [`GraphError::IngestError`].
    pub fn ingest_json(
        &mut self,
        label: &str,
        json: &str,
        opts: &IngestOptions,
    ) -> Result<IngestReport> {
        crate::ingest::run_json(self, label, json, opts)
    }

    fn commit_logged_batch(
        &mut self,
        ops: Vec<BatchOp>,
        ingest: Option<(String, usize)>,
    ) -> Result<(usize, usize)> {
        // Read-only guard: catches empty-batch calls before the early-return
        // that skips log_then_apply_with, ensuring all mutation entry points fail.
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        let recs = {
            let mut preview = MutPreview::new(self);
            let mut recs = Vec::with_capacity(ops.len());
            for op in ops {
                match op {
                    BatchOp::InsertNode { label, key, props } => {
                        preview.check_insert_node(&key)?;
                        preview.note_insert_node(&key, &props);
                        recs.push(WalRecord::InsertNode { label, key, props });
                    }
                    BatchOp::InsertEdge {
                        edge_type,
                        src_key,
                        dst_key,
                    } => {
                        if preview.prepare_insert_edge(&edge_type, &src_key, &dst_key)? {
                            preview.note_insert_edge(&edge_type, &src_key, &dst_key);
                            recs.push(WalRecord::InsertEdge {
                                edge_type,
                                src_key,
                                dst_key,
                            });
                        }
                    }
                    BatchOp::SetProp { key, field, value } => {
                        preview.check_live_key(&key)?;
                        preview.note_set_prop(&key, &field, &value);
                        recs.push(WalRecord::SetProp { key, field, value });
                    }
                    BatchOp::RemoveProp { key, field } => {
                        if preview.prepare_remove_prop(&key, &field)? {
                            preview.note_remove_prop(&key, &field);
                            recs.push(WalRecord::RemoveProp { key, field });
                        }
                    }
                    BatchOp::DeleteEdge {
                        edge_type,
                        src_key,
                        dst_key,
                    } => {
                        if preview.prepare_delete_edge(&edge_type, &src_key, &dst_key)? {
                            preview.note_delete_edge(&edge_type, &src_key, &dst_key);
                            recs.push(WalRecord::DeleteEdge {
                                edge_type,
                                src_key,
                                dst_key,
                            });
                        }
                    }
                    BatchOp::DeleteNode { key } => {
                        preview.check_live_key(&key)?;
                        preview.note_delete_node(&key);
                        recs.push(WalRecord::DeleteNode { key });
                    }
                    BatchOp::CreateRule(def) => {
                        preview.check_create_rule(&def)?;
                        let def_bytes =
                            bincode::serialize(&def).map_err(|e| GraphError::Corrupt {
                                detail: format!("serialize rule: {e}"),
                            })?;
                        preview.note_create_rule(&def.name);
                        recs.push(WalRecord::CreateRule { def_bytes });
                    }
                    BatchOp::DeleteRule { name } => {
                        preview.check_delete_rule(&name)?;
                        preview.note_delete_rule(&name);
                        recs.push(WalRecord::DeleteRule { name });
                    }
                }
            }
            recs
        };
        if recs.is_empty() {
            return Ok((0, 0));
        }
        // rewrite_wal_dense converts every InsertNode/InsertEdge into its
        // *Id form, so only the dense variants can appear in `recs` here.
        let recs = self.rewrite_wal_dense(recs)?;
        let nodes_inserted = recs
            .iter()
            .filter(|r| matches!(r, WalRecord::InsertNodeId { .. }))
            .count();
        let edges_inserted = recs
            .iter()
            .filter(|r| matches!(r, WalRecord::InsertEdgeId { .. }))
            .count();
        // Ingest / write_batch / query_write: one Batch frame. Strict and
        // Batched both fsync once at frame end; Relaxed still skips.
        let policy = match self.fsync {
            FsyncPolicy::Relaxed => FsyncPolicy::Relaxed,
            FsyncPolicy::Strict | FsyncPolicy::Batched => FsyncPolicy::Batched,
        };
        self.log_then_apply_with(WalRecord::Batch(recs), ingest, policy)?;
        Ok((nodes_inserted, edges_inserted))
    }

    fn commit_batch(&mut self, ops: Vec<BatchOp>) -> Result<(usize, usize)> {
        self.commit_logged_batch(ops, None)
    }

    pub fn insert_node(
        &mut self,
        label: &str,
        key: &str,
        props: Vec<(String, Value)>,
    ) -> Result<()> {
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        MutPreview::new(self).check_insert_node(key)?;
        self.log_dense(vec![WalRecord::InsertNode {
            label: label.into(),
            key: key.into(),
            props,
        }])
    }

    pub fn insert_edge(&mut self, edge_type: &str, src_key: &str, dst_key: &str) -> Result<bool> {
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        if !MutPreview::new(self).prepare_insert_edge(edge_type, src_key, dst_key)? {
            return Ok(false);
        }
        self.log_dense(vec![WalRecord::InsertEdge {
            edge_type: edge_type.into(),
            src_key: src_key.into(),
            dst_key: dst_key.into(),
        }])?;
        Ok(true)
    }

    pub fn set_prop(&mut self, key: &str, field: &str, value: Value) -> Result<()> {
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        if let Some(view_name) = self.view_store.view_for_prop(field) {
            return Err(GraphError::ViewPropReadOnly {
                view_name: view_name.to_string(),
            });
        }
        MutPreview::new(self).check_live_key(key)?;
        self.log_dense(vec![WalRecord::SetProp {
            key: key.into(),
            field: field.into(),
            value,
        }])
    }

    /// Remove a property. Returns `Ok(false)` (and does not log) if the field
    /// is already absent. Unknown or tombstoned keys are `Err(KeyNotFound)`.
    pub fn remove_prop(&mut self, key: &str, field: &str) -> Result<bool> {
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        if let Some(view_name) = self.view_store.view_for_prop(field) {
            return Err(GraphError::ViewPropReadOnly {
                view_name: view_name.to_string(),
            });
        }
        if !MutPreview::new(self).prepare_remove_prop(key, field)? {
            return Ok(false);
        }
        self.log_then_apply(WalRecord::RemoveProp {
            key: key.into(),
            field: field.into(),
        })?;
        Ok(true)
    }

    /// Delete a user edge. Returns `Ok(false)` (and does not log) if the edge
    /// is absent. Unknown keys are `Err(KeyNotFound)`. Rule-owned edges — in
    /// provenance, or a pair a live rule would derive — are `Err(RuleOwned)`
    /// (the rule would just put the edge back; delete or change the rule).
    pub fn delete_edge(&mut self, edge_type: &str, src_key: &str, dst_key: &str) -> Result<bool> {
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        if !MutPreview::new(self).prepare_delete_edge(edge_type, src_key, dst_key)? {
            return Ok(false);
        }
        self.log_then_apply(WalRecord::DeleteEdge {
            edge_type: edge_type.into(),
            src_key: src_key.into(),
            dst_key: dst_key.into(),
        })?;
        Ok(true)
    }

    /// Delete a live node. Unknown or already-tombstoned keys are
    /// `Err(KeyNotFound)` and are not logged. Validation runs before the WAL
    /// write; `apply` of a logged `DeleteNode` for an already-tombstoned key
    /// (crash window) is a clean no-op.
    ///
    /// Returns a [`DeleteReport`] with counts of manual and derived edges
    /// removed (computed from live state before the deletion is applied).
    pub fn delete_node(&mut self, key: &str) -> Result<DeleteReport> {
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        let id = self
            .ids
            .get(key)
            .ok_or_else(|| GraphError::KeyNotFound { key: key.into() })?;

        // Count edges before the delete is applied so we can report counts.
        let derived_set: BTreeSet<(u32, u32, u32)> = self
            .engine
            .provenance_touching(id)
            .map(|(_, etype, src, dst)| (etype, src, dst))
            .collect();
        let derived_edges = derived_set.len() as u64;

        let mut total_topo = 0u64;
        let tv = self.topo_view();
        for et in tv.etypes() {
            total_topo += tv.neighbors(et, Direction::Out, id).len() as u64
                + tv.neighbors(et, Direction::In, id).len() as u64;
        }
        // For symmetric rules (e.g. Overlap), a→b and b→a are two separate directed
        // triples in both the topo scan (Out and In from id) and in provenance_touching.
        // The subtraction remains correct because both counts include both directions.
        let manual_edges = total_topo.saturating_sub(derived_edges);

        self.log_then_apply(WalRecord::DeleteNode { key: key.into() })?;
        Ok(DeleteReport {
            manual_edges,
            derived_edges,
        })
    }

    /// Return the IVF drift counter for the dst-side candidate index of `rule`.
    /// `None` if the rule does not exist or is not approximate.
    ///
    /// The drift counter increments on IVF insert/remove after the last fit.
    /// When dst-side drift exceeds [`core_rules::IVF_DRIFT_REBUILD`], apply
    /// WAL-logs `RebuildRule` as a second commit (rebuild resets the counter).
    pub fn ivf_dst_drift(&self, rule: &str) -> Option<u64> {
        // SideIvfExport = (centroids, node→cluster, drift)
        self.engine
            .export_ivf_state()
            .remove(rule)
            .map(|(_src, dst)| dst.2)
    }

    /// Validate and WAL-log a new rule, then backfill derived edges inside apply.
    /// Validation and duplicate-name check run before logging so invalid rules
    /// never enter the WAL.
    pub fn create_rule(&mut self, def: RuleDef) -> Result<()> {
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        MutPreview::new(self).check_create_rule(&def)?;
        let def_bytes = bincode::serialize(&def).map_err(|e| GraphError::Corrupt {
            detail: format!("serialize rule: {e}"),
        })?;
        self.log_then_apply(WalRecord::CreateRule { def_bytes })
    }

    /// WAL-log rule deletion. Returns RuleNotFound if the rule does not exist.
    pub fn delete_rule(&mut self, name: &str) -> Result<()> {
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        MutPreview::new(self).check_delete_rule(name)?;
        self.log_then_apply(WalRecord::DeleteRule { name: name.into() })
    }

    /// Return a snapshot of all registered rules.
    pub fn rules(&self) -> Vec<RuleDef> {
        self.engine.rules().cloned().collect()
    }

    // -----------------------------------------------------------------------
    // Rule suggestion API
    // -----------------------------------------------------------------------

    /// Profile the database and suggest linking rules with previewed edge counts.
    ///
    /// Uses the default seed ([`core_rules::SUGGEST_DEFAULT_SEED`]) for deterministic
    /// sampling. Suggestions are sorted by estimated edge count (descending).
    /// **NO auto-accept** — call [`GraphDb::create_rule`] explicitly to apply.
    pub fn suggest_rules(&self) -> Vec<core_rules::RuleSuggestion> {
        self.suggest_rules_seeded(core_rules::SUGGEST_DEFAULT_SEED)
    }

    /// Like [`suggest_rules`] but with a caller-supplied RNG seed for
    /// reproducibility. Same seed + same data = identical output.
    pub fn suggest_rules_seeded(&self, seed: u64) -> Vec<core_rules::RuleSuggestion> {
        self.suggest_rules_with_config(&core_rules::suggest::SuggestConfig::default(), seed)
            .suggestions
    }

    /// [`suggest_rules_seeded`] with a fully custom [`SuggestConfig`].
    ///
    /// Returns a [`core_rules::SuggestReport`] that includes both the candidate list
    /// and a `truncated` flag indicating whether the global budget fired before all
    /// candidates were evaluated.
    pub fn suggest_rules_with_config(
        &self,
        config: &core_rules::suggest::SuggestConfig,
        seed: u64,
    ) -> core_rules::SuggestReport {
        use std::collections::BTreeMap;

        // Collect (node_id, key) pairs per label, skipping tombstoned nodes.
        let mut label_nodes: BTreeMap<String, Vec<(u32, String)>> = BTreeMap::new();
        for id in 0..self.ids.len() as u32 {
            let Some(key) = self.ids.key_of(id) else {
                continue;
            };
            let Some(&sym) = self.labels.get(id as usize) else {
                continue;
            };
            if sym == u32::MAX {
                continue; // tombstoned
            }
            let Some(label) = self.syms.resolve(sym) else {
                continue;
            };
            label_nodes
                .entry(label.to_string())
                .or_default()
                .push((id, key.to_string()));
        }

        let existing = self.rules();
        let pv = build_props_view(&self.props, &self.base);
        let all_fields: Vec<String> = pv.field_names();

        core_rules::suggest::suggest_rules(
            &label_nodes,
            &|id, field| pv.get(id, field).map(|vr| vr.into_value()),
            &all_fields,
            &existing,
            config,
            seed,
        )
    }

    /// Recompute a rule's derived edges from scratch. WAL-logged so un-trip
    /// plus later mutations replay identically (rebuild is a pure function
    /// of state).
    ///
    /// Only exit from the tripped latch: if the full desired set fits the
    /// budget, it is applied completely and `tripped` clears; if it still
    /// exceeds the budget, provenance is left untouched and `tripped` stays
    /// true. Counts as a fire evaluation (see [`RuleStats::fires`]).
    /// Unknown rule → `RuleNotFound`, nothing logged.
    pub fn rebuild_rule(&mut self, name: &str) -> Result<()> {
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        if !self.engine.rules().any(|r| r.name == name) {
            return Err(GraphError::RuleNotFound { name: name.into() });
        }
        self.log_then_apply(WalRecord::RebuildRule { name: name.into() })
    }

    // -----------------------------------------------------------------------
    // Materialized view API
    // -----------------------------------------------------------------------

    /// Register a new materialized property view, backfill its values for all
    /// existing nodes, and WAL-log the definition.
    ///
    /// # Errors
    /// - `ReadOnly`: called on an as-of instance.
    /// - `RuleInvalid`: name collision, view_prop collision, or invalid def.
    pub fn create_view(&mut self, def: ViewDef) -> Result<()> {
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        // Pre-validate before WAL write.
        def.validate()
            .map_err(|e| GraphError::RuleInvalid { detail: e })?;
        if self.view_store.has_view(&def.name) {
            return Err(GraphError::RuleInvalid {
                detail: format!("view {:?} already exists", def.name),
            });
        }
        if let Some(existing) = self.view_store.view_for_prop(&def.view_prop) {
            return Err(GraphError::RuleInvalid {
                detail: format!(
                    "view_prop {:?} is already used by view {:?}",
                    def.view_prop, existing
                ),
            });
        }
        let def_bytes = bincode::serialize(&def).map_err(|e| GraphError::Corrupt {
            detail: format!("serialize view: {e}"),
        })?;
        // Enable delta accumulation before the view is registered so subsequent
        // incremental edge events reach view maintenance from this point onward.
        // (The backfill inside create_view reads topo directly; it does not rely
        // on pending deltas.)
        self.engine.set_emit_deltas(true);
        self.log_then_apply(WalRecord::CreateView { def_bytes })
    }

    /// Remove a named view and delete its values from every node.
    ///
    /// # Errors
    /// - `ReadOnly`: called on an as-of instance.
    /// - `RuleNotFound`: view does not exist.
    pub fn delete_view(&mut self, name: &str) -> Result<()> {
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        if !self.view_store.has_view(name) {
            return Err(GraphError::RuleNotFound { name: name.into() });
        }
        let result = self.log_then_apply(WalRecord::DeleteView { name: name.into() });
        // After deletion, disable accumulation if no listeners remain.
        if !self.needs_emit_deltas() {
            self.engine.set_emit_deltas(false);
        }
        result
    }

    /// Snapshot of all registered view definitions.
    pub fn views(&self) -> Vec<ViewDef> {
        self.view_store.views().cloned().collect()
    }

    // -----------------------------------------------------------------------
    // Full-text-lite API
    // -----------------------------------------------------------------------

    /// Enable full-text indexing for all nodes of `label` on property `field`.
    ///
    /// After this call, every subsequent write to `(label, field)` is reflected
    /// in the index incrementally.  Existing nodes are backfilled immediately.
    /// The declaration is persisted as a WAL record; the index itself is rebuilt
    /// from scratch on re-open (no snapshot format changes).
    ///
    /// # Errors
    /// - [`GraphError::ReadOnly`]: called on an as-of instance.
    /// - [`GraphError::RuleInvalid`]: `(label, field)` is already indexed.
    pub fn enable_fulltext(&mut self, label: &str, field: &str) -> Result<()> {
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        if self.fulltext.is_enabled(label, field) {
            return Err(GraphError::RuleInvalid {
                detail: format!("full-text index for ({label:?}, {field:?}) already enabled"),
            });
        }
        self.log_then_apply(WalRecord::EnableFulltext {
            label: label.into(),
            field: field.into(),
        })
    }

    /// Disable full-text indexing for `(label, field)` and drop its postings.
    ///
    /// # Errors
    /// - [`GraphError::ReadOnly`]: called on an as-of instance.
    /// - [`GraphError::RuleNotFound`]: `(label, field)` is not currently indexed.
    pub fn disable_fulltext(&mut self, label: &str, field: &str) -> Result<()> {
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        if !self.fulltext.is_enabled(label, field) {
            return Err(GraphError::RuleNotFound {
                name: format!("fulltext({label},{field})"),
            });
        }
        self.log_then_apply(WalRecord::DisableFulltext {
            label: label.into(),
            field: field.into(),
        })
    }

    /// Whether `(label, field)` is currently indexed for full-text search.
    pub fn is_fulltext_enabled(&self, label: &str, field: &str) -> bool {
        self.fulltext.is_enabled(label, field)
    }

    /// Search a full-text-indexed field.
    ///
    /// Returns `(node_key, match_count)` pairs sorted by match_count descending,
    /// ties broken by key (lexicographic).  Tombstoned nodes are excluded.
    ///
    /// **Query syntax:**
    /// - Space-separated terms are AND'd: `"foo bar"` requires both.
    /// - `OR` between terms forms disjunction: `"foo OR bar"` matches either.
    /// - Trailing `*` on a term is a prefix match: `"rust*"` matches `rustlang`, `rusty`.
    /// - `AND` keyword is accepted explicitly and is the default.
    /// - Tokenization is unicode-alphanumeric (same as index time); case-insensitive.
    ///
    /// **Unindexed field:** returns `Ok(vec![])` if `field` is not indexed.
    /// Pin: this is the documented, tested, stable behavior for v1.
    ///
    /// **Memory / performance:** O(postings) lookup; no scan.  The index is
    /// in-memory and proportional to total indexed text across all enabled fields.
    pub fn search(&self, field: &str, query: &str) -> Vec<(String, usize)> {
        // Resolve node_ids to keys (excluding tombstones) then re-sort by
        // (match_count DESC, key ASC) to give a deterministic, key-lexicographic
        // tiebreak.  FulltextIndex::search sorts by (count DESC, node_id ASC)
        // which diverges from key order when nodes were not inserted in key-lex order.
        let mut results: Vec<(String, usize)> = self
            .fulltext
            .search(field, query)
            .into_iter()
            .filter_map(|(id, count)| self.ids.key_of(id).map(|key| (key.to_string(), count)))
            .collect();
        results.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        results
    }

    /// Hybrid search: Reciprocal Rank Fusion (RRF) over fulltext + vector results.
    ///
    /// Takes up to `4*k` fulltext hits for `(text_field, query_text)` and up to
    /// `4*k` vector hits for `(vector_field, query_vec, min=0.0)`, then fuses
    /// them with RRF using a fixed constant of 60.
    ///
    /// ```text
    /// score(d) = Σ  1 / (60 + rank_i(d))    (rank 1-based per list)
    /// ```
    ///
    /// Returns the top `k` nodes by fused score, ties broken by node key
    /// ascending (deterministic).
    ///
    /// # Vector leg fallback
    ///
    /// When `query_vec` is empty the vector leg is skipped entirely and
    /// results are ranked by the text list alone through the same RRF path
    /// (each text result scores `1/(60 + rank)` from that single list).
    ///
    /// When `label` is `None` and no HNSW rule covers `vector_field`, the
    /// brute-force scan cannot enumerate a node universe; `find_similar_vector`
    /// returns an empty result and the fused ranking is text-only.  Document
    /// this in your application layer if you rely on it.
    pub fn search_hybrid(
        &mut self,
        text_field: &str,
        query_text: &str,
        vector_field: &str,
        query_vec: &[f64],
        label: Option<&str>,
        k: usize,
    ) -> Vec<(String, f64)> {
        use std::collections::HashMap;

        const RRF_K: f64 = 60.0;
        let pool = 4 * k.max(1);

        // Accumulate per-node RRF scores.
        let mut scores: HashMap<String, f64> = HashMap::new();

        // Text leg.
        let text_hits = self.search(text_field, query_text);
        for (rank0, (key, _count)) in text_hits.into_iter().take(pool).enumerate() {
            let rank = (rank0 + 1) as f64;
            *scores.entry(key).or_insert(0.0) += 1.0 / (RRF_K + rank);
        }

        // Vector leg (skipped when query_vec is empty).
        if !query_vec.is_empty() {
            let lbl = label.unwrap_or("");
            let vec_hits = self.find_similar_vector(vector_field, lbl, query_vec, pool, 0.0);
            for (rank0, (key, _sim)) in vec_hits.into_iter().enumerate() {
                let rank = (rank0 + 1) as f64;
                *scores.entry(key).or_insert(0.0) += 1.0 / (RRF_K + rank);
            }
        }

        // Sort: score DESC, then key ASC for deterministic tie-breaking.
        let mut ranked: Vec<(String, f64)> = scores.into_iter().collect();
        ranked.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });
        ranked.truncate(k);
        ranked
    }

    /// For DST/testing: scratch full-text search over live nodes without using
    /// the index.  Walks every live node, tokenizes the field value, and returns
    /// nodes matching the query.  Results are sorted match_count desc, key asc.
    ///
    /// The oracle: `search(field, q)` must equal `scratch_search(field, q)`.
    #[doc(hidden)]
    pub fn scratch_search(&self, field: &str, query: &str) -> Vec<(String, usize)> {
        use core_storage::fulltext::{parse_query, tokenize};
        use std::collections::BTreeSet;
        let groups = parse_query(query);
        let mut results: Vec<(String, usize)> = Vec::new();
        for id in 0..self.ids.len() as u32 {
            let Some(key) = self.ids.key_of(id) else {
                continue;
            };
            let Some(&sym) = self.labels.get(id as usize) else {
                continue;
            };
            if sym == u32::MAX {
                continue;
            }
            // Only scan nodes whose label has this field indexed.
            let label = match self.syms.resolve(sym) {
                Some(l) => l,
                None => continue,
            };
            if !self.fulltext.is_enabled(label, field) {
                continue;
            }
            let Some(value) = self.props_view().get(id, field).map(|vr| vr.into_value()) else {
                continue;
            };
            let node_tokens: BTreeSet<String> = match &value {
                Value::Str(s) => tokenize(s).into_iter().collect(),
                Value::List(items) => items
                    .iter()
                    .flat_map(|v| {
                        if let Value::Str(s) = v {
                            tokenize(s)
                        } else {
                            vec![]
                        }
                    })
                    .collect(),
                _ => BTreeSet::new(),
            };
            // Count OR-group matches.
            let mut count = 0usize;
            for group in &groups {
                let mut group_match = true;
                for term in group {
                    let matched = if term.prefix {
                        node_tokens.iter().any(|t| t.starts_with(&term.token))
                    } else {
                        node_tokens.contains(&term.token)
                    };
                    if !matched {
                        group_match = false;
                        break;
                    }
                }
                if group_match {
                    count += 1;
                }
            }
            if count > 0 {
                results.push((key.to_string(), count));
            }
        }
        results.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        results
    }

    /// Return the current view-maintained value of `view_prop` for node `key`.
    /// Equivalent to `get_prop` but documents that it reads a view-managed column.
    pub fn get_view_prop(&self, key: &str, view_prop: &str) -> Option<Value> {
        let id = self.ids.get(key)?;
        self.props_view()
            .get(id, view_prop)
            .map(|vr| vr.into_value())
    }

    /// For testing / DST oracle: scratch recompute of a view value for one node.
    ///
    /// Returns `None` if the node does not exist, the view does not exist, or
    /// the view has no result for the node (e.g. Avg with no qualifying neighbors).
    #[doc(hidden)]
    pub fn scratch_view_value(&self, key: &str, view_name: &str) -> Option<Value> {
        let node = self.ids.get(key)?;
        let def = self.view_store.views().find(|v| v.name == view_name)?;
        // Use TopologyView so that NeighborAgg sees base + overlay edges
        // without materialising a temporary Topology (I1).
        let topo_view = self.topo_view();
        core_rules::views::compute_view_value(
            def,
            node,
            self.props_view(),
            &topo_view,
            &self.ids,
            &self.syms,
            &self.labels,
        )
    }

    // -----------------------------------------------------------------------
    // Graph algorithm API
    // -----------------------------------------------------------------------

    /// Run PageRank over the unified topology (manual + derived edges).
    ///
    /// Returns a [`PageRankReport`] with scores sorted descending (ties: key
    /// ascending).  Set `config.edge_type` to restrict to one edge type.
    /// `config.converged` is `true` only when the power iteration converged
    /// within `config.max_iters` and within any time budget.
    pub fn pagerank(&self, config: &crate::algo::PageRankConfig) -> crate::algo::PageRankReport {
        crate::algo::pagerank(&self.topo, &self.ids, &self.syms, &self.labels, config)
    }

    /// Weakly-connected components over the unified topology (treated as
    /// undirected regardless of how edges were inserted).
    ///
    /// Component IDs are the key of the smallest member in the component
    /// (deterministic).  Result sorted by (component_id, key).
    pub fn connected_components(&self, config: &crate::algo::WccConfig) -> crate::algo::WccReport {
        crate::algo::wcc(&self.topo, &self.ids, &self.syms, &self.labels, config)
    }

    /// Degree centrality for every live node.
    ///
    /// `direction`: `AlgoDir::Out` = out-degree, `AlgoDir::In` = in-degree,
    /// `AlgoDir::Both` = out + in (total directed degree).
    ///
    /// For one-shot ranking use this; for a live property updated on every
    /// write, create a Degree materialized view instead (see `docs/site/algorithms.md`).
    pub fn degree_centrality(
        &self,
        config: &crate::algo::DegreeConfig,
    ) -> crate::algo::DegreeReport {
        crate::algo::degree_centrality(&self.topo, &self.ids, &self.syms, &self.labels, config)
    }

    /// Write a vector of `(node_key, score)` pairs as `prop_name` on each node,
    /// atomically via a single write-batch (one WAL frame, one fsync).
    ///
    /// # Errors
    /// - [`GraphError::ReadOnly`]: called on an as-of instance.
    /// - [`GraphError::RuleInvalid`]: `prop_name` is managed by an existing view
    ///   (collision check mirrors `create_view`).
    /// - [`GraphError::KeyNotFound`]: a key in `scores` does not exist as a live node.
    pub fn write_scores(&mut self, prop_name: &str, scores: &[(String, f64)]) -> Result<()> {
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        // Collision check: refuse if prop_name is view-managed.
        if let Some(view_name) = self.view_store.view_for_prop(prop_name) {
            return Err(GraphError::RuleInvalid {
                detail: format!(
                    "prop {:?} is managed by view {:?} and cannot be written as scores",
                    prop_name, view_name
                ),
            });
        }
        // Refuse if prop_name is a view name itself (confusing namespace collision).
        if self.view_store.has_view(prop_name) {
            return Err(GraphError::RuleInvalid {
                detail: format!(
                    "prop_name {:?} collides with an existing view name",
                    prop_name
                ),
            });
        }
        // Write all scores in a single crash-atomic batch.
        self.write_batch(|b| {
            for (key, score) in scores {
                b.set_prop(key, prop_name, Value::Float(*score));
            }
        })?;
        Ok(())
    }

    /// Return the value of `field` for the node with key `key`, or `None` if
    /// the node or field is absent.  Reads through the overlay-over-base
    /// `ColumnsView`, materialising base values on demand (zero heap cost for
    /// overlay hits; one clone per base hit).
    pub fn get_prop(&self, key: &str, field: &str) -> Option<Value> {
        let id = self.ids.get(key)?;
        self.props_view().get(id, field).map(|vr| vr.into_value())
    }

    pub fn has_node(&self, key: &str) -> bool {
        self.ids.get(key).is_some()
    }

    /// Borrow the raw id map. Used by `NodeMask::from_keys` to resolve keys.
    pub(crate) fn ids(&self) -> &IdMap {
        &self.ids
    }

    // -----------------------------------------------------------------------
    // RBAC role resolution
    // -----------------------------------------------------------------------

    /// Parse `roles.json` bytes from `fs`.
    ///
    /// Return values:
    ///   `Ok(Some(roles))` — file present and valid (roles may be an empty vec)
    ///   `Ok(Some(vec![]))` — file absent (fs returns empty bytes) → no roles defined
    ///   `Ok(None)`        — file present but corrupt or unrecognised version
    ///                       → poisoned state; `mask_for_role` will return `Err` for any role token
    ///
    /// Note: absent and healthy-but-empty both produce `Some`; `None` means
    /// corrupt — the opposite of what an optional "file missing" convention would
    /// suggest.  The open path stores this result on `db.roles` directly.
    fn load_roles_from_fs(fs: &F) -> Result<Option<Vec<RoleDef>>> {
        let bytes = fs.read(FileId::Roles).map_err(GraphError::Io)?;
        if bytes.is_empty() {
            // Missing file: no roles defined.
            return Ok(Some(vec![]));
        }
        match serde_json::from_slice::<RolesFile>(&bytes) {
            Ok(f) if f.version == 1 => Ok(Some(f.roles)),
            // Corrupt or unrecognised version: poison the roles state.
            _ => Ok(None),
        }
    }

    /// Resolve a role to a node-visibility mask against the current graph state.
    ///
    /// Returns `Err` when:
    /// - `roles.json` was present but corrupt at open (poisoned state), or
    /// - `role` does not match any defined role name.
    ///
    /// The mask union is: explicit `keys` (unknown keys silently ignored) plus
    /// all live nodes carrying any label in `labels`.  Label resolution is live
    /// — new nodes of an allowed label are visible without re-applying the
    /// schema.  An empty union = empty mask = sees nothing.
    pub fn mask_for_role(&self, role: &str) -> Result<crate::mask::NodeMask> {
        let roles = self.roles.as_ref().ok_or_else(|| GraphError::Corrupt {
            detail:
                "roles.json was corrupt at open; fix the file and re-open to restore role access"
                    .into(),
        })?;
        let def = roles
            .iter()
            .find(|r| r.name == role)
            .ok_or_else(|| GraphError::KeyNotFound {
                key: format!("role:{role}"),
            })?;

        let mut visible = std::collections::HashSet::new();

        // Key leg: resolve explicit keys to dense ids (unknown keys ignored).
        for key in &def.keys {
            if let Some(id) = self.ids.get(key) {
                visible.insert(id);
            }
        }

        // Label leg: live scan — iterate labels vec for matching symbol.
        for label_name in &def.labels {
            if let Some(sym) = self.syms.get(label_name) {
                for (i, &s) in self.labels.iter().enumerate() {
                    if s == sym {
                        visible.insert(i as u32);
                    }
                }
            }
        }

        Ok(crate::mask::NodeMask { visible })
    }

    /// Return the current list of role definitions.
    ///
    /// Returns an empty list when no roles are defined or when `roles.json`
    /// was corrupt at open (check [`mask_for_role`](Self::mask_for_role) for
    /// the fail-loud error in that case).
    pub fn roles(&self) -> Vec<RoleDef> {
        self.roles.as_deref().unwrap_or(&[]).to_vec()
    }

    /// Write `roles` to `roles.json` atomically and update the in-memory list.
    ///
    /// Called by `apply_schema` when roles change. Never called on unchanged
    /// re-apply — this preserves byte-identical idempotency.
    pub(crate) fn commit_roles(&mut self, roles: Vec<RoleDef>) -> Result<()> {
        let file = RolesFile::v1(roles.clone());
        let bytes = serde_json::to_vec(&file).map_err(|e| GraphError::Corrupt {
            detail: format!("roles serialization: {e}"),
        })?;
        self.fs
            .write_atomic(FileId::Roles, &bytes)
            .map_err(GraphError::Io)?;
        self.roles = Some(roles);
        Ok(())
    }

    fn view(&self) -> GraphView<'_> {
        GraphView {
            ids: &self.ids,
            syms: &self.syms,
            labels: &self.labels,
            props: self.props_view(),
            topo: self.topo_view(),
            edge_props: &self.edge_props,
            mask: None,
        }
    }

    fn view_masked<'a>(&'a self, mask: &'a crate::mask::NodeMask) -> GraphView<'a> {
        GraphView {
            ids: &self.ids,
            syms: &self.syms,
            labels: &self.labels,
            props: self.props_view(),
            topo: self.topo_view(),
            edge_props: &self.edge_props,
            mask: Some(&mask.visible),
        }
    }

    /// Execute a read-only Cypher query with a node visibility mask.
    ///
    /// Only nodes whose key is in `mask` are accessible: label scans, key
    /// lookups, and neighbor expansions all respect the mask. Edges where
    /// either endpoint is hidden are silently dropped.
    ///
    /// Returns `Err` with a "masked queries are read-only" message when
    /// `cypher` is a write statement (CREATE / MERGE / MATCH…SET / DELETE).
    pub fn query_masked(
        &self,
        cypher: &str,
        params: &std::collections::BTreeMap<String, Value>,
        mask: &crate::mask::NodeMask,
    ) -> Result<ResultSet> {
        // Reject write statements up front.
        let tokens = lex(cypher).map_err(|e| GraphError::QueryError {
            detail: format!("lex: {e}"),
        })?;
        if is_write_tokens(&tokens) {
            return Err(GraphError::QueryError {
                detail: "masked queries are read-only".into(),
            });
        }
        let ast = parse(&tokens).map_err(|e| GraphError::QueryError {
            detail: format!("parse: {e}"),
        })?;
        let ops = plan(&ast).map_err(|e| GraphError::QueryError {
            detail: format!("plan: {e}"),
        })?;
        execute(&self.view_masked(mask), &ops, &Params(params)).map_err(|e| {
            GraphError::QueryError {
                detail: format!("execute: {e}"),
            }
        })
    }

    pub fn node_ref(&self, key: &str) -> Option<NodeRef<'_, F>> {
        let id = self.ids.get(key)?;
        Some(NodeRef { db: self, id })
    }

    /// BFS neighborhood expansion restricted to visible nodes in `mask`.
    ///
    /// Hidden nodes are neither returned nor used as traversal intermediaries —
    /// a visible node reachable only through a hidden node will not appear.
    /// Returns `None` when `key` does not exist (caller should 404).
    pub fn neighborhood_masked(
        &self,
        key: &str,
        depth: u32,
        edge_types: Option<&[&str]>,
        dir: Dir,
        mask: &crate::mask::NodeMask,
    ) -> Option<ResultSet> {
        let id = self.ids.get(key)?;
        let view = self.view_masked(mask);
        let resolved: Option<Vec<u32>> = edge_types.map(|names| {
            names
                .iter()
                .filter_map(|name| view.syms.get(name))
                .collect()
        });
        let nb = neighborhood(&view, id, depth, resolved.as_deref(), dir);
        let mut rs = ResultSet::new(vec!["key".into(), "label".into(), "depth".into()]);
        for (nid, d) in nb.nodes {
            let key = view.key_of(nid);
            let label = view
                .label_of(nid)
                .expect("real nodes always have a label; u32::MAX sentinel cannot occur");
            rs.push_row(vec![
                Some(Value::Str(key.to_string())),
                Some(Value::Str(label.to_string())),
                Some(Value::Int(d as i64)),
            ]);
        }
        Some(rs)
    }

    /// Live node's key, label, and columnar props. Unknown or tombstoned → `None`.
    pub fn node_info(&self, key: &str) -> Option<NodeInfo> {
        let n = self.node_ref(key)?;
        Some(NodeInfo {
            key: n.key().to_string(),
            label: n.label().to_string(),
            props: n.props(),
        })
    }

    /// Every directed edge incident on `key`, both directions, every etype.
    ///
    /// Walk is `topology.etypes()` × `{Out, In}` × `neighbors()`. `derived` is
    /// membership in [`RuleEngine::provenance_touching`] (O(degree) via the
    /// Plan-8 `by_node` index). Sorted by `(edge_type, src_key, dst_key)`.
    /// Unknown key → [`GraphError::KeyNotFound`].
    pub fn node_edges(&self, key: &str) -> Result<Vec<EdgeInfo>> {
        let id = self
            .ids
            .get(key)
            .ok_or_else(|| GraphError::KeyNotFound { key: key.into() })?;
        let derived: BTreeSet<(u32, u32, u32)> = self
            .engine
            .provenance_touching(id)
            .map(|(_rule, etype, src, dst)| (etype, src, dst))
            .collect();
        let mut edges = Vec::new();
        let tv = self.topo_view();
        for etype in tv.etypes() {
            let edge_type = self
                .syms
                .resolve(etype)
                .expect("topology etype is interned")
                .to_string();
            for dir in [Direction::Out, Direction::In] {
                for &nbr in tv.neighbors(etype, dir, id).as_ref() {
                    let (src, dst, src_key, dst_key) = match dir {
                        Direction::Out => (
                            id,
                            nbr,
                            key.to_string(),
                            self.ids
                                .key_of(nbr)
                                .ok_or_else(|| GraphError::Corrupt {
                                    detail: format!("topology id {nbr} has no key"),
                                })?
                                .to_string(),
                        ),
                        Direction::In => (
                            nbr,
                            id,
                            self.ids
                                .key_of(nbr)
                                .ok_or_else(|| GraphError::Corrupt {
                                    detail: format!("topology id {nbr} has no key"),
                                })?
                                .to_string(),
                            key.to_string(),
                        ),
                    };
                    edges.push(EdgeInfo {
                        edge_type: edge_type.clone(),
                        src_key,
                        dst_key,
                        derived: derived.contains(&(etype, src, dst)),
                    });
                }
            }
        }
        edges.sort_by(|a, b| {
            a.edge_type
                .cmp(&b.edge_type)
                .then(a.src_key.cmp(&b.src_key))
                .then(a.dst_key.cmp(&b.dst_key))
        });
        // Self-loops appear in both Out and In; sort makes the pair adjacent
        // (sort key matches PartialEq for this case) so one pass drops the dup.
        edges.dedup();
        Ok(edges)
    }

    pub fn nodes_with_label(&self, label: &str) -> Vec<NodeRef<'_, F>> {
        self.view()
            .nodes_with_label(label)
            .into_iter()
            .map(|id| NodeRef { db: self, id })
            .collect()
    }

    pub fn find_nodes(&self, label: &str, filter: &Filter) -> Vec<NodeRef<'_, F>> {
        let view = self.view();
        view.nodes_with_label(label)
            .into_iter()
            .filter(|&id| {
                eval_filter(filter, &|field| {
                    view.prop(id, field).map(|vr| vr.into_value())
                })
            })
            .map(|id| NodeRef { db: self, id })
            .collect()
    }

    /// Find nodes with the given `label` whose `field` vector is most similar
    /// to `q` (cosine similarity), returning up to `k` results with similarity
    /// ≥ `min`, sorted descending.
    ///
    /// Uses the HNSW index when one is available (fast path); otherwise falls
    /// back to an O(n) brute-force scan over all nodes with that label (exact).
    pub fn find_similar_vector(
        &mut self,
        field: &str,
        label: &str,
        q: &[f64],
        k: usize,
        min: f64,
    ) -> Vec<(String, f64)> {
        // Ensure any HNSW blobs retained from the snapshot are deserialized
        // before the first ANN query on a clean-open (no-WAL) path.
        self.engine.ensure_hnsw_loaded();
        // L2-normalise query for cosine via dot product.
        let norm: f64 = q.iter().map(|x| x * x).sum::<f64>().sqrt();
        if norm == 0.0 {
            return vec![];
        }
        let q_unit: Vec<f64> = q.iter().map(|x| x / norm).collect();

        // Try HNSW fast path.
        if let Some(hits) = self.engine.hnsw_search_dst(field, label, &q_unit, k) {
            let mut out: Vec<(String, f64)> = hits
                .into_iter()
                .filter(|&(_, sim)| sim >= min)
                .filter_map(|(id, sim)| self.ids.key_of(id).map(|key| (key.to_string(), sim)))
                .collect();
            out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            out.truncate(k);
            return out;
        }

        // Brute-force fallback: O(n) scan.
        let view = self.view();
        let mut scored: Vec<(String, f64)> = view
            .nodes_with_label(label)
            .into_iter()
            .filter_map(|id| {
                let v = view.prop(id, field)?;
                let v_owned = v.into_value();
                let xs = value_as_float_list(&v_owned)?;
                let v_norm: f64 = xs.iter().map(|x| x * x).sum::<f64>().sqrt();
                if v_norm == 0.0 {
                    return None;
                }
                let dot: f64 = q_unit
                    .iter()
                    .zip(xs.iter())
                    .map(|(a, b)| a * (b / v_norm))
                    .sum();
                if dot < min {
                    return None;
                }
                let key = self.ids.key_of(id)?.to_string();
                Some((key, dot))
            })
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(k);
        scored
    }

    /// Lex → parse → plan → execute `cypher` over a read-only view.
    /// Every pipeline `Err(String)` becomes `GraphError::QueryError` with a
    /// stage prefix (`lex:` / `parse:` / `plan:` / `execute:`).
    pub fn query(&self, cypher: &str, params: &BTreeMap<String, Value>) -> Result<ResultSet> {
        let tokens = lex(cypher).map_err(|e| GraphError::QueryError {
            detail: format!("lex: {e}"),
        })?;
        let ast = parse(&tokens).map_err(|e| GraphError::QueryError {
            detail: format!("parse: {e}"),
        })?;
        let ops = plan(&ast).map_err(|e| GraphError::QueryError {
            detail: format!("plan: {e}"),
        })?;
        execute(&self.view(), &ops, &Params(params)).map_err(|e| GraphError::QueryError {
            detail: format!("execute: {e}"),
        })
    }

    /// Convenience entry-point that accepts a slice of `(name, value)` pairs
    /// instead of a pre-built `BTreeMap`.  Equivalent to building the map and
    /// calling [`GraphDb::query`].
    pub fn query_with_params(&self, cypher: &str, params: &[(&str, Value)]) -> Result<ResultSet> {
        let map: BTreeMap<String, Value> = params
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
        self.query(cypher, &map)
    }

    /// Execute a Cypher write statement (CREATE / MATCH…SET / MATCH…DELETE / MERGE).
    ///
    /// All mutations flow through the same `insert_node` / `set_prop` /
    /// `delete_edge` / `insert_edge` path as the Rust API so the rule engine
    /// fires and the WAL captures everything with one fsync per statement.
    ///
    /// Returns a one-row [`ResultSet`] with columns `created`, `properties_set`,
    /// and `deleted` matching the write-result contract.
    ///
    /// **Mutation routing**: mutations are collected into a single
    /// [`BatchBuilder`] and committed atomically (one WAL `Batch` frame, one
    /// fsync). The MATCH phase for SET/DELETE uses a read-only `execute` call
    /// over `self.view()` — the borrow is dropped before the batch is opened.
    ///
    /// **Limitations (v1)**:
    /// - SET RHS must be a literal, `$param`, or arithmetic; bare property copy → named error.
    /// - `DETACH DELETE n` → calls `delete_node` for each matched node (removes all edges).
    /// - Bare `DELETE n` → error if n has any incident edges; succeeds for isolated nodes.
    /// - MERGE supports `ON CREATE SET` / `ON MATCH SET` in the same write batch.
    /// - Deleting a derived edge → named error "cannot delete derived edge".
    pub fn query_write(
        &mut self,
        cypher: &str,
        params: &BTreeMap<String, Value>,
    ) -> Result<ResultSet> {
        let tokens = lex(cypher).map_err(|e| GraphError::QueryError {
            detail: format!("lex: {e}"),
        })?;
        let stmt = parse_write(&tokens).map_err(|e| GraphError::QueryError {
            detail: format!("parse: {e}"),
        })?;
        self.exec_write_stmt(stmt, params)
    }

    fn exec_write_stmt(
        &mut self,
        stmt: WriteStatement,
        params: &BTreeMap<String, Value>,
    ) -> Result<ResultSet> {
        match stmt {
            WriteStatement::Create(s) => self.exec_create(s, params),
            WriteStatement::MatchSet(s) => self.exec_match_set(s, params),
            WriteStatement::MatchDelete(s) => self.exec_match_delete(s, params),
            WriteStatement::MatchDeleteNode(s) => self.exec_match_delete_node(s, params),
            WriteStatement::Merge(s) => self.exec_merge(s, params),
        }
    }

    fn exec_create(
        &mut self,
        stmt: core_query::cypher::CreateStmt,
        params: &BTreeMap<String, Value>,
    ) -> Result<ResultSet> {
        // Extract the node key from props: require a string-valued `id` field.
        let mut var_to_key: BTreeMap<String, String> = BTreeMap::new();
        for node in &stmt.nodes {
            let var = node.var.as_deref().unwrap_or("_cn0");
            let key = node
                .props
                .iter()
                .find(|(f, _)| f == "id")
                .and_then(|(_, v)| {
                    if let Value::Str(s) = v {
                        Some(s.clone())
                    } else {
                        None
                    }
                })
                .ok_or_else(|| GraphError::QueryError {
                    detail: format!(
                        "CREATE node ({}:{}) requires a string 'id' property",
                        var, node.label
                    ),
                })?;
            var_to_key.insert(var.to_string(), key);
        }

        let mut batch = self.batch();
        let mut created: usize = 0;
        for node in &stmt.nodes {
            let var = node.var.as_deref().unwrap_or("_cn0");
            let key = &var_to_key[var];
            batch.insert_node(&node.label, key, node.props.clone());
            created += 1;
        }
        for edge in &stmt.edges {
            let src_key = var_to_key
                .get(&edge.src_var)
                .ok_or_else(|| GraphError::QueryError {
                    detail: format!("CREATE edge src variable '{}' is not bound", edge.src_var),
                })?;
            let dst_key = var_to_key
                .get(&edge.dst_var)
                .ok_or_else(|| GraphError::QueryError {
                    detail: format!("CREATE edge dst variable '{}' is not bound", edge.dst_var),
                })?;
            batch.insert_edge(&edge.etype, src_key, dst_key);
        }
        batch.commit()?;

        // Optional RETURN clause: project created bindings as a read result.
        if let Some(returns) = stmt.returns {
            // Each created node is looked up by its key via a separate MATCH pattern.
            // Multiple single-node patterns cross-join to produce 1 output row with
            // all variables bound (each pattern returns exactly 1 row).
            let patterns: Vec<Pattern> = stmt
                .nodes
                .iter()
                .map(|node| {
                    let var = node.var.as_deref().unwrap_or("_cn0");
                    let key = var_to_key[var].clone();
                    Pattern {
                        start: NodePat {
                            var: Some(var.to_string()),
                            label: Some(node.label.clone()),
                            props: vec![("id".to_string(), Operand::Lit(Value::Str(key)))],
                        },
                        chain: vec![],
                        shortest: false,
                    }
                })
                .collect();
            let q = Query {
                matches: patterns,
                optional_clauses: vec![],
                where_expr: None,
                unwinds: vec![],
                post_unwind_where: None,
                stages: vec![],
                returns,
                distinct: false,
                order_by: vec![],
                skip: None,
                limit: None,
            };
            let ops = plan(&q).map_err(|e| GraphError::QueryError {
                detail: format!("plan: {e}"),
            })?;
            return execute(&self.view(), &ops, &Params(params)).map_err(|e| {
                GraphError::QueryError {
                    detail: format!("execute: {e}"),
                }
            });
        }

        let mut rs = write_result_set();
        rs.push_row(vec![
            Some(Value::Int(created as i64)),
            Some(Value::Int(0)),
            Some(Value::Int(0)),
        ]);
        Ok(rs)
    }

    fn exec_match_set(
        &mut self,
        stmt: core_query::cypher::MatchSetStmt,
        params: &BTreeMap<String, Value>,
    ) -> Result<ResultSet> {
        let project_returns = stmt.returns.clone();
        // Collect unique node vars targeted by SET clauses, plus RETURN bindings
        // so the post-write projection can look them up by key.
        let mut set_vars: Vec<String> = Vec::new();
        for s in &stmt.sets {
            if !set_vars.contains(&s.var) {
                set_vars.push(s.var.clone());
            }
        }
        let rel_vars = pattern_rel_vars(&stmt.matches);
        let mut lookup_vars = set_vars.clone();
        for v in pattern_node_vars(&stmt.matches) {
            add_var(&mut lookup_vars, &v);
        }
        if let Some(ref returns) = project_returns {
            for v in ret_node_vars(returns) {
                if !rel_vars.iter().any(|r| r == &v) {
                    add_var(&mut lookup_vars, &v);
                }
            }
        }

        // Synthesize a read query: MATCH … WHERE … RETURN <lookup_vars>, <set_values…>
        // SET values are projected as ScalarExpr items so that arithmetic expressions
        // (e.g. `SET n.score = n.score * 1.5`) are evaluated in the matched-row context.
        let mut set_returns: Vec<RetItem> = lookup_vars
            .iter()
            .map(|v| RetItem {
                value: RetVal::Var(v.clone()),
                alias: None,
            })
            .collect();
        // One computed column per SET clause; alias is `__sv_<i>`.
        let set_val_cols: Vec<String> = stmt
            .sets
            .iter()
            .enumerate()
            .map(|(i, _)| format!("__sv_{i}"))
            .collect();
        for (sc, col) in stmt.sets.iter().zip(&set_val_cols) {
            set_returns.push(RetItem {
                value: RetVal::ScalarExpr(sc.value.clone()),
                alias: Some(col.clone()),
            });
        }
        // Capture relationship types while r is bound; SET does not change them.
        for r in &rel_vars {
            set_returns.push(RetItem {
                value: RetVal::FuncCall {
                    name: "type".into(),
                    args: vec![Operand::Var(r.clone())],
                },
                alias: Some(rel_type_alias(r)),
            });
        }

        let read_q = Query {
            matches: stmt.matches.clone(),
            optional_clauses: vec![],
            where_expr: stmt.where_expr.clone(),
            unwinds: vec![],
            post_unwind_where: None,
            stages: vec![],
            returns: set_returns,
            distinct: false,
            order_by: vec![],
            skip: None,
            limit: None,
        };
        let ops = plan(&read_q).map_err(|e| GraphError::QueryError {
            detail: format!("plan: {e}"),
        })?;
        // MATCH phase is read-only; borrow ends before batch opens.
        let match_rs =
            execute(&self.view(), &ops, &Params(params)).map_err(|e| GraphError::QueryError {
                detail: format!("execute: {e}"),
            })?;

        // Collect (key, field, value) for each matched row × each SET clause.
        let mut set_ops: Vec<(String, String, Value)> = Vec::new();
        for row_i in 0..match_rs.len() {
            for (sc, col) in stmt.sets.iter().zip(&set_val_cols) {
                let key = match match_rs.get(row_i, &sc.var) {
                    Some(Value::Str(k)) => k.clone(),
                    _ => {
                        return Err(GraphError::QueryError {
                            detail: format!(
                                "SET variable '{}' did not resolve to a node key",
                                sc.var
                            ),
                        })
                    }
                };
                // The SET value was already evaluated by the executor.
                let value = match match_rs.get(row_i, col) {
                    Some(v) => v.clone(),
                    None => {
                        return Err(GraphError::QueryError {
                            detail: format!(
                                "SET value for {}.{} evaluated to null",
                                sc.var, sc.field
                            ),
                        })
                    }
                };
                set_ops.push((key, sc.field.clone(), value));
            }
        }

        // Apply as one atomic batch.
        let props_set = set_ops.len();
        let mut batch = self.batch();
        for (key, field, value) in set_ops {
            batch.set_prop(&key, &field, value);
        }
        batch.commit()?;

        if let Some(returns) = project_returns {
            return project_set_return_rows(self, &rel_vars, &match_rs, &returns, params);
        }

        let mut rs = write_result_set();
        rs.push_row(vec![
            Some(Value::Int(0)),
            Some(Value::Int(props_set as i64)),
            Some(Value::Int(0)),
        ]);
        Ok(rs)
    }

    fn exec_match_delete(
        &mut self,
        stmt: core_query::cypher::MatchDeleteStmt,
        params: &BTreeMap<String, Value>,
    ) -> Result<ResultSet> {
        // Collect unique node vars needed to identify edge endpoints.
        let mut node_vars: Vec<String> = Vec::new();
        for ed in &stmt.deletes {
            if !node_vars.contains(&ed.src_var) {
                node_vars.push(ed.src_var.clone());
            }
            if !node_vars.contains(&ed.dst_var) {
                node_vars.push(ed.dst_var.clone());
            }
        }

        // Synthesize read query.
        let returns: Vec<RetItem> = node_vars
            .iter()
            .map(|v| RetItem {
                value: RetVal::Var(v.clone()),
                alias: None,
            })
            .collect();
        let read_q = Query {
            matches: stmt.matches,
            optional_clauses: vec![],
            where_expr: stmt.where_expr,
            unwinds: vec![],
            post_unwind_where: None,
            stages: vec![],
            returns,
            distinct: false,
            order_by: vec![],
            skip: None,
            limit: None,
        };
        let ops = plan(&read_q).map_err(|e| GraphError::QueryError {
            detail: format!("plan: {e}"),
        })?;
        let match_rs =
            execute(&self.view(), &ops, &Params(params)).map_err(|e| GraphError::QueryError {
                detail: format!("execute: {e}"),
            })?;

        // Collect (etype, src_key, dst_key) for each row × each delete target.
        let mut del_ops: Vec<(String, String, String)> = Vec::new();
        for row_i in 0..match_rs.len() {
            for ed in &stmt.deletes {
                let src_key = match match_rs.get(row_i, &ed.src_var) {
                    Some(Value::Str(k)) => k.clone(),
                    _ => {
                        return Err(GraphError::QueryError {
                            detail: format!(
                                "DELETE src variable '{}' did not resolve to a node key",
                                ed.src_var
                            ),
                        })
                    }
                };
                let dst_key = match match_rs.get(row_i, &ed.dst_var) {
                    Some(Value::Str(k)) => k.clone(),
                    _ => {
                        return Err(GraphError::QueryError {
                            detail: format!(
                                "DELETE dst variable '{}' did not resolve to a node key",
                                ed.dst_var
                            ),
                        })
                    }
                };
                del_ops.push((ed.etype.clone(), src_key, dst_key));
            }
        }

        // Apply as one atomic batch.
        let deleted = del_ops.len();
        let mut batch = self.batch();
        for (etype, src_key, dst_key) in del_ops {
            batch.delete_edge(&etype, &src_key, &dst_key);
        }
        batch.commit().map_err(|e| match e {
            GraphError::RuleOwned { .. } => GraphError::QueryError {
                detail: "cannot delete derived edge; retract via the rule or change the property"
                    .to_string(),
            },
            other => other,
        })?;

        let mut rs = write_result_set();
        rs.push_row(vec![
            Some(Value::Int(0)),
            Some(Value::Int(0)),
            Some(Value::Int(deleted as i64)),
        ]);
        Ok(rs)
    }

    /// Execute `MATCH … [DETACH] DELETE <node_var> [, …]`.
    ///
    /// Collects the matching node keys via an ephemeral read query, then calls
    /// `delete_node` on each one.  When `stmt.detach` is `false` (bare DELETE)
    /// the executor first checks that the node has no incident edges; if any
    /// remain it returns a named error matching openCypher semantics.
    fn exec_match_delete_node(
        &mut self,
        stmt: MatchDeleteNodeStmt,
        params: &BTreeMap<String, Value>,
    ) -> Result<ResultSet> {
        // Build a read query returning only the node keys we need.
        let returns: Vec<RetItem> = stmt
            .node_vars
            .iter()
            .map(|v| RetItem {
                value: RetVal::Var(v.clone()),
                alias: None,
            })
            .collect();
        let read_q = Query {
            matches: stmt.matches,
            optional_clauses: vec![],
            where_expr: stmt.where_expr,
            unwinds: vec![],
            post_unwind_where: None,
            stages: vec![],
            returns,
            distinct: false,
            order_by: vec![],
            skip: None,
            limit: None,
        };
        let ops = plan(&read_q).map_err(|e| GraphError::QueryError {
            detail: format!("plan: {e}"),
        })?;
        let match_rs =
            execute(&self.view(), &ops, &Params(params)).map_err(|e| GraphError::QueryError {
                detail: format!("execute: {e}"),
            })?;

        // Collect unique node keys to delete (deduplicate across rows × vars).
        let mut keys: Vec<String> = Vec::new();
        for row_i in 0..match_rs.len() {
            for var in &stmt.node_vars {
                if let Some(Value::Str(k)) = match_rs.get(row_i, var) {
                    if !keys.contains(k) {
                        keys.push(k.clone());
                    }
                }
            }
        }

        if !stmt.detach {
            // openCypher bare DELETE: error if any matched node has incident edges.
            for key in &keys {
                if let Some(id) = self.ids.get(key) {
                    let tv = self.topo_view();
                    let has_edges = tv.etypes().any(|et| {
                        !tv.neighbors(et, Direction::Out, id).is_empty()
                            || !tv.neighbors(et, Direction::In, id).is_empty()
                    });
                    if has_edges {
                        return Err(GraphError::QueryError {
                            detail: format!(
                                "Cannot delete node `{key}` because it still has incident edges. \
                                 Use DETACH DELETE to remove the node and all its edges."
                            ),
                        });
                    }
                }
            }
        }

        let mut nodes_deleted = 0i64;
        let mut edges_deleted = 0i64;
        for key in keys {
            match self.delete_node(&key) {
                Ok(report) => {
                    nodes_deleted += 1;
                    edges_deleted += (report.manual_edges + report.derived_edges) as i64;
                }
                Err(GraphError::KeyNotFound { .. }) => {
                    // Node may have been deleted by an earlier iteration (e.g., via
                    // multiple MATCH rows for the same node).  Safe to skip.
                }
                Err(e) => return Err(e),
            }
        }

        let mut rs = write_result_set();
        rs.push_row(vec![
            Some(Value::Int(0)),
            Some(Value::Int(0)),
            Some(Value::Int(nodes_deleted + edges_deleted)),
        ]);
        Ok(rs)
    }

    fn exec_merge(
        &mut self,
        stmt: core_query::cypher::MergeStmt,
        params: &BTreeMap<String, Value>,
    ) -> Result<ResultSet> {
        // MERGE: check if a node with the given key already exists.
        let key = match &stmt.key_value {
            Value::Str(s) => s.clone(),
            _ => {
                return Err(GraphError::QueryError {
                    detail: format!(
                        "MERGE key value must be a string (got {:?})",
                        stmt.key_value
                    ),
                })
            }
        };

        if let Some(var) = stmt.var.as_deref() {
            for sc in stmt.on_create.iter().chain(&stmt.on_match) {
                if sc.var != var {
                    return Err(GraphError::QueryError {
                        detail: format!(
                            "SET variable '{}' does not match MERGE variable '{var}'",
                            sc.var
                        ),
                    });
                }
            }
        }

        let existed = self.has_node(&key);
        let mut created = 0i64;
        if !existed || !stmt.on_match.is_empty() {
            let mut batch = self.batch();
            if !existed {
                let props = vec![(stmt.key_field.clone(), stmt.key_value.clone())];
                batch.insert_node(&stmt.label, &key, props);
                for sc in &stmt.on_create {
                    let value = resolve_merge_set_value(&sc.value, params)?;
                    batch.set_prop(&key, &sc.field, value);
                }
                created = 1;
            } else {
                for sc in &stmt.on_match {
                    let value = resolve_merge_set_value(&sc.value, params)?;
                    batch.set_prop(&key, &sc.field, value);
                }
            }
            batch.commit()?;
        }

        // Optional RETURN clause: project the node (created or matched) as a read result.
        if let Some(returns) = stmt.returns {
            let var = stmt.var.as_deref().unwrap_or("_mn0");
            let q = Query {
                matches: vec![Pattern {
                    start: NodePat {
                        var: Some(var.to_string()),
                        label: Some(stmt.label.clone()),
                        props: vec![("id".to_string(), Operand::Lit(stmt.key_value.clone()))],
                    },
                    chain: vec![],
                    shortest: false,
                }],
                optional_clauses: vec![],
                where_expr: None,
                unwinds: vec![],
                post_unwind_where: None,
                stages: vec![],
                returns,
                distinct: false,
                order_by: vec![],
                skip: None,
                limit: None,
            };
            let ops = plan(&q).map_err(|e| GraphError::QueryError {
                detail: format!("plan: {e}"),
            })?;
            return execute(&self.view(), &ops, &Params(params)).map_err(|e| {
                GraphError::QueryError {
                    detail: format!("execute: {e}"),
                }
            });
        }

        let mut rs = write_result_set();
        rs.push_row(vec![
            Some(Value::Int(created)),
            Some(Value::Int(0)),
            Some(Value::Int(0)),
        ]);
        Ok(rs)
    }

    /// Return all rule-owned edges between `key_a` and `key_b` (either direction),
    /// annotated with rule name, edge type, direction, and weight.
    /// Results are sorted by (rule, edge_type).
    /// Returns `Err(KeyNotFound)` if either key is unknown.
    pub fn explain(&self, key_a: &str, key_b: &str) -> Result<Vec<Explanation>> {
        let id_a = self
            .ids
            .get(key_a)
            .ok_or_else(|| GraphError::KeyNotFound { key: key_a.into() })?;
        let id_b = self
            .ids
            .get(key_b)
            .ok_or_else(|| GraphError::KeyNotFound { key: key_b.into() })?;

        let mut results = Vec::new();

        // Walk the smaller incident set so explain is O(min(deg(a), deg(b)))
        // rather than O(total provenance).
        let scan = if self.engine.provenance_touching_len(id_a)
            <= self.engine.provenance_touching_len(id_b)
        {
            id_a
        } else {
            id_b
        };
        for (rule_name, etype, src, dst) in self.engine.provenance_touching(scan) {
            if !((src == id_a && dst == id_b) || (src == id_b && dst == id_a)) {
                continue;
            }
            let Some(rule_def) = self.engine.rules().find(|r| r.name == rule_name) else {
                continue;
            };
            let edge_type = match self.syms.resolve(etype) {
                Some(s) => s.to_string(),
                None => continue,
            };
            let src_key = self
                .ids
                .key_of(src)
                .expect("provenance ids always resolvable")
                .to_string();
            let dst_key = self
                .ids
                .key_of(dst)
                .expect("provenance ids always resolvable")
                .to_string();
            let weight = rule_def.weight_prop.as_deref().and_then(|prop| {
                self.edge_props.get(etype, src, dst, prop).and_then(|v| {
                    if let Value::Float(f) = v {
                        Some(*f)
                    } else {
                        None
                    }
                })
            });
            results.push(Explanation {
                rule: rule_name.to_string(),
                edge_type,
                src_key,
                dst_key,
                weight,
                predicate: PredicateSummary {
                    approximate: rule_def.approximate,
                    ..PredicateSummary::from(&rule_def.predicate)
                },
            });
        }

        results.sort_by(|a, b| a.rule.cmp(&b.rule).then(a.edge_type.cmp(&b.edge_type)));
        Ok(results)
    }

    pub fn neighbors(&self, key: &str, edge_type: &str, dir: Direction) -> Result<Vec<String>> {
        let id = self
            .ids
            .get(key)
            .ok_or_else(|| GraphError::KeyNotFound { key: key.into() })?;
        let Some(sym) = self.syms.get(edge_type) else {
            return Ok(Vec::new());
        };
        self.topo_view()
            .neighbors(sym, dir, id)
            .iter()
            .map(|&n| {
                self.ids
                    .key_of(n)
                    .map(|k| k.to_string())
                    .ok_or_else(|| GraphError::Corrupt {
                        detail: format!("topology id {n} has no key"),
                    })
            })
            .collect::<Result<Vec<_>>>()
    }

    pub fn node_count(&self) -> usize {
        self.ids.len()
    }

    /// Return the per-node change history for `key` by scanning the on-disk WAL.
    ///
    /// ## Horizon
    ///
    /// History reaches back only to the last WAL-truncating snapshot, exactly like `open_at`.
    /// Snapshots written with `keep_wal: true` preserve deeper history. This is the honest,
    /// zero-cost contract; a durable history log is out of scope.
    ///
    /// ## Derived edges
    ///
    /// Rule-created (derived) edges are **not** in the WAL and therefore do not appear in
    /// history. Only edges written directly by the application are recorded.
    ///
    /// ## Deleted nodes
    ///
    /// For nodes that have been deleted, dense-id records (SetPropId, InsertEdgeId) that
    /// predate the deletion may not resolve (the id is tombstoned in the live map). The
    /// string-keyed `DeleteNode` record still matches and produces a `NodeDeleted` entry.
    /// Prop/edge history of a deleted node may therefore be partially unresolvable.
    ///
    /// ## Dense-id edge entries and tombstoned partners
    ///
    /// Edge entries from dense-id WAL records (`InsertEdgeId`) are omitted when the partner
    /// endpoint's dense id is tombstoned. As a result, a live node's history can contain an
    /// `EdgeRemoved` (string-keyed, always resolves) without a corresponding `EdgeAdded`.
    pub fn node_history(&self, key: &str) -> Result<Vec<crate::history::HistoryEntry>> {
        use crate::history::{HistoryChange, HistoryEntry};
        use core_storage::wal::WalRecord;

        let bytes = self.fs.read(FileId::Wal)?;
        let (frames, _) = decode_all(&bytes);

        let mut out: Vec<HistoryEntry> = Vec::new();

        for (commit, frame) in frames.iter().enumerate() {
            let commit = commit as u64;
            // Collect the inner records to process — Batch is one commit, single records are one commit.
            let records: &[WalRecord] = match frame {
                WalRecord::Batch(inner) => inner.as_slice(),
                single => std::slice::from_ref(single),
            };

            for rec in records {
                let change = match rec {
                    WalRecord::InsertNode { label, key: k, .. } if k == key => {
                        Some(HistoryChange::NodeInserted {
                            label: label.clone(),
                        })
                    }
                    WalRecord::InsertNodeId { label, key: k, .. } if k == key => {
                        let label_str = match self.syms.resolve(*label) {
                            Some(s) => s.to_string(),
                            None => continue,
                        };
                        Some(HistoryChange::NodeInserted { label: label_str })
                    }
                    WalRecord::SetProp {
                        key: k,
                        field,
                        value,
                    } if k == key => Some(HistoryChange::PropSet {
                        field: field.clone(),
                        value: value.clone(),
                    }),
                    WalRecord::SetPropId { id, field, value } => match self.ids.key_of(*id) {
                        Some(resolved) if resolved == key => {
                            let field_str = match self.syms.resolve(*field) {
                                Some(s) => s.to_string(),
                                None => continue,
                            };
                            Some(HistoryChange::PropSet {
                                field: field_str,
                                value: value.clone(),
                            })
                        }
                        _ => None,
                    },
                    WalRecord::RemoveProp { key: k, field } if k == key => {
                        Some(HistoryChange::PropRemoved {
                            field: field.clone(),
                        })
                    }
                    WalRecord::InsertEdge {
                        edge_type,
                        src_key,
                        dst_key,
                    } => {
                        if src_key == key {
                            Some(HistoryChange::EdgeAdded {
                                edge_type: edge_type.clone(),
                                other: dst_key.clone(),
                                outgoing: true,
                            })
                        } else if dst_key == key {
                            Some(HistoryChange::EdgeAdded {
                                edge_type: edge_type.clone(),
                                other: src_key.clone(),
                                outgoing: false,
                            })
                        } else {
                            None
                        }
                    }
                    WalRecord::InsertEdgeId { etype, src, dst } => {
                        let etype_str = match self.syms.resolve(*etype) {
                            Some(s) => s.to_string(),
                            None => continue,
                        };
                        let src_key = self.ids.key_of(*src);
                        let dst_key = self.ids.key_of(*dst);
                        if src_key == Some(key) {
                            let other = match dst_key {
                                Some(s) => s.to_string(),
                                None => continue,
                            };
                            Some(HistoryChange::EdgeAdded {
                                edge_type: etype_str,
                                other,
                                outgoing: true,
                            })
                        } else if dst_key == Some(key) {
                            let other = match src_key {
                                Some(s) => s.to_string(),
                                None => continue,
                            };
                            Some(HistoryChange::EdgeAdded {
                                edge_type: etype_str,
                                other,
                                outgoing: false,
                            })
                        } else {
                            None
                        }
                    }
                    WalRecord::DeleteEdge {
                        edge_type,
                        src_key,
                        dst_key,
                    } => {
                        if src_key == key {
                            Some(HistoryChange::EdgeRemoved {
                                edge_type: edge_type.clone(),
                                other: dst_key.clone(),
                                outgoing: true,
                            })
                        } else if dst_key == key {
                            Some(HistoryChange::EdgeRemoved {
                                edge_type: edge_type.clone(),
                                other: src_key.clone(),
                                outgoing: false,
                            })
                        } else {
                            None
                        }
                    }
                    WalRecord::DeleteNode { key: k } if k == key => {
                        Some(HistoryChange::NodeDeleted)
                    }
                    // Skip: rule/view/fulltext/intern metadata; Batch wrapper handled above.
                    _ => None,
                };

                if let Some(change) = change {
                    out.push(HistoryEntry { commit, change });
                }
            }
        }

        Ok(out)
    }

    pub fn edge_count(&self) -> u64 {
        self.topo_view().edge_count()
    }

    /// Live/tombstone/edge counts plus per-rule provenance size, trip latch,
    /// and fire counter (includes rebuild evaluations). Rules are sorted by name.
    pub fn stats(&self) -> Stats {
        let rules: Vec<RuleStats> = self
            .engine
            .rules()
            .map(|r| RuleStats {
                name: r.name.clone(),
                edges: self
                    .engine
                    .provenance()
                    .get(&r.name)
                    .map(|s| s.len() as u64)
                    .unwrap_or(0),
                tripped: self.engine.is_tripped(&r.name),
                fires: self.engine.fire_count(&r.name),
                approximate: r.approximate,
            })
            .collect();
        Stats {
            nodes_live: self.ids.live_len(),
            nodes_tombstoned: self.ids.len() - self.ids.live_len(),
            edges: self.topo_view().edge_count(),
            rules,
        }
    }

    /// On-disk snapshot format version this binary writes and reads.
    pub fn format_version() -> u16 {
        core_storage::snapshot::VERSION
    }

    /// Test-support: total bytes appended (SimFs only usage).
    pub fn fs_total_appended(&self) -> usize
    where
        F: FsIntrospect,
    {
        self.fs.total_appended()
    }

    /// Test-support: successful `Fs::sync` calls (SimFs / counting fs).
    pub fn fs_sync_count(&self) -> usize
    where
        F: FsIntrospect,
    {
        self.fs.sync_count()
    }

    /// Consume the db, returning its fs (for crash simulation).
    pub fn into_fs(self) -> F {
        self.fs
    }

    pub fn snapshot(&mut self) -> Result<()> {
        self.snapshot_with(SnapshotOptions::default())
    }

    /// Snapshot with explicit options.
    ///
    /// # `keep_wal`
    ///
    /// When `keep_wal` is `false` (the default, same as [`snapshot`]):
    ///   - The WAL is replaced with a minimal baseline containing one
    ///     `EnableFulltext` record per active declaration.  All pre-snapshot
    ///     history is discarded; `open_at` can only reach post-snapshot commits.
    ///
    /// When `keep_wal` is `true`:
    ///   - The WAL is left intact.  All pre-snapshot commits remain reachable
    ///     via `open_at`.  The existing WAL already contains the original
    ///     `EnableFulltext` records, so no baseline re-write is needed; the
    ///     recovery guards in `apply()` silently skip any duplicate records on
    ///     replay.
    ///   - Crash window: a crash after the snapshot write but before the next
    ///     WAL write leaves the full pre-snapshot WAL intact.  On reopen the
    ///     snapshot is loaded and the WAL replayed idempotently over it — safe
    ///     because every `apply()` arm is idempotent when replayed over an
    ///     already-current snapshot.
    pub fn snapshot_with(&mut self, opts: SnapshotOptions) -> Result<()> {
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        let (rule_defs_typed, provenance, rule_tripped, rule_fires) = self.engine.to_persist();
        let rule_defs = rule_defs_typed
            .iter()
            .map(|r| bincode::serialize(r).expect("RuleDef serialize cannot fail"))
            .collect();
        // Collect IVF state for approximate rules (V4).
        let hnsw_state = self.engine.export_hnsw_state();
        let raw_ivf = self.engine.export_ivf_state();
        let ivf_state: BTreeMap<String, core_storage::snapshot::PerRuleIvfState> = raw_ivf
            .into_iter()
            .map(|(name, ((sc, sa, sd), (dc, da, dd)))| {
                (
                    name,
                    core_storage::snapshot::PerRuleIvfState {
                        src: core_storage::snapshot::SideIvfState {
                            centroids: sc,
                            clusters: sa,
                            drift: sd,
                        },
                        dst: core_storage::snapshot::SideIvfState {
                            centroids: dc,
                            clusters: da,
                            drift: dd,
                        },
                    },
                )
            })
            .collect();
        let view_defs: Vec<Vec<u8>> = self
            .view_store
            .views()
            .map(|v| bincode::serialize(v).expect("ViewDef serialize cannot fail"))
            .collect();
        if self.base.is_some() {
            // V8 merge-snapshot path: encode base+overlay into a new V8 snapshot,
            // write it atomically, remap it as the new base, then clear the overlay.
            let meta = V8Meta {
                labels: self.labels.clone(),
                edge_props: self.edge_props.clone(),
                rule_defs,
                provenance,
                rule_tripped,
                rule_fires,
                ivf_state,
                view_defs,
                wal_truncated: !opts.keep_wal,
                hnsw: hnsw_state,
            };
            let mut buf: Vec<u8> = Vec::new();
            {
                // Clone the Arc so the old base stays alive while we encode.
                // The borrow of archived_csr (into old_base's mmap) is released
                // at the end of this block, before we replace self.base.
                let old_base = self.base.clone().expect("is_some checked above");
                let archived_csr = old_base.topology().map_err(|e| GraphError::Corrupt {
                    detail: format!("v8 snapshot: topology section: {e:?}"),
                })?;
                let archived_cols = old_base.columns().map_err(|e| GraphError::Corrupt {
                    detail: format!("v8 snapshot: columns section: {e:?}"),
                })?;
                encode_v8(
                    Some(archived_csr),
                    Some(archived_cols),
                    &self.topo,
                    &self.props,
                    &self.ids,
                    &self.syms,
                    &meta,
                    &mut buf,
                )?;
            }
            self.fs.write_atomic(FileId::Snapshot, &buf)?;
            // Remap the freshly-written snapshot as the new base.
            // C2: use file mmap on RealFs; fall back to from_bytes on SimFs.
            let new_base = if let Some(snap_path) = self.fs.snapshot_path() {
                core_storage::v8::MappedBase::map(&snap_path)
            } else {
                core_storage::v8::MappedBase::from_bytes(buf)
            }
            .map_err(|e| GraphError::Corrupt {
                detail: format!("v8 snapshot: remap new base: {e:?}"),
            })?;
            self.base = Some(Arc::new(new_base));
            // Clear the overlay and prop tombstones — all data is now in the new base.
            self.topo = Topology::new();
            self.props = core_storage::columns::ColumnStore::new();
        } else {
            // Legacy path (V5–V7 stores without a V8 base).
            let state = core_storage::snapshot::SnapshotState {
                ids: self.ids.clone(),
                syms: self.syms.clone(),
                topo: self.topo.clone(),
                props: self.props.clone(),
                labels: self.labels.clone(),
                edge_props: self.edge_props.clone(),
                rule_defs,
                provenance,
                rule_tripped,
                rule_fires,
                ivf_state,
                hnsw_state,
                view_defs,
                wal_truncated: !opts.keep_wal,
            };
            self.fs
                .write_atomic(FileId::Snapshot, &core_storage::snapshot::encode(&state)?)?;
        }

        if opts.keep_wal {
            // keep_wal=true: WAL is left untouched.  The existing WAL already
            // contains the EnableFulltext records from the original enable calls;
            // replay is idempotent (guards in apply() skip already-live entries).
            // No baseline re-write is needed or safe here — the full WAL history
            // must remain intact for open_at to reach pre-snapshot commits.
        } else {
            // keep_wal=false (default): truncate by replacing the WAL with a
            // minimal baseline of one EnableFulltext record per active pair.
            //
            // Crash-ordering: write_atomic is atomic.
            //   • Crash before snapshot write  → WAL unchanged.  Safe.
            //   • Crash after snapshot write but before this WAL write → full
            //     pre-snapshot WAL still present; open_with replays idempotently.
            //   • Crash after both writes → normal post-snapshot state.
            let mut baseline_wal: Vec<u8> = Vec::new();
            for (label, field) in self.fulltext.enabled_pairs() {
                let rec = WalRecord::EnableFulltext {
                    label: label.clone(),
                    field: field.clone(),
                };
                baseline_wal.extend_from_slice(&encode_record(&rec));
            }
            self.fs.write_atomic(FileId::Wal, &baseline_wal)?;
        }
        Ok(())
    }
}

/// Queued mutation for a [`BatchBuilder`].
enum BatchOp {
    InsertNode {
        label: String,
        key: String,
        props: Vec<(String, Value)>,
    },
    InsertEdge {
        edge_type: String,
        src_key: String,
        dst_key: String,
    },
    SetProp {
        key: String,
        field: String,
        value: Value,
    },
    RemoveProp {
        key: String,
        field: String,
    },
    DeleteEdge {
        edge_type: String,
        src_key: String,
        dst_key: String,
    },
    DeleteNode {
        key: String,
    },
    CreateRule(RuleDef),
    DeleteRule {
        name: String,
    },
}

/// Overlay of ops already accepted earlier in the same batch. Never written
/// back to the database — validation only.
#[derive(Default)]
struct Overlay {
    extra_keys: BTreeSet<String>,
    deleted_keys: BTreeSet<String>,
    extra_props: BTreeMap<(String, String), Value>,
    removed_props: BTreeSet<(String, String)>,
    extra_edges: BTreeSet<(String, String, String)>,
    deleted_edges: BTreeSet<(String, String, String)>,
    extra_rules: BTreeSet<String>,
    deleted_rules: BTreeSet<String>,
}

/// Read-only view of live db state plus a batch overlay. Shared by single-op
/// public methods (empty overlay) and `commit_batch`.
struct MutPreview<'a, F: Fs> {
    db: &'a GraphDb<F>,
    overlay: Overlay,
}

impl<'a, F: Fs> MutPreview<'a, F> {
    fn new(db: &'a GraphDb<F>) -> Self {
        Self {
            db,
            overlay: Overlay::default(),
        }
    }

    fn has_key(&self, key: &str) -> bool {
        if self.overlay.extra_keys.contains(key) {
            return true;
        }
        if self.overlay.deleted_keys.contains(key) {
            return false;
        }
        self.db.ids.get(key).is_some()
    }

    fn has_prop(&self, key: &str, field: &str) -> bool {
        if !self.has_key(key) {
            return false;
        }
        let k = (key.to_string(), field.to_string());
        if self.overlay.removed_props.contains(&k) {
            return false;
        }
        if self.overlay.extra_props.contains_key(&k) {
            return true;
        }
        // Fresh identity (first insert in this batch, or delete+reinsert):
        // ignore props still sitting on the soon-to-be-tombstoned slot.
        if self.overlay.extra_keys.contains(key) {
            return false;
        }
        self.db.get_prop(key, field).is_some()
    }

    fn has_edge(&self, edge_type: &str, src_key: &str, dst_key: &str) -> bool {
        let k = (
            edge_type.to_string(),
            src_key.to_string(),
            dst_key.to_string(),
        );
        if self.overlay.deleted_edges.contains(&k) {
            return false;
        }
        if self.overlay.extra_edges.contains(&k) {
            return true;
        }
        // A key created in this batch (including reinsert) has no db edges.
        if self.overlay.extra_keys.contains(src_key) || self.overlay.extra_keys.contains(dst_key) {
            return false;
        }
        if self.overlay.deleted_keys.contains(src_key)
            || self.overlay.deleted_keys.contains(dst_key)
        {
            return false;
        }
        let Some(src) = self.db.ids.get(src_key) else {
            return false;
        };
        let Some(dst) = self.db.ids.get(dst_key) else {
            return false;
        };
        let Some(sym) = self.db.syms.get(edge_type) else {
            return false;
        };
        self.db
            .topo_view()
            .neighbors(sym, Direction::Out, src)
            .binary_search(&dst)
            .is_ok()
    }

    fn has_rule(&self, name: &str) -> bool {
        if self.overlay.extra_rules.contains(name) {
            return true;
        }
        if self.overlay.deleted_rules.contains(name) {
            return false;
        }
        self.db.engine.rules().any(|r| r.name == name)
    }

    fn is_rule_owned(&self, edge_type: &str, src_key: &str, dst_key: &str) -> bool {
        if self.overlay.extra_keys.contains(src_key) || self.overlay.extra_keys.contains(dst_key) {
            return false;
        }
        if self.overlay.deleted_keys.contains(src_key)
            || self.overlay.deleted_keys.contains(dst_key)
        {
            return false;
        }
        let Some(src) = self.db.ids.get(src_key) else {
            return false;
        };
        let Some(dst) = self.db.ids.get(dst_key) else {
            return false;
        };
        let Some(et) = self.db.syms.get(edge_type) else {
            return false;
        };
        // extra_rules is deliberately not consulted: a CreateRule earlier in
        // this batch has not fired, so it contributes no provenance. That is
        // the documented rule-window gap (see GraphDb::batch).
        if self.overlay.deleted_rules.is_empty() {
            return self.db.engine.is_owned(et, src, dst);
        }
        for (rule, triples) in self.db.engine.provenance() {
            if self.overlay.deleted_rules.contains(rule) {
                continue;
            }
            if triples.contains(&(et, src, dst)) {
                return true;
            }
        }
        false
    }

    fn check_insert_node(&self, key: &str) -> Result<()> {
        if self.has_key(key) {
            Err(GraphError::DuplicateKey { key: key.into() })
        } else {
            Ok(())
        }
    }

    fn check_live_key(&self, key: &str) -> Result<()> {
        if self.has_key(key) {
            Ok(())
        } else {
            Err(GraphError::KeyNotFound { key: key.into() })
        }
    }

    fn prepare_insert_edge(&self, edge_type: &str, src_key: &str, dst_key: &str) -> Result<bool> {
        for k in [src_key, dst_key] {
            if !self.has_key(k) {
                return Err(GraphError::KeyNotFound { key: k.into() });
            }
        }
        if self.is_rule_owned(edge_type, src_key, dst_key) {
            return Err(GraphError::RuleOwned {
                detail: format!("edge {edge_type} {src_key}→{dst_key} is rule-owned"),
            });
        }
        Ok(!self.has_edge(edge_type, src_key, dst_key))
    }

    fn prepare_remove_prop(&self, key: &str, field: &str) -> Result<bool> {
        self.check_live_key(key)?;
        Ok(self.has_prop(key, field))
    }

    fn prepare_delete_edge(&self, edge_type: &str, src_key: &str, dst_key: &str) -> Result<bool> {
        for k in [src_key, dst_key] {
            if !self.has_key(k) {
                return Err(GraphError::KeyNotFound { key: k.into() });
            }
        }
        // Provenance-owned OR a live rule would derive this pair. User-first
        // edges that a later rule matches are not in `owned`, but deleting
        // them would leave a hole `rebuild_rule` immediately fills.
        if self.is_rule_owned(edge_type, src_key, dst_key) {
            return Err(GraphError::RuleOwned {
                detail: format!(
                    "edge {edge_type} {src_key}→{dst_key} is rule-owned; \
                     delete or change the owning rule"
                ),
            });
        }
        if self.would_derive(edge_type, src_key, dst_key) {
            return Err(GraphError::RuleOwned {
                detail: format!(
                    "edge {edge_type} {src_key}→{dst_key} is rule-owned; \
                     delete or change the owning rule, or a live rule would re-derive it"
                ),
            });
        }
        Ok(self.has_edge(edge_type, src_key, dst_key))
    }

    /// True if any live rule (minus overlay-deleted names) would derive
    /// `(edge_type, src, dst)` from current overlay-visible props/labels.
    /// CreateRule names in `extra_rules` are ignored — same documented
    /// same-batch rule-window as [`Self::is_rule_owned`].
    fn would_derive(&self, edge_type: &str, src_key: &str, dst_key: &str) -> bool {
        if src_key == dst_key {
            return false;
        }
        let Some(src_label) = self.label_of(src_key) else {
            return false;
        };
        let Some(dst_label) = self.label_of(dst_key) else {
            return false;
        };
        for rule in self.db.engine.rules() {
            if self.overlay.deleted_rules.contains(&rule.name) {
                continue;
            }
            if rule.edge_type != edge_type {
                continue;
            }
            if rule.src_label != src_label || rule.dst_label != dst_label {
                continue;
            }
            let src_props = |f: &str| self.prop_value(src_key, f);
            let dst_props = |f: &str| self.prop_value(dst_key, f);
            let src_view = NodeView {
                key: src_key,
                props: &src_props,
            };
            let dst_view = NodeView {
                key: dst_key,
                props: &dst_props,
            };
            if evaluate(&rule.predicate, &src_view, &dst_view).is_some() {
                return true;
            }
        }
        false
    }

    fn label_of(&self, key: &str) -> Option<String> {
        if self.overlay.deleted_keys.contains(key) {
            return None;
        }
        // Fresh identities created in this batch have no stored label in the
        // overlay; they cannot be provenance-owned yet either.
        let id = self.db.ids.get(key)?;
        let sym = self.db.labels.get(id as usize).copied()?;
        if sym == u32::MAX {
            return None;
        }
        self.db.syms.resolve(sym).map(str::to_string)
    }

    fn prop_value(&self, key: &str, field: &str) -> Option<Value> {
        if !self.has_key(key) {
            return None;
        }
        let k = (key.to_string(), field.to_string());
        if self.overlay.removed_props.contains(&k) {
            return None;
        }
        if let Some(v) = self.overlay.extra_props.get(&k) {
            return Some(v.clone());
        }
        if self.overlay.extra_keys.contains(key) {
            return None;
        }
        self.db.get_prop(key, field)
    }

    fn check_create_rule(&self, def: &RuleDef) -> Result<()> {
        def.validate()
            .map_err(|e| GraphError::RuleInvalid { detail: e })?;
        if self.has_rule(&def.name) {
            return Err(GraphError::RuleInvalid {
                detail: format!("rule {:?} already exists", def.name),
            });
        }
        Ok(())
    }

    fn check_delete_rule(&self, name: &str) -> Result<()> {
        if self.has_rule(name) {
            Ok(())
        } else {
            Err(GraphError::RuleNotFound { name: name.into() })
        }
    }

    fn note_insert_node(&mut self, key: &str, props: &[(String, Value)]) {
        self.overlay.deleted_keys.remove(key);
        self.overlay.extra_keys.insert(key.to_string());
        self.overlay.extra_props.retain(|(k, _), _| k != key);
        self.overlay.removed_props.retain(|(k, _)| k != key);
        for (field, value) in props {
            self.overlay
                .extra_props
                .insert((key.to_string(), field.clone()), value.clone());
        }
    }

    fn note_insert_edge(&mut self, edge_type: &str, src_key: &str, dst_key: &str) {
        let k = (
            edge_type.to_string(),
            src_key.to_string(),
            dst_key.to_string(),
        );
        self.overlay.deleted_edges.remove(&k);
        self.overlay.extra_edges.insert(k);
    }

    fn note_set_prop(&mut self, key: &str, field: &str, value: &Value) {
        let k = (key.to_string(), field.to_string());
        self.overlay.removed_props.remove(&k);
        self.overlay.extra_props.insert(k, value.clone());
    }

    fn note_remove_prop(&mut self, key: &str, field: &str) {
        let k = (key.to_string(), field.to_string());
        self.overlay.extra_props.remove(&k);
        self.overlay.removed_props.insert(k);
    }

    fn note_delete_edge(&mut self, edge_type: &str, src_key: &str, dst_key: &str) {
        let k = (
            edge_type.to_string(),
            src_key.to_string(),
            dst_key.to_string(),
        );
        self.overlay.extra_edges.remove(&k);
        self.overlay.deleted_edges.insert(k);
    }

    fn note_delete_node(&mut self, key: &str) {
        self.overlay.extra_keys.remove(key);
        self.overlay.deleted_keys.insert(key.to_string());
        self.overlay.extra_props.retain(|(k, _), _| k != key);
        self.overlay.removed_props.retain(|(k, _)| k != key);
        self.overlay
            .extra_edges
            .retain(|(_, s, d)| s != key && d != key);
        self.overlay
            .deleted_edges
            .retain(|(_, s, d)| s != key && d != key);
    }

    fn note_create_rule(&mut self, name: &str) {
        self.overlay.deleted_rules.remove(name);
        self.overlay.extra_rules.insert(name.to_string());
    }

    fn note_delete_rule(&mut self, name: &str) {
        self.overlay.extra_rules.remove(name);
        self.overlay.deleted_rules.insert(name.to_string());
        // Treat the deleted rule's current provenance as gone so a later
        // delete_edge of those triples is a no-op (matches sequential).
        if let Some(triples) = self.db.engine.provenance().get(name) {
            for &(et, s, d) in triples {
                let Some(etype) = self.db.syms.resolve(et) else {
                    continue;
                };
                let Some(src) = self.db.ids.key_of(s) else {
                    continue;
                };
                let Some(dst) = self.db.ids.key_of(d) else {
                    continue;
                };
                let k = (etype.to_string(), src.to_string(), dst.to_string());
                self.overlay.extra_edges.remove(&k);
                self.overlay.deleted_edges.insert(k);
            }
        }
    }
}

/// Collects mutations and commits them as one WAL `Batch` frame.
///
/// Holds `&mut GraphDb` for its lifetime. Queue with the same method names
/// as [`GraphDb`]; call [`commit`](Self::commit) to validate, log, and apply.
/// See [`GraphDb::batch`] for validation and atomicity rules.
pub struct BatchBuilder<'a, F: Fs> {
    db: &'a mut GraphDb<F>,
    ops: Vec<BatchOp>,
}

impl<'a, F: Fs> BatchBuilder<'a, F> {
    pub fn insert_node(
        &mut self,
        label: &str,
        key: &str,
        props: Vec<(String, Value)>,
    ) -> &mut Self {
        self.ops.push(BatchOp::InsertNode {
            label: label.into(),
            key: key.into(),
            props,
        });
        self
    }

    pub fn insert_edge(&mut self, edge_type: &str, src_key: &str, dst_key: &str) -> &mut Self {
        self.ops.push(BatchOp::InsertEdge {
            edge_type: edge_type.into(),
            src_key: src_key.into(),
            dst_key: dst_key.into(),
        });
        self
    }

    pub fn set_prop(&mut self, key: &str, field: &str, value: Value) -> &mut Self {
        self.ops.push(BatchOp::SetProp {
            key: key.into(),
            field: field.into(),
            value,
        });
        self
    }

    pub fn remove_prop(&mut self, key: &str, field: &str) -> &mut Self {
        self.ops.push(BatchOp::RemoveProp {
            key: key.into(),
            field: field.into(),
        });
        self
    }

    pub fn delete_edge(&mut self, edge_type: &str, src_key: &str, dst_key: &str) -> &mut Self {
        self.ops.push(BatchOp::DeleteEdge {
            edge_type: edge_type.into(),
            src_key: src_key.into(),
            dst_key: dst_key.into(),
        });
        self
    }

    pub fn delete_node(&mut self, key: &str) -> &mut Self {
        self.ops.push(BatchOp::DeleteNode { key: key.into() });
        self
    }

    pub fn create_rule(&mut self, def: RuleDef) -> &mut Self {
        self.ops.push(BatchOp::CreateRule(def));
        self
    }

    pub fn delete_rule(&mut self, name: &str) -> &mut Self {
        self.ops.push(BatchOp::DeleteRule { name: name.into() });
        self
    }

    /// Validate every queued op, then log one `Batch` frame and apply.
    /// Empty / all-noop batches return `Ok(())` without writing the WAL.
    /// A second `commit()` after a successful one is an empty-batch no-op
    /// (queued ops were taken).
    /// Takes `&mut self` so it chains after the queue methods (`b.insert_node(..).commit()`)
    /// and also works as `let mut b = db.batch(); b.insert_node(..); b.commit()`.
    ///
    /// **Rule-window limitation:** batch validation cannot see edges that a
    /// rule created earlier in the *same* batch will derive at apply time, so
    /// a `delete_edge` / `insert_edge` in that window is silently no-oped
    /// where sequential calls would return `Err(RuleOwned)`. State integrity
    /// is unaffected (idempotent apply, provenance intact). Create rules in
    /// their own batch, or sequentially, when later ops may touch derived
    /// edges.
    /// Validate every queued op and commit atomically.
    ///
    /// Returns `(nodes_inserted, edges_inserted)` — the counts of node and edge
    /// WAL records actually written (duplicate edges are silent no-ops and are
    /// NOT counted). Both are 0 when the batch is empty or all-noop.
    pub fn commit(&mut self) -> Result<(usize, usize)> {
        let ops = std::mem::take(&mut self.ops);
        self.db.commit_batch(ops)
    }

    /// Same as [`commit`](Self::commit) but tail the inner events with
    /// [`MutationEvent::Ingested`] instead of [`MutationEvent::BatchApplied`].
    pub(crate) fn commit_ingest(&mut self, label: &str, inserted: usize) -> Result<(usize, usize)> {
        let ops = std::mem::take(&mut self.ops);
        self.db
            .commit_logged_batch(ops, Some((label.to_string(), inserted)))
    }
}

pub struct NodeRef<'a, F: Fs> {
    db: &'a GraphDb<F>,
    id: u32,
}

impl<'a, F: Fs> NodeRef<'a, F> {
    pub fn key(&self) -> &str {
        self.db.ids.key_of(self.id).expect("dense ids")
    }

    pub fn label(&self) -> &str {
        let sym = self
            .db
            .labels
            .get(self.id as usize)
            .copied()
            .filter(|&s| s != u32::MAX)
            .expect("real nodes always have a label; u32::MAX sentinel cannot occur");
        self.db.syms.resolve(sym).expect("interned label symbol")
    }

    pub fn prop(&self, field: &str) -> Option<Value> {
        self.db
            .props_view()
            .get(self.id, field)
            .map(|vr| vr.into_value())
    }

    /// All stored fields for this node, sorted by field name.
    ///
    /// Reads from the full base+overlay view so that props stored only in the
    /// V8 snapshot base (i.e. before any post-snapshot WAL writes) are visible.
    pub fn props(&self) -> BTreeMap<String, Value> {
        let mut out = BTreeMap::new();
        let pv = self.db.props_view();
        for field in pv.field_names() {
            if let Some(vr) = pv.get(self.id, &field) {
                out.insert(field, vr.into_value());
            }
        }
        out
    }

    /// depth-N BFS as a ResultSet: columns ["key","label","depth"], BFS order.
    pub fn neighborhood(&self, depth: u32, edge_types: Option<&[&str]>, dir: Dir) -> ResultSet {
        let view = self.db.view();
        let resolved: Option<Vec<u32>> = edge_types.map(|names| {
            names
                .iter()
                .filter_map(|name| view.syms.get(name))
                .collect()
        });
        let nb = neighborhood(&view, self.id, depth, resolved.as_deref(), dir);
        let mut rs = ResultSet::new(vec!["key".into(), "label".into(), "depth".into()]);
        for (nid, d) in nb.nodes {
            let key = view.key_of(nid);
            let label = view
                .label_of(nid)
                .expect("real nodes always have a label; u32::MAX sentinel cannot occur");
            rs.push_row(vec![
                Some(Value::Str(key.to_string())),
                Some(Value::Str(label.to_string())),
                Some(Value::Int(d as i64)),
            ]);
        }
        rs
    }

    /// 1-hop, Both directions: edge-type name → sorted unique neighbor keys.
    pub fn grouped_by_edge_type(&self) -> BTreeMap<String, Vec<String>> {
        let view = self.db.view();
        let mut groups: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for e in expand(&view, self.id, None, Dir::Both) {
            let etype = view
                .syms
                .resolve(e.etype)
                .expect("topology etype is interned")
                .to_string();
            let nbr = if e.src == self.id { e.dst } else { e.src };
            groups
                .entry(etype)
                .or_default()
                .insert(view.key_of(nbr).to_string());
        }
        groups
            .into_iter()
            .map(|(k, v)| (k, v.into_iter().collect()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_rules::Predicate;

    fn tmp_dir(name: &str) -> std::path::PathBuf {
        let d =
            std::env::temp_dir().join(format!("graphdb-db-unit-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn fk_rule() -> RuleDef {
        RuleDef {
            name: "works_at".into(),
            src_label: "Person".into(),
            dst_label: "Org".into(),
            predicate: Predicate::KeyMatch {
                field: "org_id".into(),
            },
            edge_type: "WORKS_AT".into(),
            weight_prop: None,
            max_edges: None,
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
        }
    }

    /// Regression guard for the no-views delta-copy fast path.
    ///
    /// When no views are defined, `pending_deltas_since().to_vec()` must never
    /// be called — even during a large CreateRule backfill. The DELTA_COPY_COUNT
    /// thread-local is incremented inside every `if !view_store.is_empty()` block;
    /// a count of 0 after the entire sequence proves the guard fires correctly.
    #[test]
    fn no_delta_copy_when_no_views() {
        DELTA_COPY_COUNT.with(|c| c.set(0));
        let dir = tmp_dir("no-delta-copy");
        {
            let mut db = GraphDb::open(&dir).unwrap();
            // Insert 50 Org + 50 Person nodes with FK links.
            for i in 0..50u32 {
                db.insert_node("Org", &format!("o{i}"), vec![]).unwrap();
            }
            for i in 0..50u32 {
                db.insert_node(
                    "Person",
                    &format!("p{i}"),
                    vec![("org_id".into(), Value::Str(format!("o{i}")))],
                )
                .unwrap();
            }
            // CreateRule backfill should NOT invoke to_vec() when no views are defined.
            db.create_rule(fk_rule()).unwrap();

            // Counter must stay 0 — no views, no copies.
            let copies = DELTA_COPY_COUNT.with(|c| c.get());
            assert_eq!(
                copies, 0,
                "pending_deltas_since().to_vec() called despite no views"
            );

            // Derived edges must still be correct (the guard skips only the
            // empty delta propagation loop, not the rule application itself).
            let nbrs = db.neighbors("p0", "WORKS_AT", Direction::Out).unwrap();
            assert_eq!(
                nbrs,
                vec!["o0"],
                "rule must derive edges even with no views"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Gating regression: subscribe AFTER a backfill must see no stale events.
    /// subscribe BEFORE a backfill must see every edge-fire event.
    #[test]
    fn subscribe_after_backfill_no_stale_events() {
        let dir = tmp_dir("sub-after-backfill");
        {
            let mut db = GraphDb::open(&dir).unwrap();
            for i in 0..10u32 {
                db.insert_node("Org", &format!("o{i}"), vec![]).unwrap();
                db.insert_node(
                    "Person",
                    &format!("p{i}"),
                    vec![("org_id".into(), Value::Str(format!("o{i}")))],
                )
                .unwrap();
            }
            // Create rule BEFORE subscribing — emit_deltas is false during backfill.
            db.create_rule(fk_rule()).unwrap();

            // Subscribe AFTER the backfill — queue must be empty (no stale events).
            let sub = db.subscribe_all_rules().unwrap();
            // No events should have queued for the prior backfill.
            assert!(
                sub.try_recv().is_none(),
                "subscribe after backfill must see no stale events"
            );

            // Inserting a new node now should fire an event (emit_deltas is now true).
            db.insert_node("Org", "o_new", vec![]).unwrap();
            db.insert_node(
                "Person",
                "p_new",
                vec![("org_id".into(), Value::Str("o_new".into()))],
            )
            .unwrap();
            let ev = sub.recv_timeout(std::time::Duration::from_millis(200));
            assert!(
                ev.is_some(),
                "edge-fire event must arrive after subscribe (emit_deltas=true)"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Gating regression: subscribe BEFORE a backfill → events flow.
    #[test]
    fn subscribe_before_backfill_events_flow() {
        let dir = tmp_dir("sub-before-backfill");
        {
            let mut db = GraphDb::open(&dir).unwrap();
            // Subscribe FIRST — emit_deltas becomes true.
            let sub = db.subscribe_all_rules().unwrap();

            for i in 0..5u32 {
                db.insert_node("Org", &format!("o{i}"), vec![]).unwrap();
                db.insert_node(
                    "Person",
                    &format!("p{i}"),
                    vec![("org_id".into(), Value::Str(format!("o{i}")))],
                )
                .unwrap();
            }
            // Backfill fires with emit_deltas=true → events queued.
            db.create_rule(fk_rule()).unwrap();

            // Should receive at least one edge-fired event from the backfill.
            let mut received = 0usize;
            while sub.try_recv().is_some() {
                received += 1;
            }
            assert!(
                received > 0,
                "subscribe before backfill must receive edge-fire events (got 0)"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Companion: when a view IS defined, the delta path fires and view values update.
    #[test]
    fn delta_copy_fires_when_view_exists() {
        use core_rules::ViewSource;
        DELTA_COPY_COUNT.with(|c| c.set(0));
        let dir = tmp_dir("delta-copy-with-view");
        {
            let mut db = GraphDb::open(&dir).unwrap();
            db.insert_node("Org", "o1", vec![]).unwrap();
            db.insert_node(
                "Person",
                "p1",
                vec![("org_id".into(), Value::Str("o1".into()))],
            )
            .unwrap();
            // Declare a Degree view so is_empty() returns false.
            db.create_view(ViewDef {
                name: "degree_out".into(),
                label: "Person".into(),
                view_prop: "degree_out".into(),
                source: ViewSource::Degree {
                    edge_type: "WORKS_AT".into(),
                    direction: Direction::Out,
                },
            })
            .unwrap();
            db.create_rule(fk_rule()).unwrap();

            // At least one delta copy should have happened (CreateRule backfill).
            let copies = DELTA_COPY_COUNT.with(|c| c.get());
            assert!(
                copies > 0,
                "expected delta copy to fire when a view is defined"
            );

            // View value should be computed: p1 has one WORKS_AT out-edge.
            let info = db.node_info("p1").unwrap();
            let degree = info.props.get("degree_out");
            assert!(
                degree.is_some(),
                "view prop should be written to node props"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression: `open_at_with` must call `rebuild_all` after WAL replay so
    /// derived-edge-driven view values reflect the as-of state rather than just
    /// the initial backfill written at `CreateView` time.
    ///
    /// History (6 WAL frames, indices 0..=5):
    ///   0: insert Org "o1"
    ///   1: create_view "employee_count" (Degree / WORKS_AT / In) on Org
    ///   2: create_rule fk_rule (WORKS_AT, Person→Org via org_id)
    ///   3: insert Person "p1" → rule fires WORKS_AT p1→o1 (degree = 1)  ← mid
    ///   4: insert Person "p2" → rule fires WORKS_AT p2→o1 (degree = 2)
    ///   5: insert Person "p3" → rule fires WORKS_AT p3→o1 (degree = 3)  ← latest
    ///
    /// Without `rebuild_all`, the as-of instance's "emp" view stays at the
    /// initial backfill value (0) instead of reflecting the replayed derived edges.
    #[test]
    fn open_at_derived_edge_view_values_correct() {
        use core_rules::ViewSource;
        let dir = tmp_dir("open-at-view-rebuild");
        {
            let mut db = GraphDb::open(&dir).unwrap();
            // frame 0
            db.insert_node("Org", "o1", vec![]).unwrap();
            // frame 1: create view — initial backfill sees 0 derived edges (none fired yet)
            db.create_view(ViewDef {
                name: "employee_count".into(),
                label: "Org".into(),
                view_prop: "emp".into(),
                source: ViewSource::Degree {
                    edge_type: "WORKS_AT".into(),
                    direction: Direction::In,
                },
            })
            .unwrap();
            // frame 2: create rule — no Persons yet; backfill is a no-op
            db.create_rule(fk_rule()).unwrap();
            // frame 3: p1 — rule fires WORKS_AT p1→o1; degree = 1
            db.insert_node(
                "Person",
                "p1",
                vec![("org_id".into(), Value::Str("o1".into()))],
            )
            .unwrap();
            // frame 4: p2 — degree = 2
            db.insert_node(
                "Person",
                "p2",
                vec![("org_id".into(), Value::Str("o1".into()))],
            )
            .unwrap();
            // frame 5: p3 — degree = 3
            db.insert_node(
                "Person",
                "p3",
                vec![("org_id".into(), Value::Str("o1".into()))],
            )
            .unwrap();
            // Sanity: normal open sees degree = 3.
            assert_eq!(
                db.get_view_prop("o1", "emp"),
                Some(Value::Int(3)),
                "normal db must show degree 3 after 3 derived edges"
            );
        } // WAL flushed

        // Re-open normally to get the authoritative reference value.
        let normal_db = GraphDb::open(&dir).unwrap();
        let normal_emp = normal_db.get_view_prop("o1", "emp");
        assert_eq!(
            normal_emp,
            Some(Value::Int(3)),
            "re-opened normal db must show degree 3"
        );

        // Latest as-of (commit 5 = frames 0..=5): must match the normal open.
        let aof_latest = GraphDb::open_at(&dir, 5).unwrap();
        assert_eq!(
            aof_latest.get_view_prop("o1", "emp"),
            normal_emp,
            "open_at latest: derived-edge view must equal normal open (rebuild_all required)"
        );

        // Mid-history as-of (commit 3 = frames 0..=3): only p1; degree = 1.
        let aof_mid = GraphDb::open_at(&dir, 3).unwrap();
        assert_eq!(
            aof_mid.get_view_prop("o1", "emp"),
            Some(Value::Int(1)),
            "open_at mid-history: only p1 exists at frame 3, degree must be 1"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Pin: subscribe_* on an as-of instance must return Err(ReadOnly) —
    /// as-of instances never commit, so distribute_events never runs and any
    /// subscription would wait forever.
    #[test]
    fn subscribe_on_as_of_returns_read_only_error() {
        let dir = tmp_dir("sub-as-of-read-only");
        {
            let mut db = GraphDb::open(&dir).unwrap();
            db.insert_node("Org", "o1", vec![]).unwrap();
            db.create_rule(fk_rule()).unwrap();
        }
        let mut aof = GraphDb::open_at(&dir, 0).unwrap();

        assert!(
            matches!(
                aof.subscribe_all_rules(),
                Err(core_storage::GraphError::ReadOnly)
            ),
            "subscribe_all_rules on as-of must return ReadOnly"
        );
        assert!(
            matches!(
                aof.subscribe_writes(),
                Err(core_storage::GraphError::ReadOnly)
            ),
            "subscribe_writes on as-of must return ReadOnly"
        );
        assert!(
            matches!(
                aof.subscribe_rule("works_at"),
                Err(core_storage::GraphError::ReadOnly)
            ),
            "subscribe_rule on as-of must return ReadOnly"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression: a failed dense WAL rewrite must not leave speculative
    /// interns in `syms`. If it does, the next successful mutation logs an
    /// `Intern` record with an inflated id; replay (which never saw the
    /// orphans) assigns a smaller id and the WAL becomes unreplayable.
    #[test]
    fn dense_rewrite_error_rolls_back_speculative_interns() {
        let dir = tmp_dir("dense-rewrite-rollback");
        {
            let mut db = GraphDb::open(&dir).unwrap();
            db.insert_node("Person", "a", vec![]).unwrap();

            // Bypass MutPreview validation to hit the rewrite's own error path
            // (same shape as an id-exhaustion failure mid-rewrite). The
            // InsertEdge arm interns the edge type before it resolves keys.
            let err = db.rewrite_wal_dense(vec![WalRecord::InsertEdge {
                edge_type: "ORPHAN_TYPE".into(),
                src_key: "missing".into(),
                dst_key: "a".into(),
            }]);
            assert!(err.is_err(), "rewrite of a missing src key must fail");
            assert_eq!(
                db.syms.get("ORPHAN_TYPE"),
                None,
                "failed rewrite must roll back speculative interns"
            );

            // A later successful mutation must produce a replayable WAL.
            db.set_prop("a", "later_field", Value::Int(2)).unwrap();
        }
        let db = GraphDb::open(&dir).expect("WAL must replay after failed rewrite");
        assert_eq!(db.get_prop("a", "later_field"), Some(Value::Int(2)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
