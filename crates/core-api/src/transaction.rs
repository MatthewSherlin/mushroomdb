//! Interactive transactions: BEGIN, any number of Cypher statements, then
//! COMMIT or ROLLBACK.
//!
//! # Model
//!
//! A [`Transaction`] is optimistic. BEGIN freezes a [`ReaderSnapshot`] — the
//! *base snapshot* — and takes no lock after that: statements run against the
//! base snapshot, and writes are buffered as [`BatchOp`]s instead of being
//! applied. COMMIT submits the whole buffer as **one** WAL `Batch` frame through
//! the compare-and-set path ([`SharedDb::submit_batch_cas`]), so a committed
//! transaction is one commit number for time travel. ROLLBACK — or dropping
//! the transaction — discards the buffer and writes nothing.
//!
//! # Isolation: snapshot isolation
//!
//! The transaction records every node key it reads or writes, with the
//! `last_change` commit that key had in the base snapshot. COMMIT carries one
//! [`Precondition::NodeUnchangedSince`] per recorded key, checked under the
//! same write guard that applies the batch, so any change to one of them after
//! BEGIN fails the commit with [`GraphError::CasConflict`] and writes nothing.
//! That is a transient conflict: retry the whole transaction from BEGIN.
//!
//! - **Lost updates are impossible**: every key a statement writes is in the
//!   set.
//! - **Edges** have no precondition of their own. An edge read or written puts
//!   both endpoint keys in the set, which is enough because `last_change`
//!   advances both endpoints on every explicit edge insert and delete.
//!   A `DETACH DELETE` is the exception: the WAL holds only the `DeleteNode`,
//!   and the edges it removes implicitly advance no neighbour's
//!   `last_change`. So a transaction that read or wrote only a neighbour `y`
//!   of a node another writer detach-deleted still commits — allowed under
//!   snapshot isolation, because it never read the removed edge (reading the
//!   edge would have put the deleted node itself in the set, and that node is
//!   gone). Pinned by `neighbour_of_concurrent_detach_delete_does_not_conflict`.
//!   A transaction's *own* `DETACH DELETE` records both endpoints of every edge
//!   it removes, so a concurrent write to such a neighbour does conflict with it.
//! - **A key absent at BEGIN** is recorded with `expected = 0`, so another
//!   writer inserting that key before the commit is a conflict.
//! - **Write skew through a label scan is possible.** A read records the keys
//!   it matched, not the predicate it matched them with: two transactions that
//!   each see "no `Leader` yet" and each create one both commit. This is the
//!   documented limit of snapshot isolation (a phantom), pinned by the test
//!   `write_skew_through_label_scan_is_possible`.
//! - **Not in the read set**: nodes bound only in a `MATCH` after a `WITH`, and
//!   the intermediate nodes of a variable-length or shortest path. Their
//!   endpoints and every node a write statement touches are.
//! - A transaction that wrote nothing commits without a WAL frame and without
//!   checking its reads, and returns the base snapshot's commit number.
//!
//! # Reads see the transaction's own writes
//!
//! Each statement reads the base snapshot with the buffered records layered on
//! top as a private, uncommitted delta — the same mechanism a reader snapshot
//! uses for its committed delta tail. Rules and views run at commit only, so
//! **rule-derived edges and view values produced by the transaction's own
//! writes are not visible inside it**; they appear once it commits.
//! Validation that needs the writer (view-owned properties, namespace and
//! edge-ownership checks, constraints) also runs at commit, against committed
//! state plus the batch, and a failure there rejects the whole transaction.
//!
//! # Cypher inside a transaction
//!
//! Supported write forms, resolved against the base snapshot plus the buffer:
//!
//! - `CREATE (a:L {id: …}) [-[:T]->(b:L {id: …})] [RETURN …]`
//! - `MATCH … [WHERE …] SET n.f = … [, …]` (without `RETURN`)
//! - `MATCH … [WHERE …] REMOVE n.f [, …]` (without `RETURN`)
//! - `MATCH … [WHERE …] DELETE r` (edges)
//! - `MATCH … [WHERE …] [DETACH] DELETE n` (nodes)
//! - `MERGE (n:L {id: …}) [ON CREATE SET …] [ON MATCH SET …] [RETURN …]`
//!
//! Every read query the engine accepts runs as it would outside a
//! transaction. A write form that cannot be expressed as buffered ops yet —
//! today `MATCH … SET … RETURN` and `MATCH … REMOVE … RETURN` — fails with a `QueryError` reading
//! `not supported in mushroomdb 0.10 inside a transaction: <form>`, and nothing
//! is buffered; it is never applied outside the transaction.
//!
//! A statement is atomic inside the transaction: one that fails buffers
//! nothing, and the transaction stays usable.

use std::collections::{BTreeMap, HashMap};
use std::time::Instant;

use core_query::cypher::{
    execute, execute_union, is_write_tokens, lex, parse_read, parse_write, plan, CreateStmt,
    MatchDeleteNodeStmt, MatchDeleteStmt, MatchRemoveStmt, MatchSetStmt, MergeStmt, NodePat,
    Operand, Params, Pattern, Query, RetItem, RetVal, UnionQuery, WriteStatement,
};
use core_query::{GraphView, ResultSet};
use core_storage::wal::WalRecord;
use core_storage::{GraphError, Result, Value};

use crate::db::{
    add_var, pattern_rel_vars, resolve_merge_set_value, write_result_set, BatchOp, Precondition,
    EDGE_COUNT_PROP, NS_PROP,
};
use crate::mask::StoreId;
use crate::reader::ReaderSnapshot;
use crate::shared::SharedDb;

/// An open interactive transaction. See the [module docs](self) for the
/// isolation model and the supported statements.
///
/// Obtained from [`SharedDb::begin_transaction`]. Holds no lock: other readers
/// and writers proceed while it is open. Owns a clone of the [`SharedDb`]
/// handle, so it carries no lifetime and is `Send`: it can be stored and
/// moved between threads while open. Dropping it without calling
/// [`commit`](Self::commit) is a rollback.
pub struct Transaction {
    /// A clone of the handle BEGIN ran on — cheap (`Arc`s), and what lets a
    /// transaction outlive the borrow it was opened from: a server's map of
    /// open transactions, a Python object, a Bolt connection.
    db: SharedDb,
    /// Mutations to submit at COMMIT, in statement order.
    buffer: Vec<BatchOp>,
    /// The state every statement reads, frozen at BEGIN.
    base_snap: ReaderSnapshot,
    opened_at: Instant,
    /// Conflict set: node key → its `last_change` commit in `base_snap`.
    read_write_keys: HashMap<String, u64>,
    /// Identity of the store load `base_snap` came from.
    base_store: StoreId,
    /// `buffer` as the WAL records the overlay applies.
    records: Vec<WalRecord>,
    /// `base_snap` plus `records`; `None` while nothing is buffered.
    overlay: Option<ReaderSnapshot>,
}

const _: () = {
    fn assert_send<T: Send>() {}
    let _ = assert_send::<Transaction>;
};

/// One statement's effect, held until the statement has fully succeeded.
#[derive(Default)]
struct Staged {
    ops: Vec<BatchOp>,
    records: Vec<WalRecord>,
    keys: Vec<String>,
}

impl Staged {
    fn insert_node(&mut self, label: &str, key: &str, props: Vec<(String, Value)>) {
        self.records.push(WalRecord::InsertNode {
            label: label.into(),
            key: key.into(),
            props: props.clone(),
        });
        self.ops.push(BatchOp::InsertNode {
            label: label.into(),
            key: key.into(),
            props,
        });
        self.touch(key);
    }

    fn insert_edge(&mut self, edge_type: &str, src_key: &str, dst_key: &str) {
        self.records.push(WalRecord::InsertEdge {
            edge_type: edge_type.into(),
            src_key: src_key.into(),
            dst_key: dst_key.into(),
        });
        self.ops.push(BatchOp::InsertEdge {
            edge_type: edge_type.into(),
            src_key: src_key.into(),
            dst_key: dst_key.into(),
        });
        self.touch(src_key);
        self.touch(dst_key);
    }

    fn set_prop(&mut self, key: &str, field: &str, value: Value) {
        self.records.push(WalRecord::SetProp {
            key: key.into(),
            field: field.into(),
            value: value.clone(),
        });
        self.ops.push(BatchOp::SetProp {
            key: key.into(),
            field: field.into(),
            value,
        });
        self.touch(key);
    }

    fn remove_prop(&mut self, key: &str, field: &str) {
        self.records.push(WalRecord::RemoveProp {
            key: key.into(),
            field: field.into(),
        });
        self.ops.push(BatchOp::RemoveProp {
            key: key.into(),
            field: field.into(),
        });
        self.touch(key);
    }

    fn delete_edge(&mut self, edge_type: &str, src_key: &str, dst_key: &str) {
        self.records.push(WalRecord::DeleteEdge {
            edge_type: edge_type.into(),
            src_key: src_key.into(),
            dst_key: dst_key.into(),
        });
        self.ops.push(BatchOp::DeleteEdge {
            edge_type: edge_type.into(),
            src_key: src_key.into(),
            dst_key: dst_key.into(),
        });
        self.touch(src_key);
        self.touch(dst_key);
    }

    fn delete_node(&mut self, key: &str) {
        self.records.push(WalRecord::DeleteNode { key: key.into() });
        self.ops.push(BatchOp::DeleteNode { key: key.into() });
        self.touch(key);
    }

    fn touch(&mut self, key: &str) {
        add_var(&mut self.keys, key);
    }
}

fn query_err(phase: &str) -> impl Fn(String) -> GraphError + '_ {
    move |e| GraphError::QueryError {
        detail: format!("{phase}: {e}"),
    }
}

fn not_supported(form: &str) -> GraphError {
    GraphError::QueryError {
        detail: format!("not supported in mushroomdb 0.10 inside a transaction: {form}"),
    }
}

/// Give every anonymous node in `pats` a generated variable, and append every
/// node variable to `out`, so a MATCH phase can return the keys it bound.
fn bind_node_vars(pats: &mut [Pattern], next: &mut usize, out: &mut Vec<String>) {
    let mut bind = |node: &mut NodePat| {
        let var = node.var.get_or_insert_with(|| {
            *next += 1;
            format!("__tx_n{next}")
        });
        add_var(out, var);
    };
    for p in pats {
        bind(&mut p.start);
        for (_, dest) in &mut p.chain {
            bind(dest);
        }
    }
}

fn var_items(vars: &[String]) -> Vec<RetItem> {
    vars.iter()
        .map(|v| RetItem {
            value: RetVal::Var(v.clone()),
            alias: None,
        })
        .collect()
}

fn match_query(matches: Vec<Pattern>, where_expr: Option<core_query::cypher::Expr>) -> Query {
    Query {
        matches,
        optional_clauses: vec![],
        where_expr,
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

/// `MATCH (var:label {id: key})` — one row binding `var` to the node `key`.
fn node_by_key(var: &str, label: &str, key: Value) -> Pattern {
    Pattern {
        start: NodePat {
            var: Some(var.to_string()),
            label: Some(label.to_string()),
            props: vec![("id".to_string(), Operand::Lit(key))],
        },
        chain: vec![],
        shortest: false,
    }
}

fn run(view: &GraphView<'_>, q: &Query, params: &BTreeMap<String, Value>) -> Result<ResultSet> {
    let ops = plan(q).map_err(query_err("plan"))?;
    execute(view, &ops, &Params(params)).map_err(query_err("execute"))
}

/// Every key bound to one of `vars` in any row of `rs`.
fn keys_in(rs: &ResultSet, vars: &[String], out: &mut Vec<String>) {
    for row in 0..rs.len() {
        for var in vars {
            if let Some(Value::Str(k)) = rs.get(row, var) {
                add_var(out, k);
            }
        }
    }
}

impl Transaction {
    pub(crate) fn new(db: SharedDb, base_snap: ReaderSnapshot, base_store: StoreId) -> Self {
        Self {
            db,
            buffer: Vec::new(),
            base_snap,
            opened_at: Instant::now(),
            read_write_keys: HashMap::new(),
            base_store,
            records: Vec::new(),
            overlay: None,
        }
    }

    /// When BEGIN ran. Surfaces with an idle timeout read it.
    pub fn opened_at(&self) -> Instant {
        self.opened_at
    }

    /// The commit number of the base snapshot every statement reads.
    pub fn base_version(&self) -> u64 {
        self.base_snap.version
    }

    /// Run one Cypher statement inside the transaction.
    ///
    /// A read returns what it would return against the base snapshot with this
    /// transaction's writes applied. A write is buffered and returns the same
    /// result shape it returns outside a transaction (the `created` /
    /// `properties_set` / `deleted` row, or its `RETURN` projection); nothing
    /// reaches the store until [`commit`](Self::commit). A failed statement
    /// buffers nothing and leaves the transaction usable.
    pub fn execute(&mut self, cypher: &str, params: &BTreeMap<String, Value>) -> Result<ResultSet> {
        let tokens = lex(cypher).map_err(query_err("lex"))?;
        if is_write_tokens(&tokens) {
            let stmt = parse_write(&tokens).map_err(query_err("parse"))?;
            return self.execute_write(stmt, params);
        }
        let union = parse_read(&tokens).map_err(query_err("parse"))?;
        let view = self.current();
        let rs = view
            .with_view(|v| execute_union(v, &union, &Params(params)))?
            .map_err(query_err("execute"))?;
        let keys = read_set(view, &union, params);
        self.record_keys(&keys);
        Ok(rs)
    }

    /// Submit the buffered writes as one WAL `Batch` frame and return its
    /// commit number.
    ///
    /// Fails with [`GraphError::CasConflict`] — writing nothing — when any key
    /// this transaction read or wrote changed after BEGIN; retry from BEGIN.
    /// Any other error the batch would raise through
    /// [`SharedDb::submit_batch`] also rejects it whole. The transaction is
    /// consumed either way, so a failed commit can never be re-submitted
    /// half-applied.
    ///
    /// A transaction that wrote nothing writes no frame and returns
    /// [`base_version`](Self::base_version).
    pub fn commit(self) -> Result<u64> {
        if self.buffer.is_empty() {
            return Ok(self.base_snap.version);
        }
        // Sorted so the key a conflict names does not depend on hash order.
        let mut keys: Vec<(String, u64)> = self.read_write_keys.into_iter().collect();
        keys.sort();
        let preconds = keys
            .into_iter()
            .map(|(key, expected)| Precondition::NodeUnchangedSince { key, expected })
            .collect();
        self.db
            .submit_batch_cas_with_seq(preconds, self.buffer)
            .map(|(_, seq)| seq)
    }

    /// Discard the buffered writes. Nothing reaches the WAL.
    pub fn rollback(self) {}

    // ── Internals ────────────────────────────────────────────────────────────

    /// A stored property of `key` as statements see it (base plus buffer).
    fn stored_prop(&self, key: &str, field: &str) -> Option<Value> {
        self.current().node_info(key)?.props.get(field).cloned()
    }

    /// The state statements read: the base snapshot plus the buffer.
    fn current(&self) -> &ReaderSnapshot {
        self.overlay.as_ref().unwrap_or(&self.base_snap)
    }

    /// Add `keys` not yet in the conflict set, each with the `last_change` it
    /// had in the base snapshot.
    fn record_keys(&mut self, keys: &[String]) {
        let fresh: Vec<(&String, bool)> = keys
            .iter()
            .filter(|k| !self.read_write_keys.contains_key(*k))
            .map(|k| (k, self.base_snap.resolve_key(k).is_some()))
            .collect();
        if fresh.is_empty() {
            return;
        }
        let db = self.db.read();
        for (key, in_base) in fresh {
            let seq = db.tx_base_last_change(key, self.base_snap.version, self.base_store, in_base);
            self.read_write_keys.insert(key.clone(), seq);
        }
    }

    /// The overlay `staged` would leave, materialised so a record that will not
    /// apply surfaces here rather than on the next read.
    fn candidate(&self, staged: &Staged) -> Result<ReaderSnapshot> {
        let mut records = self.records.clone();
        records.extend(staged.records.iter().cloned());
        let snap = self.base_snap.with_private_delta(records);
        snap.with_view(|_| ())?;
        Ok(snap)
    }

    /// Make a fully successful statement part of the transaction.
    fn accept(&mut self, staged: Staged, overlay: Option<ReaderSnapshot>) {
        self.record_keys(&staged.keys);
        if staged.ops.is_empty() {
            return;
        }
        self.buffer.extend(staged.ops);
        self.records.extend(staged.records);
        self.overlay = overlay;
    }

    fn execute_write(
        &mut self,
        stmt: WriteStatement,
        params: &BTreeMap<String, Value>,
    ) -> Result<ResultSet> {
        let mut staged = Staged::default();
        let result = match stmt {
            WriteStatement::Create(s) => self.stage_create(s, params, &mut staged)?,
            WriteStatement::MatchSet(s) => self.stage_match_set(s, params, &mut staged)?,
            WriteStatement::MatchSetMap(_) => {
                // The replace form must know which properties the node has and
                // which a view owns; the buffer cannot answer the second.
                return Err(not_supported("MATCH … SET n = {…} / SET n += {…}"));
            }
            WriteStatement::MatchRemove(s) => self.stage_match_remove(s, params, &mut staged)?,
            WriteStatement::MatchDelete(s) => self.stage_match_delete(s, params, &mut staged)?,
            WriteStatement::MatchDeleteNode(s) => {
                self.stage_match_delete_node(s, params, &mut staged)?
            }
            WriteStatement::Merge(s) => self.stage_merge(s, params, &mut staged)?,
        };
        let (rs, overlay) = result;
        self.accept(staged, overlay);
        Ok(rs)
    }

    /// Write result row `created, properties_set, deleted`, with the
    /// candidate overlay the statement's ops produce.
    fn counts(
        &self,
        staged: &Staged,
        created: i64,
        props_set: i64,
        deleted: i64,
    ) -> Result<(ResultSet, Option<ReaderSnapshot>)> {
        let overlay = if staged.ops.is_empty() {
            None
        } else {
            Some(self.candidate(staged)?)
        };
        let mut rs = write_result_set();
        rs.push_row(vec![
            Some(Value::Int(created)),
            Some(Value::Int(props_set)),
            Some(Value::Int(deleted)),
        ]);
        Ok((rs, overlay))
    }

    fn stage_create(
        &self,
        stmt: CreateStmt,
        params: &BTreeMap<String, Value>,
        staged: &mut Staged,
    ) -> Result<(ResultSet, Option<ReaderSnapshot>)> {
        let current = self.current();
        let mut var_to_key: BTreeMap<String, String> = BTreeMap::new();
        for node in &stmt.nodes {
            let var = node.var.as_deref().unwrap_or("_cn0");
            let key = match node.props.iter().find(|(f, _)| f == "id") {
                Some((_, Value::Str(s))) => s.clone(),
                _ => {
                    return Err(GraphError::QueryError {
                        detail: format!(
                            "CREATE node ({}:{}) requires a string 'id' property",
                            var, node.label
                        ),
                    })
                }
            };
            // Caught here, not at COMMIT: the overlay cannot hold two nodes
            // under one key, and the batch would refuse it anyway.
            if current.resolve_key(&key).is_some() || var_to_key.values().any(|k| k == &key) {
                return Err(GraphError::DuplicateKey { key });
            }
            var_to_key.insert(var.to_string(), key);
        }
        for node in &stmt.nodes {
            let var = node.var.as_deref().unwrap_or("_cn0");
            staged.insert_node(&node.label, &var_to_key[var], node.props.clone());
        }
        for edge in &stmt.edges {
            let src = var_to_key
                .get(&edge.src_var)
                .ok_or_else(|| GraphError::QueryError {
                    detail: format!("CREATE edge src variable '{}' is not bound", edge.src_var),
                })?;
            let dst = var_to_key
                .get(&edge.dst_var)
                .ok_or_else(|| GraphError::QueryError {
                    detail: format!("CREATE edge dst variable '{}' is not bound", edge.dst_var),
                })?;
            staged.insert_edge(&edge.etype, src, dst);
        }
        let created = stmt.nodes.len() as i64;
        let Some(returns) = stmt.returns else {
            return self.counts(staged, created, 0, 0);
        };
        let overlay = self.candidate(staged)?;
        let patterns = stmt
            .nodes
            .iter()
            .map(|node| {
                let var = node.var.as_deref().unwrap_or("_cn0");
                node_by_key(var, &node.label, Value::Str(var_to_key[var].clone()))
            })
            .collect();
        let mut q = match_query(patterns, None);
        q.returns = returns;
        let rs = overlay.with_view(|v| run(v, &q, params))??;
        Ok((rs, Some(overlay)))
    }

    fn stage_match_set(
        &self,
        stmt: MatchSetStmt,
        params: &BTreeMap<String, Value>,
        staged: &mut Staged,
    ) -> Result<(ResultSet, Option<ReaderSnapshot>)> {
        if stmt.returns.is_some() {
            return Err(not_supported("MATCH … SET … RETURN"));
        }
        let rel_vars = pattern_rel_vars(&stmt.matches);
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
        let mut matches = stmt.matches;
        let mut node_vars = Vec::new();
        bind_node_vars(&mut matches, &mut 0, &mut node_vars);
        let mut lookup = node_vars.clone();
        for s in &stmt.sets {
            add_var(&mut lookup, &s.var);
        }
        let mut q = match_query(matches, stmt.where_expr);
        q.returns = var_items(&lookup);
        let set_cols: Vec<String> = (0..stmt.sets.len()).map(|i| format!("__sv_{i}")).collect();
        for (sc, col) in stmt.sets.iter().zip(&set_cols) {
            q.returns.push(RetItem {
                value: RetVal::ScalarExpr(sc.value.clone()),
                alias: Some(col.clone()),
            });
        }
        let match_rs = self.current().with_view(|v| run(v, &q, params))??;
        keys_in(&match_rs, &node_vars, &mut staged.keys);
        let mut props_set = 0i64;
        for row in 0..match_rs.len() {
            for (sc, col) in stmt.sets.iter().zip(&set_cols) {
                let key = match match_rs.get(row, &sc.var) {
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
                let value =
                    match_rs
                        .get(row, col)
                        .cloned()
                        .ok_or_else(|| GraphError::QueryError {
                            detail: format!(
                                "SET value for {}.{} evaluated to null",
                                sc.var, sc.field
                            ),
                        })?;
                if sc.field == "id" && !rel_vars.iter().any(|r| r == &sc.var) {
                    crate::db::check_key_property_write(
                        &key,
                        self.stored_prop(&key, "id").as_ref(),
                        Some(&value),
                    )?;
                }
                staged.set_prop(&key, &sc.field, value);
                props_set += 1;
            }
        }
        self.counts(staged, 0, props_set, 0)
    }

    fn stage_match_remove(
        &self,
        stmt: MatchRemoveStmt,
        params: &BTreeMap<String, Value>,
        staged: &mut Staged,
    ) -> Result<(ResultSet, Option<ReaderSnapshot>)> {
        if stmt.returns.is_some() {
            return Err(not_supported("MATCH … REMOVE … RETURN"));
        }
        let mut matches = stmt.matches;
        let mut node_vars = Vec::new();
        bind_node_vars(&mut matches, &mut 0, &mut node_vars);
        let mut lookup = node_vars.clone();
        for (var, _) in &stmt.removals {
            add_var(&mut lookup, var);
        }
        let mut q = match_query(matches, stmt.where_expr);
        q.returns = var_items(&lookup);
        let current = self.current();
        let match_rs = current.with_view(|v| run(v, &q, params))??;
        keys_in(&match_rs, &node_vars, &mut staged.keys);
        let mut targets: Vec<(String, String)> = Vec::new();
        let mut seen: std::collections::HashSet<(String, String)> =
            std::collections::HashSet::new();
        for row in 0..match_rs.len() {
            for (var, field) in &stmt.removals {
                let key = match match_rs.get(row, var) {
                    Some(Value::Str(k)) => k.clone(),
                    _ => {
                        return Err(GraphError::QueryError {
                            detail: format!(
                                "REMOVE variable '{var}' did not resolve to a node key"
                            ),
                        })
                    }
                };
                let pair = (key, field.clone());
                if seen.insert(pair.clone()) {
                    targets.push(pair);
                }
            }
        }
        let mut props_removed = 0i64;
        for (key, field) in &targets {
            if let Some(id) = current.resolve_key(key) {
                if current.with_view(|v| v.prop(id, field).is_some())? {
                    props_removed += 1;
                }
            }
            if field == "id" {
                crate::db::check_key_property_removal(key, self.stored_prop(key, "id").as_ref())?;
            }
            staged.remove_prop(key, field);
        }
        self.counts(staged, 0, props_removed, 0)
    }

    fn stage_match_delete(
        &self,
        stmt: MatchDeleteStmt,
        params: &BTreeMap<String, Value>,
        staged: &mut Staged,
    ) -> Result<(ResultSet, Option<ReaderSnapshot>)> {
        let mut matches = stmt.matches;
        let mut node_vars = Vec::new();
        bind_node_vars(&mut matches, &mut 0, &mut node_vars);
        for ed in &stmt.deletes {
            add_var(&mut node_vars, &ed.src_var);
            add_var(&mut node_vars, &ed.dst_var);
        }
        let mut q = match_query(matches, stmt.where_expr);
        q.returns = var_items(&node_vars);
        let match_rs = self.current().with_view(|v| run(v, &q, params))??;
        keys_in(&match_rs, &node_vars, &mut staged.keys);
        let mut deleted = 0i64;
        for row in 0..match_rs.len() {
            for ed in &stmt.deletes {
                let endpoint = |var: &str, which: &str| match match_rs.get(row, var) {
                    Some(Value::Str(k)) => Ok(k.clone()),
                    _ => Err(GraphError::QueryError {
                        detail: format!(
                            "DELETE {which} variable '{var}' did not resolve to a node key"
                        ),
                    }),
                };
                let src = endpoint(&ed.src_var, "src")?;
                let dst = endpoint(&ed.dst_var, "dst")?;
                staged.delete_edge(&ed.etype, &src, &dst);
                deleted += 1;
            }
        }
        self.counts(staged, 0, 0, deleted)
    }

    fn stage_match_delete_node(
        &self,
        stmt: MatchDeleteNodeStmt,
        params: &BTreeMap<String, Value>,
        staged: &mut Staged,
    ) -> Result<(ResultSet, Option<ReaderSnapshot>)> {
        let mut matches = stmt.matches;
        let mut node_vars = Vec::new();
        bind_node_vars(&mut matches, &mut 0, &mut node_vars);
        let mut q = match_query(matches, stmt.where_expr);
        q.returns = var_items(&node_vars);
        let current = self.current();
        let match_rs = current.with_view(|v| run(v, &q, params))??;
        keys_in(&match_rs, &node_vars, &mut staged.keys);
        let mut doomed: Vec<String> = Vec::new();
        keys_in(&match_rs, &stmt.node_vars, &mut doomed);
        let mut deleted = 0i64;
        for key in &doomed {
            let edges = current.node_edges(key)?;
            if !stmt.detach && !edges.is_empty() {
                return Err(GraphError::QueryError {
                    detail: format!(
                        "Cannot delete node `{key}` because it still has incident edges. \
                         Use DETACH DELETE to remove the node and all its edges."
                    ),
                });
            }
            // Every edge the delete removes is an edge written: both its
            // endpoints join the conflict set.
            for e in &edges {
                staged.touch(&e.src_key);
                staged.touch(&e.dst_key);
            }
            deleted += 1 + edges.len() as i64;
            staged.delete_node(key);
        }
        self.counts(staged, 0, 0, deleted)
    }

    fn stage_merge(
        &self,
        stmt: MergeStmt,
        params: &BTreeMap<String, Value>,
        staged: &mut Staged,
    ) -> Result<(ResultSet, Option<ReaderSnapshot>)> {
        let key = match &stmt.key_value {
            Value::Str(s) => s.clone(),
            other => {
                return Err(GraphError::QueryError {
                    detail: format!("MERGE key value must be a string (got {other:?})"),
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
        let existed = self.current().resolve_key(&key).is_some();
        staged.touch(&key);
        let mut created = 0i64;
        if existed {
            for sc in &stmt.on_match {
                let value = resolve_merge_set_value(&sc.value, params)?;
                if sc.field == "id" {
                    crate::db::check_key_property_write(
                        &key,
                        self.stored_prop(&key, "id").as_ref(),
                        Some(&value),
                    )?;
                }
                staged.set_prop(&key, &sc.field, value);
            }
        } else {
            let mut props = vec![(stmt.key_field.clone(), stmt.key_value.clone())];
            if let Some(ns) = &stmt.ns {
                props.push((NS_PROP.to_string(), ns.clone()));
            }
            staged.insert_node(&stmt.label, &key, props);
            for sc in &stmt.on_create {
                let value = resolve_merge_set_value(&sc.value, params)?;
                if sc.field == "id" {
                    // The new node's `id` is its key exactly when MERGE keys on `id`.
                    let stored = (stmt.key_field == "id").then(|| stmt.key_value.clone());
                    crate::db::check_key_property_write(&key, stored.as_ref(), Some(&value))?;
                }
                staged.set_prop(&key, &sc.field, value);
            }
            created = 1;
        }
        let Some(returns) = stmt.returns else {
            return self.counts(staged, created, 0, 0);
        };
        let overlay = if staged.ops.is_empty() {
            None
        } else {
            Some(self.candidate(staged)?)
        };
        let var = stmt.var.as_deref().unwrap_or("_mn0");
        let mut q = match_query(
            vec![node_by_key(var, &stmt.label, stmt.key_value.clone())],
            None,
        );
        q.returns = returns;
        let view = overlay.as_ref().unwrap_or(self.current());
        let rs = view.with_view(|v| run(v, &q, params))??;
        Ok((rs, overlay))
    }
}

/// The node keys a read query matched, for the conflict set.
///
/// Each part's top-level `MATCH` / `OPTIONAL MATCH` patterns and `WHERE` are
/// re-run returning every node they bind, anonymous ones included, so both
/// endpoints of every matched edge are recorded. Best effort by design: if the
/// filtered re-run cannot be planned the unfiltered one is used (a wider set is
/// only more conservative), and a read whose re-run fails records nothing —
/// snapshot isolation needs the write set, which every write statement records
/// exactly.
fn read_set(
    view: &ReaderSnapshot,
    union: &UnionQuery,
    params: &BTreeMap<String, Value>,
) -> Vec<String> {
    let mut keys = Vec::new();
    for part in &union.parts {
        if part.matches.is_empty() && part.optional_clauses.is_empty() {
            continue;
        }
        let mut next = 0;
        let mut vars = Vec::new();
        let mut matches = part.matches.clone();
        bind_node_vars(&mut matches, &mut next, &mut vars);
        let mut optional_clauses = part.optional_clauses.clone();
        for oc in &mut optional_clauses {
            bind_node_vars(&mut oc.patterns, &mut next, &mut vars);
        }
        let mut q = match_query(matches, part.where_expr.clone());
        q.optional_clauses = optional_clauses;
        q.returns = var_items(&vars);
        q.distinct = true;
        let rs = view
            .with_view(|v| run(v, &q, params))
            .and_then(|r| r)
            .or_else(|_| {
                q.where_expr = None;
                view.with_view(|v| run(v, &q, params)).and_then(|r| r)
            });
        if let Ok(rs) = rs {
            keys_in(&rs, &vars, &mut keys);
        }
    }
    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d =
            std::env::temp_dir().join(format!("mushroomdb-tx-unit-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn no_params() -> BTreeMap<String, Value> {
        BTreeMap::new()
    }

    #[test]
    fn rollback_leaves_no_wal_frame() {
        let dir = tmp("rollback-frames");
        let db = SharedDb::open(&dir).unwrap();
        let before = crate::wal_commit_count_at(&dir).unwrap();
        let mut tx = db.begin_transaction();
        tx.execute("CREATE (n:P {id: 'a'})", &no_params()).unwrap();
        tx.rollback();
        assert_eq!(crate::wal_commit_count_at(&dir).unwrap(), before);
    }

    #[test]
    fn commit_is_exactly_one_wal_frame() {
        let dir = tmp("one-frame");
        let db = SharedDb::open(&dir).unwrap();
        let before = crate::wal_commit_count_at(&dir).unwrap();
        let mut tx = db.begin_transaction();
        tx.execute("CREATE (n:P {id: 'a'})", &no_params()).unwrap();
        tx.execute("CREATE (n:P {id: 'b'})", &no_params()).unwrap();
        tx.execute("MATCH (n:P {id: 'a'}) SET n.x = 1", &no_params())
            .unwrap();
        tx.commit().unwrap();
        assert_eq!(crate::wal_commit_count_at(&dir).unwrap(), before + 1);
    }

    #[test]
    fn conflict_set_records_both_endpoints_of_an_edge() {
        let dir = tmp("edge-endpoints");
        let db = SharedDb::open(&dir).unwrap();
        db.submit_batch(vec![
            BatchOp::InsertNode {
                label: "P".into(),
                key: "a".into(),
                props: vec![],
            },
            BatchOp::InsertNode {
                label: "P".into(),
                key: "b".into(),
                props: vec![],
            },
            BatchOp::InsertEdge {
                edge_type: "KNOWS".into(),
                src_key: "a".into(),
                dst_key: "b".into(),
            },
        ])
        .unwrap();
        let mut tx = db.begin_transaction();
        tx.execute(
            "MATCH (:P {id: 'a'})-[:KNOWS]->() RETURN 1 AS one",
            &no_params(),
        )
        .unwrap();
        let mut keys: Vec<&String> = tx.read_write_keys.keys().collect();
        keys.sort();
        assert_eq!(keys, ["a", "b"]);
    }

    #[test]
    fn absent_key_records_zero() {
        let dir = tmp("absent-zero");
        let db = SharedDb::open(&dir).unwrap();
        let mut tx = db.begin_transaction();
        tx.execute("CREATE (n:P {id: 'new'})", &no_params())
            .unwrap();
        assert_eq!(tx.read_write_keys.get("new"), Some(&0));
    }

    #[test]
    fn failed_statement_buffers_nothing() {
        let dir = tmp("failed-stmt");
        let db = SharedDb::open(&dir).unwrap();
        let mut tx = db.begin_transaction();
        tx.execute("CREATE (n:P {id: 'a'})", &no_params()).unwrap();
        let err = tx
            .execute("CREATE (n:P {id: 'a'})", &no_params())
            .unwrap_err();
        assert!(matches!(err, GraphError::DuplicateKey { .. }), "{err:?}");
        assert_eq!(tx.buffer.len(), 1);
        assert_eq!(tx.records.len(), 1);
    }
}
