//! Cypher executor: `PlanOp` sequence → `ResultSet` over a binding table.

use crate::cypher::ast::{AggArg, AggFunc, Expr, Operand, OrderItem, OrderTarget, RetItem, RetVal};
use crate::cypher::plan::PlanOp;
use crate::cypher::RelDir;
use crate::filter::eval_cmp;
use crate::result::ResultSet;
use crate::traverse::{expand, Dir, EdgeRef};
use crate::value_ops::{cmp_optional, values_equal};
use crate::view::GraphView;
use core_storage::{Value, ValueKey};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Test-only counter incremented each time the fused ScanLabel+Filter arm
/// executes.  Lets property tests assert the fast path actually fires for
/// matching query shapes (and does NOT fire for fallback shapes).
#[cfg(test)]
static FUSED_SCAN_FIRES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Query parameters. Missing names anywhere in the plan are an error at
/// execution start (the plan is walked before any rows are produced).
pub struct Params<'a>(pub &'a BTreeMap<String, Value>);

#[derive(Clone, Copy, Debug)]
enum Cell {
    Node(u32),
    Rel(EdgeRef),
    /// Virtual path cell produced by `VarExpand` / `ShortestPath`.
    /// The only accessible property is `length` → `Value::Int(hops)`.
    Path(u8),
}

/// Binding-table row: one slot per interned variable. Cheaper to clone than
/// `BTreeMap<String, Cell>` on every Expand/Scan (the two-hop hot path).
type Row = Vec<Option<Cell>>;

struct VarTable {
    names: Vec<String>,
}

impl VarTable {
    fn intern(&mut self, name: &str) -> usize {
        if let Some(i) = self.names.iter().position(|n| n == name) {
            return i;
        }
        self.names.push(name.to_string());
        self.names.len() - 1
    }

    fn slot(&self, name: &str) -> Option<usize> {
        self.names.iter().position(|n| n == name)
    }
}

struct Projected {
    columns: Vec<String>,
    rows: Vec<Vec<Option<Value>>>,
}

/// Production cap on the executor binding table after each `scan_label` /
/// `expand`. Unjoined multi-MATCH cross-joins OOM without this.
const MAX_INTERMEDIATE_ROWS: usize = 1_000_000;

/// Cap on the number of distinct groups a `GroupAggregate` plan may produce.
const MAX_GROUPS: usize = 1_000_000;

/// Group-key type: one `Option<ValueKey>` per group-key RETURN item.
/// `None` represents a null value; null keys group together (openCypher).
type GroupKey = Vec<Option<ValueKey>>;
/// Per-group entry: first-seen display values for key columns, plus accumulators.
/// Display values are the original `Value`s before normalization so that
/// `Int(42)` groups still display as `Int(42)` even though they are hashed as
/// `FloatBits`.  For mixed Int/Float groups the first-seen value wins.
type GroupEntry = (Vec<Option<Value>>, Vec<AggAcc>);

#[cfg(test)]
thread_local! {
    static TEST_MAX_INTERMEDIATE_ROWS: std::cell::Cell<Option<usize>> =
        const { std::cell::Cell::new(None) };
    /// Accumulator for rows emitted by `exec_expand` during a bounded test run.
    /// `None` means the counter is inactive (no test is watching).
    static TEST_EXPAND_PRODUCED: std::cell::Cell<Option<usize>> =
        const { std::cell::Cell::new(None) };
    /// Override for `MAX_GROUPS` used in tests.
    static TEST_MAX_GROUPS: std::cell::Cell<Option<usize>> =
        const { std::cell::Cell::new(None) };
}

fn max_intermediate_rows() -> usize {
    #[cfg(test)]
    {
        TEST_MAX_INTERMEDIATE_ROWS
            .with(|c| c.get())
            .unwrap_or(MAX_INTERMEDIATE_ROWS)
    }
    #[cfg(not(test))]
    {
        MAX_INTERMEDIATE_ROWS
    }
}

fn max_groups() -> usize {
    #[cfg(test)]
    {
        TEST_MAX_GROUPS.with(|c| c.get()).unwrap_or(MAX_GROUPS)
    }
    #[cfg(not(test))]
    {
        MAX_GROUPS
    }
}

/// Test hook: run `f` with a smaller group cap so the group-count error path
/// can fire without creating a million groups.
#[cfg(test)]
pub(crate) fn with_max_groups<R>(cap: usize, f: impl FnOnce() -> R) -> R {
    TEST_MAX_GROUPS.with(|c| {
        let prev = c.replace(Some(cap));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        c.set(prev);
        match result {
            Ok(v) => v,
            Err(p) => std::panic::resume_unwind(p),
        }
    })
}

/// Test hook: run `f` with a smaller intermediate-row cap so the error path
/// can fire without allocating a million rows. Restores the previous override
/// (including across panics).
#[cfg(test)]
pub(crate) fn with_max_intermediate_rows<R>(cap: usize, f: impl FnOnce() -> R) -> R {
    TEST_MAX_INTERMEDIATE_ROWS.with(|c| {
        let prev = c.replace(Some(cap));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        c.set(prev);
        match result {
            Ok(v) => v,
            Err(p) => std::panic::resume_unwind(p),
        }
    })
}

/// Increment the test-only expand-row counter (no-op when counter is inactive).
#[cfg(test)]
fn record_expand_row() {
    TEST_EXPAND_PRODUCED.with(|c| {
        if let Some(prev) = c.get() {
            c.set(Some(prev + 1));
        }
    });
}

/// Run `f` while counting every row emitted by `exec_expand`.
/// Returns `(result_of_f, total_rows_produced)`.
///
/// Used by tests to assert early-termination: bounded execution must emit
/// ≤ `row_bound + ε` rows, while an equivalent unbounded run emits ≥ 100×.
#[cfg(test)]
pub(crate) fn with_expand_counter<R>(f: impl FnOnce() -> R) -> (R, usize) {
    TEST_EXPAND_PRODUCED.with(|c| c.set(Some(0)));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    let count = TEST_EXPAND_PRODUCED.with(|c| c.get().unwrap_or(0));
    TEST_EXPAND_PRODUCED.with(|c| c.set(None));
    match result {
        Ok(v) => (v, count),
        Err(p) => std::panic::resume_unwind(p),
    }
}

fn row_cap_err(cap: usize) -> String {
    format!(
        "intermediate result exceeds {cap} rows; add a LIMIT or constrain patterns with shared variables"
    )
}

fn group_cap_err() -> String {
    format!(
        "group count exceeds {} distinct keys; add a WHERE clause or constrain the grouping key",
        max_groups()
    )
}

/// Convert a group-key value to a `ValueKey` with numeric unification.
///
/// `Int(n)` and `Float(n as f64)` produce the same `FloatBits` key so that
/// nodes with `score = 1` (Int) and `score = 1.0` (Float) land in the same
/// group, matching openCypher's `1 = 1.0` equality rule.
///
/// Note: integers whose magnitude exceeds 2^53 lose precision when cast to
/// `f64`, so two large integers that differ only beyond the float mantissa
/// width could be incorrectly unified. This is a known limitation documented
/// in `docs/site/query.md`.
fn group_key_normalize(v: &Value) -> Option<ValueKey> {
    match v {
        Value::Int(n) => Some(ValueKey::FloatBits((*n as f64).to_bits())),
        Value::Float(f) => Some(ValueKey::FloatBits(f.to_bits())),
        _ => ValueKey::from_value(v),
    }
}

/// Execute a plan against a view. Row order before OrderBy is deterministic
/// (scan order = dense ids; expand order = expand()'s sorted order).
///
/// Precondition: `OrderBy` items produced by `plan()` use `OrderTarget::Alias`
/// only; other variants are accepted defensively but non-standard.
///
/// When the plan has a `Limit` and no `OrderBy`, the executor switches to a
/// demand-driven (pull-based) strategy: all producer stages terminate as soon
/// as `SKIP + LIMIT` final rows have been collected.  No intermediate table is
/// ever fully materialised for the bounded path — the 1 M cap is the safety
/// net for unbounded queries only.
pub fn execute(view: &GraphView, plan: &[PlanOp], params: &Params) -> Result<ResultSet, String> {
    use crate::cypher::plan::row_bound;
    execute_inner(view, plan, params, row_bound(plan))
}

/// Like `execute` but always disables LIMIT push-down (row_bound = None).
///
/// Used in tests as the reference implementation: the result must equal that of
/// `execute` (which applies push-down) modulo the subset selected by SKIP/LIMIT.
#[cfg(test)]
pub(crate) fn execute_unbounded(
    view: &GraphView,
    plan: &[PlanOp],
    params: &Params,
) -> Result<ResultSet, String> {
    execute_inner(view, plan, params, None)
}

fn execute_inner(
    view: &GraphView,
    plan: &[PlanOp],
    params: &Params,
    row_bound: Option<usize>,
) -> Result<ResultSet, String> {
    check_params(plan, params)?;

    // VarExpand / ShortestPath plans always take the staged path, even when
    // an Aggregate op is also present.  row_bound() already returns None for
    // these plans (so the pull path is never chosen), but the aggregate path
    // check below must also be skipped so that VarExpand+Aggregate falls
    // through to the staged path where both ops are handled.
    let has_var_expand = plan
        .iter()
        .any(|op| matches!(op, PlanOp::VarExpand { .. } | PlanOp::ShortestPath { .. }));

    // GroupAggregate plans: streaming HashMap accumulator (O(groups) memory).
    // Checked before the bounded path and before single-aggregate so they are
    // never routed to pull.  Skipped for VarExpand plans (go to staged path).
    if !has_var_expand && plan.iter().any(|op| matches!(op, PlanOp::GroupAggregate { .. })) {
        return execute_group_aggregate(view, plan, params);
    }

    // Aggregate plans: streaming accumulator path (O(1) memory, no budget).
    // Checked before the bounded path so aggregates are never routed to pull.
    // Skipped for VarExpand plans — they go to the staged path instead.
    if !has_var_expand && plan.iter().any(|op| matches!(op, PlanOp::Aggregate { .. })) {
        return execute_aggregate(view, plan, params);
    }

    // Bounded plans use the pull-based (demand-driven) executor so that ALL
    // producer stages terminate as soon as `bound` final rows are collected.
    // VarExpand plans have row_bound=None, so this branch never fires for them.
    if let Some(bound) = row_bound {
        return execute_pull(view, plan, params, bound);
    }

    // Staged (unbounded) path — full materialisation with the 1 M safety cap.
    let vars = collect_vars(plan);
    let mut rows: Vec<Row> = vec![vec![None; vars.names.len()]];
    let mut projected: Option<Projected> = None;

    for op in plan {
        match op {
            PlanOp::ScanLabel { var, label } => {
                rows = scan_label(view, &vars, &rows, var, label.as_deref())?;
            }
            PlanOp::LookupProps { var, props } => {
                rows = retain_node(view, &vars, &rows, var, None, props, params)?;
            }
            PlanOp::JoinBound { var, label, props } => {
                rows = retain_node(view, &vars, &rows, var, label.as_deref(), props, params)?;
            }
            PlanOp::Expand { .. } => {
                rows = exec_expand(view, &vars, &rows, op, params)?;
            }
            PlanOp::VarExpand {
                from,
                rel_var,
                etype,
                dir,
                to,
                min,
                max,
            } => {
                rows = exec_var_expand(
                    view, &vars, &rows, from, rel_var, etype, *dir, to, *min, *max,
                )?;
            }
            PlanOp::ShortestPath {
                from,
                rel_var,
                etype,
                dir,
                to,
                max_hops,
            } => {
                rows = exec_shortest_path(
                    view, &vars, &rows, from, rel_var, etype, *dir, to, *max_hops,
                )?;
            }
            PlanOp::Filter { expr } => {
                rows = exec_filter(view, &vars, &rows, expr, params)?;
            }
            PlanOp::Project { items } => {
                projected = Some(exec_project(view, &vars, &rows, items)?);
            }
            PlanOp::OrderBy { items } => {
                let table = projected
                    .as_mut()
                    .ok_or_else(|| "ORDER BY before PROJECT".to_string())?;
                exec_order_by(table, items)?;
            }
            PlanOp::Skip(n) => {
                if let Some(table) = projected.as_mut() {
                    apply_skip(&mut table.rows, *n);
                } else {
                    apply_skip(&mut rows, *n);
                }
            }
            PlanOp::Limit(n) => {
                if let Some(table) = projected.as_mut() {
                    apply_limit(&mut table.rows, *n);
                } else {
                    apply_limit(&mut rows, *n);
                }
            }
            // GroupAggregate plans without VarExpand are routed to
            // execute_group_aggregate() before reaching the staged path.
            // Plans that combine VarExpand with GroupAggregate fall through
            // to here; group over the already-materialised rows.
            PlanOp::GroupAggregate { keys, aggs } => {
                let mut grp_groups: HashMap<GroupKey, GroupEntry> = HashMap::new();
                let mut grp_key_order: Vec<GroupKey> = Vec::new();
                for row in &rows {
                    let mut gk: GroupKey = Vec::with_capacity(keys.len());
                    let mut display_vals: Vec<Option<Value>> = Vec::with_capacity(keys.len());
                    for (_, item) in keys {
                        let val = project_item(view, &vars, row, item)?;
                        gk.push(val.as_ref().and_then(group_key_normalize));
                        display_vals.push(val);
                    }
                    if !grp_groups.contains_key(&gk) {
                        if grp_groups.len() >= max_groups() {
                            return Err(group_cap_err());
                        }
                        grp_key_order.push(gk.clone());
                        grp_groups.insert(
                            gk.clone(),
                            (display_vals, aggs.iter().map(|(f, _, _)| AggAcc::new(f)).collect()),
                        );
                    }
                    let (_, accs) = grp_groups.get_mut(&gk).unwrap();
                    for (acc, (func, arg, _)) in accs.iter_mut().zip(aggs.iter()) {
                        update_acc(view, &vars, row, func, arg, acc)?;
                    }
                }
                // Same openCypher rule as execute_group_aggregate: no-key multi-agg on
                // empty input must emit exactly one row with zero/null accumulators.
                if keys.is_empty() && grp_key_order.is_empty() {
                    let empty_key: GroupKey = vec![];
                    grp_key_order.push(empty_key.clone());
                    grp_groups.insert(
                        empty_key,
                        (vec![], aggs.iter().map(|(f, _, _)| AggAcc::new(f)).collect()),
                    );
                }
                projected = Some(build_group_projected(keys, aggs, grp_key_order, &mut grp_groups));
            }
            // Aggregate plans without VarExpand are routed to execute_aggregate()
            // before reaching the staged path.  Plans that combine VarExpand with
            // an Aggregate fall through to here; accumulate over the materialised
            // rows using the same agg_stream terminal logic (empty ops slice).
            PlanOp::Aggregate { func, arg, column } => {
                let ctx = AggStreamCtx {
                    view,
                    vars: &vars,
                    params,
                    func,
                    arg,
                };
                let mut acc = AggAcc::new(func);
                for row in &rows {
                    // agg_stream with empty ops hits the terminal branch (accumulate).
                    agg_stream(&ctx, &[], row, &mut acc)?;
                }
                let value = acc.finish();
                let mut rs = ResultSet::new(vec![column.clone()]);
                rs.push_row(vec![value]);
                return Ok(rs);
            }
        }
    }

    Ok(match projected {
        Some(table) => finish(table),
        None => ResultSet::new(vec![]),
    })
}

fn finish(table: Projected) -> ResultSet {
    let mut rs = ResultSet::new(table.columns);
    for row in table.rows {
        rs.push_row(row);
    }
    rs
}

fn check_params(plan: &[PlanOp], params: &Params) -> Result<(), String> {
    let mut names = Vec::new();
    let mut seen = BTreeSet::new();
    for op in plan {
        match op {
            PlanOp::LookupProps { props, .. }
            | PlanOp::JoinBound { props, .. }
            | PlanOp::Expand {
                to_props: props, ..
            } => {
                for (_, operand) in props {
                    collect_operand(operand, &mut names, &mut seen);
                }
            }
            PlanOp::Filter { expr } => collect_expr(expr, &mut names, &mut seen, 0)?,
            _ => {}
        }
    }
    for name in names {
        if !params.0.contains_key(&name) {
            return Err(format!("missing parameter `{name}`"));
        }
    }
    Ok(())
}

fn collect_operand(op: &Operand, names: &mut Vec<String>, seen: &mut BTreeSet<String>) {
    if let Operand::Param(n) = op {
        if seen.insert(n.clone()) {
            names.push(n.clone());
        }
    }
}

fn collect_expr(
    expr: &Expr,
    names: &mut Vec<String>,
    seen: &mut BTreeSet<String>,
    depth: u32,
) -> Result<(), String> {
    if depth > 256 {
        return Err("expression nesting too deep".into());
    }
    match expr {
        Expr::And(lhs, rhs) | Expr::Or(lhs, rhs) => {
            collect_expr(lhs, names, seen, depth + 1)?;
            collect_expr(rhs, names, seen, depth + 1)
        }
        Expr::Not(inner) => collect_expr(inner, names, seen, depth + 1),
        Expr::Cmp { lhs, rhs, .. } => {
            collect_operand(lhs, names, seen);
            collect_operand(rhs, names, seen);
            Ok(())
        }
    }
}

fn collect_vars(plan: &[PlanOp]) -> VarTable {
    let mut vars = VarTable { names: Vec::new() };
    for op in plan {
        match op {
            PlanOp::ScanLabel { var, .. } => {
                vars.intern(var);
            }
            PlanOp::LookupProps { var, props } | PlanOp::JoinBound { var, props, .. } => {
                vars.intern(var);
                for (_, operand) in props {
                    intern_operand(&mut vars, operand);
                }
            }
            PlanOp::Expand {
                from,
                rel_var,
                to,
                to_props,
                ..
            } => {
                vars.intern(from);
                vars.intern(to);
                if let Some(r) = rel_var {
                    vars.intern(r);
                }
                for (_, operand) in to_props {
                    intern_operand(&mut vars, operand);
                }
            }
            PlanOp::VarExpand {
                from,
                rel_var,
                to,
                ..
            } => {
                vars.intern(from);
                vars.intern(to);
                if let Some(r) = rel_var {
                    vars.intern(r);
                }
            }
            PlanOp::ShortestPath {
                from,
                rel_var,
                to,
                ..
            } => {
                vars.intern(from);
                vars.intern(to);
                if let Some(r) = rel_var {
                    vars.intern(r);
                }
            }
            PlanOp::Filter { expr } => intern_expr(&mut vars, expr),
            PlanOp::Project { items } => {
                for item in items {
                    match &item.value {
                        RetVal::Var(name) | RetVal::Prop { var: name, .. } => {
                            vars.intern(name);
                        }
                        RetVal::Agg { .. } => {} // handled by Aggregate op
                    }
                }
            }
            PlanOp::Aggregate { arg, .. } => match arg {
                AggArg::Star => {}
                AggArg::Var(v) => {
                    vars.intern(v);
                }
                AggArg::Prop { var, .. } => {
                    vars.intern(var);
                }
            },
            PlanOp::GroupAggregate { keys, aggs } => {
                for (_, item) in keys {
                    match &item.value {
                        RetVal::Var(name) | RetVal::Prop { var: name, .. } => {
                            vars.intern(name);
                        }
                        RetVal::Agg { .. } => {}
                    }
                }
                for (_, arg, _) in aggs {
                    match arg {
                        AggArg::Star => {}
                        AggArg::Var(v) => {
                            vars.intern(v);
                        }
                        AggArg::Prop { var, .. } => {
                            vars.intern(var);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    vars
}

fn intern_operand(vars: &mut VarTable, operand: &Operand) {
    if let Operand::Prop { var, .. } = operand {
        vars.intern(var);
    }
}

fn intern_expr(vars: &mut VarTable, expr: &Expr) {
    match expr {
        Expr::And(lhs, rhs) | Expr::Or(lhs, rhs) => {
            intern_expr(vars, lhs);
            intern_expr(vars, rhs);
        }
        Expr::Not(inner) => intern_expr(vars, inner),
        Expr::Cmp { lhs, rhs, .. } => {
            intern_operand(vars, lhs);
            intern_operand(vars, rhs);
        }
    }
}

fn scan_ids(view: &GraphView, label: Option<&str>) -> Vec<u32> {
    match label {
        Some(label) => view.nodes_with_label(label),
        // Real nodes always have labels; sentinel slots are gaps.
        None => (0..view.ids.len() as u32)
            .filter(|&id| view.label_of(id).is_some())
            .collect(),
    }
}

fn scan_label(
    view: &GraphView,
    vars: &VarTable,
    rows: &[Row],
    var: &str,
    label: Option<&str>,
) -> Result<Vec<Row>, String> {
    let ids = scan_ids(view, label);
    let slot = vars
        .slot(var)
        .ok_or_else(|| format!("unbound variable `{var}`"))?;
    let cap = max_intermediate_rows();
    let mut out = Vec::with_capacity(rows.len().saturating_mul(ids.len()).min(cap));
    for row in rows {
        for &id in &ids {
            if out.len() >= cap {
                return Err(row_cap_err(cap));
            }
            let mut next = row.clone();
            next[slot] = Some(Cell::Node(id));
            out.push(next);
        }
    }
    Ok(out)
}

fn require_cell<'a>(row: &'a Row, vars: &VarTable, var: &str) -> Result<&'a Cell, String> {
    let slot = vars
        .slot(var)
        .ok_or_else(|| format!("unbound variable `{var}`"))?;
    row.get(slot)
        .and_then(|c| c.as_ref())
        .ok_or_else(|| format!("unbound variable `{var}`"))
}

fn require_node(row: &Row, vars: &VarTable, var: &str) -> Result<u32, String> {
    match require_cell(row, vars, var)? {
        Cell::Node(id) => Ok(*id),
        Cell::Rel(_) => Err(format!("variable `{var}` is not a node")),
        Cell::Path(_) => Err(format!("variable `{var}` is a path, not a node")),
    }
}

fn resolve_operand(
    view: &GraphView,
    vars: &VarTable,
    row: &Row,
    operand: &Operand,
    params: &Params,
) -> Result<Option<Value>, String> {
    match operand {
        Operand::Lit(v) => Ok(Some(v.clone())),
        Operand::Param(name) => match params.0.get(name) {
            Some(v) => Ok(Some(v.clone())),
            None => Err(format!("missing parameter `{name}`")),
        },
        Operand::Prop { var, field } => resolve_prop(view, vars, row, var, field),
    }
}

fn resolve_prop(
    view: &GraphView,
    vars: &VarTable,
    row: &Row,
    var: &str,
    field: &str,
) -> Result<Option<Value>, String> {
    match require_cell(row, vars, var)? {
        Cell::Node(id) => Ok(view.prop(*id, field).cloned()),
        Cell::Rel(e) => Ok(view.edge_props.get(e.etype, e.src, e.dst, field).cloned()),
        // Virtual path cell: only `length` is exposed.
        Cell::Path(hops) => {
            if field == "length" {
                Ok(Some(Value::Int(*hops as i64)))
            } else {
                Ok(None)
            }
        }
    }
}

fn node_matches(
    view: &GraphView,
    vars: &VarTable,
    row: &Row,
    id: u32,
    label: Option<&str>,
    props: &[(String, Operand)],
    params: &Params,
) -> Result<bool, String> {
    if let Some(want) = label {
        match view.label_of(id) {
            Some(got) if got == want => {}
            _ => return Ok(false),
        }
    }
    for (field, operand) in props {
        let Some(expected) = resolve_operand(view, vars, row, operand, params)? else {
            return Ok(false);
        };
        match view.prop(id, field) {
            Some(got) if values_equal(got, &expected) => {}
            _ => return Ok(false),
        }
    }
    Ok(true)
}

fn retain_node(
    view: &GraphView,
    vars: &VarTable,
    rows: &[Row],
    var: &str,
    label: Option<&str>,
    props: &[(String, Operand)],
    params: &Params,
) -> Result<Vec<Row>, String> {
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let id = require_node(row, vars, var)?;
        if node_matches(view, vars, row, id, label, props, params)? {
            out.push(row.clone());
        }
    }
    Ok(out)
}

fn map_dir(dir: RelDir) -> Dir {
    match dir {
        RelDir::Right => Dir::Out,
        RelDir::Left => Dir::In,
        RelDir::Undirected => Dir::Both,
    }
}

fn neighbor(from: u32, e: &EdgeRef, dir: RelDir) -> u32 {
    match dir {
        RelDir::Right => e.dst,
        RelDir::Left => e.src,
        RelDir::Undirected => {
            if e.src == from {
                e.dst
            } else {
                e.src
            }
        }
    }
}

fn row_has_edge(row: &Row, e: &EdgeRef) -> bool {
    row.iter()
        .any(|c| matches!(c, Some(Cell::Rel(existing)) if existing == e))
}

fn resolve_etypes(view: &GraphView, etype: Option<&str>) -> Option<Vec<u32>> {
    etype.map(|name| view.syms.get(name).into_iter().collect())
}

fn exec_expand(
    view: &GraphView,
    vars: &VarTable,
    rows: &[Row],
    op: &PlanOp,
    params: &Params,
) -> Result<Vec<Row>, String> {
    let PlanOp::Expand {
        from,
        rel_var,
        etype,
        dir,
        to,
        to_label,
        to_props,
    } = op
    else {
        return Err("internal: expected Expand".into());
    };
    let etypes = resolve_etypes(view, etype.as_deref());
    let exp_dir = map_dir(*dir);
    let to_slot = vars
        .slot(to)
        .ok_or_else(|| format!("unbound variable `{to}`"))?;
    let rel_slot = rel_var.as_ref().and_then(|rv| vars.slot(rv));
    let cap = max_intermediate_rows();
    let mut out = Vec::with_capacity(rows.len().saturating_mul(2).min(cap));
    for row in rows {
        let from_id = require_node(row, vars, from)?;
        let bound_to = match row.get(to_slot).and_then(|c| c.as_ref()) {
            Some(Cell::Node(id)) => Some(*id),
            Some(Cell::Rel(_) | Cell::Path(_)) => return Err(format!("variable `{to}` is not a node")),
            None => None,
        };
        for e in expand(view, from_id, etypes.as_deref(), exp_dir) {
            if row_has_edge(row, &e) {
                continue;
            }
            let nbr = neighbor(from_id, &e, *dir);
            if let Some(want) = bound_to {
                if nbr != want {
                    continue;
                }
            }
            if !node_matches(view, vars, row, nbr, to_label.as_deref(), to_props, params)? {
                continue;
            }
            if out.len() >= cap {
                return Err(row_cap_err(cap));
            }
            let mut next = row.clone();
            if let Some(slot) = rel_slot {
                next[slot] = Some(Cell::Rel(e));
            }
            if bound_to.is_none() {
                next[to_slot] = Some(Cell::Node(nbr));
            }
            out.push(next);
            #[cfg(test)]
            record_expand_row();
        }
    }
    Ok(out)
}

/// BFS variable-length expand with per-path edge-uniqueness (Cypher
/// relationship isomorphism).  A single path never reuses the same `EdgeRef`;
/// node revisits are allowed.
///
/// Emits one row per (start, end, depth) combination where `min ≤ depth ≤ max`.
/// The 1 M intermediate-row budget applies to `out.len()`.
#[allow(clippy::too_many_arguments)]
fn exec_var_expand(
    view: &GraphView,
    vars: &VarTable,
    rows: &[Row],
    from: &str,
    rel_var: &Option<String>,
    etype: &Option<String>,
    dir: RelDir,
    to: &str,
    min: u8,
    max: u8,
) -> Result<Vec<Row>, String> {
    let etypes = resolve_etypes(view, etype.as_deref());
    let exp_dir = map_dir(dir);
    let to_slot = vars
        .slot(to)
        .ok_or_else(|| format!("unbound variable `{to}`"))?;
    let rel_slot = rel_var.as_ref().and_then(|rv| vars.slot(rv));
    let cap = max_intermediate_rows();
    let mut out: Vec<Row> = Vec::new();

    for row in rows {
        let from_id = require_node(row, vars, from)?;
        let bound_to = match row.get(to_slot).and_then(|c| c.as_ref()) {
            Some(Cell::Node(id)) => Some(*id),
            Some(_) => return Err(format!("variable `{to}` is not a node")),
            None => None,
        };

        // BFS state: (current_node_id, edges_used_in_this_path).
        // Vec<EdgeRef> is cheap for paths ≤ 10 edges; contains() is O(max) = O(10).
        struct PathState {
            node: u32,
            edges: Vec<EdgeRef>,
        }

        let mut frontier: Vec<PathState> = vec![PathState {
            node: from_id,
            edges: Vec::new(),
        }];

        // Running count of all PathStates ever retained in the frontier (across all
        // depths and all input rows).  Counted even during the pre-emission phase
        // (depth < min) so that high-min queries on dense graphs cannot exhaust RAM
        // before the budget fires.
        let mut frontier_count: usize = 0;

        for depth in 1u8..=max {
            let mut next_frontier: Vec<PathState> = Vec::new();
            for state in &frontier {
                for e in expand(view, state.node, etypes.as_deref(), exp_dir) {
                    // Per-path edge-uniqueness: reject edges already used in this path.
                    if state.edges.contains(&e) {
                        continue;
                    }
                    let nbr = neighbor(state.node, &e, dir);

                    // Emit a result row if we're within the requested depth range
                    // and the destination matches any bound constraint.
                    if depth >= min {
                        let dest_matches = match bound_to {
                            Some(want) => nbr == want,
                            None => true,
                        };
                        if dest_matches {
                            if out.len() >= cap {
                                return Err(row_cap_err(cap));
                            }
                            let mut next = row.clone();
                            if let Some(slot) = rel_slot {
                                next[slot] = Some(Cell::Path(depth));
                            }
                            next[to_slot] = Some(Cell::Node(nbr));
                            out.push(next);
                        }
                    }

                    // Continue expanding if we haven't hit the max depth yet.
                    // Count each retained PathState against the budget so that
                    // high-min queries on dense graphs are caught before emission.
                    if depth < max {
                        frontier_count += 1;
                        if frontier_count >= cap {
                            return Err(row_cap_err(cap));
                        }
                        let mut new_edges = state.edges.clone();
                        new_edges.push(e);
                        next_frontier.push(PathState {
                            node: nbr,
                            edges: new_edges,
                        });
                    }
                }
            }
            frontier = next_frontier;
            if frontier.is_empty() {
                break;
            }
        }
    }
    Ok(out)
}

/// BFS shortest-path between two already-bound nodes.
///
/// Uses standard BFS with a visited-node set for efficiency.  Since BFS
/// expands nodes level-by-level, the first time `to` is reached is the
/// shortest path.  Emits exactly 0 or 1 rows.
#[allow(clippy::too_many_arguments)]
fn exec_shortest_path(
    view: &GraphView,
    vars: &VarTable,
    rows: &[Row],
    from: &str,
    rel_var: &Option<String>,
    etype: &Option<String>,
    dir: RelDir,
    to: &str,
    max_hops: u8,
) -> Result<Vec<Row>, String> {
    let etypes = resolve_etypes(view, etype.as_deref());
    let exp_dir = map_dir(dir);
    let rel_slot = rel_var.as_ref().and_then(|rv| vars.slot(rv));
    let mut out: Vec<Row> = Vec::new();

    for row in rows {
        let from_id = require_node(row, vars, from)?;
        let to_id = require_node(row, vars, to)?;

        // BFS with visited-node tracking.
        let mut visited = std::collections::BTreeSet::new();
        visited.insert(from_id);
        let mut frontier: Vec<u32> = vec![from_id];

        'bfs: for depth in 1u8..=max_hops {
            let mut next_frontier: Vec<u32> = Vec::new();
            for &node in &frontier {
                for e in expand(view, node, etypes.as_deref(), exp_dir) {
                    let nbr = neighbor(node, &e, dir);
                    if nbr == to_id {
                        // Found the shortest path at this depth — emit one row and stop.
                        let mut next = row.clone();
                        if let Some(slot) = rel_slot {
                            next[slot] = Some(Cell::Path(depth));
                        }
                        out.push(next);
                        break 'bfs;
                    }
                    if !visited.contains(&nbr) {
                        visited.insert(nbr);
                        next_frontier.push(nbr);
                    }
                }
            }
            frontier = next_frontier;
            if frontier.is_empty() {
                break;
            }
        }
    }
    Ok(out)
}

fn exec_filter(
    view: &GraphView,
    vars: &VarTable,
    rows: &[Row],
    expr: &Expr,
    params: &Params,
) -> Result<Vec<Row>, String> {
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        if eval_expr(view, vars, row, expr, params, 0)? {
            out.push(row.clone());
        }
    }
    Ok(out)
}

/// Demand-driven executor for bounded plans (LIMIT without ORDER BY).
///
/// Locates the `Project` op, extracts the producer slice, and drives
/// `pull_rows` to collect up to `bound` (= SKIP + LIMIT) projected rows.
/// The 1 M intermediate-row cap is **not** applied here — it is the safety
/// net for the staged (unbounded) path only.
///
/// # PARTIAL-row caveat
///
/// `bound` counts *final projected* rows (post-filter, post-uniqueness).
/// A source row that is dropped by a Filter or by relationship-uniqueness
/// does **not** count toward the bound.  Execution may therefore visit
/// slightly more source rows than the bare LIMIT number, but will never
/// emit more than `bound` result rows.
/// Immutable context shared across all `pull_rows` recursive calls.
struct PullCtx<'a> {
    view: &'a GraphView<'a>,
    vars: &'a VarTable,
    project_items: &'a [RetItem],
    params: &'a Params<'a>,
    bound: usize,
}

fn execute_pull(
    view: &GraphView,
    plan: &[PlanOp],
    params: &Params,
    bound: usize,
) -> Result<ResultSet, String> {
    let proj_pos = match plan
        .iter()
        .position(|op| matches!(op, PlanOp::Project { .. }))
    {
        Some(p) => p,
        None => return Ok(ResultSet::new(vec![])),
    };
    let producers = &plan[..proj_pos];
    let project_items = match &plan[proj_pos] {
        PlanOp::Project { items } => items,
        _ => unreachable!(),
    };
    let columns: Vec<String> = project_items.iter().map(column_name).collect();
    let vars = collect_vars(plan);
    let ctx = PullCtx {
        view,
        vars: &vars,
        project_items,
        params,
        bound,
    };
    let mut initial_row: Row = vec![None; vars.names.len()];
    let mut result_rows: Vec<Vec<Option<Value>>> = Vec::with_capacity(bound);
    pull_rows(&ctx, producers, &mut initial_row, &mut result_rows)?;
    // SKIP: discard the leading rows (bound = SKIP+LIMIT ensures there are enough).
    let skip_n = plan[proj_pos + 1..]
        .iter()
        .find_map(|op| match op {
            PlanOp::Skip(n) => Some(*n),
            _ => None,
        })
        .unwrap_or(0);
    let skip_n = usize::try_from(skip_n).unwrap_or(usize::MAX);
    let mut rs = ResultSet::new(columns);
    for row in result_rows.into_iter().skip(skip_n) {
        rs.push_row(row);
    }
    Ok(rs)
}

// ─── Aggregate execution path ────────────────────────────────────────────────
//
// Aggregate plans stream through ALL matching rows one at a time and maintain
// a single accumulator value.  Memory is O(1) regardless of graph size:
//
//   - No binding table is materialised.
//   - The 1 M intermediate-row budget does **not** apply.  Applying it would
//     produce wrong counts/sums on large graphs and is unnecessary here because
//     memory is bounded by the accumulator, not by the number of rows.
//
// v1 scope: exactly one aggregate function per query, no grouping keys.

/// Running state for a single aggregate accumulation.
enum AggAcc {
    Count(u64),
    Sum { val: f64, has_value: bool },
    Avg { sum: f64, n: u64 },
    Min(Option<Value>),
    Max(Option<Value>),
}

impl AggAcc {
    fn new(func: &AggFunc) -> Self {
        match func {
            AggFunc::Count => AggAcc::Count(0),
            AggFunc::Sum => AggAcc::Sum {
                val: 0.0,
                has_value: false,
            },
            AggFunc::Avg => AggAcc::Avg { sum: 0.0, n: 0 },
            AggFunc::Min => AggAcc::Min(None),
            AggFunc::Max => AggAcc::Max(None),
        }
    }

    fn finish(self) -> Option<Value> {
        match self {
            // Saturating cast: a graph with >i64::MAX matched rows is not a
            // realistic concern today, but a silent wrapping cast would produce
            // a wrong (negative) count. Clamping to i64::MAX is the least-
            // surprising failure mode.
            AggAcc::Count(n) => Some(Value::Int(i64::try_from(n).unwrap_or(i64::MAX))),
            AggAcc::Sum { val, has_value } => {
                if has_value {
                    Some(Value::Float(val))
                } else {
                    None
                }
            }
            AggAcc::Avg { sum, n } => {
                if n > 0 {
                    Some(Value::Float(sum / n as f64))
                } else {
                    None
                }
            }
            AggAcc::Min(v) => v,
            AggAcc::Max(v) => v,
        }
    }
}

/// Extract a numeric (f64) value from a `Value`, returning `None` for null /
/// non-numeric types.  Silently skipped per the aggregate contract.
fn numeric_val(v: &Value) -> Option<f64> {
    match v {
        Value::Int(n) => Some(*n as f64),
        Value::Float(f) => Some(*f),
        _ => None,
    }
}

/// Shared context for `agg_stream`.
struct AggStreamCtx<'a> {
    view: &'a GraphView<'a>,
    vars: &'a VarTable,
    params: &'a Params<'a>,
    func: &'a AggFunc,
    arg: &'a AggArg,
}

/// Streaming accumulator executor for a single aggregate.
///
/// Locates the `Aggregate` op, builds the producer slice (everything before
/// it), then calls `agg_stream` to walk all matching rows and accumulate.
fn execute_aggregate(
    view: &GraphView,
    plan: &[PlanOp],
    params: &Params,
) -> Result<ResultSet, String> {
    let agg_pos = match plan
        .iter()
        .position(|op| matches!(op, PlanOp::Aggregate { .. }))
    {
        Some(p) => p,
        None => return Ok(ResultSet::new(vec![])),
    };
    let producers = &plan[..agg_pos];
    let (func, arg, column) = match &plan[agg_pos] {
        PlanOp::Aggregate { func, arg, column } => (func, arg, column),
        _ => unreachable!(),
    };
    let vars = collect_vars(plan);
    let ctx = AggStreamCtx {
        view,
        vars: &vars,
        params,
        func,
        arg,
    };
    let initial_row: Row = vec![None; vars.names.len()];
    let mut acc = AggAcc::new(func);
    agg_stream(&ctx, producers, &initial_row, &mut acc)?;

    let value = acc.finish();
    let mut rs = ResultSet::new(vec![column.clone()]);
    rs.push_row(vec![value]);
    Ok(rs)
}

/// Update a single accumulator for one matched row.
///
/// Shared by `agg_stream` (single-aggregate path) and `group_stream` (grouped
/// aggregation).  The logic mirrors openCypher conventions: null / non-numeric
/// values are silently skipped for SUM / AVG / MIN / MAX.
fn update_acc(
    view: &GraphView,
    vars: &VarTable,
    row: &Row,
    func: &AggFunc,
    arg: &AggArg,
    acc: &mut AggAcc,
) -> Result<(), String> {
    match (func, arg) {
        (AggFunc::Count, AggArg::Star) => {
            if let AggAcc::Count(n) = acc {
                *n += 1;
            }
        }
        (AggFunc::Count, AggArg::Var(v)) => {
            let slot = vars.slot(v);
            let is_bound = slot
                .and_then(|s| row.get(s))
                .and_then(|c| c.as_ref())
                .is_some();
            if is_bound {
                if let AggAcc::Count(n) = acc {
                    *n += 1;
                }
            }
        }
        (AggFunc::Count, AggArg::Prop { var, field }) => {
            let val = resolve_prop(view, vars, row, var, field)?;
            if val.is_some() {
                if let AggAcc::Count(n) = acc {
                    *n += 1;
                }
            }
        }
        (AggFunc::Sum, AggArg::Prop { var, field }) => {
            if let Some(v) = resolve_prop(view, vars, row, var, field)? {
                if let Some(num) = numeric_val(&v) {
                    if let AggAcc::Sum { val, has_value } = acc {
                        *val += num;
                        *has_value = true;
                    }
                }
            }
        }
        (AggFunc::Avg, AggArg::Prop { var, field }) => {
            if let Some(v) = resolve_prop(view, vars, row, var, field)? {
                if let Some(num) = numeric_val(&v) {
                    if let AggAcc::Avg { sum, n } = acc {
                        *sum += num;
                        *n += 1;
                    }
                }
            }
        }
        (AggFunc::Min, AggArg::Prop { var, field }) => {
            if let Some(v) = resolve_prop(view, vars, row, var, field)? {
                if numeric_val(&v).is_some() {
                    if let AggAcc::Min(current) = acc {
                        *current = Some(match current.take() {
                            None => v,
                            Some(prev) => {
                                if cmp_optional(Some(&prev), Some(&v), false)
                                    == std::cmp::Ordering::Greater
                                {
                                    v
                                } else {
                                    prev
                                }
                            }
                        });
                    }
                }
            }
        }
        (AggFunc::Max, AggArg::Prop { var, field }) => {
            if let Some(v) = resolve_prop(view, vars, row, var, field)? {
                if numeric_val(&v).is_some() {
                    if let AggAcc::Max(current) = acc {
                        *current = Some(match current.take() {
                            None => v,
                            Some(prev) => {
                                if cmp_optional(Some(&prev), Some(&v), true)
                                    == std::cmp::Ordering::Greater
                                {
                                    v
                                } else {
                                    prev
                                }
                            }
                        });
                    }
                }
            }
        }
        // Remaining combinations are rejected by the planner (e.g., SUM(*))
        // but handle defensively without panic.
        _ => {}
    }
    Ok(())
}

/// Recursively walk all matching rows through `ops`, updating `acc` for each
/// terminal row.  No bound — visits every row the producers can emit.
fn agg_stream(
    ctx: &AggStreamCtx<'_>,
    ops: &[PlanOp],
    row: &Row,
    acc: &mut AggAcc,
) -> Result<(), String> {
    let (op, rest) = match ops.split_first() {
        Some(pair) => pair,
        None => {
            // Terminal row — delegate to shared update_acc helper.
            update_acc(ctx.view, ctx.vars, row, ctx.func, ctx.arg, acc)?;
            return Ok(());
        }
    };

    match op {
        PlanOp::ScanLabel { var, label } => {
            let ids = scan_ids(ctx.view, label.as_deref());
            let slot = ctx
                .vars
                .slot(var)
                .ok_or_else(|| format!("unbound variable `{var}`"))?;
            for &id in &ids {
                let mut next = row.clone();
                next[slot] = Some(Cell::Node(id));
                agg_stream(ctx, rest, &next, acc)?;
            }
        }
        PlanOp::Expand {
            from,
            rel_var,
            etype,
            dir,
            to,
            to_label,
            to_props,
        } => {
            let etypes = resolve_etypes(ctx.view, etype.as_deref());
            let exp_dir = map_dir(*dir);
            let to_slot = ctx
                .vars
                .slot(to)
                .ok_or_else(|| format!("unbound variable `{to}`"))?;
            let rel_slot = rel_var.as_ref().and_then(|rv| ctx.vars.slot(rv));
            let from_id = require_node(row, ctx.vars, from)?;
            let bound_to = match row.get(to_slot).and_then(|c| c.as_ref()) {
                Some(Cell::Node(id)) => Some(*id),
                Some(Cell::Rel(_) | Cell::Path(_)) => return Err(format!("variable `{to}` is not a node")),
                None => None,
            };
            for e in expand(ctx.view, from_id, etypes.as_deref(), exp_dir) {
                if row_has_edge(row, &e) {
                    continue;
                }
                let nbr = neighbor(from_id, &e, *dir);
                if let Some(want) = bound_to {
                    if nbr != want {
                        continue;
                    }
                }
                if !node_matches(
                    ctx.view,
                    ctx.vars,
                    row,
                    nbr,
                    to_label.as_deref(),
                    to_props,
                    ctx.params,
                )? {
                    continue;
                }
                let mut next = row.clone();
                if let Some(slot) = rel_slot {
                    next[slot] = Some(Cell::Rel(e));
                }
                if bound_to.is_none() {
                    next[to_slot] = Some(Cell::Node(nbr));
                }
                agg_stream(ctx, rest, &next, acc)?;
            }
        }
        PlanOp::Filter { expr } => {
            if eval_expr(ctx.view, ctx.vars, row, expr, ctx.params, 0)? {
                agg_stream(ctx, rest, row, acc)?;
            }
        }
        PlanOp::LookupProps { var, props } => {
            let id = require_node(row, ctx.vars, var)?;
            if node_matches(ctx.view, ctx.vars, row, id, None, props, ctx.params)? {
                agg_stream(ctx, rest, row, acc)?;
            }
        }
        PlanOp::JoinBound { var, label, props } => {
            let id = require_node(row, ctx.vars, var)?;
            if node_matches(
                ctx.view,
                ctx.vars,
                row,
                id,
                label.as_deref(),
                props,
                ctx.params,
            )? {
                agg_stream(ctx, rest, row, acc)?;
            }
        }
        PlanOp::VarExpand {
            from,
            rel_var,
            etype,
            dir,
            to,
            min,
            max,
        } => {
            let new_rows =
                exec_var_expand(ctx.view, ctx.vars, std::slice::from_ref(row), from, rel_var, etype, *dir, to, *min, *max)?;
            for nr in &new_rows {
                agg_stream(ctx, rest, nr, acc)?;
            }
        }
        PlanOp::ShortestPath {
            from,
            rel_var,
            etype,
            dir,
            to,
            max_hops,
        } => {
            let new_rows =
                exec_shortest_path(ctx.view, ctx.vars, std::slice::from_ref(row), from, rel_var, etype, *dir, to, *max_hops)?;
            for nr in &new_rows {
                agg_stream(ctx, rest, nr, acc)?;
            }
        }
        // These must not appear in the producer slice of an aggregate plan.
        PlanOp::Project { .. } => {
            return Err(
                "agg executor: Project in producer slice — plan is structurally malformed"
                    .to_string(),
            );
        }
        PlanOp::OrderBy { .. } => {
            return Err(
                "agg executor: OrderBy in producer slice — structurally malformed".to_string(),
            );
        }
        PlanOp::Skip(n) => {
            return Err(format!(
                "agg executor: Skip({n}) in producer slice — structurally malformed"
            ));
        }
        PlanOp::Limit(n) => {
            return Err(format!(
                "agg executor: Limit({n}) in producer slice — structurally malformed"
            ));
        }
        PlanOp::Aggregate { .. } => {
            return Err(
                "agg executor: nested Aggregate in producer slice — structurally malformed"
                    .to_string(),
            );
        }
        PlanOp::GroupAggregate { .. } => {
            return Err(
                "agg executor: GroupAggregate in producer slice — structurally malformed"
                    .to_string(),
            );
        }
    }
    Ok(())
}

// ─── GroupAggregate execution path ──────────────────────────────────────────
//
// GroupAggregate plans stream through ALL matching rows, computing a group-key
// tuple per row and maintaining per-group accumulators in a HashMap.  Memory
// is O(distinct groups); the 1 M intermediate-row budget does not apply.

/// Context shared across all `group_stream` recursive calls.
struct GroupStreamCtx<'a> {
    view: &'a GraphView<'a>,
    vars: &'a VarTable,
    params: &'a Params<'a>,
    keys: &'a [(String, RetItem)],
    aggs: &'a [(AggFunc, AggArg, String)],
}

/// Build the `Projected` output table from a finished group map.
fn build_group_projected(
    keys: &[(String, RetItem)],
    aggs: &[(AggFunc, AggArg, String)],
    key_order: Vec<GroupKey>,
    groups: &mut HashMap<GroupKey, GroupEntry>,
) -> Projected {
    let columns: Vec<String> = keys
        .iter()
        .map(|(col, _)| col.clone())
        .chain(aggs.iter().map(|(_, _, col)| col.clone()))
        .collect();
    let mut rows: Vec<Vec<Option<Value>>> = Vec::with_capacity(key_order.len());
    for gk in key_order {
        let (display_keys, accs) = groups.remove(&gk).unwrap_or_default();
        let mut row: Vec<Option<Value>> = Vec::with_capacity(columns.len());
        // Use the first-seen original values for display; Int(42) stays Int(42)
        // even though it was normalized to FloatBits for hashing.
        for display_val in display_keys {
            row.push(display_val);
        }
        for acc in accs {
            row.push(acc.finish());
        }
        rows.push(row);
    }
    Projected { columns, rows }
}

/// Streaming grouped-aggregate executor.
///
/// Locates the `GroupAggregate` op, builds the producer slice (everything
/// before it), streams through all matching rows via `group_stream`, then
/// applies any `OrderBy` / `Skip` / `Limit` ops that follow.
fn execute_group_aggregate(
    view: &GraphView,
    plan: &[PlanOp],
    params: &Params,
) -> Result<ResultSet, String> {
    let gagg_pos = plan
        .iter()
        .position(|op| matches!(op, PlanOp::GroupAggregate { .. }))
        .ok_or_else(|| "internal: GroupAggregate op not found in plan".to_string())?;
    let producers = &plan[..gagg_pos];
    let (keys, aggs) = match &plan[gagg_pos] {
        PlanOp::GroupAggregate { keys, aggs } => (keys, aggs),
        _ => unreachable!(),
    };
    let tail = &plan[gagg_pos + 1..];
    let vars = collect_vars(plan);
    let initial_row: Row = vec![None; vars.names.len()];
    let mut groups: HashMap<GroupKey, GroupEntry> = HashMap::new();
    let mut key_order: Vec<GroupKey> = Vec::new();
    let ctx = GroupStreamCtx {
        view,
        vars: &vars,
        params,
        keys,
        aggs,
    };
    group_stream(&ctx, producers, &initial_row, &mut groups, &mut key_order)?;
    // openCypher: aggregates with no group-key items on empty input must
    // produce exactly one row (COUNT=0, SUM/AVG/etc.=null).  Seed the empty
    // key when no terminal rows arrived and there are no grouping keys.
    if keys.is_empty() && key_order.is_empty() {
        let empty_key: GroupKey = vec![];
        key_order.push(empty_key.clone());
        groups.insert(
            empty_key,
            (vec![], aggs.iter().map(|(f, _, _)| AggAcc::new(f)).collect()),
        );
    }
    let mut projected = build_group_projected(keys, aggs, key_order, &mut groups);
    // Apply OrderBy / Skip / Limit from the tail of the plan.
    for op in tail {
        match op {
            PlanOp::OrderBy { items } => exec_order_by(&mut projected, items)?,
            PlanOp::Skip(n) => apply_skip(&mut projected.rows, *n),
            PlanOp::Limit(n) => apply_limit(&mut projected.rows, *n),
            _ => {} // Ignore unexpected ops defensively.
        }
    }
    Ok(finish(projected))
}

/// Recursively walk all matching rows through `ops`, computing the group key
/// and updating per-group accumulators at each terminal row.
///
/// `groups` maps group key → per-group accumulator vector.
/// `key_order` tracks insertion order so output rows are deterministic when
/// no ORDER BY is requested.
fn group_stream(
    ctx: &GroupStreamCtx<'_>,
    ops: &[PlanOp],
    row: &Row,
    groups: &mut HashMap<GroupKey, GroupEntry>,
    key_order: &mut Vec<GroupKey>,
) -> Result<(), String> {
    let (op, rest) = match ops.split_first() {
        Some(pair) => pair,
        None => {
            // Terminal row: compute normalized group key for equality/hashing
            // and capture original values for display (first-seen wins).
            let mut gk: GroupKey = Vec::with_capacity(ctx.keys.len());
            let mut display_vals: Vec<Option<Value>> = Vec::with_capacity(ctx.keys.len());
            for (_, item) in ctx.keys {
                let val = project_item(ctx.view, ctx.vars, row, item)?;
                gk.push(val.as_ref().and_then(group_key_normalize));
                display_vals.push(val);
            }
            if !groups.contains_key(&gk) {
                if groups.len() >= max_groups() {
                    return Err(group_cap_err());
                }
                key_order.push(gk.clone());
                let init: Vec<AggAcc> =
                    ctx.aggs.iter().map(|(f, _, _)| AggAcc::new(f)).collect();
                groups.insert(gk.clone(), (display_vals, init));
            }
            let (_, accs) = groups.get_mut(&gk).unwrap();
            for (acc, (func, arg, _)) in accs.iter_mut().zip(ctx.aggs.iter()) {
                update_acc(ctx.view, ctx.vars, row, func, arg, acc)?;
            }
            return Ok(());
        }
    };

    match op {
        PlanOp::ScanLabel { var, label } => {
            let ids = scan_ids(ctx.view, label.as_deref());
            let slot = ctx
                .vars
                .slot(var)
                .ok_or_else(|| format!("unbound variable `{var}`"))?;
            for &id in &ids {
                let mut next = row.clone();
                next[slot] = Some(Cell::Node(id));
                group_stream(ctx, rest, &next, groups, key_order)?;
            }
        }
        PlanOp::Expand {
            from,
            rel_var,
            etype,
            dir,
            to,
            to_label,
            to_props,
        } => {
            let etypes = resolve_etypes(ctx.view, etype.as_deref());
            let exp_dir = map_dir(*dir);
            let to_slot = ctx
                .vars
                .slot(to)
                .ok_or_else(|| format!("unbound variable `{to}`"))?;
            let rel_slot = rel_var.as_ref().and_then(|rv| ctx.vars.slot(rv));
            let from_id = require_node(row, ctx.vars, from)?;
            let bound_to = match row.get(to_slot).and_then(|c| c.as_ref()) {
                Some(Cell::Node(id)) => Some(*id),
                Some(Cell::Rel(_) | Cell::Path(_)) => {
                    return Err(format!("variable `{to}` is not a node"))
                }
                None => None,
            };
            for e in expand(ctx.view, from_id, etypes.as_deref(), exp_dir) {
                if row_has_edge(row, &e) {
                    continue;
                }
                let nbr = neighbor(from_id, &e, *dir);
                if let Some(want) = bound_to {
                    if nbr != want {
                        continue;
                    }
                }
                if !node_matches(
                    ctx.view,
                    ctx.vars,
                    row,
                    nbr,
                    to_label.as_deref(),
                    to_props,
                    ctx.params,
                )? {
                    continue;
                }
                let mut next = row.clone();
                if let Some(slot) = rel_slot {
                    next[slot] = Some(Cell::Rel(e));
                }
                if bound_to.is_none() {
                    next[to_slot] = Some(Cell::Node(nbr));
                }
                group_stream(ctx, rest, &next, groups, key_order)?;
            }
        }
        PlanOp::Filter { expr } => {
            if eval_expr(ctx.view, ctx.vars, row, expr, ctx.params, 0)? {
                group_stream(ctx, rest, row, groups, key_order)?;
            }
        }
        PlanOp::LookupProps { var, props } => {
            let id = require_node(row, ctx.vars, var)?;
            if node_matches(ctx.view, ctx.vars, row, id, None, props, ctx.params)? {
                group_stream(ctx, rest, row, groups, key_order)?;
            }
        }
        PlanOp::JoinBound { var, label, props } => {
            let id = require_node(row, ctx.vars, var)?;
            if node_matches(
                ctx.view,
                ctx.vars,
                row,
                id,
                label.as_deref(),
                props,
                ctx.params,
            )? {
                group_stream(ctx, rest, row, groups, key_order)?;
            }
        }
        PlanOp::VarExpand {
            from,
            rel_var,
            etype,
            dir,
            to,
            min,
            max,
        } => {
            let new_rows = exec_var_expand(
                ctx.view,
                ctx.vars,
                std::slice::from_ref(row),
                from,
                rel_var,
                etype,
                *dir,
                to,
                *min,
                *max,
            )?;
            for nr in &new_rows {
                group_stream(ctx, rest, nr, groups, key_order)?;
            }
        }
        PlanOp::ShortestPath {
            from,
            rel_var,
            etype,
            dir,
            to,
            max_hops,
        } => {
            let new_rows = exec_shortest_path(
                ctx.view,
                ctx.vars,
                std::slice::from_ref(row),
                from,
                rel_var,
                etype,
                *dir,
                to,
                *max_hops,
            )?;
            for nr in &new_rows {
                group_stream(ctx, rest, nr, groups, key_order)?;
            }
        }
        // These ops must not appear in the producer slice of a GroupAggregate plan.
        PlanOp::Project { .. } => {
            return Err(
                "group executor: Project in producer slice — structurally malformed".to_string(),
            );
        }
        PlanOp::OrderBy { .. } => {
            return Err(
                "group executor: OrderBy in producer slice — structurally malformed".to_string(),
            );
        }
        PlanOp::Skip(n) => {
            return Err(format!(
                "group executor: Skip({n}) in producer slice — structurally malformed"
            ));
        }
        PlanOp::Limit(n) => {
            return Err(format!(
                "group executor: Limit({n}) in producer slice — structurally malformed"
            ));
        }
        PlanOp::Aggregate { .. } => {
            return Err(
                "group executor: Aggregate in producer slice — structurally malformed".to_string(),
            );
        }
        PlanOp::GroupAggregate { .. } => {
            return Err(
                "group executor: nested GroupAggregate in producer slice — structurally malformed"
                    .to_string(),
            );
        }
    }
    Ok(())
}

/// Recursively pull rows through `ops`, projecting into `result` until
/// `result.len() >= ctx.bound`.
///
/// Each arm binds one `PlanOp` and recurses on `rest`.  When `ops` is empty
/// (all producers consumed), the current `row` is projected and appended.
///
/// `row` is a mutable scratch buffer: each arm that assigns a slot saves and
/// restores it around the recursive call so the caller sees no net change.
/// This eliminates the per-row `Vec` clone that the staged path requires.
///
/// ScanLabel iterates `view.labels` directly (lazy, no intermediate `Vec<u32>`)
/// so the bound truncates the scan itself — scanning stops as soon as enough
/// result rows have been collected.
fn pull_rows(
    ctx: &PullCtx<'_>,
    ops: &[PlanOp],
    row: &mut Row,
    result: &mut Vec<Vec<Option<Value>>>,
) -> Result<(), String> {
    if result.len() >= ctx.bound {
        return Ok(());
    }
    let (op, rest) = match ops.split_first() {
        Some(pair) => pair,
        None => {
            // All producers consumed — project this final row.
            let mut cells = Vec::with_capacity(ctx.project_items.len());
            for item in ctx.project_items {
                cells.push(project_item(ctx.view, ctx.vars, row, item)?);
            }
            result.push(cells);
            return Ok(());
        }
    };
    match op {
        PlanOp::ScanLabel { var, label } => {
            let slot = ctx
                .vars
                .slot(var)
                .ok_or_else(|| format!("unbound variable `{var}`"))?;
            // Resolve the label to an interned symbol once. If the label is
            // specified but unknown, there are no matching nodes — return early.
            let want_sym = label.as_deref().and_then(|l| ctx.view.syms.get(l));
            if label.is_some() && want_sym.is_none() {
                return Ok(());
            }
            // Save the slot value so we can restore it after the loop.
            // Cell is Copy so this is a cheap register-level operation.
            let prev = row[slot];

            // Fast path: if the immediately following op is a simple
            // `Prop op Lit` comparison on this scan variable, fuse the filter
            // into the scan loop.  Pre-resolving the property column once
            // (hashing the field name once instead of per-node) eliminates the
            // outer HashMap string-hash on every candidate row.
            let fused_filter = rest.first().and_then(|next_op| {
                if let PlanOp::Filter {
                    expr:
                        Expr::Cmp {
                            lhs:
                                Operand::Prop {
                                    var: ref fv,
                                    field: ref f,
                                },
                            op: ref cmp_op_ref,
                            rhs: Operand::Lit(ref lit),
                        },
                } = *next_op
                {
                    if fv == var {
                        return Some((f.as_str(), cmp_op_ref, lit));
                    }
                }
                None
            });

            if let Some((field, cmp_op_ref, lit)) = fused_filter {
                // Fused scan+filter: column resolved once, comparison done
                // inline — no recursive call into pull_rows for the Filter arm.
                #[cfg(test)]
                FUSED_SCAN_FIRES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let col = ctx.view.props.column(field);
                let rest_after_filter = &rest[1..];
                for (i, &sym) in ctx.view.labels.iter().enumerate() {
                    if result.len() >= ctx.bound {
                        break;
                    }
                    if sym == u32::MAX {
                        continue;
                    }
                    if let Some(ws) = want_sym {
                        if sym != ws {
                            continue;
                        }
                    }
                    let id = i as u32;
                    if let Some(v) = col.get(id) {
                        if eval_cmp(cmp_op_ref, v, lit) {
                            row[slot] = Some(Cell::Node(id));
                            pull_rows(ctx, rest_after_filter, row, result)?;
                        }
                    }
                }
            } else {
                // Generic path: iterate label array lazily, no Vec allocation,
                // with early exit when the row bound is satisfied.
                for (i, &sym) in ctx.view.labels.iter().enumerate() {
                    if result.len() >= ctx.bound {
                        break;
                    }
                    // Skip tombstone / gap slots (u32::MAX sentinel).
                    if sym == u32::MAX {
                        continue;
                    }
                    // Filter by label symbol when a label is requested.
                    if let Some(ws) = want_sym {
                        if sym != ws {
                            continue;
                        }
                    }
                    row[slot] = Some(Cell::Node(i as u32));
                    pull_rows(ctx, rest, row, result)?;
                }
            }
            // SAFETY: the `?` inside both scan loops propagates Err directly to
            // `execute_pull`, which drops `initial_row` — the skipped restore is
            // unobservable.  This invariant MUST stay true: no call site between
            // here and `execute_pull` may catch an Err and re-use the row.
            // Future refactors adding mid-stream error recovery must audit this site.
            row[slot] = prev;
        }
        PlanOp::Expand {
            from,
            rel_var,
            etype,
            dir,
            to,
            to_label,
            to_props,
        } => {
            let etypes = resolve_etypes(ctx.view, etype.as_deref());
            let exp_dir = map_dir(*dir);
            let to_slot = ctx
                .vars
                .slot(to)
                .ok_or_else(|| format!("unbound variable `{to}`"))?;
            let rel_slot = rel_var.as_ref().and_then(|rv| ctx.vars.slot(rv));
            let from_id = require_node(row, ctx.vars, from)?;
            let bound_to = match row.get(to_slot).and_then(|c| c.as_ref()) {
                Some(Cell::Node(id)) => Some(*id),
                Some(Cell::Rel(_) | Cell::Path(_)) => return Err(format!("variable `{to}` is not a node")),
                None => None,
            };
            for e in expand(ctx.view, from_id, etypes.as_deref(), exp_dir) {
                if result.len() >= ctx.bound {
                    break;
                }
                if row_has_edge(row, &e) {
                    continue;
                }
                let nbr = neighbor(from_id, &e, *dir);
                if let Some(want) = bound_to {
                    if nbr != want {
                        continue;
                    }
                }
                if !node_matches(
                    ctx.view,
                    ctx.vars,
                    row,
                    nbr,
                    to_label.as_deref(),
                    to_props,
                    ctx.params,
                )? {
                    continue;
                }
                let mut next = row.clone();
                if let Some(slot) = rel_slot {
                    next[slot] = Some(Cell::Rel(e));
                }
                if bound_to.is_none() {
                    next[to_slot] = Some(Cell::Node(nbr));
                }
                #[cfg(test)]
                record_expand_row();
                pull_rows(ctx, rest, &mut next, result)?;
            }
        }
        PlanOp::Filter { expr } => {
            if eval_expr(ctx.view, ctx.vars, row, expr, ctx.params, 0)? {
                pull_rows(ctx, rest, row, result)?;
            }
        }
        PlanOp::LookupProps { var, props } => {
            let id = require_node(row, ctx.vars, var)?;
            if node_matches(ctx.view, ctx.vars, row, id, None, props, ctx.params)? {
                pull_rows(ctx, rest, row, result)?;
            }
        }
        PlanOp::JoinBound { var, label, props } => {
            let id = require_node(row, ctx.vars, var)?;
            if node_matches(
                ctx.view,
                ctx.vars,
                row,
                id,
                label.as_deref(),
                props,
                ctx.params,
            )? {
                pull_rows(ctx, rest, row, result)?;
            }
        }
        // The ops below must never appear inside the producers slice that
        // pull_rows receives.  Explicitly reject each so that adding a new
        // PlanOp variant to the enum forces a compile-time decision here
        // rather than silently falling through and producing wrong results.
        PlanOp::Project { .. } => {
            return Err(
                "pull executor: Project reached pull_rows — plan is structurally malformed"
                    .to_string(),
            );
        }
        PlanOp::OrderBy { .. } => {
            return Err(
                "pull executor: OrderBy reached pull_rows — queries with ORDER BY \
                 must use the staged path (row_bound returns None)"
                    .to_string(),
            );
        }
        PlanOp::Skip(n) => {
            return Err(format!(
                "pull executor: Skip({n}) reached pull_rows — Skip must appear after Project"
            ));
        }
        PlanOp::Limit(n) => {
            return Err(format!(
                "pull executor: Limit({n}) reached pull_rows — Limit must appear after Project"
            ));
        }
        PlanOp::Aggregate { .. } => {
            return Err(
                "pull executor: Aggregate reached pull_rows — aggregate plans must use \
                 the execute_aggregate path (routed before pull in execute_inner)"
                    .to_string(),
            );
        }
        // VarExpand and ShortestPath always take the staged path (row_bound()
        // returns None for plans containing these ops, so pull_rows is never
        // called with them in the producer slice).  This arm exists so that
        // adding new variants to PlanOp forces a compile-time decision here.
        PlanOp::VarExpand { .. } => {
            return Err(
                "pull executor: VarExpand reached pull_rows — variable-length path \
                 plans must use the staged path (row_bound returns None)"
                    .to_string(),
            );
        }
        PlanOp::ShortestPath { .. } => {
            return Err(
                "pull executor: ShortestPath reached pull_rows — shortestPath plans \
                 must use the staged path (row_bound returns None)"
                    .to_string(),
            );
        }
        PlanOp::GroupAggregate { .. } => {
            return Err(
                "pull executor: GroupAggregate reached pull_rows — grouped aggregate plans \
                 must use the execute_group_aggregate path (routed before pull in execute_inner)"
                    .to_string(),
            );
        }
    }
    Ok(())
}

fn eval_expr(
    view: &GraphView,
    vars: &VarTable,
    row: &Row,
    expr: &Expr,
    params: &Params,
    depth: u32,
) -> Result<bool, String> {
    if depth > 256 {
        return Err("expression nesting too deep".into());
    }
    match expr {
        Expr::And(lhs, rhs) => {
            let l = eval_expr(view, vars, row, lhs, params, depth + 1)?;
            let r = eval_expr(view, vars, row, rhs, params, depth + 1)?;
            Ok(l && r)
        }
        Expr::Or(lhs, rhs) => {
            let l = eval_expr(view, vars, row, lhs, params, depth + 1)?;
            let r = eval_expr(view, vars, row, rhs, params, depth + 1)?;
            Ok(l || r)
        }
        Expr::Not(inner) => Ok(!eval_expr(view, vars, row, inner, params, depth + 1)?),
        Expr::Cmp { lhs, op, rhs } => {
            let l = resolve_operand(view, vars, row, lhs, params)?;
            let r = resolve_operand(view, vars, row, rhs, params)?;
            match (l, r) {
                (Some(a), Some(b)) => Ok(eval_cmp(op, &a, &b)),
                _ => Ok(false),
            }
        }
    }
}

fn column_name(item: &RetItem) -> String {
    if let Some(alias) = &item.alias {
        return alias.clone();
    }
    match &item.value {
        RetVal::Var(v) => v.clone(),
        RetVal::Prop { var, field } => format!("{var}.{field}"),
        // Agg column names are computed by the planner and stored in Aggregate.column;
        // this branch is unreachable for well-formed plans but needed for exhaustiveness.
        RetVal::Agg { func, arg } => {
            use crate::cypher::ast::AggArg;
            let f = match func {
                AggFunc::Count => "COUNT",
                AggFunc::Sum => "SUM",
                AggFunc::Avg => "AVG",
                AggFunc::Min => "MIN",
                AggFunc::Max => "MAX",
            };
            let a = match arg {
                AggArg::Star => "*".to_string(),
                AggArg::Var(v) => v.clone(),
                AggArg::Prop { var, field } => format!("{var}.{field}"),
            };
            format!("{f}({a})")
        }
    }
}

fn exec_project(
    view: &GraphView,
    vars: &VarTable,
    rows: &[Row],
    items: &[RetItem],
) -> Result<Projected, String> {
    let columns: Vec<String> = items.iter().map(column_name).collect();
    let mut out_rows = Vec::with_capacity(rows.len());
    for row in rows {
        let mut cells = Vec::with_capacity(items.len());
        for item in items {
            cells.push(project_item(view, vars, row, item)?);
        }
        out_rows.push(cells);
    }
    Ok(Projected {
        columns,
        rows: out_rows,
    })
}

fn project_item(
    view: &GraphView,
    vars: &VarTable,
    row: &Row,
    item: &RetItem,
) -> Result<Option<Value>, String> {
    match &item.value {
        RetVal::Var(v) => {
            let id = require_node(row, vars, v)?;
            match view.ids.key_of(id) {
                Some(key) => Ok(Some(Value::Str(key.to_owned()))),
                None => Err(format!("unknown node id {id}")),
            }
        }
        RetVal::Prop { var, field } => resolve_prop(view, vars, row, var, field),
        // Agg items are never projected by exec_project (aggregate plans have no
        // Project op). This arm exists solely to satisfy the exhaustive match.
        RetVal::Agg { .. } => Err(
            "project_item: Agg variant reached exec_project — aggregate plans must not contain Project"
                .to_string(),
        ),
    }
}

fn order_column(item: &OrderItem) -> String {
    match &item.target {
        OrderTarget::Alias(name) | OrderTarget::Var(name) => name.clone(),
        OrderTarget::Prop { var, field } => format!("{var}.{field}"),
    }
}

fn exec_order_by(table: &mut Projected, items: &[OrderItem]) -> Result<(), String> {
    let mut keys = Vec::with_capacity(items.len());
    for item in items {
        let name = order_column(item);
        let idx = table
            .columns
            .iter()
            .position(|c| c == &name)
            .ok_or_else(|| format!("ORDER BY target `{name}` is not a projected column"))?;
        keys.push((idx, item.descending));
    }
    table.rows.sort_by(|a, b| {
        for &(idx, desc) in &keys {
            let c = cmp_optional(
                a.get(idx).and_then(|x| x.as_ref()),
                b.get(idx).and_then(|x| x.as_ref()),
                desc,
            );
            if c != std::cmp::Ordering::Equal {
                return c;
            }
        }
        std::cmp::Ordering::Equal
    });
    Ok(())
}

fn apply_skip<T>(rows: &mut Vec<T>, n: u64) {
    let n = usize::try_from(n).unwrap_or(usize::MAX);
    if n >= rows.len() {
        rows.clear();
    } else {
        rows.drain(0..n);
    }
}

fn apply_limit<T>(rows: &mut Vec<T>, n: u64) {
    let n = usize::try_from(n).unwrap_or(usize::MAX);
    rows.truncate(n);
}

#[cfg(test)]
mod tests {
    use super::{execute, Params};
    use crate::cypher::ast::{Operand, OrderItem, OrderTarget, RetItem, RetVal};
    use crate::cypher::plan::{plan, PlanOp};
    use crate::cypher::{lex, parse, RelDir};
    use crate::result::ResultSet;
    use crate::view::GraphView;
    use core_storage::{ColumnStore, EdgeProps, IdMap, Interner, Topology, Value};
    use proptest::prelude::*;
    use std::collections::BTreeMap;

    struct Fx {
        ids: IdMap,
        syms: Interner,
        labels: Vec<u32>,
        props: ColumnStore,
        topo: Topology,
        eprops: EdgeProps,
    }

    impl Fx {
        fn new() -> Self {
            Fx {
                ids: IdMap::new(),
                syms: Interner::new(),
                labels: vec![],
                props: ColumnStore::new(),
                topo: Topology::new(),
                eprops: EdgeProps::new(),
            }
        }

        fn add(&mut self, label: &str, key: &str, props: Vec<(&str, Value)>) -> u32 {
            let id = self.ids.get_or_insert(key);
            let sym = self.syms.intern(label);
            self.labels.resize(id as usize + 1, u32::MAX);
            self.labels[id as usize] = sym;
            for (f, v) in props {
                self.props.set(id, f, v);
            }
            id
        }

        fn edge(&mut self, etype: &str, src: u32, dst: u32, props: Vec<(&str, Value)>) {
            let et = self.syms.intern(etype);
            self.topo.add_edge(et, src, dst);
            for (f, v) in props {
                self.eprops.set(et, src, dst, f, v);
            }
        }

        fn view(&self) -> GraphView<'_> {
            GraphView {
                ids: &self.ids,
                syms: &self.syms,
                labels: &self.labels,
                props: &self.props,
                topo: &self.topo,
                edge_props: &self.eprops,
            }
        }
    }

    fn compile(src: &str) -> Vec<PlanOp> {
        plan(&parse(&lex(src).expect("lex")).expect("parse")).expect("plan")
    }

    fn run(
        view: &GraphView,
        src: &str,
        params: &BTreeMap<String, Value>,
    ) -> Result<ResultSet, String> {
        execute(view, &compile(src), &Params(params))
    }

    fn s(v: &str) -> Value {
        Value::Str(v.into())
    }

    fn f(v: f64) -> Value {
        Value::Float(v)
    }

    fn i(v: i64) -> Value {
        Value::Int(v)
    }

    fn rows_of(rs: &ResultSet) -> Vec<Vec<Option<Value>>> {
        (0..rs.len()).map(|i| rs.row(i).to_vec()).collect()
    }

    fn col(rs: &ResultSet, name: &str) -> Vec<Option<Value>> {
        (0..rs.len()).map(|i| rs.get(i, name).cloned()).collect()
    }

    fn hop_graph() -> Fx {
        let mut fx = Fx::new();
        let ada = fx.add("Person", "ada", vec![]);
        let bob = fx.add("Person", "bob", vec![]);
        let cam = fx.add("Person", "cam", vec![]);
        let acme = fx.add("Company", "acme", vec![]);
        fx.edge("KNOWS", ada, bob, vec![]);
        fx.edge("KNOWS", ada, cam, vec![]);
        fx.edge("KNOWS", bob, cam, vec![]);
        fx.edge("LIKES", ada, acme, vec![]);
        fx
    }

    fn undirected_graph() -> Fx {
        let mut fx = Fx::new();
        let a = fx.add("N", "a", vec![]);
        let b = fx.add("N", "b", vec![]);
        fx.edge("T", a, b, vec![("w", i(42))]);
        fx
    }

    fn triangle() -> Fx {
        let mut fx = Fx::new();
        let a = fx.add("N", "a", vec![]);
        let b = fx.add("N", "b", vec![]);
        let c = fx.add("N", "c", vec![]);
        fx.edge("T", a, b, vec![("eid", i(1))]);
        fx.edge("T", b, c, vec![("eid", i(2))]);
        fx.edge("T", c, a, vec![("eid", i(3))]);
        fx
    }

    fn single_edge() -> (Fx, u32, u32) {
        let mut fx = Fx::new();
        let a = fx.add("N", "a", vec![]);
        let b = fx.add("N", "b", vec![]);
        fx.edge("T", a, b, vec![]);
        (fx, a, b)
    }

    /// Companies with scored INDUSTRY_ALIGNMENT / SPECIALTY_MATCH edges to t1.
    fn dogfood_graph() -> Fx {
        let mut fx = Fx::new();
        let t1 = fx.add("Talent", "t1", vec![("id", s("t1"))]);
        let acme = fx.add("Company", "acme", vec![]);
        let beta = fx.add("Company", "beta", vec![]);
        let gamma = fx.add("Company", "gamma", vec![]);
        let delta = fx.add("Company", "delta", vec![]);
        let echo = fx.add("Company", "echo", vec![]);
        let foxtrot = fx.add("Company", "foxtrot", vec![]);
        let zeta = fx.add("Company", "zeta", vec![]);
        fx.edge("INDUSTRY_ALIGNMENT", acme, t1, vec![("score", f(0.9))]);
        fx.edge("SPECIALTY_MATCH", acme, t1, vec![("score", f(0.8))]);
        fx.edge("INDUSTRY_ALIGNMENT", beta, t1, vec![("score", f(0.6))]);
        fx.edge("SPECIALTY_MATCH", beta, t1, vec![("score", f(0.7))]);
        fx.edge("INDUSTRY_ALIGNMENT", gamma, t1, vec![("score", f(0.4))]);
        fx.edge("SPECIALTY_MATCH", gamma, t1, vec![("score", f(0.9))]);
        fx.edge("INDUSTRY_ALIGNMENT", delta, t1, vec![("score", f(0.8))]);
        fx.edge("SPECIALTY_MATCH", delta, t1, vec![("score", f(0.3))]);
        fx.edge("INDUSTRY_ALIGNMENT", echo, t1, vec![("score", f(0.5))]);
        fx.edge("SPECIALTY_MATCH", echo, t1, vec![("score", f(0.5))]);
        fx.edge("INDUSTRY_ALIGNMENT", foxtrot, t1, vec![("score", f(0.95))]);
        fx.edge("INDUSTRY_ALIGNMENT", zeta, t1, vec![("score", f(0.9))]);
        fx.edge("SPECIALTY_MATCH", zeta, t1, vec![("score", f(0.6))]);
        fx
    }

    const DOGFOOD: &str = "\
MATCH (t:Talent {id: $tid}) \
MATCH (c:Company)-[i:INDUSTRY_ALIGNMENT]->(t) \
MATCH (c)-[s:SPECIALTY_MATCH]->(t) \
WHERE i.score >= 0.5 AND s.score >= 0.5 \
RETURN c, i.score AS industry, s.score AS specialty \
ORDER BY industry DESC, specialty DESC \
LIMIT 10";

    fn tid_params() -> BTreeMap<String, Value> {
        let mut p = BTreeMap::new();
        p.insert("tid".into(), s("t1"));
        p
    }

    #[test]
    fn single_hop_match_label_and_etype_filters() {
        let fx = hop_graph();
        let v = fx.view();
        let rs = run(
            &v,
            "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a, b",
            &BTreeMap::new(),
        )
        .expect("single-hop");
        assert_eq!(rs.columns(), &["a".to_string(), "b".to_string()]);
        assert_eq!(
            rows_of(&rs),
            vec![
                vec![Some(s("ada")), Some(s("bob"))],
                vec![Some(s("ada")), Some(s("cam"))],
                vec![Some(s("bob")), Some(s("cam"))],
            ]
        );
        // dest label filters out ada -LIKES-> acme; etype filters it too
        let likes = run(
            &v,
            "MATCH (a:Person)-[:LIKES]->(b:Company) RETURN a, b",
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(rows_of(&likes), vec![vec![Some(s("ada")), Some(s("acme"))]]);
        let no_combo = run(
            &v,
            "MATCH (a:Person)-[:KNOWS]->(b:Company) RETURN a, b",
            &BTreeMap::new(),
        )
        .unwrap();
        assert!(no_combo.is_empty());
    }

    #[test]
    fn undirected_match_finds_both_orientations_and_binds_true_triple() {
        let fx = undirected_graph();
        let v = fx.view();
        // w lives only on the true (T, a, b) triple. A reversed (T, b, a)
        // binding would project None.
        let rs =
            run(&v, "MATCH (x)-[r:T]-(y) RETURN x, y, r.w", &BTreeMap::new()).expect("undirected");
        assert_eq!(
            rows_of(&rs),
            vec![
                vec![Some(s("a")), Some(s("b")), Some(i(42))],
                vec![Some(s("b")), Some(s("a")), Some(i(42))],
            ]
        );

        let left = run(
            &v,
            "MATCH (y)<-[r:T]-(x) RETURN x, y, r.w",
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(
            rows_of(&left),
            vec![vec![Some(s("a")), Some(s("b")), Some(i(42))]]
        );
    }

    #[test]
    fn relationship_uniqueness_triangle_and_two_hop_cycle() {
        let tri = triangle();
        let v = tri.view();
        let rs = run(
            &v,
            "MATCH (x)-[r1:T]->(y)-[r2:T]->(z) RETURN x, y, z, r1.eid, r2.eid",
            &BTreeMap::new(),
        )
        .expect("triangle 2-hop");
        assert_eq!(
            rows_of(&rs),
            vec![
                vec![
                    Some(s("a")),
                    Some(s("b")),
                    Some(s("c")),
                    Some(i(1)),
                    Some(i(2))
                ],
                vec![
                    Some(s("b")),
                    Some(s("c")),
                    Some(s("a")),
                    Some(i(2)),
                    Some(i(3))
                ],
                vec![
                    Some(s("c")),
                    Some(s("a")),
                    Some(s("b")),
                    Some(i(3)),
                    Some(i(1))
                ],
            ]
        );
        for row in rows_of(&rs) {
            assert_ne!(row[3], row[4], "r1 must never bind the same edge as r2");
        }

        let (mut one, a, b) = single_edge();
        let v = one.view();
        let cycle = run(
            &v,
            "MATCH (x)-[:T]->(y)-[:T]->(x) RETURN x",
            &BTreeMap::new(),
        )
        .expect("2-hop cycle");
        assert!(
            cycle.is_empty(),
            "single directed edge cannot close a 2-hop cycle"
        );

        // Undirected 2-hop back would reuse the only EdgeRef without uniqueness.
        let undirected_cycle = run(&v, "MATCH (x)-[:T]-(y)-[:T]-(x) RETURN x", &BTreeMap::new())
            .expect("undirected uniqueness");
        assert!(
            undirected_cycle.is_empty(),
            "relationship uniqueness must reject walking the same triple back"
        );

        one.edge("T", b, a, vec![]);
        let v = one.view();
        let with_recip = run(
            &v,
            "MATCH (x)-[:T]->(y)-[:T]->(x) RETURN x",
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(col(&with_recip, "x"), vec![Some(s("a")), Some(s("b"))]);
    }

    #[test]
    fn multi_match_join_bound_and_bound_destination_expand() {
        let fx = dogfood_graph();
        let v = fx.view();
        // No WHERE: any company with *both* edge types into the bound talent.
        // foxtrot has industry only → dropped by the second (bound-dest) expand.
        let rs = run(
            &v,
            "MATCH (t:Talent {id: $tid}) \
             MATCH (c:Company)-[i:INDUSTRY_ALIGNMENT]->(t) \
             MATCH (c)-[s:SPECIALTY_MATCH]->(t) \
             RETURN c",
            &tid_params(),
        )
        .expect("join + bound dest");
        assert_eq!(
            col(&rs, "c"),
            vec![
                Some(s("acme")),
                Some(s("beta")),
                Some(s("gamma")),
                Some(s("delta")),
                Some(s("echo")),
                Some(s("zeta")),
            ]
        );

        // Empty JoinBound (MATCH 2 start already bound, no label/props) keeps all.
        let keep = run(&v, "MATCH (c:Company) MATCH (c) RETURN c", &BTreeMap::new()).unwrap();
        assert_eq!(keep.len(), 7);
        // Label re-check on JoinBound drops everything.
        let drop = run(
            &v,
            "MATCH (c:Company) MATCH (c:Talent) RETURN c",
            &BTreeMap::new(),
        )
        .unwrap();
        assert!(drop.is_empty());
    }

    #[test]
    fn rel_var_edge_prop_filter() {
        let mut fx = Fx::new();
        let a = fx.add("N", "a", vec![]);
        let b = fx.add("N", "b", vec![]);
        let c = fx.add("N", "c", vec![]);
        fx.edge("T", a, b, vec![("w", f(0.7))]);
        fx.edge("T", a, c, vec![("w", f(0.3))]);
        let v = fx.view();
        let rs = run(
            &v,
            "MATCH (x)-[r:T]->(y) WHERE r.w >= 0.5 RETURN y, r.w",
            &BTreeMap::new(),
        )
        .expect("edge-prop filter");
        assert_eq!(rows_of(&rs), vec![vec![Some(s("b")), Some(f(0.7))]]);
        let fail = run(
            &v,
            "MATCH (x)-[r:T]->(y) WHERE r.w >= 0.8 RETURN y",
            &BTreeMap::new(),
        )
        .unwrap();
        assert!(fail.is_empty());
    }

    #[test]
    fn params_present_resolve_missing_is_err_before_rows() {
        let fx = dogfood_graph();
        let v = fx.view();
        let hit =
            run(&v, "MATCH (t:Talent {id: $tid}) RETURN t", &tid_params()).expect("present param");
        assert_eq!(col(&hit, "t"), vec![Some(s("t1"))]);

        // Unknown label would yield Ok(empty) if params were not walked first.
        let err = run(
            &v,
            "MATCH (t:NoSuchLabel {id: $tid}) RETURN t",
            &BTreeMap::new(),
        )
        .expect_err("missing param must be Err, not Ok(empty)");
        assert!(
            err.contains("tid")
                && (err.contains("param") || err.contains("Param") || err.contains("missing")),
            "missing-param error must name the parameter, got: {err}"
        );

        let err = run(
            &v,
            "MATCH (t:Talent) WHERE t.id = $tid RETURN t",
            &BTreeMap::new(),
        )
        .expect_err("missing WHERE param");
        assert!(err.contains("tid"), "got: {err}");
    }

    #[test]
    fn order_by_none_last_then_skip_limit() {
        let mut fx = Fx::new();
        fx.add("Person", "ada", vec![("age", i(30))]);
        fx.add("Person", "bob", vec![]); // missing age → None
        fx.add("Person", "cam", vec![("age", i(10))]);
        fx.add("Person", "dan", vec![("age", i(20))]);
        let v = fx.view();

        let asc = run(
            &v,
            "MATCH (p:Person) RETURN p, p.age AS age ORDER BY age",
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(
            rows_of(&asc),
            vec![
                vec![Some(s("cam")), Some(i(10))],
                vec![Some(s("dan")), Some(i(20))],
                vec![Some(s("ada")), Some(i(30))],
                vec![Some(s("bob")), None],
            ]
        );

        let desc = run(
            &v,
            "MATCH (p:Person) RETURN p, p.age AS age ORDER BY age DESC",
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(
            rows_of(&desc),
            vec![
                vec![Some(s("ada")), Some(i(30))],
                vec![Some(s("dan")), Some(i(20))],
                vec![Some(s("cam")), Some(i(10))],
                vec![Some(s("bob")), None],
            ]
        );

        let skip_lim = run(
            &v,
            "MATCH (p:Person) RETURN p, p.age AS age ORDER BY age SKIP 1 LIMIT 2",
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(
            rows_of(&skip_lim),
            vec![
                vec![Some(s("dan")), Some(i(20))],
                vec![Some(s("ada")), Some(i(30))],
            ]
        );

        let desc_sl = run(
            &v,
            "MATCH (p:Person) RETURN p, p.age AS age ORDER BY age DESC SKIP 1 LIMIT 2",
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(
            rows_of(&desc_sl),
            vec![
                vec![Some(s("dan")), Some(i(20))],
                vec![Some(s("cam")), Some(i(10))],
            ]
        );
    }

    #[test]
    fn unknown_label_and_etype_are_ok_empty() {
        let fx = hop_graph();
        let v = fx.view();
        let lab = run(&v, "MATCH (x:Nope) RETURN x", &BTreeMap::new()).expect("unknown label");
        assert!(lab.is_empty());
        let et = run(
            &v,
            "MATCH (a)-[:NO_SUCH_ETYPE]->(b) RETURN a",
            &BTreeMap::new(),
        )
        .expect("unknown etype");
        assert!(et.is_empty());
    }

    #[test]
    fn execute_is_deterministic() {
        let fx = dogfood_graph();
        let v = fx.view();
        let p = tid_params();
        let a = run(&v, DOGFOOD, &p).expect("first");
        let b = run(&v, DOGFOOD, &p).expect("second");
        assert_eq!(a, b);
        let hop = hop_graph();
        let hv = hop.view();
        let q = "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a, b";
        assert_eq!(run(&hv, q, &BTreeMap::new()), run(&hv, q, &BTreeMap::new()));
    }

    #[test]
    fn dogfood_pipeline_exact_rows() {
        let fx = dogfood_graph();
        let v = fx.view();
        let rs = run(&v, DOGFOOD, &tid_params()).expect("dogfood");
        assert_eq!(
            rs.columns(),
            &[
                "c".to_string(),
                "industry".to_string(),
                "specialty".to_string()
            ]
        );
        // industry DESC, specialty DESC; gamma (0.4) / delta (0.3) / foxtrot (no s) out.
        assert_eq!(
            rows_of(&rs),
            vec![
                vec![Some(s("acme")), Some(f(0.9)), Some(f(0.8))],
                vec![Some(s("zeta")), Some(f(0.9)), Some(f(0.6))],
                vec![Some(s("beta")), Some(f(0.6)), Some(f(0.7))],
                vec![Some(s("echo")), Some(f(0.5)), Some(f(0.5))],
            ]
        );
    }

    #[test]
    fn unknown_var_in_op_is_err_not_panic() {
        let fx = hop_graph();
        let v = fx.view();
        let plan = vec![
            PlanOp::ScanLabel {
                var: "a".into(),
                label: None,
            },
            PlanOp::Expand {
                from: "zzz".into(),
                rel_var: Some("r".into()),
                etype: None,
                dir: RelDir::Right,
                to: "b".into(),
                to_label: None,
                to_props: vec![],
            },
            PlanOp::Project {
                items: vec![RetItem {
                    value: RetVal::Var("a".into()),
                    alias: None,
                }],
            },
        ];
        let params = BTreeMap::new();
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            execute(&v, &plan, &Params(&params))
        }));
        assert!(caught.is_ok(), "execute panicked on unknown var");
        let err = caught.unwrap().expect_err("unknown var must be Err");
        assert!(
            err.contains("zzz") && err.to_ascii_lowercase().contains("unbound"),
            "got: {err}"
        );

        let join = vec![
            PlanOp::JoinBound {
                var: "ghost".into(),
                label: None,
                props: vec![],
            },
            PlanOp::Project {
                items: vec![RetItem {
                    value: RetVal::Var("ghost".into()),
                    alias: None,
                }],
            },
        ];
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            execute(&v, &join, &Params(&params))
        }));
        assert!(caught.is_ok(), "execute panicked on JoinBound unknown var");
        assert!(caught.unwrap().is_err());
    }

    #[test]
    fn dest_props_on_bound_expand_are_applied() {
        let fx = dogfood_graph();
        let v = fx.view();
        let rs = run(
            &v,
            "MATCH (t:Talent {id: $tid}) \
             MATCH (c:Company)-[r:INDUSTRY_ALIGNMENT]->(t:Talent {id: $tid}) \
             RETURN c",
            &tid_params(),
        )
        .unwrap();
        // foxtrot included (has industry); companies without the edge are not.
        assert_eq!(
            col(&rs, "c"),
            vec![
                Some(s("acme")),
                Some(s("beta")),
                Some(s("gamma")),
                Some(s("delta")),
                Some(s("echo")),
                Some(s("foxtrot")),
                Some(s("zeta")),
            ]
        );
        let miss = run(
            &v,
            "MATCH (t:Talent {id: $tid}) \
             MATCH (c:Company)-[r:INDUSTRY_ALIGNMENT]->(t {id: 'nope'}) \
             RETURN c",
            &tid_params(),
        )
        .unwrap();
        assert!(miss.is_empty());
    }

    #[test]
    fn missing_node_prop_projects_none_and_pattern_misses() {
        let mut fx = Fx::new();
        fx.add("Person", "ada", vec![("age", i(30))]);
        fx.add("Person", "bob", vec![]);
        let v = fx.view();
        let rs = run(&v, "MATCH (p:Person) RETURN p.age", &BTreeMap::new()).unwrap();
        assert_eq!(col(&rs, "p.age"), vec![Some(i(30)), None]);
        let pat = run(&v, "MATCH (p:Person {age: 30}) RETURN p", &BTreeMap::new()).unwrap();
        assert_eq!(col(&pat, "p"), vec![Some(s("ada"))]);
    }

    #[test]
    fn unlabeled_match_does_not_project_sentinel_ghost_key() {
        let mut fx = Fx::new();
        fx.add("Person", "ada", vec![]);
        // Hostile fixture: IdMap slot exists (key_of would yield "ghost")
        // but the label is the gap sentinel.
        fx.ids.get_or_insert("ghost");
        fx.labels.resize(fx.ids.len(), u32::MAX);
        fx.add("Person", "bob", vec![]);
        let v = fx.view();
        let rs = run(&v, "MATCH (n) RETURN n", &BTreeMap::new()).expect("unlabeled scan");
        assert_eq!(col(&rs, "n"), vec![Some(s("ada")), Some(s("bob"))]);
        assert!(
            !rows_of(&rs)
                .iter()
                .any(|row| row.iter().any(|c| *c == Some(s("ghost")))),
            "sentinel slot must not project a ghost key"
        );
    }

    #[test]
    fn execute_never_panics_on_hostile_plans() {
        let fx = hop_graph();
        let v = fx.view();
        let params = BTreeMap::new();
        let hostile = vec![
            vec![],
            vec![PlanOp::Project { items: vec![] }],
            vec![PlanOp::OrderBy {
                items: vec![OrderItem {
                    target: OrderTarget::Alias("nope".into()),
                    descending: false,
                }],
            }],
            vec![PlanOp::Filter {
                expr: crate::cypher::ast::Expr::Cmp {
                    lhs: Operand::Prop {
                        var: "missing".into(),
                        field: "x".into(),
                    },
                    op: crate::filter::CmpOp::Eq,
                    rhs: Operand::Lit(i(1)),
                },
            }],
            vec![
                PlanOp::ScanLabel {
                    var: "a".into(),
                    label: None,
                },
                PlanOp::LookupProps {
                    var: "zzz".into(),
                    props: vec![("k".into(), Operand::Lit(i(1)))],
                },
            ],
            vec![PlanOp::Skip(99), PlanOp::Limit(0)],
        ];
        for plan in hostile {
            let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                execute(&v, &plan, &Params(&params))
            }));
            assert!(caught.is_ok(), "execute panicked on hostile plan: {plan:?}");
        }
    }

    #[test]
    fn unlabeled_and_labeled_scans_skip_tombstoned_ids() {
        let mut fx = Fx::new();
        let ada = fx.add("Person", "ada", vec![]);
        let bob = fx.add("Person", "bob", vec![]);
        fx.edge("KNOWS", ada, bob, vec![]);
        // Same state delete_node leaves: id retired, label sentinel, edges gone.
        fx.ids.delete("ada");
        fx.labels[ada as usize] = u32::MAX;
        let knows = fx.syms.get("KNOWS").unwrap();
        fx.topo.remove_edge(knows, ada, bob);
        let v = fx.view();

        let labeled = run(&v, "MATCH (p:Person) RETURN p", &BTreeMap::new()).unwrap();
        assert_eq!(col(&labeled, "p"), vec![Some(s("bob"))]);
        let unlabeled = run(&v, "MATCH (n) RETURN n", &BTreeMap::new()).unwrap();
        assert_eq!(col(&unlabeled, "n"), vec![Some(s("bob"))]);
        let hop = run(&v, "MATCH (x)-[:KNOWS]->(y) RETURN x, y", &BTreeMap::new()).unwrap();
        assert!(
            hop.is_empty(),
            "expand cannot yield edges to a deleted node once topology is swept"
        );
    }

    // ──────────────────────────────────────────────────────────────────────────
    // Randomised property test (I2): bounded == unbounded[SKIP..SKIP+LIMIT]
    // ──────────────────────────────────────────────────────────────────────────

    proptest! {
        /// Randomised property test covering:
        ///
        /// - **Multi-hop** (1, 2, or 3 hops) — exercises nested pull_rows recursion.
        /// - **Optional WHERE filter** on a numeric prop `v` — bound must count
        ///   only post-filter rows.
        /// - **Cycle / shared-node topologies** — duplicate directed pairs and
        ///   reciprocal edges create paths where relationship-uniqueness rejects
        ///   some traversals; rejected rows must not count toward the bound.
        /// - **Randomized SKIP + LIMIT** — pull collects SKIP+LIMIT rows then
        ///   the wrapper discards the leading SKIP.
        ///
        /// Invariant: bounded[0..] == unbounded[skip..skip+limit] for all shapes.
        #[test]
        fn prop_bounded_equals_unbounded_slice(
            n_nodes     in 2u32..10u32,
            edge_pairs  in proptest::collection::vec(
                (any::<u32>(), any::<u32>()), 0..20usize
            ),
            // Extra reciprocal pairs to force uniqueness rejections on cycles.
            recip_pairs in proptest::collection::vec(
                (any::<u32>(), any::<u32>()), 0..8usize
            ),
            n_hops      in 1u32..4u32,   // 1, 2, or 3 hops
            use_filter  in any::<bool>(),
            threshold   in 0i64..8i64,   // filter: last-node.v > threshold
            limit       in 1u64..10u64,
            skip        in 0u64..4u64,
        ) {
            let mut fx = Fx::new();
            let mut node_ids = Vec::new();
            for idx in 0..n_nodes {
                // Every node carries a numeric prop `v` for the optional filter.
                let id = fx.add("N", &format!("n{idx}"), vec![("v", i(idx as i64 % 8))]);
                node_ids.push(id);
            }
            let n = node_ids.len();

            // Primary edges (forward direction).
            for (si, di) in &edge_pairs {
                let si = (*si as usize) % n;
                let di = (*di as usize) % n;
                if si != di {
                    fx.edge("T", node_ids[si], node_ids[di], vec![]);
                }
            }
            // Reciprocal edges — create A→B + B→A pairs so multi-hop paths
            // have uniqueness-rejected candidates (walking back the same edge).
            for (si, di) in &recip_pairs {
                let si = (*si as usize) % n;
                let di = (*di as usize) % n;
                if si != di {
                    fx.edge("T", node_ids[si], node_ids[di], vec![]);
                    fx.edge("T", node_ids[di], node_ids[si], vec![]);
                }
            }

            let v = fx.view();
            let params = BTreeMap::new();

            // Build query for `n_hops` hops with optional WHERE on the last node.
            // Variable names: a→b→c→d for hops 1/2/3.
            let var_names = ["a", "b", "c", "d"];
            let hop_count = n_hops as usize;
            let mut pattern = format!("({}:N)", var_names[0]);
            for h in 0..hop_count {
                pattern.push_str(&format!("-[:T]->({}", var_names[h + 1]));
                // Label the intermediate and last nodes as :N only for last.
                if h + 1 == hop_count {
                    pattern.push_str(":N)");
                } else {
                    pattern.push(')');
                }
            }
            let last_var = var_names[hop_count];
            let where_clause = if use_filter {
                format!(" WHERE {last_var}.v > {threshold}")
            } else {
                String::new()
            };
            let ret_vars: Vec<&str> = var_names[..=hop_count].to_vec();
            let ret_clause = ret_vars.join(", ");

            let full_q = format!("MATCH {pattern}{where_clause} RETURN {ret_clause}");
            let bounded_q = format!(
                "MATCH {pattern}{where_clause} RETURN {ret_clause} SKIP {skip} LIMIT {limit}"
            );

            let full_plan = compile(&full_q);
            // Use a generous cap for the reference run so it never errors on
            // small graphs, even with cycles.
            let unbounded = super::with_max_intermediate_rows(100_000, || {
                super::execute_unbounded(&v, &full_plan, &Params(&params))
            });
            // If the reference itself errors (shouldn't happen at this scale),
            // skip the proptest case rather than failing.
            let unbounded = match unbounded {
                Ok(rs) => rs,
                Err(_) => return Ok(()),
            };
            let total = unbounded.len();
            let full_rows = rows_of(&unbounded);

            let bounded = super::with_max_intermediate_rows(100_000, || {
                run(&v, &bounded_q, &params)
            }).expect("bounded must not error");

            let s = (skip as usize).min(total);
            let e = (skip as usize + limit as usize).min(total);
            prop_assert_eq!(
                rows_of(&bounded),
                full_rows[s..e].to_vec(),
                "hops={} filter={} threshold={} SKIP {} LIMIT {}: \
                 bounded != unbounded[{}..{}]",
                hop_count, use_filter, threshold, skip, limit, s, e
            );
        }
    }

    proptest! {
        /// Randomised property test pinning the fused ScanLabel+Filter fast path.
        ///
        /// Shape: `MATCH (n:N) WHERE n.v <op> <literal> RETURN n SKIP s LIMIT l`
        ///
        /// This shape places a `Filter { Cmp { Prop{n}, op, Lit } }` immediately
        /// after `ScanLabel { var: n }` in the plan, which triggers the fused
        /// detection in `pull_rows`.  The invariant is the same as
        /// `prop_bounded_equals_unbounded_slice`: bounded == unbounded[s..s+l].
        ///
        /// Coverage:
        ///  - All 6 CmpOps (=, <>, <, <=, >, >=)
        ///  - Nodes that have the prop (Int or Float) and nodes that are missing it
        ///  - Threshold values spanning match-all, match-none, and partial
        ///  - Randomised SKIP and LIMIT
        ///  - Boundary shape: compound AND (`WHERE n.v op lit AND n.v >= -999`)
        ///    which is semantically equivalent but bypasses fused detection
        ///    (Expr::And, not Expr::Cmp), exercising the generic fallback path
        #[test]
        fn prop_scan_filter_fused_equals_unbounded_slice(
            n_nodes     in 0u32..20u32,
            // Bit i set → node i has prop `v`.
            prop_mask   in any::<u32>(),
            // Bit i set → node i stores a Float value instead of Int.
            float_mask  in any::<u32>(),
            // Comparison threshold; range -1..8 spans all/none/partial matches
            // against node values in 0..7.
            threshold   in -1i64..8i64,
            // Index into the 6 CmpOps (Eq=0, Ne=1, Lt=2, Le=3, Gt=4, Ge=5).
            op_idx      in 0u32..6u32,
            skip        in 0u64..5u64,
            limit       in 1u64..8u64,
        ) {
            let op_str = match op_idx {
                0 => "=",
                1 => "<>",
                2 => "<",
                3 => "<=",
                4 => ">",
                _ => ">=",
            };

            let mut fx = Fx::new();
            for idx in 0..n_nodes {
                let has_prop = (prop_mask >> (idx % 32)) & 1 == 1;
                let use_float = (float_mask >> (idx % 32)) & 1 == 1;
                // Node values cycle in 0..7 so threshold spans all/none/partial.
                let props: Vec<(&str, Value)> = if has_prop {
                    if use_float {
                        vec![("v", f(idx as f64 % 7.0))]
                    } else {
                        vec![("v", i(idx as i64 % 7))]
                    }
                } else {
                    vec![]
                };
                fx.add("N", &format!("n{idx}"), props);
            }

            let v = fx.view();
            let params = BTreeMap::new();

            // ── Fused path ───────────────────────────────────────────────────
            let full_q = format!("MATCH (n:N) WHERE n.v {op_str} {threshold} RETURN n");
            let bounded_q = format!(
                "MATCH (n:N) WHERE n.v {op_str} {threshold} RETURN n SKIP {skip} LIMIT {limit}"
            );

            let full_plan = compile(&full_q);
            let unbounded = super::execute_unbounded(&v, &full_plan, &Params(&params));
            let unbounded = match unbounded {
                Ok(rs) => rs,
                Err(_) => return Ok(()),
            };
            let total = unbounded.len();
            let full_rows = rows_of(&unbounded);

            // Snapshot counter before executing the fused-shape query.
            let fires_before =
                super::FUSED_SCAN_FIRES.load(std::sync::atomic::Ordering::Relaxed);
            let bounded = run(&v, &bounded_q, &params).expect("fused bounded must not error");
            let fires_after =
                super::FUSED_SCAN_FIRES.load(std::sync::atomic::Ordering::Relaxed);

            // The planner must have emitted ScanLabel→Filter{Cmp} for this shape;
            // assert the fused arm actually executed (counter advanced).
            // Guard: with 0 nodes the label symbol is never interned, so pull_rows
            // exits before the fused detection — nothing to assert in that case.
            if n_nodes > 0 {
                prop_assert!(
                    fires_after > fires_before,
                    "fused arm did NOT fire for op={} threshold={} n_nodes={}: \
                     counter before={} after={}",
                    op_str,
                    threshold,
                    n_nodes,
                    fires_before,
                    fires_after
                );
            }

            let s = (skip as usize).min(total);
            let e = (skip as usize + limit as usize).min(total);
            prop_assert_eq!(
                rows_of(&bounded),
                full_rows[s..e].to_vec(),
                "fused path: op={} threshold={} n_nodes={} SKIP {} LIMIT {}: \
                 bounded != unbounded[{}..{}]",
                op_str, threshold, n_nodes, skip, limit, s, e
            );

            // ── Boundary: compound AND — misses fused detection → generic path ─
            // `AND n.v >= -999` is always true for our Int/Float range 0..7,
            // so the result set is identical — only the executor path differs.
            let compound_q = format!(
                "MATCH (n:N) WHERE n.v {} {} AND n.v >= -999 RETURN n SKIP {} LIMIT {}",
                op_str, threshold, skip, limit
            );
            let fires_before_compound =
                super::FUSED_SCAN_FIRES.load(std::sync::atomic::Ordering::Relaxed);
            let compound_bounded =
                run(&v, &compound_q, &params).expect("compound-AND bounded must not error");
            let fires_after_compound =
                super::FUSED_SCAN_FIRES.load(std::sync::atomic::Ordering::Relaxed);

            // Compound AND must NOT activate the fused arm.
            prop_assert_eq!(
                fires_after_compound,
                fires_before_compound,
                "fused arm fired for compound-AND shape (should use generic path): \
                 op={} threshold={} n_nodes={}",
                op_str,
                threshold,
                n_nodes
            );
            prop_assert_eq!(
                rows_of(&compound_bounded),
                full_rows[s..e].to_vec(),
                "compound-AND fallback: op={} threshold={} n_nodes={} SKIP {} LIMIT {}: \
                 result differs from fused",
                op_str, threshold, n_nodes, skip, limit
            );
        }
    }

    // ──────────────────────────────────────────────────────────────────────────
    // C1 / C2: dense hop-1 with downstream filter
    //
    // The actual failing shape: 1 source → N leaves (N > cap), and a downstream
    // Filter that passes only a small fraction.  The per-stage approach (Round 1)
    // errors because hop-1 Expand runs to the full cap before the Filter sees
    // anything.  The pull-based approach cascades the bound: once `limit` rows
    // have passed the Filter, all upstream loops stop.
    // ──────────────────────────────────────────────────────────────────────────

    #[test]
    fn dense_hop1_with_filter_survives_pull() {
        const LEAVES: usize = 120; // > CAP so staged always errors
        const CAP: usize = 100;

        let mut fx = Fx::new();
        let src = fx.add("Src", "src", vec![]);
        for idx in 0..LEAVES {
            let leaf = fx.add("Leaf", &format!("l{idx}"), vec![("v", i(idx as i64))]);
            fx.edge("T", src, leaf, vec![]);
        }
        let v = fx.view();
        let params = BTreeMap::new();

        // Staged (unbounded) path: expand produces 120 rows > cap=100 → error.
        let staged_err = super::with_max_intermediate_rows(CAP, || {
            super::execute_unbounded(
                &v,
                &compile("MATCH (s:Src)-[:T]->(l:Leaf) WHERE l.v >= 110 RETURN l, l.v"),
                &Params(&params),
            )
        });
        assert!(
            staged_err.is_err(),
            "staged path must error on 120 leaves with cap={CAP}"
        );
        assert!(
            staged_err
                .unwrap_err()
                .contains("intermediate result exceeds"),
            "wrong error message"
        );

        // Pull-based (LIMIT 5): never materialises more than 5 rows → survives.
        // WHERE l.v >= 110 means only leaves 110..119 pass (10 survivors), so
        // 5 results are found well before all 120 leaves are expanded.
        let ok = super::with_max_intermediate_rows(CAP, || {
            run(
                &v,
                "MATCH (s:Src)-[:T]->(l:Leaf) WHERE l.v >= 110 RETURN l, l.v LIMIT 5",
                &params,
            )
        });
        let rs = ok.expect("pull-based must survive despite dense hop-1 exceeding cap");
        assert_eq!(rs.len(), 5, "LIMIT 5 must return exactly 5 rows");

        // Verify all returned rows have l.v >= 110 (correct filter application).
        let vs: Vec<i64> = (0..rs.len())
            .filter_map(|i| match rs.get(i, "l.v") {
                Some(Value::Int(n)) => Some(*n),
                _ => None,
            })
            .collect();
        assert_eq!(vs.len(), 5, "all projected rows must have v");
        for v_val in &vs {
            assert!(*v_val >= 110, "filter must hold: v={v_val} is not >= 110");
        }

        // Early-termination proof: use a filter that passes early leaves (v < 10)
        // so pull stops after visiting just 5 leaves, while staged visits all 120.
        // Ratio: 120 / 5 = 24× — well above the 10× threshold.
        let (pull_result, pull_produced) = super::with_expand_counter(|| {
            super::with_max_intermediate_rows(1_000_000, || {
                run(
                    &v,
                    "MATCH (s:Src)-[:T]->(l:Leaf) WHERE l.v < 10 RETURN l LIMIT 5",
                    &params,
                )
            })
        });
        pull_result.expect("pull must succeed without cap");
        // Pull visits leaves 0..4 (all pass v < 10, LIMIT 5 satisfied immediately).
        assert!(
            pull_produced <= 5,
            "pull expand count {pull_produced} should be ≤ 5 (stops after 5 passing leaves)"
        );

        let (staged_result, staged_produced) = super::with_expand_counter(|| {
            super::with_max_intermediate_rows(1_000_000, || {
                super::execute_unbounded(
                    &v,
                    &compile("MATCH (s:Src)-[:T]->(l:Leaf) WHERE l.v < 10 RETURN l"),
                    &Params(&params),
                )
            })
        });
        staged_result.expect("staged must succeed with 1M cap");
        assert_eq!(
            staged_produced, LEAVES,
            "staged must expand all {LEAVES} leaves"
        );

        assert!(
            staged_produced >= pull_produced * 10,
            "staged ({staged_produced}) must be ≥ 10× pull ({pull_produced})"
        );
    }

    // ──────────────────────────────────────────────────────────────────────────
    // Harness-shape two-hop dense test
    //
    // Replicates the exact failure shape from the public benchmark table at
    // 1/100 scale (70 Talent + 20 Company with 3 industry categories).
    // IA edges are added directly (bypassing the rule engine) to reproduce the
    // dense edge structure that `Predicate::FieldEqual { field: "industry" }`
    // generates at full scale.
    //
    // Query: MATCH (t:Talent)-[:INDUSTRY_ALIGNMENT]->(c:Company)
    //               <-[:INDUSTRY_ALIGNMENT]-(t2:Talent)
    //        RETURN t, c, t2 LIMIT 10
    //
    // Before (staged with cap=100): hop-1 expands 70×~7=~466 rows > 100 → error
    // After  (pull-based):          finds 10 results, stops, returns correctly
    // ──────────────────────────────────────────────────────────────────────────

    #[test]
    fn harness_shape_two_hop_dense_survives_pull() {
        const N_TALENT: usize = 70;
        const N_COMPANY: usize = 20;
        const N_INDUSTRY: usize = 3;
        const CAP: usize = 100;
        const LIMIT: usize = 10;

        let mut fx = Fx::new();

        // Build Talent nodes with industry tag.
        let mut talent_ids: Vec<u32> = Vec::new();
        let mut talent_industry: Vec<usize> = Vec::new();
        for i in 0..N_TALENT {
            let ind = i % N_INDUSTRY;
            let id = fx.add(
                "Talent",
                &format!("t{i}"),
                vec![("industry", s(&ind.to_string()))],
            );
            talent_ids.push(id);
            talent_industry.push(ind);
        }

        // Build Company nodes with industry tag.
        let mut company_ids: Vec<u32> = Vec::new();
        let mut company_industry: Vec<usize> = Vec::new();
        for i in 0..N_COMPANY {
            let ind = i % N_INDUSTRY;
            let id = fx.add(
                "Company",
                &format!("c{i}"),
                vec![("industry", s(&ind.to_string()))],
            );
            company_ids.push(id);
            company_industry.push(ind);
        }

        // INDUSTRY_ALIGNMENT: Talent → Company when same industry.
        for (ti, &tid) in talent_ids.iter().enumerate() {
            for (ci, &cid) in company_ids.iter().enumerate() {
                if talent_industry[ti] == company_industry[ci] {
                    fx.edge("INDUSTRY_ALIGNMENT", tid, cid, vec![]);
                }
            }
        }

        let v = fx.view();
        let params = BTreeMap::new();
        let query = format!(
            "MATCH (t:Talent)-[:INDUSTRY_ALIGNMENT]->(c:Company)\
             <-[:INDUSTRY_ALIGNMENT]-(t2:Talent) RETURN t, c, t2 LIMIT {LIMIT}"
        );

        // Staged (unbounded) path: hop-1 expands 70×~7=~466 rows > cap=100 → error.
        const UNBOUNDED_Q: &str = "MATCH (t:Talent)-[:INDUSTRY_ALIGNMENT]->(c:Company)\
             <-[:INDUSTRY_ALIGNMENT]-(t2:Talent) RETURN t, c, t2";
        let staged_err = super::with_max_intermediate_rows(CAP, || {
            super::execute_unbounded(&v, &compile(UNBOUNDED_Q), &Params(&params))
        });
        assert!(
            staged_err.is_err(),
            "staged must error with cap={CAP} on harness-shape graph"
        );
        assert!(
            staged_err
                .unwrap_err()
                .contains("intermediate result exceeds"),
            "wrong error"
        );

        // Pull-based (LIMIT 10): cascades bound through both hops → completes.
        let ok = super::with_max_intermediate_rows(CAP, || run(&v, &query, &params));
        let rs = ok.expect("pull-based must complete on harness-shape with LIMIT 10");
        assert_eq!(rs.len(), LIMIT, "must return exactly {LIMIT} rows");

        // Each result row (t, c, t2) must have matching industry across all 3 variables.
        // t and c share an IA edge (same industry); c and t2 share an IA edge too.
        // Since we can't directly query industry from the ResultSet without projecting it,
        // just verify the result is semantically plausible: 3 non-None columns per row.
        for i in 0..rs.len() {
            let row = rs.row(i);
            assert_eq!(row.len(), 3, "each row must have 3 columns (t, c, t2)");
            assert!(row.iter().all(|c| c.is_some()), "all cells must be Some");
        }
    }

    #[test]
    fn intermediate_row_cap_errors_on_scan_and_expand() {
        let cap_msg = |n: usize| {
            format!(
                "intermediate result exceeds {n} rows; add a LIMIT or constrain patterns with shared variables"
            )
        };

        let mut scan_fx = Fx::new();
        scan_fx.add("N", "a", vec![]);
        scan_fx.add("N", "b", vec![]);
        scan_fx.add("N", "c", vec![]);
        let sv = scan_fx.view();
        let scan_err = super::with_max_intermediate_rows(2, || {
            run(&sv, "MATCH (n:N) RETURN n", &BTreeMap::new())
        })
        .expect_err("3-row scan must exceed cap 2");
        assert_eq!(scan_err, cap_msg(2));

        let mut exp_fx = Fx::new();
        let src = exp_fx.add("Src", "s", vec![]);
        let d1 = exp_fx.add("Dst", "d1", vec![]);
        let d2 = exp_fx.add("Dst", "d2", vec![]);
        exp_fx.edge("T", src, d1, vec![]);
        exp_fx.edge("T", src, d2, vec![]);
        let ev = exp_fx.view();
        // Scan of :Src is 1 row (under cap); expand to two dests would be 2.
        let exp_err = super::with_max_intermediate_rows(1, || {
            run(&ev, "MATCH (x:Src)-[:T]->(y) RETURN x, y", &BTreeMap::new())
        })
        .expect_err("2-row expand must exceed cap 1");
        assert_eq!(exp_err, cap_msg(1));
    }

    // ──────────────────────────────────────────────────────────────────────────
    // LIMIT push-down: semantics, early-termination proof, and budget survival
    // ──────────────────────────────────────────────────────────────────────────

    /// Property test: bounded execution (execute with push-down) produces the
    /// same rows as the reference unbounded path (execute_unbounded) sliced to
    /// the first LIMIT rows.  Exercises several LIMIT and SKIP+LIMIT values,
    /// including queries that have a Filter so the bound falls on Filter output,
    /// and queries with relationship-uniqueness rejections.
    #[test]
    fn bounded_matches_unbounded_slice_various_limits() {
        // ── single-hop, no Filter ──────────────────────────────────────────────
        let fx = hop_graph();
        let v = fx.view();
        let params = BTreeMap::new();

        // Full 3-row reference (ada→bob, ada→cam, bob→cam).
        let full_plan = compile("MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a, b");
        let full_rs = super::execute_unbounded(&v, &full_plan, &Params(&params)).unwrap();
        let full_rows = rows_of(&full_rs);
        assert_eq!(full_rows.len(), 3, "hop_graph has exactly 3 KNOWS paths");

        for limit in [1u64, 2, 3, 10] {
            let q = format!("MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a, b LIMIT {limit}");
            let rs = run(&v, &q, &params).unwrap();
            let expected_len = (limit as usize).min(full_rows.len());
            assert_eq!(
                rs.len(),
                expected_len,
                "LIMIT {limit}: expected {expected_len} rows, got {}",
                rs.len()
            );
            assert_eq!(
                rows_of(&rs),
                full_rows[..expected_len],
                "LIMIT {limit}: rows differ from reference slice"
            );
        }

        // SKIP + LIMIT: bound = SKIP + LIMIT so slicing is correct.
        let skip_rs = run(
            &v,
            "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a, b SKIP 1 LIMIT 2",
            &params,
        )
        .unwrap();
        assert_eq!(rows_of(&skip_rs), full_rows[1..3]);

        // ── with WHERE (bound falls on Filter, not Expand) ────────────────────
        let mut fx2 = Fx::new();
        fx2.add("N", "a", vec![("v", i(1))]);
        fx2.add("N", "b", vec![("v", i(2))]);
        fx2.add("N", "c", vec![("v", i(3))]);
        let v2 = fx2.view();
        // Full result in order: a(1), b(2), c(3).  After WHERE v > 1: b, c.
        let filter_full = super::execute_unbounded(
            &v2,
            &compile("MATCH (n:N) WHERE n.v > 1 RETURN n"),
            &Params(&params),
        )
        .unwrap();
        assert_eq!(filter_full.len(), 2);

        let filter_lim = run(&v2, "MATCH (n:N) WHERE n.v > 1 RETURN n LIMIT 1", &params).unwrap();
        assert_eq!(filter_lim.len(), 1, "LIMIT 1 on filter query");
        assert_eq!(rows_of(&filter_lim), rows_of(&filter_full)[..1]);

        // ── relationship-uniqueness rejections do not count toward the bound ──
        let tri = triangle();
        let tv = tri.view();
        // Triangle has 3 two-hop paths; uniqueness rejects 6 (reversed pairs).
        let tri_full = super::execute_unbounded(
            &tv,
            &compile("MATCH (x)-[r1:T]->(y)-[r2:T]->(z) RETURN x, y, z"),
            &Params(&params),
        )
        .unwrap();
        assert_eq!(tri_full.len(), 3, "triangle has 3 unique two-hop paths");
        for limit in [1u64, 2, 3, 5] {
            let q = format!("MATCH (x)-[r1:T]->(y)-[r2:T]->(z) RETURN x, y, z LIMIT {limit}");
            let rs = run(&tv, &q, &params).unwrap();
            let expected_len = (limit as usize).min(3);
            assert_eq!(
                rs.len(),
                expected_len,
                "triangle LIMIT {limit}: got {} rows",
                rs.len()
            );
            assert_eq!(
                rows_of(&rs),
                rows_of(&tri_full)[..expected_len],
                "triangle LIMIT {limit}: rows differ"
            );
        }
    }

    /// Early-termination proof: on a star graph (1 hub → 500 leaves), a bounded
    /// query (LIMIT 5) must cause `exec_expand` to emit ≤ 5 rows, while an
    /// unbounded run emits all 500 (≥ 100× the bounded count).
    #[test]
    fn expand_terminates_early_with_row_bound() {
        const LEAVES: usize = 500;
        let mut fx = Fx::new();
        let hub = fx.add("Hub", "hub", vec![]);
        for i in 0..LEAVES {
            let leaf = fx.add("Leaf", &format!("leaf-{i}"), vec![]);
            fx.edge("T", hub, leaf, vec![]);
        }
        let v = fx.view();
        let params = BTreeMap::new();
        let full_plan = compile("MATCH (h:Hub)-[:T]->(x:Leaf) RETURN x");

        // Bounded path via the regular execute() entry point (LIMIT 5 → bound 5).
        let (bounded_result, bounded_produced) = super::with_expand_counter(|| {
            run(&v, "MATCH (h:Hub)-[:T]->(x:Leaf) RETURN x LIMIT 5", &params)
        });
        let bounded_rs = bounded_result.unwrap();
        assert_eq!(bounded_rs.len(), 5, "LIMIT 5 must return exactly 5 rows");
        assert!(
            bounded_produced <= 5,
            "bounded: exec_expand emitted {bounded_produced} rows, expected ≤ 5"
        );

        // Unbounded reference path via execute_unbounded (no push-down).
        let (unbounded_result, unbounded_produced) = super::with_expand_counter(|| {
            super::execute_unbounded(&v, &full_plan, &Params(&params))
        });
        let unbounded_rs = unbounded_result.unwrap();
        assert_eq!(
            unbounded_rs.len(),
            LEAVES,
            "unbounded must return all {LEAVES} rows"
        );
        assert_eq!(
            unbounded_produced, LEAVES,
            "unbounded: exec_expand must emit all {LEAVES} rows"
        );

        // Early-termination ratio ≥ 100×.
        assert!(
            unbounded_produced >= bounded_produced * 100,
            "unbounded ({unbounded_produced}) must be ≥ 100× bounded ({bounded_produced})"
        );
    }

    /// Budget-survival: a bounded query (LIMIT pushdown active) must complete
    /// without triggering the intermediate-row cap, even when the same query
    /// without pushdown (execute_unbounded) would exceed a reduced cap.
    ///
    /// This also verifies that the 1 M cap is still live for unbounded queries.
    #[test]
    fn bounded_query_survives_low_intermediate_row_cap() {
        // 20 leaf nodes: unbounded would emit 20 rows, exceeding a cap of 10.
        let mut fx = Fx::new();
        let src = fx.add("Src", "s", vec![]);
        for i in 0..20usize {
            let dst = fx.add("Dst", &format!("d{i}"), vec![]);
            fx.edge("T", src, dst, vec![]);
        }
        let v = fx.view();
        let params = BTreeMap::new();
        let full_plan = compile("MATCH (s:Src)-[:T]->(d:Dst) RETURN d");

        // Unbounded hits the cap — 1 M budget check must remain live.
        let cap_err = super::with_max_intermediate_rows(10, || {
            super::execute_unbounded(&v, &full_plan, &Params(&params))
        });
        assert!(
            cap_err.is_err(),
            "unbounded must hit the intermediate-row cap"
        );
        assert!(
            cap_err.unwrap_err().contains("intermediate result exceeds"),
            "cap error message must be the budget message"
        );

        // Bounded (LIMIT 5) stops before the cap — must complete successfully.
        let ok = super::with_max_intermediate_rows(10, || {
            run(&v, "MATCH (s:Src)-[:T]->(d:Dst) RETURN d LIMIT 5", &params)
        });
        assert_eq!(
            ok.unwrap().len(),
            5,
            "bounded (LIMIT 5) must complete with 5 rows, not a cap error"
        );

        // Bounded with LIMIT exactly at cap also survives.
        let at_cap = super::with_max_intermediate_rows(10, || {
            run(&v, "MATCH (s:Src)-[:T]->(d:Dst) RETURN d LIMIT 10", &params)
        });
        assert_eq!(
            at_cap.unwrap().len(),
            10,
            "bounded at LIMIT==cap must complete with 10 rows"
        );
    }

    // ──────────────────────────────────────────────────────────────────────────
    // Aggregate execution tests
    // ──────────────────────────────────────────────────────────────────────────

    #[test]
    fn count_star_returns_total_node_count() {
        let mut fx = Fx::new();
        fx.add("Person", "ada", vec![]);
        fx.add("Person", "bob", vec![]);
        fx.add("Person", "cam", vec![]);
        let v = fx.view();
        let params = BTreeMap::new();

        let rs = run(&v, "MATCH (n:Person) RETURN COUNT(*)", &params).expect("COUNT(*)");
        assert_eq!(rs.columns(), &["COUNT(*)".to_string()]);
        assert_eq!(rs.len(), 1);
        assert_eq!(rs.row(0), &[Some(i(3))]);

        // Empty graph: COUNT(*) should return 0.
        let rs_empty = run(&v, "MATCH (n:Ghost) RETURN COUNT(*)", &params).expect("COUNT(*) empty");
        assert_eq!(rs_empty.row(0), &[Some(i(0))]);
    }

    #[test]
    fn count_star_alias_sets_column_name() {
        let mut fx = Fx::new();
        fx.add("N", "a", vec![]);
        let v = fx.view();
        let params = BTreeMap::new();
        let rs = run(&v, "MATCH (n:N) RETURN COUNT(*) AS total", &params).expect("COUNT AS");
        assert_eq!(rs.columns(), &["total".to_string()]);
        assert_eq!(rs.row(0), &[Some(i(1))]);
    }

    #[test]
    fn count_var_skips_null_node_bindings() {
        // COUNT(n) counts rows where n is bound. Since ScanLabel always binds n,
        // this matches COUNT(*) for nodes. Primarily documents the semantics.
        let mut fx = Fx::new();
        fx.add("N", "a", vec![]);
        fx.add("N", "b", vec![]);
        let v = fx.view();
        let params = BTreeMap::new();
        let rs = run(&v, "MATCH (n:N) RETURN COUNT(n)", &params).expect("COUNT(n)");
        assert_eq!(rs.columns(), &["COUNT(n)".to_string()]);
        assert_eq!(rs.row(0), &[Some(i(2))]);
    }

    #[test]
    fn sum_numeric_prop_ignores_null_and_non_numeric() {
        let mut fx = Fx::new();
        fx.add("N", "a", vec![("v", i(10))]);
        fx.add("N", "b", vec![("v", i(20))]);
        fx.add("N", "c", vec![]); // missing prop — skipped
        let v = fx.view();
        let params = BTreeMap::new();
        let rs = run(&v, "MATCH (n:N) RETURN SUM(n.v)", &params).expect("SUM");
        assert_eq!(rs.columns(), &["SUM(n.v)".to_string()]);
        // 10.0 + 20.0 = 30.0 (null skipped)
        assert_eq!(rs.row(0), &[Some(f(30.0))]);

        // All props null → result is null.
        let rs_null = run(&v, "MATCH (n:N) RETURN SUM(n.missing)", &params).expect("SUM null");
        assert_eq!(rs_null.row(0), &[None]);
    }

    #[test]
    fn avg_numeric_prop() {
        let mut fx = Fx::new();
        fx.add("N", "a", vec![("v", i(10))]);
        fx.add("N", "b", vec![("v", i(30))]);
        let v = fx.view();
        let params = BTreeMap::new();
        let rs = run(&v, "MATCH (n:N) RETURN AVG(n.v) AS avg_v", &params).expect("AVG");
        assert_eq!(rs.columns(), &["avg_v".to_string()]);
        // (10 + 30) / 2 = 20.0
        assert_eq!(rs.row(0), &[Some(f(20.0))]);

        // Empty graph: AVG returns null.
        let rs_empty = run(&v, "MATCH (n:Ghost) RETURN AVG(n.v)", &params).expect("AVG empty");
        assert_eq!(rs_empty.row(0), &[None]);
    }

    #[test]
    fn min_max_numeric_prop() {
        let mut fx = Fx::new();
        fx.add("N", "a", vec![("v", i(5))]);
        fx.add("N", "b", vec![("v", i(1))]);
        fx.add("N", "c", vec![("v", i(9))]);
        fx.add("N", "d", vec![]); // null skipped
        let v = fx.view();
        let params = BTreeMap::new();

        let min_rs = run(&v, "MATCH (n:N) RETURN MIN(n.v)", &params).expect("MIN");
        assert_eq!(min_rs.row(0), &[Some(i(1))]);

        let max_rs = run(&v, "MATCH (n:N) RETURN MAX(n.v)", &params).expect("MAX");
        assert_eq!(max_rs.row(0), &[Some(i(9))]);
    }

    /// M-2: MIN/MAX with mixed Int and Float props.  The cmp_optional ordering
    /// places Int and Float by numeric value (cross-variant numeric comparison).
    #[test]
    fn min_max_mixed_int_float_props() {
        let mut fx = Fx::new();
        // Int 3, Float 1.5, Int 7, Float 2.0 — min=1.5 (Float), max=7 (Int).
        fx.add("N", "a", vec![("v", i(3))]);
        fx.add("N", "b", vec![("v", f(1.5))]);
        fx.add("N", "c", vec![("v", i(7))]);
        fx.add("N", "d", vec![("v", f(2.0))]);
        let v = fx.view();
        let params = BTreeMap::new();

        let min_rs = run(&v, "MATCH (n:N) RETURN MIN(n.v)", &params).expect("MIN mixed");
        // 1.5 < 2.0 < 3 < 7 — minimum is Float(1.5).
        assert_eq!(min_rs.row(0), &[Some(f(1.5))]);

        let max_rs = run(&v, "MATCH (n:N) RETURN MAX(n.v)", &params).expect("MAX mixed");
        // Maximum is Int(7).
        assert_eq!(max_rs.row(0), &[Some(i(7))]);
    }

    /// I-1: LIMIT, SKIP, and ORDER BY are silently dropped for aggregate
    /// queries (always one result row).  Pin both boundary values.
    #[test]
    fn aggregate_limit_skip_order_by_are_no_ops() {
        let mut fx = Fx::new();
        fx.add("N", "a", vec![]);
        fx.add("N", "b", vec![]);
        fx.add("N", "c", vec![]);
        let v = fx.view();
        let params = BTreeMap::new();

        // LIMIT 5 — aggregate always returns exactly 1 row regardless.
        let rs_lim5 =
            run(&v, "MATCH (n:N) RETURN COUNT(*) LIMIT 5", &params).expect("COUNT(*) LIMIT 5");
        assert_eq!(
            rs_lim5.len(),
            1,
            "aggregate with LIMIT 5 must still return 1 row"
        );
        assert_eq!(rs_lim5.row(0), &[Some(i(3))]);

        // LIMIT 0 — even LIMIT 0 does not suppress the aggregate row.
        let rs_lim0 =
            run(&v, "MATCH (n:N) RETURN COUNT(*) LIMIT 0", &params).expect("COUNT(*) LIMIT 0");
        assert_eq!(
            rs_lim0.len(),
            1,
            "aggregate with LIMIT 0 must still return 1 row"
        );
        assert_eq!(rs_lim0.row(0), &[Some(i(3))]);

        // SKIP 100 — does not discard the single result row.
        let rs_skip =
            run(&v, "MATCH (n:N) RETURN COUNT(*) SKIP 100", &params).expect("COUNT(*) SKIP 100");
        assert_eq!(
            rs_skip.len(),
            1,
            "aggregate with large SKIP must still return 1 row"
        );

        // ORDER BY is a no-op on a single-row result (but must not panic).
        // Note: the planner drops ORDER BY for aggregates; verify that the plan
        // compiles without error and returns the correct count.
        let rs_ord = plan_src("MATCH (n:N) RETURN COUNT(*) ORDER BY n");
        // ORDER BY on aggregate: planner drops ORDER BY, so this should plan OK.
        // (The planner exits early after emitting Aggregate, so ORDER BY is ignored.)
        assert!(
            rs_ord.is_ok(),
            "COUNT(*) ORDER BY should plan without error (ORDER BY dropped)"
        );
        let plan_ops = rs_ord.unwrap();
        // Must not contain an OrderBy op — it was dropped.
        assert!(
            !plan_ops
                .iter()
                .any(|op| matches!(op, crate::cypher::plan::PlanOp::OrderBy { .. })),
            "aggregate plan must not contain OrderBy"
        );
    }

    #[test]
    fn count_star_no_budget_cap_applies() {
        // COUNT(*) with a dense graph that would error the staged path.
        // The aggregate path must complete without hitting the cap.
        let mut fx = Fx::new();
        let src = fx.add("Src", "s", vec![]);
        for i in 0..30usize {
            let dst = fx.add("Dst", &format!("d{i}"), vec![]);
            fx.edge("T", src, dst, vec![]);
        }
        let v = fx.view();
        let params = BTreeMap::new();

        // Staged path errors on 30 nodes > cap 10.
        let cap_err =
            super::with_max_intermediate_rows(10, || run(&v, "MATCH (n:Dst) RETURN n", &params));
        assert!(
            cap_err.is_err(),
            "staged path must error on 30 nodes with cap=10"
        );

        // COUNT(*) does not apply the cap — must complete and return 30.
        let agg_ok = super::with_max_intermediate_rows(10, || {
            run(&v, "MATCH (n:Dst) RETURN COUNT(*)", &params)
        })
        .expect("aggregate must not hit the intermediate-row cap");
        assert_eq!(
            agg_ok.row(0),
            &[Some(i(30))],
            "COUNT(*) must count all 30 nodes regardless of cap"
        );
    }

    fn plan_src(src: &str) -> Result<Vec<crate::cypher::plan::PlanOp>, String> {
        use crate::cypher::{lex, parse, plan};
        let toks = lex(src).map_err(|e| format!("lex: {e}"))?;
        let ast = parse(&toks).map_err(|e| format!("parse: {e}"))?;
        plan(&ast).map_err(|e| format!("plan: {e}"))
    }

    /// Updated from the v1 pin: grouped aggregation is now supported.
    /// Verifies plan routing and that the only remaining plan-error is SUM(*).
    #[test]
    fn grouped_aggregation_plan_routing() {
        use crate::cypher::plan::PlanOp;

        // RETURN a, COUNT(*) — grouped aggregation now routes to GroupAggregate.
        let ops = plan_src("MATCH (a:N) RETURN a, COUNT(*)")
            .expect("grouped aggregation must now succeed");
        assert!(
            ops.iter().any(|op| matches!(op, PlanOp::GroupAggregate { .. })),
            "grouped aggregation plan must contain GroupAggregate op, got: {ops:?}"
        );

        // Multiple aggregates without group keys also routes to GroupAggregate.
        let ops2 = plan_src("MATCH (a:N) RETURN COUNT(*), COUNT(a)")
            .expect("multi-aggregate must now succeed");
        assert!(
            ops2.iter().any(|op| matches!(op, PlanOp::GroupAggregate { .. })),
            "multi-aggregate plan must contain GroupAggregate op, got: {ops2:?}"
        );

        // SUM(*) is still a plan error: Star is invalid for SUM.
        let err3 = plan_src("MATCH (a:N) RETURN SUM(*)").expect_err("SUM(*) must be plan error");
        assert!(
            err3.to_ascii_lowercase().contains("sum") || err3.to_ascii_lowercase().contains("*"),
            "error must mention SUM or *, got: {err3}"
        );
    }

    // ─── Grouped aggregation execution tests ─────────────────────────────────

    #[test]
    fn grouped_single_key_count() {
        // Graph: 3 nodes with "t" prop — two "X", one "Y".
        let mut fx = Fx::new();
        fx.add("N", "a", vec![("t", s("X"))]);
        fx.add("N", "b", vec![("t", s("X"))]);
        fx.add("N", "c", vec![("t", s("Y"))]);
        let v = fx.view();
        let params = BTreeMap::new();

        let rs = run(&v, "MATCH (n:N) RETURN n.t, COUNT(*) AS cnt", &params)
            .expect("single-key grouped COUNT must succeed");
        assert_eq!(
            rs.columns(),
            &["n.t".to_string(), "cnt".to_string()],
            "columns must match RETURN clause"
        );
        assert_eq!(rs.len(), 2, "must produce exactly 2 groups (X and Y)");

        // Find each group regardless of row order.
        let find = |label: &Value| {
            (0..rs.len()).find(|&i| rs.row(i)[0].as_ref() == Some(label))
        };
        let xi = find(&s("X")).expect("group X must exist");
        let yi = find(&s("Y")).expect("group Y must exist");
        assert_eq!(rs.row(xi)[1], Some(i(2)), "X group count must be 2");
        assert_eq!(rs.row(yi)[1], Some(i(1)), "Y group count must be 1");
    }

    #[test]
    fn grouped_two_keys_sum_avg() {
        // Four nodes with two categorical props and a numeric value.
        let mut fx = Fx::new();
        fx.add("N", "a", vec![("cat", s("A")), ("sub", s("1")), ("v", i(10))]);
        fx.add("N", "b", vec![("cat", s("A")), ("sub", s("1")), ("v", i(20))]);
        fx.add("N", "c", vec![("cat", s("A")), ("sub", s("2")), ("v", i(5))]);
        fx.add("N", "d", vec![("cat", s("B")), ("sub", s("1")), ("v", i(100))]);
        let v = fx.view();
        let params = BTreeMap::new();

        let rs = run(
            &v,
            "MATCH (n:N) RETURN n.cat, n.sub, SUM(n.v) AS total, AVG(n.v) AS avg_v",
            &params,
        )
        .expect("two-key SUM + AVG must succeed");
        assert_eq!(
            rs.columns(),
            &[
                "n.cat".to_string(),
                "n.sub".to_string(),
                "total".to_string(),
                "avg_v".to_string()
            ]
        );
        assert_eq!(rs.len(), 3, "must produce 3 groups: (A,1), (A,2), (B,1)");

        let find = |cat: &Value, sub: &Value| {
            (0..rs.len()).find(|&i| {
                rs.row(i)[0].as_ref() == Some(cat) && rs.row(i)[1].as_ref() == Some(sub)
            })
        };
        let a1 = find(&s("A"), &s("1")).expect("group (A,1) must exist");
        assert_eq!(rs.row(a1)[2], Some(f(30.0)), "(A,1) SUM must be 30.0");
        assert_eq!(rs.row(a1)[3], Some(f(15.0)), "(A,1) AVG must be 15.0");

        let a2 = find(&s("A"), &s("2")).expect("group (A,2) must exist");
        assert_eq!(rs.row(a2)[2], Some(f(5.0)), "(A,2) SUM must be 5.0");

        let b1 = find(&s("B"), &s("1")).expect("group (B,1) must exist");
        assert_eq!(rs.row(b1)[2], Some(f(100.0)), "(B,1) SUM must be 100.0");
        assert_eq!(rs.row(b1)[3], Some(f(100.0)), "(B,1) AVG must be 100.0");
    }

    #[test]
    fn grouped_order_by_count_desc_limit() {
        // 5 categories with different node counts: D=4, A=3, B=2, C=1, E=1.
        let mut fx = Fx::new();
        fx.add("N", "a1", vec![("cat", s("A"))]);
        fx.add("N", "a2", vec![("cat", s("A"))]);
        fx.add("N", "a3", vec![("cat", s("A"))]);
        fx.add("N", "b1", vec![("cat", s("B"))]);
        fx.add("N", "b2", vec![("cat", s("B"))]);
        fx.add("N", "c1", vec![("cat", s("C"))]);
        fx.add("N", "d1", vec![("cat", s("D"))]);
        fx.add("N", "d2", vec![("cat", s("D"))]);
        fx.add("N", "d3", vec![("cat", s("D"))]);
        fx.add("N", "d4", vec![("cat", s("D"))]);
        fx.add("N", "e1", vec![("cat", s("E"))]);
        let v = fx.view();
        let params = BTreeMap::new();

        let rs = run(
            &v,
            "MATCH (n:N) RETURN n.cat, COUNT(*) AS cnt ORDER BY cnt DESC LIMIT 3",
            &params,
        )
        .expect("ORDER BY count DESC LIMIT 3 must succeed");
        assert_eq!(rs.len(), 3, "LIMIT 3 must return exactly 3 groups");
        // Top 3: D(4), A(3), B(2) — descending order.
        assert_eq!(rs.row(0)[1], Some(i(4)), "row 0 must be count 4");
        assert_eq!(rs.row(0)[0], Some(s("D")), "row 0 must be category D");
        assert_eq!(rs.row(1)[1], Some(i(3)), "row 1 must be count 3");
        assert_eq!(rs.row(1)[0], Some(s("A")), "row 1 must be category A");
        assert_eq!(rs.row(2)[1], Some(i(2)), "row 2 must be count 2");
        assert_eq!(rs.row(2)[0], Some(s("B")), "row 2 must be category B");

        // Also verify row_bound is None for this plan (LIMIT must not be pushed).
        let plan_ops = plan_src("MATCH (n:N) RETURN n.cat, COUNT(*) AS cnt ORDER BY cnt DESC LIMIT 3")
            .expect("plan must succeed");
        assert_eq!(
            crate::cypher::plan::row_bound(&plan_ops),
            None,
            "GroupAggregate plan with LIMIT must have row_bound = None"
        );
    }

    #[test]
    fn grouped_empty_input_yields_zero_groups() {
        let fx = Fx::new(); // empty graph
        let v = fx.view();
        let params = BTreeMap::new();

        let rs = run(&v, "MATCH (n:N) RETURN n.t, COUNT(*) AS cnt", &params)
            .expect("grouped aggregate on empty graph must succeed");
        assert_eq!(rs.len(), 0, "empty input must yield zero groups");
        assert_eq!(
            rs.columns(),
            &["n.t".to_string(), "cnt".to_string()],
            "columns must still be present even with zero rows"
        );
    }

    #[test]
    fn grouped_null_key_groups_together() {
        // openCypher semantics: NULL group keys group together.
        let mut fx = Fx::new();
        fx.add("N", "a", vec![("t", s("X"))]);
        fx.add("N", "b", vec![]); // no "t" prop → null key
        fx.add("N", "c", vec![]); // null key — groups with b
        fx.add("N", "d", vec![("t", s("Y"))]);
        let v = fx.view();
        let params = BTreeMap::new();

        let rs = run(&v, "MATCH (n:N) RETURN n.t, COUNT(*) AS cnt", &params)
            .expect("null-key grouped aggregate must succeed");
        assert_eq!(rs.len(), 3, "must produce 3 groups: X, null, Y");

        // Locate the null group: row where n.t column is None (null).
        let null_row = (0..rs.len())
            .find(|&i| rs.row(i)[0].is_none())
            .expect("null group must be present");
        assert_eq!(
            rs.row(null_row)[1],
            Some(i(2)),
            "null group must count 2 rows (b and c)"
        );

        let x_row = (0..rs.len())
            .find(|&i| rs.row(i)[0] == Some(s("X")))
            .expect("X group must exist");
        assert_eq!(rs.row(x_row)[1], Some(i(1)));

        let y_row = (0..rs.len())
            .find(|&i| rs.row(i)[0] == Some(s("Y")))
            .expect("Y group must exist");
        assert_eq!(rs.row(y_row)[1], Some(i(1)));
    }

    #[test]
    fn grouped_cap_error_on_high_cardinality() {
        // Use with_max_groups to cap at 2 groups, then run a query that would
        // produce 3 groups → must return the named cap error.
        let mut fx = Fx::new();
        fx.add("N", "a", vec![("t", s("A"))]);
        fx.add("N", "b", vec![("t", s("B"))]);
        fx.add("N", "c", vec![("t", s("C"))]);
        let v = fx.view();
        let params = BTreeMap::new();

        let err = super::with_max_groups(2, || {
            run(&v, "MATCH (n:N) RETURN n.t, COUNT(*) AS cnt", &params)
        })
        .expect_err("must error when group count exceeds cap");
        assert!(
            err.to_ascii_lowercase().contains("group count"),
            "error must mention group count, got: {err}"
        );
    }

    #[test]
    fn multi_aggregate_no_keys() {
        // RETURN COUNT(*), COUNT(n) — multiple aggregates, no group keys.
        // Routes to GroupAggregate with empty keys.
        let mut fx = Fx::new();
        fx.add("N", "a", vec![]);
        fx.add("N", "b", vec![]);
        let v = fx.view();
        let params = BTreeMap::new();

        let rs = run(&v, "MATCH (n:N) RETURN COUNT(*), COUNT(n)", &params)
            .expect("multi-aggregate no keys must succeed");
        assert_eq!(rs.len(), 1, "must produce exactly one result row");
        assert_eq!(rs.row(0)[0], Some(i(2)), "COUNT(*) must be 2");
        assert_eq!(rs.row(0)[1], Some(i(2)), "COUNT(n) must be 2");
    }

    #[test]
    fn aggregate_over_hop_counts_edges() {
        let fx = hop_graph();
        let v = fx.view();
        let params = BTreeMap::new();
        // hop_graph has 3 KNOWS edges
        let rs = run(
            &v,
            "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN COUNT(*)",
            &params,
        )
        .expect("COUNT(*) on hop graph");
        assert_eq!(rs.row(0), &[Some(i(3))]);
    }

    // ──────────────────────────────────────────────────────────────────────────
    // C-1 fix tests: empty-input no-keys multi-aggregate must return 1 row
    // ──────────────────────────────────────────────────────────────────────────

    #[test]
    fn multi_aggregate_no_keys_empty_graph() {
        // C-1: RETURN COUNT(*), COUNT(n) on an empty graph must produce exactly
        // one row (COUNT=0, COUNT=0), not zero rows.  Routes to GroupAggregate
        // (keys=[], aggs=[Count, Count]).  Verifies parity with the single-agg
        // fast path, which also returns one row on empty input.
        let fx = Fx::new(); // empty — no nodes
        let v = fx.view();
        let params = BTreeMap::new();

        let rs = run(&v, "MATCH (n:N) RETURN COUNT(*), COUNT(n)", &params)
            .expect("empty-graph multi-agg must succeed");
        assert_eq!(rs.len(), 1, "must produce exactly 1 row on empty input");
        assert_eq!(rs.row(0)[0], Some(i(0)), "COUNT(*) on empty graph must be 0");
        assert_eq!(rs.row(0)[1], Some(i(0)), "COUNT(n) on empty graph must be 0");
    }

    #[test]
    fn fast_path_and_group_path_agree_on_empty_input() {
        // Equivalence pin: the single-agg fast path (Aggregate) and the
        // multi-agg GroupAggregate path must agree on COUNT for empty input.
        // Single-agg fast path: RETURN COUNT(*) → execute_aggregate.
        // Group path: RETURN COUNT(*), COUNT(n) → execute_group_aggregate.
        // Both must report COUNT = 0 on an empty graph.
        let fx = Fx::new();
        let v = fx.view();
        let params = BTreeMap::new();

        let fast = run(&v, "MATCH (n:N) RETURN COUNT(*)", &params)
            .expect("fast-path COUNT on empty graph");
        assert_eq!(fast.len(), 1, "fast path: 1 row on empty input");
        let fast_count = fast.row(0)[0].clone();

        let grouped = run(&v, "MATCH (n:N) RETURN COUNT(*), COUNT(n)", &params)
            .expect("group-path COUNT on empty graph");
        assert_eq!(grouped.len(), 1, "group path: 1 row on empty input");
        let group_count = grouped.row(0)[0].clone();

        assert_eq!(
            fast_count, group_count,
            "fast path and group path must agree on COUNT(*) for empty input"
        );

        // Same check on non-empty input: both paths should see count = 2.
        let mut fx2 = Fx::new();
        fx2.add("N", "x", vec![]);
        fx2.add("N", "y", vec![]);
        let v2 = fx2.view();

        let fast2 = run(&v2, "MATCH (n:N) RETURN COUNT(*)", &params)
            .expect("fast-path COUNT on 2-node graph");
        assert_eq!(fast2.row(0)[0], Some(i(2)), "fast path: COUNT(*) = 2");

        let grouped2 = run(&v2, "MATCH (n:N) RETURN COUNT(*), COUNT(n)", &params)
            .expect("group-path COUNT on 2-node graph");
        assert_eq!(grouped2.row(0)[0], Some(i(2)), "group path: COUNT(*) = 2");
        assert_eq!(grouped2.row(0)[1], Some(i(2)), "group path: COUNT(n) = 2");
    }

    // ──────────────────────────────────────────────────────────────────────────
    // I-1 fix tests: Int/Float group-key unification
    // ──────────────────────────────────────────────────────────────────────────

    #[test]
    fn grouped_int_float_key_unification() {
        // openCypher equality: 1 = 1.0.  Nodes with score=1 (Int) and score=1.0
        // (Float) must land in the same group after group_key_normalize maps
        // both to FloatBits.  Result: one group with count=2.
        //
        // Display value: first-seen wins.  Node "a" (Int(1)) is scanned first
        // (lower id), so the key column must display as Int(1), not Float(1.0).
        let mut fx = Fx::new();
        fx.add("N", "a", vec![("score", i(1))]); // scanned first → first-seen
        fx.add("N", "b", vec![("score", f(1.0))]);
        let v = fx.view();
        let params = BTreeMap::new();

        let rs = run(&v, "MATCH (n:N) RETURN n.score, COUNT(*) AS cnt", &params)
            .expect("int/float unification must succeed");
        assert_eq!(rs.len(), 1, "Int(1) and Float(1.0) must group together into 1 group");
        // Key column must display the first-seen original value (Int), not Float.
        assert_eq!(rs.row(0)[0], Some(i(1)), "key column must display Int(1) (first-seen)");
        assert_eq!(rs.row(0)[1], Some(i(2)), "unified group must have count=2");
    }

    #[test]
    fn pure_int_keys_display_as_int() {
        // N-1 regression: group keys that are integers must not be upcast to
        // Float in the output row.  Previously group_key_normalize converted
        // Int→FloatBits and value_key_to_value reconstructed Float(42.0) from
        // the key; now the original Value is stored and reused for display.
        let mut fx = Fx::new();
        fx.add("N", "a", vec![("age", i(10))]);
        fx.add("N", "b", vec![("age", i(20))]);
        fx.add("N", "c", vec![("age", i(10))]);
        let v = fx.view();
        let params = BTreeMap::new();

        let rs = run(
            &v,
            "MATCH (n:N) RETURN n.age, COUNT(*) AS cnt ORDER BY n.age",
            &params,
        )
        .expect("pure-Int group keys must succeed");
        assert_eq!(rs.len(), 2, "must have 2 groups: age=10 and age=20");
        // Key column must be Int, not Float.
        assert_eq!(rs.row(0)[0], Some(i(10)), "age=10 key column must display as Int(10)");
        assert_eq!(rs.row(0)[1], Some(i(2)), "age=10 group has 2 nodes");
        assert_eq!(rs.row(1)[0], Some(i(20)), "age=20 key column must display as Int(20)");
        assert_eq!(rs.row(1)[1], Some(i(1)), "age=20 group has 1 node");
    }

    // ──────────────────────────────────────────────────────────────────────────
    // M-1: VarExpand + GroupAggregate staged-path integration test
    // ──────────────────────────────────────────────────────────────────────────

    #[test]
    fn var_expand_group_aggregate_staged_path() {
        // Combines variable-length MATCH with a grouped RETURN — exercises the
        // staged-path GroupAggregate arm that groups over materialised VarExpand
        // rows.
        //
        // Graph: chain a -T-> b -T-> c  (3 nodes, 2 edges)
        // MATCH (x)-[*1..2]->(y) RETURN y.key, COUNT(*)
        //   1-hop results: (x=a, y=b), (x=b, y=c)       → b gets 1, c gets 1
        //   2-hop results: (x=a, y=c)                    → c gets 1 more
        //   So: b → 1, c → 2.
        let mut fx = Fx::new();
        let a = fx.add("N", "a", vec![("key", s("a"))]);
        let b = fx.add("N", "b", vec![("key", s("b"))]);
        let c = fx.add("N", "c", vec![("key", s("c"))]);
        fx.edge("T", a, b, vec![]);
        fx.edge("T", b, c, vec![]);
        let v = fx.view();
        let params = BTreeMap::new();

        let rs = run(
            &v,
            "MATCH (x:N)-[*1..2]->(y:N) RETURN y.key, COUNT(*) AS cnt ORDER BY y.key",
            &params,
        )
        .expect("VarExpand + GroupAggregate must succeed");

        assert_eq!(rs.len(), 2, "must have 2 destination groups: b and c");
        assert_eq!(rs.row(0)[0], Some(s("b")), "first group key must be 'b'");
        assert_eq!(rs.row(0)[1], Some(i(1)), "b is reached via 1 path");
        assert_eq!(rs.row(1)[0], Some(s("c")), "second group key must be 'c'");
        assert_eq!(rs.row(1)[1], Some(i(2)), "c is reached via 2 paths (1-hop and 2-hop)");
    }

    // ──────────────────────────────────────────────────────────────────────────
    // pull_rows defense-in-depth: VarExpand and ShortestPath Err arms
    // These arms exist so that adding PlanOp variants forces a compile-time
    // decision in pull_rows.  Routing prevention is tested separately; these
    // tests verify the arms themselves are not dead code.
    // ──────────────────────────────────────────────────────────────────────────

    #[test]
    fn pull_rows_var_expand_arm_returns_named_err() {
        let fx = Fx::new();
        let view = fx.view();
        let vars = super::VarTable { names: vec!["a".into(), "b".into()] };
        let project_items: Vec<crate::cypher::ast::RetItem> = vec![];
        let empty_params = BTreeMap::new();
        let params = super::Params(&empty_params);
        let ctx = super::PullCtx {
            view: &view,
            vars: &vars,
            project_items: &project_items,
            params: &params,
            bound: 100,
        };
        let ops = vec![PlanOp::VarExpand {
            from: "a".into(),
            rel_var: None,
            etype: None,
            dir: crate::cypher::RelDir::Right,
            to: "b".into(),
            min: 1,
            max: 3,
        }];
        let mut row = vec![None; vars.names.len()];
        let mut result = Vec::new();
        let err = super::pull_rows(&ctx, &ops, &mut row, &mut result)
            .expect_err("VarExpand must Err in pull_rows");
        assert!(
            err.contains("VarExpand") && err.contains("pull executor"),
            "error must name VarExpand and pull executor, got: {err}"
        );
    }

    #[test]
    fn pull_rows_shortest_path_arm_returns_named_err() {
        let fx = Fx::new();
        let view = fx.view();
        let vars = super::VarTable { names: vec!["a".into(), "b".into()] };
        let project_items: Vec<crate::cypher::ast::RetItem> = vec![];
        let empty_params = BTreeMap::new();
        let params = super::Params(&empty_params);
        let ctx = super::PullCtx {
            view: &view,
            vars: &vars,
            project_items: &project_items,
            params: &params,
            bound: 100,
        };
        let ops = vec![PlanOp::ShortestPath {
            from: "a".into(),
            rel_var: None,
            etype: None,
            dir: crate::cypher::RelDir::Right,
            to: "b".into(),
            max_hops: 5,
        }];
        let mut row = vec![None; vars.names.len()];
        let mut result = Vec::new();
        let err = super::pull_rows(&ctx, &ops, &mut row, &mut result)
            .expect_err("ShortestPath must Err in pull_rows");
        assert!(
            err.contains("ShortestPath") && err.contains("pull executor"),
            "error must name ShortestPath and pull executor, got: {err}"
        );
    }
}
