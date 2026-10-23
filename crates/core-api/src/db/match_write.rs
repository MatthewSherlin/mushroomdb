//! `MATCH … SET` / `SET n = {…}` / `SET n += {…}` / `REMOVE`, computed over a
//! [`RowSource`]: the batch ops a statement makes and what its `RETURN`
//! projects once they are applied. [`GraphDb`] commits the ops (auto-commit);
//! a transaction buffers them and projects over its own state, so both make
//! the same decisions.

use super::row_eval::{self, RowSource};
use super::*;

/// The ops a `MATCH … SET` / `REMOVE` statement makes, and what it returns.
pub(crate) struct MatchWrite {
    /// The statement's writes, in order.
    pub(crate) ops: Vec<BatchOp>,
    /// The `properties_set` count.
    props: i64,
    /// The relationship variables and the `RETURN` items, when it has one.
    projection: Option<(Vec<String>, Vec<RetItem>)>,
    /// The MATCH's rows.
    rows: ResultSet,
}

impl MatchWrite {
    /// The statement's result once its ops are applied: the write counts, or
    /// the `RETURN` projected from the matched rows, reading node properties
    /// from `src` (the state with the ops applied).
    pub(crate) fn result<S: RowSource + ?Sized>(
        self,
        src: &S,
        params: &BTreeMap<String, Value>,
    ) -> Result<ResultSet> {
        let stats = WriteStats {
            properties_set: self.props,
            ..WriteStats::default()
        };
        let rs = match self.projection {
            Some((rel_vars, returns)) => {
                let mut rs = project_set_return_rows(src, &rel_vars, &self.rows, &returns, params)?;
                // A bare node in the RETURN is a node column, as its MATCH
                // column was; its details are read from the written state.
                rs.set_column_kinds(
                    returns
                        .iter()
                        .map(|item| match &item.value {
                            RetVal::Var(v) => {
                                let col_name = if rel_vars.contains(v) {
                                    rel_value_alias(v)
                                } else {
                                    v.clone()
                                };
                                self.rows
                                    .columns()
                                    .iter()
                                    .position(|c| *c == col_name)
                                    .and_then(|col| self.rows.column_kind(col))
                            }
                            _ => None,
                        })
                        .collect(),
                );
                src.row_view(|view| finish_result(view, rs))
            }
            None => write_counts(0, self.props, 0, 0),
        };
        Ok(rs.with_stats(stats))
    }
}

/// `MATCH … [WHERE …] SET n.f = … [, …] [RETURN …]`, over `src`: the batch ops
/// and what the `RETURN` projects once they are applied. Nothing is written.
pub(crate) fn match_set_ops<S: RowSource + ?Sized>(
    src: &S,
    stmt: core_query::cypher::MatchSetStmt,
    params: &BTreeMap<String, Value>,
) -> Result<MatchWrite> {
    let project_returns = stmt.returns.clone();
    // Refused before anything is written: the projection runs after the
    // commit, and must not fail (or differ) once the write has happened.
    if let Some(detail) = project_returns
        .as_deref()
        .and_then(returns_write_return_refusal)
    {
        return Err(GraphError::QueryError {
            detail: detail.into(),
        });
    }
    // Collect unique node vars targeted by SET clauses, plus RETURN bindings
    // so the post-write projection can look them up by key.
    let mut set_vars: Vec<String> = Vec::new();
    for s in &stmt.sets {
        if !set_vars.contains(&s.var) {
            set_vars.push(s.var.clone());
        }
    }
    let rel_vars = pattern_rel_vars(&stmt.matches);
    // `count` is the engine's, on an edge: it is the insert-count §5.13
    // maintains, and a `SET` that overwrote it would make the number mean
    // whatever the last writer said rather than how many times the pair was
    // inserted. Refused by name here, before the match runs, so the caller
    // is told what is actually wrong instead of meeting the executor's
    // generic "did not resolve to a node key" — and so the answer does not
    // depend on whether the pattern happened to match a row. The same name
    // on a *node* is an ordinary property and is untouched.
    for s in &stmt.sets {
        if s.field == EDGE_COUNT_PROP && rel_vars.iter().any(|r| r == &s.var) {
            return Err(GraphError::QueryError {
                detail: format!(
                    "cannot SET {}.{EDGE_COUNT_PROP}: `{EDGE_COUNT_PROP}` is a reserved edge \
                         property holding the pair's insert count",
                    s.var
                ),
            });
        }
    }
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
    // Capture each relationship while r is bound; SET does not change it.
    set_returns.extend(rel_capture_items(&rel_vars));

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
    //
    // When a role-scoped write is in flight, run the MATCH read through
    // view_masked so hidden nodes are invisible → hidden ≡ absent ≡
    // zero-rows (no SetProp ops generated, no existence-oracle 403).
    // Full-authority writes (pending_write_authz=None) keep view().
    let match_rs = src
        .row_view(|view| execute(view, &ops, &Params(params)))
        .map_err(|e| GraphError::QueryError {
            detail: format!("execute: {e}"),
        })?;

    // Collect (key, field, value) for each matched row × each SET clause.
    //
    // A value that reads a node's properties is evaluated as its turn comes,
    // against what the statement has written so far, so two rows for one
    // node (`MATCH (n), (m) SET n.c = n.c + 1`) and two items of one clause
    // (`SET n.v = 10, n.w = n.v`) read each other's writes, as in openCypher.
    // A value with no such read keeps its column from the MATCH.
    let node_names = pattern_node_vars(&stmt.matches);
    let dynamic: Vec<bool> = stmt
        .sets
        .iter()
        .map(|sc| {
            row_eval::reads_node(&sc.value, &node_names)
                && !row_eval::reads_node(&sc.value, &rel_vars)
        })
        .collect();
    let any_dynamic = dynamic.iter().any(|d| *d);
    let mut overlay = row_eval::Overlay::default();
    let mut set_ops: Vec<(String, String, Value)> = Vec::new();
    for row_i in 0..match_rs.len() {
        for ((sc, col), is_dynamic) in stmt.sets.iter().zip(&set_val_cols).zip(&dynamic) {
            let key = match match_rs.get(row_i, &sc.var) {
                Some(Value::Str(k)) => k.clone(),
                _ => {
                    return Err(GraphError::QueryError {
                        detail: format!("SET variable '{}' did not resolve to a node key", sc.var),
                    })
                }
            };
            let evaluated = if *is_dynamic {
                let node_key = |name: &str| -> Option<Option<String>> {
                    node_names
                        .iter()
                        .any(|n| n == name)
                        .then(|| match match_rs.get(row_i, name) {
                            Some(Value::Str(k)) => Some(k.clone()),
                            _ => None,
                        })
                };
                row_eval::uw_eval(src, &mut overlay, &sc.value, &node_key, &[], params)?
            } else {
                // The SET value was already evaluated by the executor.
                match_rs.get(row_i, col).cloned()
            };
            let Some(value) = evaluated.filter(|v| !matches!(v, Value::Null)) else {
                return Err(GraphError::QueryError {
                    detail: format!("SET value for {}.{} evaluated to null", sc.var, sc.field),
                });
            };
            if any_dynamic {
                overlay.note_set(&key, &sc.field, &value);
            }
            set_ops.push((key, sc.field.clone(), value));
        }
    }

    let props_set = set_ops.len() as i64;
    let ops = set_ops
        .into_iter()
        .map(|(key, field, value)| BatchOp::SetProp { key, field, value })
        .collect();
    Ok(MatchWrite {
        ops,
        props: props_set,
        projection: project_returns.map(|returns| (rel_vars, returns)),
        rows: match_rs,
    })
}

/// `MATCH … [WHERE …] SET n = {…}` and `SET n += {…}`.
///
/// The MATCH runs first (masked under a role-scoped write); each map value
/// is evaluated per matched row by the same executor that evaluates
/// `SET n.f = expr`. The resulting `SetProp`/`RemoveProp` ops commit as one
/// batch (at COMMIT, inside a transaction), so they meet the same choke-point
/// as every other write: TY-5
/// constraints, `ns` immutability, the view-owned refusal, and rule
/// re-evaluation for each removal. See [`plan_map_write`] for which
/// properties each form touches.
pub(crate) fn match_set_map_ops<S: RowSource + ?Sized>(
    src: &S,
    stmt: core_query::cypher::MatchSetMapStmt,
    params: &BTreeMap<String, Value>,
) -> Result<MatchWrite> {
    let project_returns = stmt.returns.clone();
    let rel_vars = pattern_rel_vars(&stmt.matches);
    let mut lookup_vars: Vec<String> = Vec::new();
    add_var(&mut lookup_vars, &stmt.var);
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
    let mut match_returns: Vec<RetItem> = lookup_vars
        .iter()
        .map(|v| RetItem {
            value: RetVal::Var(v.clone()),
            alias: None,
        })
        .collect();
    let value_cols: Vec<String> = (0..stmt.map.len()).map(|i| format!("__sv_{i}")).collect();
    for ((_, operand), col) in stmt.map.iter().zip(&value_cols) {
        match_returns.push(RetItem {
            value: RetVal::ScalarExpr(operand.clone()),
            alias: Some(col.clone()),
        });
    }
    if let Some(name) = &stmt.param {
        match_returns.push(RetItem {
            value: RetVal::ScalarExpr(Operand::Param(name.clone())),
            alias: Some("__sv_param".into()),
        });
    }
    match_returns.extend(rel_capture_items(&rel_vars));
    let read_q = Query {
        matches: stmt.matches.clone(),
        optional_clauses: vec![],
        where_expr: stmt.where_expr.clone(),
        unwinds: vec![],
        post_unwind_where: None,
        stages: vec![],
        returns: match_returns,
        distinct: false,
        order_by: vec![],
        skip: None,
        limit: None,
    };
    let ops = plan(&read_q).map_err(|e| GraphError::QueryError {
        detail: format!("plan: {e}"),
    })?;
    let match_rs = src
        .row_view(|view| execute(view, &ops, &Params(params)))
        .map_err(|e| GraphError::QueryError {
            detail: format!("execute: {e}"),
        })?;

    // One entry list per matched row, in row order. A node bound by several
    // rows is written by each in turn under `+=`; under `=` the last row's
    // map is the node's final state, so earlier rows for it are dropped.
    let node_names = pattern_node_vars(&stmt.matches);
    let map_dynamic = stmt.map.iter().any(|(_, operand)| {
        row_eval::reads_node(operand, &node_names) && !row_eval::reads_node(operand, &rel_vars)
    });
    let mut overlay = row_eval::Overlay::default();
    let mut per_node: Vec<(String, MapEntries)> = Vec::new();
    for row_i in 0..match_rs.len() {
        let key = match match_rs.get(row_i, &stmt.var) {
            Some(Value::Str(k)) => k.clone(),
            _ => {
                return Err(GraphError::QueryError {
                    detail: format!("SET variable '{}' did not resolve to a node key", stmt.var),
                })
            }
        };
        if let Some(name) = &stmt.param {
            // The parameter is a map: its keys are the map's, a null value
            // is a null entry. Anything else is a named error, before any
            // write.
            let entries = match match_rs.get(row_i, "__sv_param") {
                Some(Value::Map(m)) => m
                    .iter()
                    .map(|(k, v)| {
                        let v = if matches!(v, Value::Null) {
                            None
                        } else {
                            Some(v.clone())
                        };
                        (k.clone(), v)
                    })
                    .collect::<MapEntries>(),
                other => {
                    return Err(GraphError::QueryError {
                        detail: format!(
                            "SET {} {}= ${name}: the parameter must be a map, got {}",
                            stmt.var,
                            if stmt.merge { "+" } else { "" },
                            other.map_or("null".to_string(), |v| format!("{v:?}")),
                        ),
                    })
                }
            };
            if !stmt.merge {
                per_node.retain(|(k, _)| k != &key);
            }
            per_node.push((key, entries));
            continue;
        }
        let mut entries: MapEntries = Vec::with_capacity(stmt.map.len());
        for ((field, operand), col) in stmt.map.iter().zip(&value_cols) {
            let value = if row_eval::reads_node(operand, &node_names)
                && !row_eval::reads_node(operand, &rel_vars)
            {
                // Read as its turn comes, against the statement's own writes.
                let node_key = |name: &str| -> Option<Option<String>> {
                    node_names
                        .iter()
                        .any(|n| n == name)
                        .then(|| match match_rs.get(row_i, name) {
                            Some(Value::Str(k)) => Some(k.clone()),
                            _ => None,
                        })
                };
                row_eval::uw_eval(src, &mut overlay, operand, &node_key, &[], params)?
            } else {
                match match_rs.get(row_i, col) {
                    None | Some(Value::Null) => None,
                    Some(v) => Some(v.clone()),
                }
            };
            entries.push((field.clone(), value));
        }
        if map_dynamic {
            row_eval::uw_apply_map(src, &mut overlay, &key, stmt.merge, &entries);
        }
        if !stmt.merge {
            per_node.retain(|(k, _)| k != &key);
        }
        per_node.push((key, entries));
    }

    let mut writes: Vec<(String, MapWrite)> = Vec::new();
    let mut props_changed = 0i64;
    for (key, entries) in &per_node {
        let existing = src.row_props(key);
        let node_writes = plan_map_write(key, &existing, stmt.merge, entries, &|field| {
            src.row_view_owns(field)
        });
        for write in node_writes {
            match &write {
                MapWrite::Set { .. } => props_changed += 1,
                MapWrite::Remove { field } => {
                    if existing.contains_key(field) {
                        props_changed += 1;
                    }
                }
            }
            writes.push((key.clone(), write));
        }
    }
    let ops = writes
        .into_iter()
        .map(|(key, write)| match write {
            MapWrite::Set { field, value } => BatchOp::SetProp { key, field, value },
            MapWrite::Remove { field } => BatchOp::RemoveProp { key, field },
        })
        .collect();
    Ok(MatchWrite {
        ops,
        props: props_changed,
        projection: project_returns.map(|returns| (rel_vars, returns)),
        rows: match_rs,
    })
}

/// `MATCH … [WHERE …] REMOVE n.f [, …] [RETURN …]`.
///
/// The MATCH runs first (masked under a role-scoped write, so a hidden node
/// is absent and nothing is removed from it); the removals then commit as
/// one batch of `RemoveProp` ops (at COMMIT, inside a transaction), which
/// meet the same `prepare_remove_prop`
/// choke-point as `remove_prop`: view-owned fields and `ns` are refused there,
/// a field the node lacks is a no-op, and rules re-evaluate each removal.
/// The result row's `properties_set` slot counts the properties that existed
/// and were removed.
pub(crate) fn match_remove_ops<S: RowSource + ?Sized>(
    src: &S,
    stmt: core_query::cypher::MatchRemoveStmt,
    params: &BTreeMap<String, Value>,
) -> Result<MatchWrite> {
    let project_returns = stmt.returns.clone();
    let rel_vars = pattern_rel_vars(&stmt.matches);
    let mut lookup_vars: Vec<String> = Vec::new();
    for (var, _) in &stmt.removals {
        add_var(&mut lookup_vars, var);
    }
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
    let mut match_returns: Vec<RetItem> = lookup_vars
        .iter()
        .map(|v| RetItem {
            value: RetVal::Var(v.clone()),
            alias: None,
        })
        .collect();
    // Capture each relationship while r is bound, as SET does.
    match_returns.extend(rel_capture_items(&rel_vars));
    let read_q = Query {
        matches: stmt.matches.clone(),
        optional_clauses: vec![],
        where_expr: stmt.where_expr.clone(),
        unwinds: vec![],
        post_unwind_where: None,
        stages: vec![],
        returns: match_returns,
        distinct: false,
        order_by: vec![],
        skip: None,
        limit: None,
    };
    let ops = plan(&read_q).map_err(|e| GraphError::QueryError {
        detail: format!("plan: {e}"),
    })?;
    let match_rs = src
        .row_view(|view| execute(view, &ops, &Params(params)))
        .map_err(|e| GraphError::QueryError {
            detail: format!("execute: {e}"),
        })?;

    // Each (node, field) once, however many rows bound the node.
    let mut targets: Vec<(String, String)> = Vec::new();
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    for row_i in 0..match_rs.len() {
        for (var, field) in &stmt.removals {
            let key = match match_rs.get(row_i, var) {
                Some(Value::Str(k)) => k.clone(),
                _ => {
                    return Err(GraphError::QueryError {
                        detail: format!("REMOVE variable '{var}' did not resolve to a node key"),
                    })
                }
            };
            let pair = (key, field.clone());
            if seen.insert(pair.clone()) {
                targets.push(pair);
            }
        }
    }

    // Counted before the commit, from the state the removals act on.
    let props_removed = targets
        .iter()
        .filter(|(key, field)| src.row_prop(key, field).is_some())
        .count() as i64;
    let ops = targets
        .into_iter()
        .map(|(key, field)| BatchOp::RemoveProp { key, field })
        .collect();
    Ok(MatchWrite {
        ops,
        props: props_removed,
        projection: project_returns.map(|returns| (rel_vars, returns)),
        rows: match_rs,
    })
}

/// The read-part columns that capture each relationship variable while it is
/// bound: its type (for `type(r)`) and the relationship itself (for a bare
/// `RETURN r`, rendered as its `{type, start, end}` map).
fn rel_capture_items(rel_vars: &[String]) -> Vec<RetItem> {
    rel_vars
        .iter()
        .flat_map(|r| {
            [
                RetItem {
                    value: RetVal::FuncCall {
                        name: "type".into(),
                        args: vec![Operand::Var(r.clone())],
                    },
                    alias: Some(rel_type_alias(r)),
                },
                RetItem {
                    value: RetVal::Var(r.clone()),
                    alias: Some(rel_value_alias(r)),
                },
            ]
        })
        .collect()
}
