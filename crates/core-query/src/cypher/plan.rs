//! Cypher logical planner: `Query` → `Vec<PlanOp>`.
//!
//! Pure (no `GraphView`). Never panics. Bound-destination handling lives on
//! `Expand` (see `PlanOp::Expand`); `JoinBound` is only emitted for the
//! *start* node of a MATCH whose variable is already bound.

use super::ast::{
    AggArg, AggFunc, Expr, LimitSkip, NodePat, Operand, OptionalClause, OrderItem, OrderTarget,
    Pattern, Query, RelDir, RelPat, RetItem, RetVal, UnwindExpr, WithStage,
};
use std::collections::BTreeSet;

/// One operator in the logical plan. Patterns compile left-to-right into
/// scan / join / expand ops; WHERE is a single `Filter`; then `Project`,
/// rewritten `OrderBy`, `Skip`, `Limit` in that order.
#[derive(Debug, Clone, PartialEq)]
pub enum PlanOp {
    /// Seed rows from all nodes, or those with `label`.
    ScanLabel {
        var: String,
        label: Option<String>,
    },
    /// Point lookup by IdMap key. Emitted when a MATCH node property map is
    /// exactly one equality on field `id` (mixed maps stay ScanLabel+LookupProps).
    ScanKey {
        var: String,
        key: Operand,
        label: Option<String>,
    },
    /// Retain rows whose `var` node matches the pattern-map props.
    LookupProps {
        var: String,
        props: Vec<(String, Operand)>,
    },
    /// Expand from `from` along `etype`/`dir`, binding `rel_var` and `to`.
    ///
    /// Destination label/prop checks ride on this op (`to_label` / `to_props`).
    /// If `to` is already bound in the row, the executor keeps only edges
    /// that land on that bound id (JoinBound semantics *inside* Expand).
    /// `rel_var` is always `Some` after planning: user name or `_rN`.
    Expand {
        from: String,
        rel_var: Option<String>,
        etype: Option<String>,
        dir: RelDir,
        to: String,
        to_label: Option<String>,
        to_props: Vec<(String, Operand)>,
    },
    /// Pattern-*start* node whose var is already bound: label/prop re-check only.
    JoinBound {
        var: String,
        label: Option<String>,
        props: Vec<(String, Operand)>,
    },
    Filter {
        expr: Expr,
    },
    Project {
        items: Vec<RetItem>,
    },
    /// Deduplicate projected rows (`RETURN DISTINCT`). Hashed with the same
    /// numeric unify as `GroupAggregate`. Caps distinct rows at the
    /// intermediate-row budget.
    Distinct,
    /// After `plan`, every item's `target` is `OrderTarget::Alias(column)`
    /// where `column` is a projected column name. The executor resolves
    /// ORDER BY against the post-Project table only.
    OrderBy {
        items: Vec<OrderItem>,
    },
    Skip(LimitSkip),
    Limit(LimitSkip),
    /// Single aggregate over all matched rows (no grouping).
    ///
    /// Execution routes to a streaming accumulator path (O(1) memory).
    /// The 1 M intermediate-row budget does **not** apply: the accumulator
    /// holds a single running value regardless of how many source rows exist.
    ///
    /// Null/non-numeric values in `arg` are silently skipped for SUM/AVG/MIN/MAX.
    Aggregate {
        func: AggFunc,
        arg: AggArg,
        /// Projected column name — alias if provided, else the canonical
        /// function call string (`COUNT(*)`, `SUM(n.age)`, etc.).
        column: String,
    },
    /// Grouped aggregation: one or more group-key items and one or more aggregate
    /// functions, computed per distinct group.
    ///
    /// The executor streams through all matching rows, computing one
    /// `Option<ValueKey>` per group-key item; `None` represents a null value,
    /// and null keys group together (openCypher semantics).
    ///
    /// Group count is capped at 1,000,000; exceeding the cap is an error.
    ///
    /// `ORDER BY` / `SKIP` / `LIMIT` ops that follow in the plan apply to the
    /// finished group table (sort the groups, then slice).  `row_bound()` always
    /// returns `None` for plans containing this op so that LIMIT is never pushed
    /// into producers.
    GroupAggregate {
        /// Non-aggregate RETURN items: `(projected_column_name, ret_item)`.
        keys: Vec<(String, RetItem)>,
        /// Aggregate RETURN items: `(func, arg, projected_column_name)`.
        aggs: Vec<(AggFunc, AggArg, String)>,
    },
    /// Variable-length path expansion: BFS from `from`, emitting one row per
    /// (start, end, depth) path found with `min ≤ depth ≤ max`.
    ///
    /// Per-path edge-uniqueness (Cypher relationship isomorphism): a single
    /// path may not reuse the same edge (`EdgeRef`) twice; node revisits ARE
    /// allowed.  `rel_var`, when present, is bound to a virtual path cell
    /// whose sole accessible property is `length` (hop count as `Int`).
    ///
    /// Always executes via the **staged path** regardless of LIMIT.
    /// The 1 M intermediate-row budget applies to the output row count.
    VarExpand {
        from: String,
        rel_var: Option<String>,
        etype: Option<String>,
        dir: RelDir,
        to: String,
        min: u8,
        max: u8,
    },
    /// Shortest path between two already-bound nodes via BFS.
    ///
    /// Both `from` and `to` must be bound in the current row before this op
    /// executes.  BFS terminates at the first depth where `to` is reached.
    /// If `to` is unreachable within `max_hops`, zero rows are emitted.
    /// Exactly one row is emitted when a path exists.
    ///
    /// `rel_var`, when present, binds to a virtual path cell; `r.length`
    /// yields the hop count as `Int`.
    ///
    /// Always executes via the **staged path** regardless of LIMIT.
    ShortestPath {
        from: String,
        rel_var: Option<String>,
        etype: Option<String>,
        dir: RelDir,
        to: String,
        max_hops: u8,
    },
    /// Non-aggregate WITH: apply filter / order / skip / limit to the current
    /// row set without projecting. Node bindings in the row survive as-is so
    /// that subsequent MATCH clauses can join against them.
    ///
    /// Always executes via the **staged path** (row_bound returns None for any
    /// plan containing this op).
    With {
        items: Vec<RetItem>,
        where_expr: Option<Expr>,
        order_by: Vec<OrderItem>,
        skip: Option<LimitSkip>,
        limit: Option<LimitSkip>,
    },
    /// UNWIND: expand each input row into N rows by iterating a list value.
    ///
    /// - `list: UnwindExpr::Lit(v)` → use a literal list.
    /// - `list: UnwindExpr::Prop { var, field }` → resolve the list from a node property.
    /// - `list: UnwindExpr::Var(name)` → look up a scalar binding from a prior WITH.
    ///
    /// null / empty list → 0 output rows (openCypher).
    /// Non-list → named error at execution time.
    ///
    /// Always executes via the **staged path** (row_bound returns None).
    Unwind {
        expr: UnwindExpr,
        alias: String,
    },
    /// OPTIONAL MATCH: left-outer-join semantics.
    ///
    /// For each input row the `inner` plan is executed in isolation.  If the
    /// inner plan produces at least one output row, those rows replace the
    /// input row (inner join semantics for the rows that match).  If the inner
    /// plan produces **zero** rows, the input row survives with every variable
    /// listed in `optional_vars` set to null (left-outer fallback).
    ///
    /// `optional_vars` lists the variables that are introduced inside the
    /// optional pattern (i.e., the variables that must be nulled when the
    /// pattern fails).  Variables that were already bound before the optional
    /// clause are not listed here — they continue to hold their original values
    /// in the null row.
    ///
    /// Always executes via the **staged path** (row_bound returns None).
    LeftOuterApply {
        inner: Vec<PlanOp>,
        optional_vars: Vec<String>,
    },
}

/// Compute the effective row bound for LIMIT push-down.
///
/// Returns `Some(SKIP + LIMIT)` when the plan can terminate producers early —
/// that is, when the plan contains a `Limit` op **and no `OrderBy`** op.
/// An `OrderBy` requires full materialisation before slicing, so the bound
/// cannot be pushed past it.
///
/// Returns `None` when:
/// - No `Limit` op is present, or
/// - An `OrderBy` op is present (sorting needs every row).
///
/// # Decision table
///
/// | Plan shape (tail before Project)           | push-down? | note                                         |
/// |--------------------------------------------|------------|----------------------------------------------|
/// | Scan / Expand → Project → Limit            | YES        | pull-based; all stages stop at bound         |
/// | Scan / Expand → Filter → Project → Limit   | YES        | pull-based; Filter + earlier stages all stop |
/// | … → OrderBy → … Limit                      | NO         | sort requires all rows first                 |
///
/// When `row_bound` returns `Some`, the executor uses a demand-driven
/// (pull-based) strategy: **all** producer stages (Scan, Expand, Filter, …)
/// terminate as soon as `bound` final rows have been collected.  No
/// intermediate table is ever fully materialised for the bounded path.
///
/// `SKIP + LIMIT` is used instead of plain `LIMIT` so that there are enough
/// rows to apply the `SKIP` offset and still yield `LIMIT` final rows.
/// Saturating addition is used to guard against pathological large values.
pub fn row_bound(ops: &[PlanOp]) -> Option<usize> {
    // ORDER BY and DISTINCT require full materialisation — bound cannot be pushed.
    if ops
        .iter()
        .any(|op| matches!(op, PlanOp::OrderBy { .. } | PlanOp::Distinct))
    {
        return None;
    }
    // Aggregate plans use the streaming accumulator path, not the pull path.
    if ops.iter().any(|op| matches!(op, PlanOp::Aggregate { .. })) {
        return None;
    }
    // GroupAggregate plans are a sink over the full row stream; ORDER BY and LIMIT
    // apply to the finished group table, never to producers.
    if ops
        .iter()
        .any(|op| matches!(op, PlanOp::GroupAggregate { .. }))
    {
        return None;
    }
    // VarExpand / ShortestPath always use the staged path so that the 1M row
    // budget applies and BFS state is cleanly managed stage-by-stage.
    if ops
        .iter()
        .any(|op| matches!(op, PlanOp::VarExpand { .. } | PlanOp::ShortestPath { .. }))
    {
        return None;
    }
    // Pipeline plans (WITH / UNWIND / LeftOuterApply) always use the staged
    // path so that intermediate rows are correctly bounded and sequenced.
    if ops.iter().any(|op| {
        matches!(
            op,
            PlanOp::With { .. } | PlanOp::Unwind { .. } | PlanOp::LeftOuterApply { .. }
        )
    }) {
        return None;
    }
    let limit_n = ops.iter().rev().find_map(|op| match op {
        PlanOp::Limit(LimitSkip::Exact(n)) => Some(*n),
        PlanOp::Limit(LimitSkip::Param(_)) => None, // param-limit: can't determine bound statically
        _ => None,
    })?;
    // A param Skip means the skip count is unknown at plan time — force staged.
    if ops
        .iter()
        .any(|op| matches!(op, PlanOp::Skip(LimitSkip::Param(_))))
    {
        return None;
    }
    let skip_n = ops
        .iter()
        .rev()
        .find_map(|op| match op {
            PlanOp::Skip(LimitSkip::Exact(n)) => Some(*n),
            _ => None,
        })
        .unwrap_or(0);
    Some((skip_n as usize).saturating_add(limit_n as usize))
}

/// Returns `true` when the plan shape is supported by `subscribe_query`.
///
/// Allowlisted shapes (documented subset — not full Cypher):
///   • `MATCH (n:Label) WHERE … RETURN …`
///     → ScanLabel or ScanKey, optional LookupProps, optional Filter, Project,
///       optional Skip/Limit.
///   • `MATCH (a)-[r:TYPE]->(b) RETURN …`
///     → ScanLabel or ScanKey, single Expand, optional Filter, Project,
///       optional Skip/Limit.
///
/// Everything else is rejected: ORDER BY, DISTINCT, aggregates, variable-length
/// paths, OPTIONAL MATCH, WITH, UNWIND, JoinBound (multi-MATCH). Use LIMIT to
/// bound re-execution cost (`subscribe_query` does a full re-run per commit).
pub fn is_subscribable(ops: &[PlanOp]) -> bool {
    ops.iter().all(|op| {
        matches!(
            op,
            PlanOp::ScanLabel { .. }
                | PlanOp::ScanKey { .. }
                | PlanOp::LookupProps { .. }
                | PlanOp::Expand { .. }
                | PlanOp::Filter { .. }
                | PlanOp::Project { .. }
                | PlanOp::Skip(_)
                | PlanOp::Limit(_)
        )
    }) && ops
        .iter()
        .any(|op| matches!(op, PlanOp::ScanLabel { .. } | PlanOp::ScanKey { .. }))
        && ops.iter().any(|op| matches!(op, PlanOp::Project { .. }))
}

/// Compile `q` into a logical plan. Errors are contextual `String`s; never panics.
pub fn plan(q: &Query) -> Result<Vec<PlanOp>, String> {
    let mut bound = BTreeSet::new();
    let mut rel_bound = BTreeSet::new();
    let mut ops = Vec::new();
    let mut node_anon = 0u32;
    let mut rel_anon = 0u32;

    for pat in &q.matches {
        compile_pattern(
            pat,
            &mut ops,
            &mut bound,
            &mut rel_bound,
            &mut node_anon,
            &mut rel_anon,
        )?;
    }

    // OPTIONAL MATCH clauses (after required MATCHes).
    for oc in &q.optional_clauses {
        compile_optional_clause(
            oc,
            &mut ops,
            &mut bound,
            &mut rel_bound,
            &mut node_anon,
            &mut rel_anon,
        )?;
    }

    // Top-level UNWIND clauses.
    for uw in &q.unwinds {
        check_unwind_bound(&uw.list, &bound)?;
        bound.insert(uw.alias.clone());
        ops.push(PlanOp::Unwind {
            expr: uw.list.clone(),
            alias: uw.alias.clone(),
        });
    }

    if let Some(expr) = &q.where_expr {
        check_expr_bound(expr, &bound)?;
        ops.push(PlanOp::Filter { expr: expr.clone() });
    }

    // Post-UNWIND WHERE: filter expanded rows using UNWIND alias bindings.
    if let Some(expr) = &q.post_unwind_where {
        check_expr_bound(expr, &bound)?;
        ops.push(PlanOp::Filter { expr: expr.clone() });
    }

    // WITH pipeline stages.
    for stage in &q.stages {
        compile_with_stage(
            stage,
            &mut ops,
            &mut bound,
            &mut rel_bound,
            &mut node_anon,
            &mut rel_anon,
        )?;
    }

    check_return_bound(&q.returns, &bound, &rel_bound)?;
    check_duplicate_aliases(&q.returns)?;
    check_duplicate_columns(&q.returns)?;
    if q.distinct
        && q.returns
            .iter()
            .any(|r| matches!(&r.value, RetVal::Agg { .. }))
    {
        return Err(
            "RETURN DISTINCT is not supported with aggregate functions; use grouping".to_string(),
        );
    }

    // Detect aggregate vs non-aggregate items in RETURN.
    // For pipeline plans (with stages or top-level UNWIND), single-aggregate
    // path is not used — route to GroupAggregate or Project.
    let is_pipeline = !q.stages.is_empty()
        || !q.unwinds.is_empty()
        || q.post_unwind_where.is_some()
        || !q.optional_clauses.is_empty();
    let agg_count = q
        .returns
        .iter()
        .filter(|r| matches!(&r.value, RetVal::Agg { .. }))
        .count();

    if agg_count == 1 && q.returns.len() == 1 && !is_pipeline {
        // Single-aggregate fast path: streaming O(1) accumulator, no grouping.
        let item = &q.returns[0];
        let (func, arg) = match &item.value {
            RetVal::Agg { func, arg } => (func.clone(), arg.clone()),
            _ => unreachable!(),
        };
        // Validate: SUM/AVG/MIN/MAX require a Prop arg, not Star.
        if let (AggFunc::Sum | AggFunc::Avg | AggFunc::Min | AggFunc::Max, AggArg::Star) =
            (&func, &arg)
        {
            return Err(format!(
                "{name} does not accept '*'; use a property expression like `{name}(n.prop)`",
                name = func_name(&func),
            ));
        }
        let column = item
            .alias
            .clone()
            .unwrap_or_else(|| agg_column_name(&func, &arg));
        ops.push(PlanOp::Aggregate { func, arg, column });
        // ORDER BY and LIMIT/SKIP are ignored for single-aggregate queries
        // (always returns exactly one row).
        return Ok(ops);
    }

    if agg_count > 0 {
        // GroupAggregate: handles grouped (mix of key items and aggregates) as
        // well as multi-aggregate-no-keys (all RETURN items are aggregates).
        let mut keys: Vec<(String, RetItem)> = Vec::new();
        let mut aggs: Vec<(AggFunc, AggArg, String)> = Vec::new();
        for item in &q.returns {
            match &item.value {
                RetVal::Agg { func, arg } => {
                    // Validate: SUM/AVG/MIN/MAX require a Prop arg, not Star.
                    if let (
                        AggFunc::Sum | AggFunc::Avg | AggFunc::Min | AggFunc::Max,
                        AggArg::Star,
                    ) = (func, arg)
                    {
                        return Err(format!(
                            "{name} does not accept '*'; use a property expression like `{name}(n.prop)`",
                            name = func_name(func),
                        ));
                    }
                    let column = item
                        .alias
                        .clone()
                        .unwrap_or_else(|| agg_column_name(func, arg));
                    aggs.push((func.clone(), arg.clone(), column));
                }
                _ => {
                    keys.push((column_name(item), item.clone()));
                }
            }
        }
        ops.push(PlanOp::GroupAggregate { keys, aggs });
        // ORDER BY + SKIP + LIMIT apply to the finished group result table.
        if !q.order_by.is_empty() {
            let mut items = Vec::with_capacity(q.order_by.len());
            for item in &q.order_by {
                items.push(rewrite_order_item(item, &q.returns, &bound, &rel_bound)?);
            }
            ops.push(PlanOp::OrderBy { items });
        }
        if let Some(ls) = &q.skip {
            ops.push(PlanOp::Skip(ls.clone()));
        }
        if let Some(ls) = &q.limit {
            ops.push(PlanOp::Limit(ls.clone()));
        }
        return Ok(ops);
    }

    ops.push(PlanOp::Project {
        items: q.returns.clone(),
    });
    if q.distinct {
        ops.push(PlanOp::Distinct);
    }

    if !q.order_by.is_empty() {
        let mut items = Vec::with_capacity(q.order_by.len());
        for item in &q.order_by {
            items.push(rewrite_order_item(item, &q.returns, &bound, &rel_bound)?);
        }
        ops.push(PlanOp::OrderBy { items });
    }

    if let Some(ls) = &q.skip {
        ops.push(PlanOp::Skip(ls.clone()));
    }
    if let Some(ls) = &q.limit {
        ops.push(PlanOp::Limit(ls.clone()));
    }

    Ok(ops)
}

/// Compile one WITH pipeline stage.
fn compile_with_stage(
    stage: &WithStage,
    ops: &mut Vec<PlanOp>,
    bound: &mut BTreeSet<String>,
    rel_bound: &mut BTreeSet<String>,
    node_anon: &mut u32,
    rel_anon: &mut u32,
) -> Result<(), String> {
    let agg_count = stage
        .items
        .iter()
        .filter(|r| matches!(&r.value, RetVal::Agg { .. }))
        .count();

    if agg_count > 0 {
        // Aggregate WITH → compile GroupAggregate + optional Filter/OrderBy/Skip/Limit.
        let mut keys: Vec<(String, RetItem)> = Vec::new();
        let mut aggs: Vec<(AggFunc, AggArg, String)> = Vec::new();
        for item in &stage.items {
            match &item.value {
                RetVal::Agg { func, arg } => {
                    if let (
                        AggFunc::Sum | AggFunc::Avg | AggFunc::Min | AggFunc::Max,
                        AggArg::Star,
                    ) = (func, arg)
                    {
                        return Err(format!(
                            "{name} does not accept '*'; use a property expression like `{name}(n.prop)`",
                            name = func_name(func),
                        ));
                    }
                    let col = item
                        .alias
                        .clone()
                        .unwrap_or_else(|| agg_column_name(func, arg));
                    aggs.push((func.clone(), arg.clone(), col));
                }
                _ => {
                    keys.push((column_name(item), item.clone()));
                }
            }
        }
        ops.push(PlanOp::GroupAggregate {
            keys: keys.clone(),
            aggs: aggs.clone(),
        });

        // Update bound to reflect only what GroupAggregate outputs.
        bound.clear();
        rel_bound.clear();
        for (col, _) in &keys {
            bound.insert(col.clone());
        }
        for (_, _, col) in &aggs {
            bound.insert(col.clone());
        }

        // Optional HAVING filter (WHERE after WITH with aggregates).
        if let Some(expr) = &stage.where_expr {
            check_expr_bound(expr, bound)?;
            ops.push(PlanOp::Filter { expr: expr.clone() });
        }
        // ORDER BY on the group result rows — validate targets against GroupAggregate output.
        // `bound` was updated above (lines 435–442) to hold only group output columns.
        if !stage.order_by.is_empty() {
            for item in &stage.order_by {
                match &item.target {
                    OrderTarget::Prop { var, .. } | OrderTarget::Var(var) => {
                        require_bound(var, bound, "ORDER BY in aggregate WITH")?;
                    }
                    OrderTarget::Alias(name) => {
                        require_bound(name, bound, "ORDER BY in aggregate WITH")?;
                    }
                }
            }
            ops.push(PlanOp::OrderBy {
                items: stage.order_by.clone(),
            });
        }
        if let Some(ls) = &stage.skip {
            ops.push(PlanOp::Skip(ls.clone()));
        }
        if let Some(ls) = &stage.limit {
            ops.push(PlanOp::Limit(ls.clone()));
        }
    } else {
        // Non-aggregate WITH → validate items and emit PlanOp::With.
        check_return_bound(&stage.items, bound, rel_bound)?;

        // Optional WHERE filter on the current (pre-WITH) rows.
        // This also handles bare-variable operands (Operand::Var) referencing
        // scalar aliases produced by earlier stages.
        if let Some(expr) = &stage.where_expr {
            check_expr_bound(expr, bound)?;
        }
        // ORDER BY items reference either var names or prop paths — no rewrite needed
        // here; exec_order_by_rows handles raw row ordering.
        // ORDER BY may reference the WITH output columns (aliases) in addition to
        // variables already in scope before the WITH.
        let with_col_names: BTreeSet<String> = stage.items.iter().map(column_name).collect();
        for item in &stage.order_by {
            match &item.target {
                OrderTarget::Prop { var, .. } | OrderTarget::Var(var) => {
                    if !bound.contains(var.as_str()) && !with_col_names.contains(var.as_str()) {
                        return Err(format!("unbound variable `{var}` in ORDER BY in WITH"));
                    }
                }
                OrderTarget::Alias(name) => {
                    if !bound.contains(name.as_str()) && !with_col_names.contains(name.as_str()) {
                        return Err(format!("unbound variable `{name}` in ORDER BY in WITH"));
                    }
                }
            }
        }
        ops.push(PlanOp::With {
            items: stage.items.clone(),
            where_expr: stage.where_expr.clone(),
            order_by: stage.order_by.clone(),
            skip: stage.skip.clone(),
            limit: stage.limit.clone(),
        });

        // Update bound: after non-aggregate WITH, only the WITH items survive.
        let mut new_bound: BTreeSet<String> = BTreeSet::new();
        let mut new_rel_bound: BTreeSet<String> = BTreeSet::new();
        for item in &stage.items {
            let col = column_name(item);
            new_bound.insert(col.clone());
            // Preserve rel-bound status for relationship variables carried through.
            match &item.value {
                RetVal::Var(v) if rel_bound.contains(v.as_str()) => {
                    new_rel_bound.insert(col);
                }
                _ => {}
            }
        }
        *bound = new_bound;
        *rel_bound = new_rel_bound;
    }

    // MATCH clauses that follow this WITH.
    for pat in &stage.matches {
        compile_pattern(pat, ops, bound, rel_bound, node_anon, rel_anon)?;
    }
    // OPTIONAL MATCH clauses that follow those MATCHes.
    for oc in &stage.optional_clauses {
        compile_optional_clause(oc, ops, bound, rel_bound, node_anon, rel_anon)?;
    }
    // UNWIND clauses that follow this WITH.
    for uw in &stage.unwinds {
        check_unwind_bound(&uw.list, bound)?;
        bound.insert(uw.alias.clone());
        ops.push(PlanOp::Unwind {
            expr: uw.list.clone(),
            alias: uw.alias.clone(),
        });
    }
    // WHERE that follows those MATCHes.
    if let Some(expr) = &stage.post_where {
        check_expr_bound(expr, bound)?;
        ops.push(PlanOp::Filter { expr: expr.clone() });
    }

    Ok(())
}

fn id_lookup(props: &[(String, Operand)]) -> Option<&Operand> {
    if props.len() == 1 && props[0].0 == "id" {
        Some(&props[0].1)
    } else {
        None
    }
}

fn invert_dir(d: RelDir) -> RelDir {
    match d {
        RelDir::Right => RelDir::Left,
        RelDir::Left => RelDir::Right,
        RelDir::Undirected => RelDir::Undirected,
    }
}

fn compile_pattern(
    pat: &Pattern,
    ops: &mut Vec<PlanOp>,
    bound: &mut BTreeSet<String>,
    rel_bound: &mut BTreeSet<String>,
    node_anon: &mut u32,
    rel_anon: &mut u32,
) -> Result<(), String> {
    let start = name_node(&pat.start, node_anon, bound);
    if pat.shortest {
        // shortestPath requires both endpoints already bound.
        if !bound.contains(&start) {
            return Err(format!(
                "shortestPath: source node `{start}` is not bound; \
                 bind both endpoints before shortestPath"
            ));
        }
        ops.push(PlanOp::JoinBound {
            var: start.clone(),
            label: pat.start.label.clone(),
            props: pat.start.props.clone(),
        });
    } else if bound.contains(&start) {
        ops.push(PlanOp::JoinBound {
            var: start.clone(),
            label: pat.start.label.clone(),
            props: pat.start.props.clone(),
        });
    } else if pat.chain.len() == 1
        && pat.chain[0].0.hops.is_none()
        && pat.chain[0]
            .1
            .var
            .as_ref()
            .is_some_and(|v| bound.contains(v))
    {
        // Expand-from-bound: leftmost unbound, rightmost dest already bound,
        // single-rel *fixed-hop* pattern. Start from dest, invert dir, expand
        // toward start. Variable-length (`*min..max`) is not reversed: VarExpand
        // has no dest label/prop filter, so reversing would drop start checks.
        let (rel, dest) = &pat.chain[0];
        let dest_name = name_node(dest, node_anon, bound);
        let rel_name = name_rel(rel, rel_anon, bound);
        bound.insert(rel_name.clone());
        rel_bound.insert(rel_name.clone());
        if dest.label.is_some() || !dest.props.is_empty() {
            ops.push(PlanOp::JoinBound {
                var: dest_name.clone(),
                label: dest.label.clone(),
                props: dest.props.clone(),
            });
        }
        ops.push(PlanOp::Expand {
            from: dest_name,
            rel_var: Some(rel_name),
            etype: rel.etype.clone(),
            dir: invert_dir(rel.dir),
            to: start.clone(),
            to_label: pat.start.label.clone(),
            to_props: pat.start.props.clone(),
        });
        bound.insert(start);
        return Ok(());
    } else if let Some(key) = id_lookup(&pat.start.props) {
        ops.push(PlanOp::ScanKey {
            var: start.clone(),
            key: key.clone(),
            label: pat.start.label.clone(),
        });
        bound.insert(start.clone());
    } else {
        ops.push(PlanOp::ScanLabel {
            var: start.clone(),
            label: pat.start.label.clone(),
        });
        if !pat.start.props.is_empty() {
            ops.push(PlanOp::LookupProps {
                var: start.clone(),
                props: pat.start.props.clone(),
            });
        }
        bound.insert(start.clone());
    }

    let mut from = start;
    for (rel, dest) in &pat.chain {
        let rel_name = name_rel(rel, rel_anon, bound);
        bound.insert(rel_name.clone());
        rel_bound.insert(rel_name.clone());
        let to = name_node(dest, node_anon, bound);

        if let Some(hops) = rel.hops {
            if pat.shortest {
                // shortestPath: destination must also already be bound.
                if !bound.contains(&to) {
                    return Err(format!(
                        "shortestPath: destination node `{to}` is not bound; \
                         bind both endpoints before shortestPath"
                    ));
                }
                // A minimum hop count > 1 is not supported for shortestPath —
                // the BFS always returns the shortest (lowest-hop) path, so a
                // min constraint would silently be ignored.  Reject explicitly.
                if hops.min > 1 {
                    return Err(format!(
                        "shortestPath does not support a minimum hop count \
                         (got min={}); use a plain variable-length pattern \
                         if you need a minimum",
                        hops.min
                    ));
                }
                ops.push(PlanOp::ShortestPath {
                    from: from.clone(),
                    rel_var: Some(rel_name),
                    etype: rel.etype.clone(),
                    dir: rel.dir,
                    to: to.clone(),
                    max_hops: hops.max,
                });
            } else {
                ops.push(PlanOp::VarExpand {
                    from: from.clone(),
                    rel_var: Some(rel_name),
                    etype: rel.etype.clone(),
                    dir: rel.dir,
                    to: to.clone(),
                    min: hops.min,
                    max: hops.max,
                });
                bound.insert(to.clone());
            }
        } else {
            ops.push(PlanOp::Expand {
                from: from.clone(),
                rel_var: Some(rel_name),
                etype: rel.etype.clone(),
                dir: rel.dir,
                to: to.clone(),
                to_label: dest.label.clone(),
                to_props: dest.props.clone(),
            });
            bound.insert(to.clone());
        }
        from = to;
    }
    Ok(())
}

/// Compile one `OPTIONAL MATCH` clause into a `LeftOuterApply` op.
///
/// The inner plan is compiled from the pattern(s) and optional WHERE, starting
/// from a copy of the outer bound set.  Variables introduced inside the optional
/// scope are collected as `optional_vars` — they will be nulled in the fallback
/// row when the inner plan produces no results.
fn compile_optional_clause(
    oc: &OptionalClause,
    ops: &mut Vec<PlanOp>,
    bound: &mut BTreeSet<String>,
    rel_bound: &mut BTreeSet<String>,
    node_anon: &mut u32,
    rel_anon: &mut u32,
) -> Result<(), String> {
    // Clone the outer bound state; the inner plan compiles against it.
    let mut inner_bound = bound.clone();
    let mut inner_rel_bound = rel_bound.clone();
    let mut inner_ops: Vec<PlanOp> = Vec::new();

    for pat in &oc.patterns {
        compile_pattern(
            pat,
            &mut inner_ops,
            &mut inner_bound,
            &mut inner_rel_bound,
            node_anon,
            rel_anon,
        )?;
    }
    if let Some(expr) = &oc.where_expr {
        check_expr_bound(expr, &inner_bound)?;
        inner_ops.push(PlanOp::Filter { expr: expr.clone() });
    }

    // Variables newly introduced by the optional clause.
    let optional_vars: Vec<String> = inner_bound
        .difference(bound)
        .chain(inner_rel_bound.difference(rel_bound))
        .cloned()
        .collect();

    // Merge inner-introduced vars into the outer bound set so subsequent
    // clauses can reference them (they may be null, but they are "bound").
    for v in &optional_vars {
        bound.insert(v.clone());
    }
    for v in inner_rel_bound
        .difference(&*rel_bound)
        .cloned()
        .collect::<Vec<_>>()
    {
        rel_bound.insert(v);
    }

    ops.push(PlanOp::LeftOuterApply {
        inner: inner_ops,
        optional_vars,
    });
    Ok(())
}

fn name_node(node: &NodePat, counter: &mut u32, bound: &BTreeSet<String>) -> String {
    match &node.var {
        Some(v) => v.clone(),
        None => fresh("_n", counter, bound),
    }
}

fn name_rel(rel: &RelPat, counter: &mut u32, bound: &BTreeSet<String>) -> String {
    match &rel.var {
        Some(v) => v.clone(),
        None => fresh("_r", counter, bound),
    }
}

/// Stable `_nN` / `_rN` in encounter order. Skips names already bound so a
/// user var `_n0` does not collide with the next anonymous node.
fn fresh(prefix: &str, counter: &mut u32, bound: &BTreeSet<String>) -> String {
    for _ in 0..=u32::MAX {
        let name = format!("{prefix}{counter}");
        *counter = counter.wrapping_add(1);
        if !bound.contains(&name) {
            return name;
        }
    }
    format!("{prefix}x")
}

fn check_expr_bound(expr: &Expr, bound: &BTreeSet<String>) -> Result<(), String> {
    match expr {
        Expr::And(lhs, rhs) | Expr::Or(lhs, rhs) => {
            check_expr_bound(lhs, bound)?;
            check_expr_bound(rhs, bound)
        }
        Expr::Not(inner) => check_expr_bound(inner, bound),
        Expr::Cmp { lhs, rhs, .. } => {
            check_operand_bound(lhs, bound, "WHERE")?;
            check_operand_bound(rhs, bound, "WHERE")
        }
        Expr::Truthy(op) => check_operand_bound(op, bound, "WHERE"),
        Expr::IsNull(op) | Expr::IsNotNull(op) => check_operand_bound(op, bound, "WHERE"),
        Expr::In { expr, list } => {
            check_operand_bound(expr, bound, "WHERE")?;
            for item in list {
                check_operand_bound(item, bound, "WHERE")?;
            }
            Ok(())
        }
    }
}

fn check_operand_bound(
    operand: &Operand,
    bound: &BTreeSet<String>,
    clause: &str,
) -> Result<(), String> {
    match operand {
        Operand::Prop { var, .. } => require_bound(var, bound, clause),
        Operand::Lit(_) | Operand::Param(_) => Ok(()),
        Operand::Var(name) => require_bound(name, bound, clause),
        Operand::BinArith { left, right, .. } => {
            check_operand_bound(left, bound, clause)?;
            check_operand_bound(right, bound, clause)
        }
        Operand::FuncCall { args, .. } => {
            for arg in args {
                check_operand_bound(arg, bound, clause)?;
            }
            Ok(())
        }
    }
}

/// Validate that any variable referenced in an UNWIND expression is already bound.
fn check_unwind_bound(expr: &UnwindExpr, bound: &BTreeSet<String>) -> Result<(), String> {
    match expr {
        UnwindExpr::Lit(_) => Ok(()),
        UnwindExpr::Prop { var, .. } => require_bound(var, bound, "UNWIND"),
        UnwindExpr::Var(name) => require_bound(name, bound, "UNWIND"),
    }
}

fn require_bound(var: &str, bound: &BTreeSet<String>, clause: &str) -> Result<(), String> {
    if bound.contains(var) {
        Ok(())
    } else {
        Err(format!("unbound variable `{var}` in {clause}"))
    }
}

fn reject_bare_rel(var: &str, rel_bound: &BTreeSet<String>) -> Result<(), String> {
    if rel_bound.contains(var) {
        Err(format!(
            "cannot return relationship variable '{var}' bare; return its properties ({var}.field) instead"
        ))
    } else {
        Ok(())
    }
}

fn check_return_bound(
    items: &[RetItem],
    bound: &BTreeSet<String>,
    rel_bound: &BTreeSet<String>,
) -> Result<(), String> {
    for item in items {
        match &item.value {
            RetVal::Var(v) => {
                require_bound(v, bound, "RETURN")?;
                reject_bare_rel(v, rel_bound)?;
            }
            RetVal::Prop { var, .. } => {
                require_bound(var, bound, "RETURN")?;
            }
            RetVal::Agg { arg, .. } => match arg {
                AggArg::Star => {}
                AggArg::Var(v) => {
                    require_bound(v, bound, "RETURN")?;
                }
                AggArg::Prop { var, .. } => {
                    require_bound(var, bound, "RETURN")?;
                }
            },
            RetVal::FuncCall { args, .. } => {
                for arg in args {
                    check_operand_bound(arg, bound, "RETURN")?;
                }
            }
            RetVal::ScalarExpr(op) => {
                check_operand_bound(op, bound, "RETURN")?;
            }
        }
    }
    Ok(())
}

fn check_duplicate_aliases(items: &[RetItem]) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    for item in items {
        if let Some(alias) = &item.alias {
            if !seen.insert(alias.clone()) {
                return Err(format!("duplicate RETURN alias `{alias}`"));
            }
        }
    }
    Ok(())
}

fn check_duplicate_columns(items: &[RetItem]) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    for item in items {
        let col = column_name(item);
        if !seen.insert(col.clone()) {
            return Err(format!("duplicate RETURN column `{col}`"));
        }
    }
    Ok(())
}

/// Projected column name: alias if given, else the bare var, else `var.field`,
/// else the canonical aggregate call string, else `funcname(...)`, else `<expr>`.
fn column_name(item: &RetItem) -> String {
    if let Some(alias) = &item.alias {
        return alias.clone();
    }
    match &item.value {
        RetVal::Var(v) => v.clone(),
        RetVal::Prop { var, field } => format!("{var}.{field}"),
        RetVal::Agg { func, arg } => agg_column_name(func, arg),
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
    }
}

/// Canonical string for an aggregate without an alias, e.g. `COUNT(*)`,
/// `SUM(n.age)`.
fn agg_column_name(func: &AggFunc, arg: &AggArg) -> String {
    let f = func_name(func);
    let a = match arg {
        AggArg::Star => "*".to_string(),
        AggArg::Var(v) => v.clone(),
        AggArg::Prop { var, field } => format!("{var}.{field}"),
    };
    format!("{f}({a})")
}

fn func_name(func: &AggFunc) -> &'static str {
    match func {
        AggFunc::Count => "COUNT",
        AggFunc::Sum => "SUM",
        AggFunc::Avg => "AVG",
        AggFunc::Min => "MIN",
        AggFunc::Max => "MAX",
    }
}

fn rewrite_order_item(
    item: &OrderItem,
    returns: &[RetItem],
    bound: &BTreeSet<String>,
    rel_bound: &BTreeSet<String>,
) -> Result<OrderItem, String> {
    let column = match &item.target {
        OrderTarget::Alias(name) => {
            if returns
                .iter()
                .any(|r| r.alias.as_deref() == Some(name.as_str()))
            {
                name.clone()
            } else {
                return Err(format!("ORDER BY target `{name}` is not present in RETURN"));
            }
        }
        OrderTarget::Var(v) => {
            require_bound(v, bound, "ORDER BY")?;
            reject_bare_rel(v, rel_bound)?;
            match returns
                .iter()
                .find(|r| matches!(&r.value, RetVal::Var(x) if x == v))
            {
                Some(r) => column_name(r),
                None => {
                    return Err(format!("ORDER BY target `{v}` is not present in RETURN"));
                }
            }
        }
        OrderTarget::Prop { var, field } => {
            require_bound(var, bound, "ORDER BY")?;
            match returns.iter().find(
                |r| matches!(&r.value, RetVal::Prop { var: v, field: f } if v == var && f == field),
            ) {
                Some(r) => column_name(r),
                None => {
                    return Err(format!(
                        "ORDER BY target `{var}.{field}` is not present in RETURN"
                    ));
                }
            }
        }
    };
    Ok(OrderItem {
        target: OrderTarget::Alias(column),
        descending: item.descending,
    })
}

#[cfg(test)]
mod tests {
    use super::{plan, PlanOp};
    use crate::cypher::ast::{Expr, LimitSkip, Operand, OrderItem, OrderTarget, RetItem, RetVal};
    use crate::cypher::{lex, parse, RelDir};
    use crate::filter::CmpOp;
    use core_storage::Value;

    fn plan_src(src: &str) -> Result<Vec<PlanOp>, String> {
        plan(&parse(&lex(src)?)?)
    }

    fn assert_plan_err(src: &str, needle: &str) -> String {
        let result = std::panic::catch_unwind(|| plan_src(src));
        assert!(result.is_ok(), "plan({src:?}) panicked");
        let err = result
            .unwrap()
            .expect_err(&format!("plan({src:?}) must be Err"));
        assert!(
            err.contains(needle),
            "error must mention {needle:?}, got: {err}"
        );
        err
    }

    /// Dogfood query from T6. Shape:
    /// - MATCH 1: `t` unbound with `{id: $tid}` → `ScanKey`.
    /// - MATCH 2: `c` unbound, dest `t` already bound, single-rel → reverse:
    ///   Expand from `t` dir Left (inbound) to `c` (Company label on `to`).
    /// - MATCH 3: start `c` already bound → `JoinBound`; expand to bound `t`.
    #[test]
    fn dogfood_query_exact_plan() {
        let src = "\
MATCH (t:Talent {id: $tid}) \
MATCH (c:Company)-[i:INDUSTRY_ALIGNMENT]->(t) \
MATCH (c)-[s:SPECIALTY_MATCH]->(t) \
WHERE i.score >= 0.5 AND s.score >= 0.5 \
RETURN c, i.score AS industry, s.score AS specialty \
ORDER BY industry DESC, specialty DESC \
LIMIT 10";
        let got = plan_src(src).expect("dogfood query must plan");
        let expected = vec![
            PlanOp::ScanKey {
                var: "t".into(),
                key: Operand::Param("tid".into()),
                label: Some("Talent".into()),
            },
            PlanOp::Expand {
                from: "t".into(),
                rel_var: Some("i".into()),
                etype: Some("INDUSTRY_ALIGNMENT".into()),
                dir: RelDir::Left,
                to: "c".into(),
                to_label: Some("Company".into()),
                to_props: vec![],
            },
            PlanOp::JoinBound {
                var: "c".into(),
                label: None,
                props: vec![],
            },
            PlanOp::Expand {
                from: "c".into(),
                rel_var: Some("s".into()),
                etype: Some("SPECIALTY_MATCH".into()),
                dir: RelDir::Right,
                to: "t".into(),
                to_label: None,
                to_props: vec![],
            },
            PlanOp::Filter {
                expr: Expr::And(
                    Box::new(Expr::Cmp {
                        lhs: Operand::Prop {
                            var: "i".into(),
                            field: "score".into(),
                        },
                        op: CmpOp::Ge,
                        rhs: Operand::Lit(Value::Float(0.5)),
                    }),
                    Box::new(Expr::Cmp {
                        lhs: Operand::Prop {
                            var: "s".into(),
                            field: "score".into(),
                        },
                        op: CmpOp::Ge,
                        rhs: Operand::Lit(Value::Float(0.5)),
                    }),
                ),
            },
            PlanOp::Project {
                items: vec![
                    RetItem {
                        value: RetVal::Var("c".into()),
                        alias: None,
                    },
                    RetItem {
                        value: RetVal::Prop {
                            var: "i".into(),
                            field: "score".into(),
                        },
                        alias: Some("industry".into()),
                    },
                    RetItem {
                        value: RetVal::Prop {
                            var: "s".into(),
                            field: "score".into(),
                        },
                        alias: Some("specialty".into()),
                    },
                ],
            },
            PlanOp::OrderBy {
                items: vec![
                    OrderItem {
                        target: OrderTarget::Alias("industry".into()),
                        descending: true,
                    },
                    OrderItem {
                        target: OrderTarget::Alias("specialty".into()),
                        descending: true,
                    },
                ],
            },
            PlanOp::Limit(LimitSkip::Exact(10)),
        ];
        assert_eq!(got, expected);
    }

    /// Anonymous names increment in encounter order across the whole query.
    /// MATCH 1: start `_n0`, rel `_r0`, dest `a`.
    /// MATCH 2: start `_n1` unbound, dest already-bound `a` → reverse Expand
    /// from `a` dir Left to `_n1`.
    #[test]
    fn anonymous_node_and_rel_names_are_stable() {
        let got = plan_src("MATCH ()-[]->(a) MATCH ()-[]->(a) RETURN a").unwrap();
        assert_eq!(
            got,
            vec![
                PlanOp::ScanLabel {
                    var: "_n0".into(),
                    label: None,
                },
                PlanOp::Expand {
                    from: "_n0".into(),
                    rel_var: Some("_r0".into()),
                    etype: None,
                    dir: RelDir::Right,
                    to: "a".into(),
                    to_label: None,
                    to_props: vec![],
                },
                PlanOp::Expand {
                    from: "a".into(),
                    rel_var: Some("_r1".into()),
                    etype: None,
                    dir: RelDir::Left,
                    to: "_n1".into(),
                    to_label: None,
                    to_props: vec![],
                },
                PlanOp::Project {
                    items: vec![RetItem {
                        value: RetVal::Var("a".into()),
                        alias: None,
                    }],
                },
            ]
        );
    }

    #[test]
    fn props_on_scan_node_emit_scan_then_lookup() {
        let got = plan_src("MATCH (t:Talent {id: $tid}) RETURN t").unwrap();
        assert_eq!(
            got,
            vec![
                PlanOp::ScanKey {
                    var: "t".into(),
                    key: Operand::Param("tid".into()),
                    label: Some("Talent".into()),
                },
                PlanOp::Project {
                    items: vec![RetItem {
                        value: RetVal::Var("t".into()),
                        alias: None,
                    }],
                },
            ]
        );
    }

    #[test]
    fn mixed_id_map_stays_scan_label_then_lookup() {
        let got = plan_src("MATCH (t:Talent {id: $k, name: 'x'}) RETURN t").unwrap();
        assert_eq!(
            got,
            vec![
                PlanOp::ScanLabel {
                    var: "t".into(),
                    label: Some("Talent".into()),
                },
                PlanOp::LookupProps {
                    var: "t".into(),
                    props: vec![
                        ("id".into(), Operand::Param("k".into())),
                        ("name".into(), Operand::Lit(Value::Str("x".into()))),
                    ],
                },
                PlanOp::Project {
                    items: vec![RetItem {
                        value: RetVal::Var("t".into()),
                        alias: None,
                    }],
                },
            ]
        );
    }

    #[test]
    fn plan_id_map_is_scan_key() {
        let toks = crate::cypher::lex("MATCH (n:Person {id: $k}) RETURN n").unwrap();
        let q = crate::cypher::parse(&toks).unwrap();
        let ops = plan(&q).unwrap();
        assert!(matches!(ops[0], PlanOp::ScanKey { .. }), "{ops:?}");
    }

    #[test]
    fn plan_expands_from_bound_key() {
        let cy =
            "MATCH (t:Talent {id: $tid}) MATCH (c:Company)-[i:INDUSTRY_ALIGNMENT]->(t) RETURN c";
        let ops = plan(&crate::cypher::parse(&crate::cypher::lex(cy).unwrap()).unwrap()).unwrap();
        // first: ScanKey t; then Expand from t, dir Left (inbound)
        assert!(matches!(&ops[0], PlanOp::ScanKey { var, .. } if var == "t"));
        match &ops[1] {
            PlanOp::Expand { from, dir, to, .. } => {
                assert_eq!(from, "t");
                assert_eq!(to, "c");
                assert_eq!(*dir, RelDir::Left);
            }
            other => panic!("{other:?}"),
        }
    }

    /// VarExpand has no dest label/prop filter. Reversing
    /// `MATCH (c:Company)-[*1..2]->(t)` would drop `:Company` and bind
    /// non-Company `c`. Keep LTR: ScanLabel Company then VarExpand.
    #[test]
    fn plan_does_not_reverse_variable_length_from_bound() {
        let cy = "MATCH (t {id: $tid}) MATCH (c:Company)-[*1..2]->(t) RETURN c";
        let ops = plan_src(cy).unwrap();
        assert!(
            matches!(&ops[0], PlanOp::ScanKey { var, .. } if var == "t"),
            "{ops:?}"
        );
        assert!(
            matches!(&ops[1], PlanOp::ScanLabel { var, label } if var == "c" && label.as_deref() == Some("Company")),
            "{ops:?}"
        );
        match &ops[2] {
            PlanOp::VarExpand {
                from,
                dir,
                to,
                min,
                max,
                ..
            } => {
                assert_eq!(from, "c");
                assert_eq!(to, "t");
                assert_eq!(*dir, RelDir::Right);
                assert_eq!(*min, 1);
                assert_eq!(*max, 2);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn unbound_var_in_where_is_err() {
        let err = assert_plan_err("MATCH (a) WHERE b.x = 1 RETURN a", "b");
        assert!(
            err.to_ascii_lowercase().contains("unbound")
                && err.to_ascii_lowercase().contains("where"),
            "expected unbound-in-WHERE context, got: {err}"
        );
    }

    #[test]
    fn unbound_var_in_return_is_err() {
        let err = assert_plan_err("MATCH (a) RETURN b", "b");
        assert!(
            err.to_ascii_lowercase().contains("unbound")
                && err.to_ascii_lowercase().contains("return"),
            "expected unbound-in-RETURN context, got: {err}"
        );
    }

    #[test]
    fn unbound_var_in_order_by_is_err() {
        let err = assert_plan_err("MATCH (a) RETURN a ORDER BY b", "b");
        assert!(
            err.to_ascii_lowercase().contains("unbound")
                && (err.to_ascii_lowercase().contains("order")),
            "expected unbound-in-ORDER context, got: {err}"
        );
    }

    #[test]
    fn duplicate_alias_is_err() {
        let err = assert_plan_err("MATCH (a) RETURN a AS x, a.id AS x", "x");
        assert!(
            err.to_ascii_lowercase().contains("duplicate")
                && err.to_ascii_lowercase().contains("alias"),
            "expected duplicate-alias context, got: {err}"
        );
    }

    #[test]
    fn duplicate_column_name_is_err() {
        let err = assert_plan_err("MATCH (a) RETURN a, a", "a");
        assert!(
            err.to_ascii_lowercase().contains("duplicate")
                && err.to_ascii_lowercase().contains("column"),
            "expected duplicate-column context, got: {err}"
        );
    }

    #[test]
    fn order_by_target_absent_from_return_is_err() {
        // `a` is bound, but `a.x` is not a RETURN item.
        let err = assert_plan_err("MATCH (a) RETURN a ORDER BY a.x", "a.x");
        assert!(
            err.to_ascii_lowercase().contains("return"),
            "expected ORDER BY target-not-in-RETURN context, got: {err}"
        );
    }

    /// Alias → that alias's column; bare var → its RETURN column (alias if
    /// given, else the var name); un-aliased prop → `var.field`.
    #[test]
    fn order_by_targets_rewrite_to_projected_column_names() {
        let got = plan_src(
            "MATCH (a)-[r]->(b) \
             RETURN a, a.name AS nm, b.age \
             ORDER BY nm DESC, a ASC, b.age",
        )
        .unwrap();
        let order = got
            .iter()
            .find_map(|op| match op {
                PlanOp::OrderBy { items } => Some(items),
                _ => None,
            })
            .expect("plan must contain OrderBy");
        assert_eq!(
            order,
            &vec![
                OrderItem {
                    target: OrderTarget::Alias("nm".into()),
                    descending: true,
                },
                OrderItem {
                    target: OrderTarget::Alias("a".into()),
                    descending: false,
                },
                OrderItem {
                    target: OrderTarget::Alias("b.age".into()),
                    descending: false,
                },
            ]
        );

        let aliased_var = plan_src("MATCH (a) RETURN a AS person ORDER BY a").unwrap();
        let order = aliased_var
            .iter()
            .find_map(|op| match op {
                PlanOp::OrderBy { items } => Some(items),
                _ => None,
            })
            .expect("plan must contain OrderBy");
        assert_eq!(
            order,
            &vec![OrderItem {
                target: OrderTarget::Alias("person".into()),
                descending: false,
            }]
        );
    }

    #[test]
    fn bound_pattern_start_is_join_bound_then_expand() {
        let got = plan_src("MATCH (a:L) MATCH (a)-[r:T]->(b) RETURN a, b").unwrap();
        assert_eq!(
            got,
            vec![
                PlanOp::ScanLabel {
                    var: "a".into(),
                    label: Some("L".into()),
                },
                PlanOp::JoinBound {
                    var: "a".into(),
                    label: None,
                    props: vec![],
                },
                PlanOp::Expand {
                    from: "a".into(),
                    rel_var: Some("r".into()),
                    etype: Some("T".into()),
                    dir: RelDir::Right,
                    to: "b".into(),
                    to_label: None,
                    to_props: vec![],
                },
                PlanOp::Project {
                    items: vec![
                        RetItem {
                            value: RetVal::Var("a".into()),
                            alias: None,
                        },
                        RetItem {
                            value: RetVal::Var("b".into()),
                            alias: None,
                        },
                    ],
                },
            ]
        );
    }

    #[test]
    fn bound_dest_extra_checks_ride_on_expand() {
        let got = plan_src("MATCH (t:Talent) MATCH (c)-[r]->(t:Talent {id: 1}) RETURN t").unwrap();
        assert_eq!(
            got,
            vec![
                PlanOp::ScanLabel {
                    var: "t".into(),
                    label: Some("Talent".into()),
                },
                PlanOp::JoinBound {
                    var: "t".into(),
                    label: Some("Talent".into()),
                    props: vec![("id".into(), Operand::Lit(Value::Int(1)))],
                },
                PlanOp::Expand {
                    from: "t".into(),
                    rel_var: Some("r".into()),
                    etype: None,
                    dir: RelDir::Left,
                    to: "c".into(),
                    to_label: None,
                    to_props: vec![],
                },
                PlanOp::Project {
                    items: vec![RetItem {
                        value: RetVal::Var("t".into()),
                        alias: None,
                    }],
                },
            ]
        );
    }

    #[test]
    fn return_distinct_emits_distinct_after_project() {
        let ops = plan_src("MATCH (n) RETURN DISTINCT n").expect("DISTINCT must plan");
        let proj = ops
            .iter()
            .position(|op| matches!(op, PlanOp::Project { .. }))
            .expect("Project");
        assert!(
            matches!(ops.get(proj + 1), Some(PlanOp::Distinct)),
            "DISTINCT must follow Project, got: {ops:?}"
        );
        let bounded = plan_src("MATCH (n) RETURN DISTINCT n LIMIT 1").unwrap();
        assert!(
            super::row_bound(&bounded).is_none(),
            "DISTINCT + LIMIT must not push LIMIT into producers"
        );
    }

    #[test]
    fn skip_then_limit_follow_project() {
        let got = plan_src("MATCH (a) RETURN a SKIP 2 LIMIT 3").unwrap();
        assert_eq!(
            got,
            vec![
                PlanOp::ScanLabel {
                    var: "a".into(),
                    label: None,
                },
                PlanOp::Project {
                    items: vec![RetItem {
                        value: RetVal::Var("a".into()),
                        alias: None,
                    }],
                },
                PlanOp::Skip(LimitSkip::Exact(2)),
                PlanOp::Limit(LimitSkip::Exact(3)),
            ]
        );
    }

    #[test]
    fn aliased_prop_order_by_rewrites_to_alias_column() {
        let got = plan_src("MATCH (a) RETURN a.name AS nm ORDER BY a.name").unwrap();
        let order = got
            .iter()
            .find_map(|op| match op {
                PlanOp::OrderBy { items } => Some(items),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            order,
            &vec![OrderItem {
                target: OrderTarget::Alias("nm".into()),
                descending: false,
            }]
        );
    }

    #[test]
    fn plan_never_panics_on_hand_built_query() {
        use crate::cypher::ast::{NodePat, Pattern, Query};
        let q = Query {
            matches: vec![],
            optional_clauses: vec![],
            where_expr: None,
            unwinds: vec![],
            post_unwind_where: None,
            stages: vec![],
            returns: vec![],
            order_by: vec![],
            distinct: false,
            skip: None,
            limit: None,
        };
        let result = std::panic::catch_unwind(|| plan(&q));
        assert!(result.is_ok(), "plan panicked on empty Query");
        let _ = result.unwrap();

        let q = Query {
            matches: vec![Pattern {
                start: NodePat {
                    var: None,
                    label: None,
                    props: vec![],
                },
                chain: vec![],
                shortest: false,
            }],
            optional_clauses: vec![],
            where_expr: Some(Expr::Not(Box::new(Expr::Cmp {
                lhs: Operand::Param("p".into()),
                op: CmpOp::Eq,
                rhs: Operand::Lit(Value::Int(1)),
            }))),
            unwinds: vec![],
            post_unwind_where: None,
            stages: vec![],
            returns: vec![],
            distinct: false,
            order_by: vec![OrderItem {
                target: OrderTarget::Alias("missing".into()),
                descending: true,
            }],
            skip: Some(LimitSkip::Exact(0)),
            limit: Some(LimitSkip::Exact(0)),
        };
        let result = std::panic::catch_unwind(|| plan(&q));
        assert!(result.is_ok(), "plan panicked on hand-built Query");
        let _ = result.unwrap();
    }

    #[test]
    fn bare_relationship_var_in_return_is_err() {
        let err = assert_plan_err("MATCH (a)-[r:T]->(b) RETURN r", "r");
        assert!(
            err.to_ascii_lowercase().contains("relationship"),
            "expected bare-rel RETURN guidance, got: {err}"
        );
    }

    #[test]
    fn relationship_prop_in_return_is_ok() {
        plan_src("MATCH (a)-[r:T]->(b) RETURN r.w").expect("rel prop RETURN must plan");
    }

    #[test]
    fn bare_relationship_var_in_order_by_is_err() {
        // RETURN r.w is legal; ORDER BY r is a bare rel var (defense, not just
        // "not in RETURN").
        let err = assert_plan_err("MATCH (a)-[r:T]->(b) RETURN r.w ORDER BY r", "r");
        assert!(
            err.to_ascii_lowercase().contains("relationship"),
            "expected bare-rel ORDER BY guidance, got: {err}"
        );
    }

    #[test]
    fn relationship_prop_in_order_by_is_ok() {
        plan_src("MATCH (a)-[r:T]->(b) RETURN r.w ORDER BY r.w")
            .expect("rel prop ORDER BY must plan");
    }

    // ── Variable-length path and shortestPath planner tests ───────────────────

    #[test]
    fn var_expand_op_emitted_for_star_rel() {
        use super::row_bound;
        let ops = plan_src("MATCH (a)-[r:T*2..4]->(b) RETURN b").unwrap();
        let has_var = ops
            .iter()
            .any(|op| matches!(op, PlanOp::VarExpand { min: 2, max: 4, .. }));
        assert!(has_var, "expected VarExpand(2..4) in plan, got: {ops:?}");
        // row_bound must be None even when no ORDER BY (VarExpand overrides pull routing)
        assert_eq!(
            row_bound(&ops),
            None,
            "VarExpand plan must not use pull path"
        );
    }

    #[test]
    fn var_expand_with_limit_still_takes_staged_path() {
        use super::row_bound;
        let ops = plan_src("MATCH (a)-[r:T*1..3]->(b) RETURN b LIMIT 5").unwrap();
        // Staged path: row_bound returns None for VarExpand.
        assert_eq!(
            row_bound(&ops),
            None,
            "VarExpand + LIMIT must still use staged path"
        );
        let has_var = ops.iter().any(|op| matches!(op, PlanOp::VarExpand { .. }));
        assert!(has_var, "plan must contain VarExpand");
        let has_limit = ops
            .iter()
            .any(|op| matches!(op, PlanOp::Limit(LimitSkip::Exact(5))));
        assert!(has_limit, "plan must still emit Limit op");
    }

    #[test]
    fn shortest_path_op_emitted_for_shortest_path_clause() {
        let ops =
            plan_src("MATCH (a:N) MATCH (b:N) MATCH shortestPath((a)-[r:T*..3]->(b)) RETURN a")
                .unwrap();
        let has_sp = ops
            .iter()
            .any(|op| matches!(op, PlanOp::ShortestPath { max_hops: 3, .. }));
        assert!(
            has_sp,
            "expected ShortestPath op with max_hops=3, got: {ops:?}"
        );
    }

    #[test]
    fn shortest_path_unbound_endpoint_is_err() {
        let err = assert_plan_err(
            "MATCH shortestPath((a)-[r:T*..3]->(b)) RETURN a",
            "shortestPath",
        );
        assert!(
            err.contains("not bound") || err.contains("bound"),
            "error must mention binding, got: {err}"
        );
    }

    #[test]
    fn var_expand_rel_var_is_in_rel_bound() {
        // r.length should be allowed in RETURN (prop access)
        plan_src("MATCH (a)-[r:T*1..3]->(b) RETURN r.length").expect("r.length must plan");
        // bare r must be rejected
        assert_plan_err("MATCH (a)-[r:T*1..3]->(b) RETURN r", "r");
    }

    #[test]
    fn shortest_path_min_gt_1_is_plan_err() {
        // shortestPath with *2..5 must be rejected: min>1 is not supported
        let err = assert_plan_err(
            "MATCH (a:N) MATCH (b:N) MATCH shortestPath((a)-[r:T*2..5]->(b)) RETURN r.length",
            "shortestPath",
        );
        assert!(
            err.contains("minimum"),
            "error must mention minimum hop count, got: {err}"
        );
    }
}
