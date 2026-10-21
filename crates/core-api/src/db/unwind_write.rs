//! `UNWIND … <write clauses>` (QL-09b): the write clauses run once per row the
//! read part produces, and the whole statement commits as one batch.
//!
//! The statement runs in two phases, like every Cypher write here:
//!
//! 1. **Read.** The read part (UNWIND, MATCH, WITH, …) runs once, through the
//!    ordinary query engine, over the state before the statement. It projects
//!    one column per node variable a write clause names and one per expression
//!    a write clause evaluates, so every per-row value is computed by the same
//!    evaluator a `RETURN` uses.
//! 2. **Write.** One pass over the rows turns the write clauses into batch ops.
//!    A small overlay of the statement's own effects (nodes created, deleted,
//!    properties changed, edges created) lets a later row see an earlier one:
//!    `UNWIND [1, 1] AS x MERGE (:N {id: x})` creates one node. The ops commit
//!    as a single `Batch` frame, so a failure on any row writes nothing, and
//!    every write meets the same commit-time checks as `CREATE` or `SET`
//!    (constraints, the `id` rule, namespace and view-owned refusals, rules).
//!
//! The overlay is hash-keyed, so the pass is linear in the rows.

use super::*;
use core_query::cypher::{
    RowCreateNode, RowMerge, RowMergeNode, RowReturn, RowWrite, SetClause, UnwindWriteStmt,
};

/// What the statement has done to one node so far.
struct NodeState {
    /// The node exists at this point of the statement.
    exists: bool,
    /// The statement created the node (its edges are not in the store).
    created_here: bool,
    /// The node's properties at this point, once known.
    props: Option<BTreeMap<String, Value>>,
}

/// The statement's own effects, read back by later rows.
#[derive(Default)]
struct Overlay {
    nodes: HashMap<String, NodeState>,
    edges: HashSet<(String, String, String)>,
}

/// Where each write operand's value sits in the read part's result.
struct Columns {
    by_operand: HashMap<*const Operand, String>,
    items: Vec<RetItem>,
}

impl Columns {
    fn operand(&mut self, op: &Operand) {
        let name = format!("__uw{}", self.items.len());
        self.by_operand.insert(op as *const Operand, name.clone());
        self.items.push(RetItem {
            value: RetVal::ScalarExpr(op.clone()),
            alias: Some(name),
        });
    }

    fn column_of(&self, op: &Operand) -> &str {
        self.by_operand
            .get(&(op as *const Operand))
            .map(String::as_str)
            .unwrap_or("")
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

/// The node-variable columns and operand columns the write clauses need.
fn plan_columns(stmt: &UnwindWriteStmt) -> Columns {
    let mut cols = Columns {
        by_operand: HashMap::new(),
        items: Vec::new(),
    };
    // A constant column, so the read part always yields its row count.
    cols.items.push(RetItem {
        value: RetVal::ScalarExpr(Operand::Lit(Value::Int(1))),
        alias: Some("__uw_row".into()),
    });
    let mut var_names: Vec<String> = Vec::new();
    let mut note_var = |v: &str| {
        if stmt.scope.iter().any(|s| s == v) && !var_names.iter().any(|n| n == v) {
            var_names.push(v.to_string());
        }
    };
    for write in &stmt.writes {
        match write {
            RowWrite::Create { nodes, edges } => {
                for node in nodes {
                    for (_, op) in &node.props {
                        cols.operand(op);
                    }
                }
                for edge in edges {
                    note_var(&edge.src_var);
                    note_var(&edge.dst_var);
                }
            }
            RowWrite::Merge(merge) => {
                merge_operands(&merge.start, &mut cols);
                if let Some(edge) = &merge.edge {
                    merge_operands(&edge.end, &mut cols);
                }
                for sc in merge.on_create.iter().chain(&merge.on_match) {
                    cols.operand(&sc.value);
                }
            }
            RowWrite::Set(sets) => {
                for sc in sets {
                    note_var(&sc.var);
                    cols.operand(&sc.value);
                }
            }
            RowWrite::SetMap { var, value, .. } => {
                note_var(var);
                cols.operand(value);
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
    for name in var_names {
        cols.items.push(RetItem {
            value: RetVal::Var(name),
            alias: None,
        });
    }
    cols
}

fn merge_operands(node: &RowMergeNode, cols: &mut Columns) {
    cols.operand(&node.key);
    if let Some(ns) = &node.ns {
        cols.operand(ns);
    }
}

/// A node pattern that finds a created or merged node again by its key.
fn key_pattern(var: &str, label: &str, key_field: &str, key: &Operand) -> NodePat {
    NodePat {
        var: Some(var.to_string()),
        label: Some(label.to_string()),
        props: vec![(key_field.to_string(), key.clone())],
    }
}

/// The read query a `RETURN` runs after the commit: the read part, then one
/// stage that finds every node the writes created or merged by its key.
fn return_query(stmt: &UnwindWriteStmt, ret: &RowReturn) -> Query {
    let mut patterns: Vec<Pattern> = Vec::new();
    let single = |start: NodePat| Pattern {
        start,
        chain: vec![],
        shortest: false,
    };
    for write in &stmt.writes {
        match write {
            RowWrite::Create { nodes, .. } => {
                for node in nodes {
                    if let Some((_, key)) = node.props.iter().find(|(f, _)| f == "id") {
                        patterns.push(single(key_pattern(&node.var, &node.label, "id", key)));
                    }
                }
            }
            RowWrite::Merge(merge) => match &merge.edge {
                None => patterns.push(single(key_pattern(
                    &merge.start.var,
                    &merge.start.label,
                    &merge.start.key_field,
                    &merge.start.key,
                ))),
                Some(edge) => patterns.push(Pattern {
                    start: key_pattern(
                        &merge.start.var,
                        &merge.start.label,
                        &merge.start.key_field,
                        &merge.start.key,
                    ),
                    chain: vec![(
                        core_query::cypher::RelPat {
                            var: edge.rel_var.clone(),
                            etypes: vec![edge.etype.clone()],
                            dir: edge.dir,
                            hops: None,
                        },
                        key_pattern(
                            &edge.end.var,
                            &edge.end.label,
                            &edge.end.key_field,
                            &edge.end.key,
                        ),
                    )],
                    shortest: false,
                }),
            },
            _ => {}
        }
    }
    let mut query = stmt.read.clone();
    if !patterns.is_empty() {
        query.stages.push(core_query::cypher::WithStage {
            items: stmt
                .scope
                .iter()
                .map(|name| RetItem {
                    value: RetVal::Var(name.clone()),
                    alias: None,
                })
                .collect(),
            where_expr: None,
            order_by: Vec::new(),
            skip: None,
            limit: None,
            matches: patterns,
            optional_clauses: Vec::new(),
            unwinds: Vec::new(),
            post_where: None,
        });
    }
    query.returns = ret.items.clone();
    query.distinct = ret.distinct;
    query.order_by = ret.order_by.clone();
    query.skip = ret.skip.clone();
    query.limit = ret.limit.clone();
    query
}

impl Overlay {
    fn node_created(&mut self, key: &str, props: &[(String, Value)]) {
        self.nodes.insert(
            key.to_string(),
            NodeState {
                exists: true,
                created_here: true,
                props: Some(props.iter().cloned().collect()),
            },
        );
    }

    fn note_set(&mut self, key: &str, field: &str, value: &Value) {
        if let Some(props) = self.nodes.get_mut(key).and_then(|n| n.props.as_mut()) {
            props.insert(field.to_string(), value.clone());
        }
    }

    fn note_remove(&mut self, key: &str, field: &str) {
        if let Some(props) = self.nodes.get_mut(key).and_then(|n| n.props.as_mut()) {
            props.remove(field);
        }
    }
}

impl<F: Fs> GraphDb<F> {
    /// Whether the node `key` exists at this point of the statement.
    fn uw_exists(&self, ov: &Overlay, key: &str) -> bool {
        match ov.nodes.get(key) {
            Some(state) => state.exists,
            None => self.has_node(key),
        }
    }

    /// The properties of `key` at this point of the statement.
    fn uw_props<'o>(&self, ov: &'o mut Overlay, key: &str) -> &'o mut BTreeMap<String, Value> {
        let state = ov
            .nodes
            .entry(key.to_string())
            .or_insert_with(|| NodeState {
                exists: true,
                created_here: false,
                props: None,
            });
        state
            .props
            .get_or_insert_with(|| self.node_info(key).map(|n| n.props).unwrap_or_default())
    }

    /// Whether the edge exists at this point of the statement.
    fn uw_edge_exists(&self, ov: &Overlay, etype: &str, src: &str, dst: &str) -> bool {
        if ov
            .edges
            .contains(&(etype.to_string(), src.to_string(), dst.to_string()))
        {
            return true;
        }
        let fresh = |key: &str| ov.nodes.get(key).is_some_and(|n| n.created_here);
        if fresh(src) || fresh(dst) {
            return false;
        }
        self.uw_exists(ov, src)
            && self.uw_exists(ov, dst)
            && self.merge_edge_present(etype, src, dst)
    }

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
        let mut ov = Overlay::default();
        let mut ops: Vec<BatchOp> = Vec::new();
        let mut tally = Tally::default();
        for row in 0..rows.len() {
            let mut env: HashMap<&str, String> = HashMap::new();
            for var in &var_cols {
                if let Some(Value::Str(key)) = rows.get(row, var) {
                    env.insert(var, key.clone());
                }
            }
            for write in &stmt.writes {
                self.uw_write(
                    write, &rows, row, &cols, &mut env, &mut ov, &mut ops, &mut tally,
                )?;
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
            let mask_opt = self.pending_write_authz.as_ref().map(|a| a.mask.clone());
            return if let Some(ref mask) = mask_opt {
                execute(&self.view_masked(mask), &ops, &Params(params))
            } else {
                execute(&self.view(), &ops, &Params(params))
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
    #[allow(clippy::too_many_arguments)]
    fn uw_write<'v>(
        &self,
        write: &'v RowWrite,
        rows: &ResultSet,
        row: usize,
        cols: &Columns,
        env: &mut HashMap<&'v str, String>,
        ov: &mut Overlay,
        ops: &mut Vec<BatchOp>,
        tally: &mut Tally,
    ) -> Result<()> {
        let value = |op: &Operand| -> Option<Value> {
            match rows.get(row, cols.column_of(op)) {
                None | Some(Value::Null) => None,
                Some(v) => Some(v.clone()),
            }
        };
        match write {
            RowWrite::Create { nodes, edges } => {
                for node in nodes {
                    self.uw_create_node(node, &value, env, ov, ops)?;
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
                    ov.edges.insert((edge.etype.clone(), src, dst));
                }
            }
            RowWrite::Merge(merge) => {
                self.uw_merge(merge, &value, env, ov, ops, tally)?;
            }
            RowWrite::Set(sets) => {
                for sc in sets {
                    let Some(key) = env.get(sc.var.as_str()).cloned() else {
                        continue; // a null node (OPTIONAL MATCH): nothing to set
                    };
                    let Some(v) = value(&sc.value) else {
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
                let entries: MapEntries = match value(operand) {
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
                    ov.nodes.insert(
                        key.clone(),
                        NodeState {
                            exists: false,
                            created_here: false,
                            props: None,
                        },
                    );
                    ops.push(BatchOp::DeleteNode { key });
                    tally.deleted += 1;
                }
            }
        }
        Ok(())
    }

    /// Whether any edge touches `key` at this point of the statement.
    fn uw_has_edges(&self, ov: &Overlay, key: &str) -> bool {
        if ov.edges.iter().any(|(_, s, d)| s == key || d == key) {
            return true;
        }
        if ov.nodes.get(key).is_some_and(|n| n.created_here) {
            return false;
        }
        let Some(id) = self.ids.get(key) else {
            return false;
        };
        let tv = self.topo_view();
        tv.etypes().any(|et| {
            !tv.neighbors(et, Direction::Out, id).is_empty()
                || !tv.neighbors(et, Direction::In, id).is_empty()
        })
    }

    fn uw_create_node<'v>(
        &self,
        node: &'v RowCreateNode,
        value: &dyn Fn(&Operand) -> Option<Value>,
        env: &mut HashMap<&'v str, String>,
        ov: &mut Overlay,
        ops: &mut Vec<BatchOp>,
    ) -> Result<()> {
        let mut props: Vec<(String, Value)> = Vec::with_capacity(node.props.len());
        for (field, operand) in &node.props {
            // A null value is an absent property.
            if let Some(v) = value(operand) {
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
        value: &dyn Fn(&Operand) -> Option<Value>,
    ) -> Result<(String, Value)> {
        match value(&node.key) {
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
            Some(state) => Ok(state.exists),
            None => self.merge_node_existed(label, key),
        }
    }

    fn uw_merge<'v>(
        &self,
        merge: &'v RowMerge,
        value: &dyn Fn(&Operand) -> Option<Value>,
        env: &mut HashMap<&'v str, String>,
        ov: &mut Overlay,
        ops: &mut Vec<BatchOp>,
        tally: &mut Tally,
    ) -> Result<()> {
        let (start_key, start_value) = Self::uw_merge_key(&merge.start, value)?;
        let start_ns = merge.start.ns.as_ref().and_then(value);
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
            Self::uw_merge_sets(sets, value, |_| start_key.as_str(), ov, ops, tally)?;
            env.insert(merge.start.var.as_str(), start_key);
            return Ok(());
        };

        let (end_key, end_value) = Self::uw_merge_key(&edge.end, value)?;
        let end_ns = edge.end.ns.as_ref().and_then(value);
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
            ov.edges.insert((edge.etype.clone(), src_key, dst_key));
            &merge.on_create
        };
        Self::uw_merge_sets(
            sets,
            value,
            |var| {
                if var == merge.start.var {
                    start_key.as_str()
                } else {
                    end_key.as_str()
                }
            },
            ov,
            ops,
            tally,
        )?;
        env.insert(merge.start.var.as_str(), start_key);
        env.insert(edge.end.var.as_str(), end_key);
        Ok(())
    }

    /// `ON CREATE SET` / `ON MATCH SET` items of a MERGE, for the node `target`
    /// names.
    fn uw_merge_sets<'k>(
        sets: &[SetClause],
        value: &dyn Fn(&Operand) -> Option<Value>,
        target: impl Fn(&str) -> &'k str,
        ov: &mut Overlay,
        ops: &mut Vec<BatchOp>,
        tally: &mut Tally,
    ) -> Result<()> {
        for sc in sets {
            let Some(v) = value(&sc.value) else {
                return Err(err(format!(
                    "SET value for {}.{} evaluated to null",
                    sc.var, sc.field
                )));
            };
            let key = target(&sc.var);
            ov.note_set(key, &sc.field, &v);
            ops.push(BatchOp::SetProp {
                key: key.to_string(),
                field: sc.field.clone(),
                value: v,
            });
            tally.props_set += 1;
        }
        Ok(())
    }
}
