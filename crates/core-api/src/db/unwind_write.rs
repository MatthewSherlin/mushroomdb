//! `UNWIND … <write clauses>` (QL-09b): the write clauses run once per row the
//! read part produces, and the whole statement commits as one batch.
//!
//! The statement runs in two phases, like every Cypher write here:
//!
//! 1. **Read.** The read part (UNWIND, MATCH, WITH, …) runs once, through the
//!    ordinary query engine, over the state before the statement. It projects
//!    the node and value variables of each row, and one column for every
//!    expression a write clause evaluates that does not read a node's
//!    properties (`r.id`, `$tenant`, `toUpper(r.name)`).
//! 2. **Write.** One pass over the rows turns the write clauses into batch ops,
//!    in order. A small overlay of the statement's own effects (nodes created,
//!    deleted, properties changed, edges created) lets a later row, and a later
//!    clause, see an earlier one: `UNWIND [1, 1] AS x MERGE (:N {id: x})`
//!    creates one node, and `SET n.count = n.count + 1` over two rows that match
//!    the same node adds two. An expression that reads a node's properties is
//!    evaluated here, at its turn, against the overlay (see [`super::row_eval`]).
//!    The ops commit as a single `Batch` frame, so a failure on any row writes
//!    nothing, and every write meets the same commit-time checks as `CREATE` or
//!    `SET` (constraints, the `id` rule, namespace and view-owned refusals,
//!    rules).
//! 3. **Return.** A `RETURN` projects the rows the pass captured (each row's
//!    node keys and values) after the commit, reading node properties from the
//!    written store; a node the statement deleted is null.
//!
//! The overlay is hash-keyed, so the pass is linear in the rows.

use super::row_eval::{reads_node, Overlay};
use super::*;
use core_query::cypher::{
    ast::OptionalClause, RowCreateNode, RowMerge, RowMergeNode, RowReturn, RowWrite, SetClause,
    UnwindClause, UnwindExpr, UnwindWriteStmt,
};

/// The parameter that carries the captured rows to the `RETURN` query.
const ROWS_PARAM: &str = "__uw_rows";
/// The name the `RETURN` query gives one captured row.
const ROW_VAR: &str = "__uw_row";

/// Where each write operand's value sits in the read part's result. An operand
/// with no entry reads a node's properties and is evaluated as the rows are
/// written.
struct Columns {
    by_operand: HashMap<*const Operand, String>,
    items: Vec<RetItem>,
    /// Some write operand is evaluated per row at write time.
    dynamic: bool,
}

impl Columns {
    fn operand(&mut self, op: &Operand, node_vars: &[String]) {
        if reads_node(op, node_vars) {
            self.dynamic = true;
            return;
        }
        let name = format!("__uw{}", self.items.len());
        self.by_operand.insert(op as *const Operand, name.clone());
        self.items.push(RetItem {
            value: RetVal::ScalarExpr(op.clone()),
            alias: Some(name),
        });
    }

    /// The column that holds `op`, or `None` when it is evaluated at write time.
    fn column_of(&self, op: &Operand) -> Option<&str> {
        self.by_operand
            .get(&(op as *const Operand))
            .map(String::as_str)
    }
}

/// Counts the statement reports.
#[derive(Default)]
struct Tally {
    created: i64,
    props_set: i64,
    deleted: i64,
}

fn err(detail: String) -> GraphError {
    GraphError::QueryError { detail }
}

/// The columns the read part projects: every variable the write clauses or the
/// `RETURN` need, and one per write operand that does not read a node.
fn plan_columns(stmt: &UnwindWriteStmt) -> Columns {
    let mut cols = Columns {
        by_operand: HashMap::new(),
        items: Vec::new(),
        dynamic: false,
    };
    // A constant column, so the read part always yields its row count.
    cols.items.push(RetItem {
        value: RetVal::ScalarExpr(Operand::Lit(Value::Int(1))),
        alias: Some("__uw_row".into()),
    });
    let nodes = &stmt.node_vars;
    let mut var_names: Vec<String> = Vec::new();
    let mut note_var = |v: &str| {
        if stmt.scope.iter().any(|s| s == v) && !var_names.iter().any(|n| n == v) {
            var_names.push(v.to_string());
        }
    };
    for write in &stmt.writes {
        match write {
            RowWrite::Create { nodes: made, edges } => {
                for node in made {
                    for (_, op) in &node.props {
                        cols.operand(op, nodes);
                    }
                }
                for edge in edges {
                    note_var(&edge.src_var);
                    note_var(&edge.dst_var);
                }
            }
            RowWrite::Merge(merge) => {
                merge_operands(&merge.start, &mut cols, nodes);
                if let Some(edge) = &merge.edge {
                    merge_operands(&edge.end, &mut cols, nodes);
                }
                for sc in merge.on_create.iter().chain(&merge.on_match) {
                    cols.operand(&sc.value, nodes);
                }
            }
            RowWrite::Set(sets) => {
                for sc in sets {
                    note_var(&sc.var);
                    cols.operand(&sc.value, nodes);
                }
            }
            RowWrite::SetMap { var, value, .. } => {
                note_var(var);
                cols.operand(value, nodes);
            }
            RowWrite::Remove(items) => {
                for (var, _) in items {
                    note_var(var);
                }
            }
            RowWrite::Delete { vars, .. } => {
                for var in vars {
                    note_var(var);
                }
            }
        }
    }
    // A write evaluated at its turn, or a RETURN, needs every variable of the
    // row, not only the nodes a write clause names.
    let wanted: Vec<String> = if cols.dynamic || stmt.returns.is_some() {
        stmt.scope.clone()
    } else {
        var_names
    };
    for name in wanted {
        cols.items.push(RetItem {
            value: RetVal::Var(name),
            alias: None,
        });
    }
    cols
}

fn merge_operands(node: &RowMergeNode, cols: &mut Columns, nodes: &[String]) {
    cols.operand(&node.key, nodes);
    if let Some(ns) = &node.ns {
        cols.operand(ns, nodes);
    }
}

/// The read query a `RETURN` runs after the commit. It unwinds the rows the
/// write pass captured (each row's node keys and values), restores the value
/// variables, and finds each node variable again by its key with an
/// `OPTIONAL MATCH`, so a node the statement deleted is null rather than a
/// dropped row. The read part is not run again.
fn return_query(stmt: &UnwindWriteStmt, ret: &RowReturn) -> Query {
    let row_field = |name: &str| Operand::Prop {
        var: ROW_VAR.to_string(),
        field: name.to_string(),
    };
    let mut items: Vec<RetItem> = vec![RetItem {
        value: RetVal::Var(ROW_VAR.to_string()),
        alias: None,
    }];
    let mut optional_clauses: Vec<OptionalClause> = Vec::new();
    let mut names: Vec<&String> = stmt.scope.iter().collect();
    for node in &stmt.node_vars {
        if !names.contains(&node) {
            names.push(node);
        }
    }
    // A node variable the RETURN never names is not looked up (`RETURN count(*)`
    // over a bulk load reads no node). The test is on the clause's debug text:
    // an extra match only costs a lookup, and every name the clause uses appears
    // in it as a quoted string.
    let used = format!("{:?}{:?}", ret.items, ret.order_by);
    for name in names {
        if stmt.node_vars.contains(name) {
            if !used.contains(&format!("{name:?}")) {
                continue;
            }
            optional_clauses.push(OptionalClause {
                patterns: vec![Pattern {
                    start: NodePat {
                        var: Some(name.clone()),
                        label: None,
                        // `id` is the planner's key lookup (a `ScanKey` per row, not a scan).
                        props: vec![("id".to_string(), row_field(name))],
                    },
                    chain: vec![],
                    shortest: false,
                }],
                where_expr: None,
            });
        } else {
            items.push(RetItem {
                value: RetVal::ScalarExpr(row_field(name)),
                alias: Some(name.clone()),
            });
        }
    }
    Query {
        matches: Vec::new(),
        optional_clauses: Vec::new(),
        where_expr: None,
        unwinds: vec![UnwindClause {
            list: UnwindExpr::Expr(Operand::Param(ROWS_PARAM.to_string())),
            alias: ROW_VAR.to_string(),
        }],
        post_unwind_where: None,
        stages: vec![core_query::cypher::WithStage {
            items,
            where_expr: None,
            order_by: Vec::new(),
            skip: None,
            limit: None,
            matches: Vec::new(),
            optional_clauses,
            unwinds: Vec::new(),
            post_where: None,
        }],
        returns: ret.items.clone(),
        distinct: ret.distinct,
        order_by: ret.order_by.clone(),
        skip: ret.skip.clone(),
        limit: ret.limit.clone(),
    }
}

/// One row of the pass: how a write clause reads the row's values.
struct RowCtx<'a, F: Fs> {
    db: &'a GraphDb<F>,
    rows: &'a ResultSet,
    row: usize,
    cols: &'a Columns,
    node_vars: &'a HashSet<&'a str>,
    /// The row's non-node variables.
    values: Vec<(&'a str, Value)>,
    params: &'a BTreeMap<String, Value>,
}

impl<F: Fs> RowCtx<'_, F> {
    /// The value of `op` for this row at this point of the statement; `None`
    /// is null.
    fn eval(
        &self,
        op: &Operand,
        env: &HashMap<&str, String>,
        ov: &mut Overlay,
    ) -> Result<Option<Value>> {
        if let Some(col) = self.cols.column_of(op) {
            return Ok(match self.rows.get(self.row, col) {
                None | Some(Value::Null) => None,
                Some(v) => Some(v.clone()),
            });
        }
        let node_key = |name: &str| -> Option<Option<String>> {
            self.node_vars
                .contains(name)
                .then(|| env.get(name).cloned())
        };
        self.db
            .uw_eval(ov, op, &node_key, &self.values, self.params)
    }
}

impl<F: Fs> GraphDb<F> {
    /// Run `UNWIND … <write clauses> [RETURN …]`.
    pub(super) fn exec_unwind_write(
        &mut self,
        stmt: UnwindWriteStmt,
        params: &BTreeMap<String, Value>,
    ) -> Result<ResultSet> {
        // Plan the RETURN first: a statement that cannot be projected must not
        // commit.
        let return_plan = match &stmt.returns {
            Some(ret) => {
                let query = return_query(&stmt, ret);
                plan(&query).map_err(|e| err(format!("plan: {e}")))?;
                Some(query)
            }
            None => None,
        };

        // Phase 1: the read part, one row per iteration.
        let mut cols = plan_columns(&stmt);
        let mut read = stmt.read.clone();
        read.returns = std::mem::take(&mut cols.items);
        let ops = plan(&read).map_err(|e| err(format!("plan: {e}")))?;
        cols.items = read.returns;
        let rows = {
            let mask_opt = self.pending_write_authz.as_ref().map(|a| a.mask.clone());
            if let Some(ref mask) = mask_opt {
                execute(&self.view_masked(mask), &ops, &Params(params))
            } else {
                execute(&self.view(), &ops, &Params(params))
            }
        }
        .map_err(|e| err(format!("execute: {e}")))?;

        // Phase 2: the write clauses, per row.
        let var_cols: Vec<&str> = cols
            .items
            .iter()
            .filter(|i| i.alias.is_none())
            .filter_map(|i| match &i.value {
                RetVal::Var(v) => Some(v.as_str()),
                _ => None,
            })
            .collect();
        let node_vars: HashSet<&str> = stmt.node_vars.iter().map(String::as_str).collect();
        let capture = stmt.returns.is_some();
        let mut captured: Vec<Value> = Vec::new();
        let mut ov = Overlay::default();
        let mut ops: Vec<BatchOp> = Vec::new();
        let mut tally = Tally::default();
        for row in 0..rows.len() {
            let mut env: HashMap<&str, String> = HashMap::new();
            let mut values: Vec<(&str, Value)> = Vec::new();
            for var in &var_cols {
                match rows.get(row, var) {
                    Some(Value::Null) | None => {}
                    Some(Value::Str(key)) if node_vars.contains(var) => {
                        env.insert(var, key.clone());
                    }
                    Some(v) if !node_vars.contains(var) => values.push((var, v.clone())),
                    Some(_) => {}
                }
            }
            let ctx = RowCtx {
                db: self,
                rows: &rows,
                row,
                cols: &cols,
                node_vars: &node_vars,
                values,
                params,
            };
            for write in &stmt.writes {
                self.uw_write(write, &ctx, &mut env, &mut ov, &mut ops, &mut tally)?;
            }
            if capture {
                let mut entry: BTreeMap<String, Value> = BTreeMap::new();
                for (name, v) in &ctx.values {
                    entry.insert((*name).to_string(), v.clone());
                }
                for (name, key) in &env {
                    entry.insert((*name).to_string(), Value::Str(key.clone()));
                }
                captured.push(Value::Map(entry));
            }
        }

        if !ops.is_empty() {
            self.commit_batch(ops)?;
        }
        // Refresh the role mask so a created node is visible to the RETURN
        // (read-after-write); see `exec_merge`.
        if tally.created > 0 {
            if let Some(role) = self.pending_write_authz.as_ref().map(|a| a.role.clone()) {
                let new_mask = self.mask_for_role(&role)?;
                if let Some(a) = self.pending_write_authz.as_mut() {
                    a.mask = new_mask;
                }
            }
        }

        if let Some(query) = return_plan {
            let ops = plan(&query).map_err(|e| err(format!("plan: {e}")))?;
            // Only the parameters the RETURN names go along: the statement's own
            // (a bulk load's rows) are not copied again.
            let used = format!("{:?}", query);
            let mut with_rows: BTreeMap<String, Value> = params
                .iter()
                .filter(|(name, _)| used.contains(&format!("{name:?}")))
                .map(|(name, v)| (name.clone(), v.clone()))
                .collect();
            with_rows.insert(ROWS_PARAM.to_string(), Value::List(captured));
            let mask_opt = self.pending_write_authz.as_ref().map(|a| a.mask.clone());
            return if let Some(ref mask) = mask_opt {
                execute(&self.view_masked(mask), &ops, &Params(&with_rows))
            } else {
                execute(&self.view(), &ops, &Params(&with_rows))
            }
            .map_err(|e| err(format!("execute: {e}")));
        }
        let mut rs = write_result_set();
        rs.push_row(vec![
            Some(Value::Int(tally.created)),
            Some(Value::Int(tally.props_set)),
            Some(Value::Int(tally.deleted)),
        ]);
        Ok(rs)
    }

    /// One write clause for one row.
    fn uw_write<'v>(
        &self,
        write: &'v RowWrite,
        ctx: &RowCtx<'_, F>,
        env: &mut HashMap<&'v str, String>,
        ov: &mut Overlay,
        ops: &mut Vec<BatchOp>,
        tally: &mut Tally,
    ) -> Result<()> {
        match write {
            RowWrite::Create { nodes, edges } => {
                for node in nodes {
                    self.uw_create_node(node, ctx, env, ov, ops)?;
                    tally.created += 1;
                }
                for edge in edges {
                    let end = |var: &str, side: &str| -> Result<String> {
                        env.get(var).cloned().ok_or_else(|| {
                            err(format!("CREATE edge {side} variable '{var}' is not bound"))
                        })
                    };
                    let src = end(&edge.src_var, "src")?;
                    let dst = end(&edge.dst_var, "dst")?;
                    ops.push(BatchOp::InsertEdge {
                        edge_type: edge.etype.clone(),
                        src_key: src.clone(),
                        dst_key: dst.clone(),
                    });
                    ov.edge_created(&edge.etype, &src, &dst);
                }
            }
            RowWrite::Merge(merge) => {
                self.uw_merge(merge, ctx, env, ov, ops, tally)?;
            }
            RowWrite::Set(sets) => {
                for sc in sets {
                    let Some(key) = env.get(sc.var.as_str()).cloned() else {
                        continue; // a null node (OPTIONAL MATCH): nothing to set
                    };
                    let Some(v) = ctx.eval(&sc.value, env, ov)? else {
                        return Err(err(format!(
                            "SET value for {}.{} evaluated to null",
                            sc.var, sc.field
                        )));
                    };
                    ov.note_set(&key, &sc.field, &v);
                    ops.push(BatchOp::SetProp {
                        key,
                        field: sc.field.clone(),
                        value: v,
                    });
                    tally.props_set += 1;
                }
            }
            RowWrite::SetMap {
                var,
                merge,
                value: operand,
            } => {
                let Some(key) = env.get(var.as_str()).cloned() else {
                    return Ok(());
                };
                let entries: MapEntries = match ctx.eval(operand, env, ov)? {
                    Some(Value::Map(m)) => m
                        .into_iter()
                        .map(|(k, v)| {
                            (
                                k,
                                if matches!(v, Value::Null) {
                                    None
                                } else {
                                    Some(v)
                                },
                            )
                        })
                        .collect(),
                    other => {
                        return Err(err(format!(
                            "SET {var} {}= <map>: the value must be a map, got {}",
                            if *merge { "+" } else { "" },
                            other.map_or("null".to_string(), |v| format!("{v:?}")),
                        )))
                    }
                };
                let existing = self.uw_props(ov, &key).clone();
                let writes = plan_map_write(&key, &existing, *merge, &entries, &|field| {
                    self.view_store.view_for_prop(field).is_some()
                });
                for w in writes {
                    match w {
                        MapWrite::Set { field, value } => {
                            tally.props_set += 1;
                            ov.note_set(&key, &field, &value);
                            ops.push(BatchOp::SetProp {
                                key: key.clone(),
                                field,
                                value,
                            });
                        }
                        MapWrite::Remove { field } => {
                            if existing.contains_key(&field) {
                                tally.props_set += 1;
                            }
                            ov.note_remove(&key, &field);
                            ops.push(BatchOp::RemoveProp {
                                key: key.clone(),
                                field,
                            });
                        }
                    }
                }
            }
            RowWrite::Remove(items) => {
                for (var, field) in items {
                    let Some(key) = env.get(var.as_str()).cloned() else {
                        continue;
                    };
                    if self.uw_props(ov, &key).contains_key(field) {
                        tally.props_set += 1;
                    }
                    ov.note_remove(&key, field);
                    ops.push(BatchOp::RemoveProp {
                        key,
                        field: field.clone(),
                    });
                }
            }
            RowWrite::Delete { vars, detach } => {
                for var in vars {
                    let Some(key) = env.get(var.as_str()).cloned() else {
                        continue;
                    };
                    if !self.uw_exists(ov, &key) {
                        continue; // already deleted by an earlier row
                    }
                    if !*detach && self.uw_has_edges(ov, &key) {
                        return Err(err(format!(
                            "Cannot delete node `{key}` because it still has incident edges. \
                             Use DETACH DELETE to remove the node and all its edges."
                        )));
                    }
                    ov.node_deleted(&key);
                    ops.push(BatchOp::DeleteNode { key });
                    tally.deleted += 1;
                }
            }
        }
        Ok(())
    }

    fn uw_create_node<'v>(
        &self,
        node: &'v RowCreateNode,
        ctx: &RowCtx<'_, F>,
        env: &mut HashMap<&'v str, String>,
        ov: &mut Overlay,
        ops: &mut Vec<BatchOp>,
    ) -> Result<()> {
        let mut props: Vec<(String, Value)> = Vec::with_capacity(node.props.len());
        for (field, operand) in &node.props {
            // A null value is an absent property.
            if let Some(v) = ctx.eval(operand, env, ov)? {
                props.push((field.clone(), v));
            }
        }
        let key = match props.iter().find(|(f, _)| f == "id") {
            Some((_, Value::Str(s))) => s.clone(),
            _ => {
                return Err(err(format!(
                    "CREATE node ({}:{}) requires a string 'id' property",
                    node.var, node.label
                )))
            }
        };
        if self.uw_exists(ov, &key) {
            return Err(GraphError::DuplicateKey { key });
        }
        ov.node_created(&key, &props);
        ops.push(BatchOp::InsertNode {
            label: node.label.clone(),
            key: key.clone(),
            props,
        });
        env.insert(node.var.as_str(), key);
        Ok(())
    }

    /// The key a MERGE node pattern names for this row.
    fn uw_merge_key(
        node: &RowMergeNode,
        ctx: &RowCtx<'_, F>,
        env: &HashMap<&str, String>,
        ov: &mut Overlay,
    ) -> Result<(String, Value)> {
        match ctx.eval(&node.key, env, ov)? {
            Some(Value::Str(s)) => Ok((s.clone(), Value::Str(s))),
            other => Err(err(format!(
                "MERGE key value must be a string (got {:?})",
                other.unwrap_or(Value::Null)
            ))),
        }
    }

    /// Whether a MERGE node exists at this point, after the role-scope check
    /// (made once per key, against the store).
    fn uw_merge_existed(&self, ov: &Overlay, label: &str, key: &str) -> Result<bool> {
        match ov.nodes.get(key) {
            // Created or deleted by the statement: its own answer.
            Some(state) if state.created_here || !state.exists => Ok(state.exists),
            _ => self.merge_node_existed(label, key),
        }
    }

    fn uw_merge<'v>(
        &self,
        merge: &'v RowMerge,
        ctx: &RowCtx<'_, F>,
        env: &mut HashMap<&'v str, String>,
        ov: &mut Overlay,
        ops: &mut Vec<BatchOp>,
        tally: &mut Tally,
    ) -> Result<()> {
        let (start_key, start_value) = Self::uw_merge_key(&merge.start, ctx, env, ov)?;
        let start_ns = match &merge.start.ns {
            Some(op) => ctx.eval(op, env, ov)?,
            None => None,
        };
        let Some(edge) = &merge.edge else {
            let existed = self.uw_merge_existed(ov, &merge.start.label, &start_key)?;
            if !existed {
                let props = self.merge_create_props(
                    &merge.start.key_field,
                    &start_value,
                    start_ns.as_ref(),
                )?;
                ov.node_created(&start_key, &props);
                ops.push(BatchOp::InsertNode {
                    label: merge.start.label.clone(),
                    key: start_key.clone(),
                    props,
                });
                tally.created += 1;
            }
            let sets = if existed {
                &merge.on_match
            } else {
                &merge.on_create
            };
            env.insert(merge.start.var.as_str(), start_key);
            self.uw_merge_sets(sets, ctx, env, ov, ops, tally)?;
            return Ok(());
        };

        let (end_key, end_value) = Self::uw_merge_key(&edge.end, ctx, env, ov)?;
        let end_ns = match &edge.end.ns {
            Some(op) => ctx.eval(op, env, ov)?,
            None => None,
        };
        if start_key == end_key && merge.start.label != edge.end.label {
            return Err(err(format!(
                "MERGE: key '{start_key}' is given both label '{}' and label '{}'",
                merge.start.label, edge.end.label
            )));
        }
        let start_existed = self.uw_merge_existed(ov, &merge.start.label, &start_key)?;
        let end_existed = if end_key == start_key {
            start_existed
        } else {
            self.uw_merge_existed(ov, &edge.end.label, &end_key)?
        };
        let (src_key, dst_key) = match edge.dir {
            core_query::cypher::RelDir::Left => (end_key.clone(), start_key.clone()),
            _ => (start_key.clone(), end_key.clone()),
        };
        let edge_existed = start_existed
            && end_existed
            && self.uw_edge_exists(ov, &edge.etype, &src_key, &dst_key);
        if !start_existed {
            let props =
                self.merge_create_props(&merge.start.key_field, &start_value, start_ns.as_ref())?;
            ov.node_created(&start_key, &props);
            ops.push(BatchOp::InsertNode {
                label: merge.start.label.clone(),
                key: start_key.clone(),
                props,
            });
            tally.created += 1;
        }
        if !end_existed && end_key != start_key {
            let props =
                self.merge_create_props(&edge.end.key_field, &end_value, end_ns.as_ref())?;
            ov.node_created(&end_key, &props);
            ops.push(BatchOp::InsertNode {
                label: edge.end.label.clone(),
                key: end_key.clone(),
                props,
            });
            tally.created += 1;
        }
        let sets = if edge_existed {
            &merge.on_match
        } else {
            ops.push(BatchOp::InsertEdge {
                edge_type: edge.etype.clone(),
                src_key: src_key.clone(),
                dst_key: dst_key.clone(),
            });
            ov.edge_created(&edge.etype, &src_key, &dst_key);
            &merge.on_create
        };
        env.insert(merge.start.var.as_str(), start_key);
        env.insert(edge.end.var.as_str(), end_key);
        self.uw_merge_sets(sets, ctx, env, ov, ops, tally)
    }

    /// `ON CREATE SET` / `ON MATCH SET` items of a MERGE; each names a node
    /// the MERGE bound.
    fn uw_merge_sets(
        &self,
        sets: &[SetClause],
        ctx: &RowCtx<'_, F>,
        env: &HashMap<&str, String>,
        ov: &mut Overlay,
        ops: &mut Vec<BatchOp>,
        tally: &mut Tally,
    ) -> Result<()> {
        for sc in sets {
            let Some(v) = ctx.eval(&sc.value, env, ov)? else {
                return Err(err(format!(
                    "SET value for {}.{} evaluated to null",
                    sc.var, sc.field
                )));
            };
            let Some(key) = env.get(sc.var.as_str()).cloned() else {
                return Err(err(format!("SET variable '{}' is not bound", sc.var)));
            };
            ov.note_set(&key, &sc.field, &v);
            ops.push(BatchOp::SetProp {
                key,
                field: sc.field.clone(),
                value: v,
            });
            tally.props_set += 1;
        }
        Ok(())
    }
}
