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
//!   endpoints and every node a write statement touches are. A write
//!   statement with a read part (`MATCH … SET`, `MATCH … CREATE`, `UNWIND …
//!   MATCH … MERGE`, …) records every node that read part binds, named or
//!   anonymous and in every stage, as well as every node it writes.
//! - A transaction that wrote nothing commits without a WAL frame and without
//!   checking its reads, and returns the base snapshot's commit number.
//!
//! # Reads see the transaction's own writes
//!
//! Each statement reads the base snapshot with the buffered records layered on
//! top as a private, uncommitted overlay. The overlay is built at the first
//! write and each later statement lays only its own records over it; reads
//! consult it before the base snapshot's state. Nothing below it is copied,
//! re-applied or re-indexed, so a statement costs the same at any store size
//! (it grows only with what the statement itself reads and writes). The
//! overlay starts as a copy of the base snapshot's own delta-tail layer, which
//! costs the tail's records, not the store. Rules and views run at commit only, so
//! **rule-derived edges and view values produced by the transaction's own
//! writes are not visible inside it**; they appear once it commits.
//! Validation that needs the writer (view-owned properties, namespace and
//! edge-ownership checks, constraints) also runs at commit, against committed
//! state plus the batch, and a failure there rejects the whole transaction.
//!
//! # Cypher inside a transaction
//!
//! Every statement form an auto-commit write runs, except a schema statement,
//! runs here with the same results and counters, resolved against the base
//! snapshot plus the buffer: `CREATE`, `MERGE`, `MATCH … SET` / `SET n = {…}`
//! / `SET n += {…}` / `REMOVE` / `DELETE` (with or without `RETURN`), and the
//! per-row forms — `MATCH … CREATE`, `MATCH … MERGE` and `UNWIND … <write>`.
//! The `MATCH … SET` / `REMOVE` and per-row forms compute their ops through
//! the same code as auto-commit (`crate::db::RowSource`, with the
//! transaction's state as the source), so they read a statement's own earlier
//! rows exactly as auto-commit does; the ops are buffered instead of
//! committed, and a `RETURN` reads the transaction with them laid over it.
//! The conflict set of such a statement holds every node it writes and every
//! node its read part binds (see `statement_read_set`).
//!
//! Every read query the engine accepts runs as it would outside a
//! transaction.
//!
//! A statement is atomic inside the transaction: one that fails buffers
//! nothing, and the transaction stays usable.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Instant;

use core_query::cypher::{
    execute, execute_union, is_write_tokens, lex, parse_read, parse_write, plan, CreateStmt,
    MatchDeleteNodeStmt, MatchDeleteStmt, MergeStmt, NodePat, Operand, Params, Pattern, Query,
    RetItem, RetVal, UnionQuery, WriteStatement,
};
use core_query::{GraphView, ResultSet, WriteStats};
use core_storage::wal::WalRecord;
use core_storage::{Direction, GraphError, Result, Value};

use crate::db::{
    add_var, match_remove_ops, match_set_map_ops, match_set_ops, resolve_merge_set_value,
    unwind_rows, write_counts, BatchOp, MatchWrite, Precondition, RowPass, RowSource, NS_PROP,
};
use crate::layer::DeltaLayer;
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
    /// `buffer` as the records the overlay applies.
    records: Vec<TxRecord>,
    /// `base_snap`'s delta-tail layer with `records` laid over it, updated by
    /// each statement with that statement's records alone; `None` until the
    /// first write.
    overlay: Option<DeltaLayer>,
}

const _: () = {
    fn assert_send<T: Send>() {}
    let _ = assert_send::<Transaction>;
};

/// What a write statement returns once its records are laid over the
/// transaction.
enum Outcome {
    /// The `created, properties_set, deleted, relationships_created` row,
    /// with the statement's [`WriteStats`].
    Counts(ResultSet),
    /// The statement's `RETURN`, as a query over the transaction's state with
    /// the statement applied, and what the statement changed.
    Return(Box<Query>, WriteStats),
}

/// One buffered write as the overlay lays it: a WAL record, or a
/// relationship-property write named by the edge's type and end keys (its WAL
/// record names dense ids, which only the commit assigns).
enum TxRecord {
    Wal(WalRecord),
    EdgeProp {
        edge: (String, String, String),
        field: String,
        value: Option<Value>,
    },
}

/// Lay one buffered write over the transaction's overlay.
fn lay(
    overlay: &mut DeltaLayer,
    state: &crate::reader::FrozenOverlay,
    base: &Option<std::sync::Arc<core_storage::v8::MappedBase>>,
    rec: &TxRecord,
) -> Result<()> {
    match rec {
        TxRecord::Wal(rec) => overlay.apply(state, base, rec),
        TxRecord::EdgeProp { edge, field, value } => {
            overlay.write_edge_prop(
                state,
                base,
                (&edge.0, &edge.1, &edge.2),
                field,
                value.as_ref(),
            );
            Ok(())
        }
    }
}

/// One statement's effect, held until the statement has fully succeeded.
#[derive(Default)]
struct Staged {
    ops: Vec<BatchOp>,
    records: Vec<TxRecord>,
    keys: HashSet<String>,
}

impl Staged {
    fn insert_node(&mut self, label: &str, key: &str, props: Vec<(String, Value)>) {
        self.records.push(TxRecord::Wal(WalRecord::InsertNode {
            label: label.into(),
            key: key.into(),
            props: props.clone(),
        }));
        self.ops.push(BatchOp::InsertNode {
            label: label.into(),
            key: key.into(),
            props,
        });
        self.touch(key);
    }

    fn insert_edge(&mut self, edge_type: &str, src_key: &str, dst_key: &str) {
        self.records.push(TxRecord::Wal(WalRecord::InsertEdge {
            edge_type: edge_type.into(),
            src_key: src_key.into(),
            dst_key: dst_key.into(),
        }));
        self.ops.push(BatchOp::InsertEdge {
            edge_type: edge_type.into(),
            src_key: src_key.into(),
            dst_key: dst_key.into(),
        });
        self.touch(src_key);
        self.touch(dst_key);
    }

    fn set_prop(&mut self, key: &str, field: &str, value: Value) {
        self.records.push(TxRecord::Wal(WalRecord::SetProp {
            key: key.into(),
            field: field.into(),
            value: value.clone(),
        }));
        self.ops.push(BatchOp::SetProp {
            key: key.into(),
            field: field.into(),
            value,
        });
        self.touch(key);
    }

    fn remove_prop(&mut self, key: &str, field: &str) {
        self.records.push(TxRecord::Wal(WalRecord::RemoveProp {
            key: key.into(),
            field: field.into(),
        }));
        self.ops.push(BatchOp::RemoveProp {
            key: key.into(),
            field: field.into(),
        });
        self.touch(key);
    }

    fn delete_edge(&mut self, edge_type: &str, src_key: &str, dst_key: &str) {
        self.records.push(TxRecord::Wal(WalRecord::DeleteEdge {
            edge_type: edge_type.into(),
            src_key: src_key.into(),
            dst_key: dst_key.into(),
        }));
        self.ops.push(BatchOp::DeleteEdge {
            edge_type: edge_type.into(),
            src_key: src_key.into(),
            dst_key: dst_key.into(),
        });
        self.touch(src_key);
        self.touch(dst_key);
    }

    fn delete_node(&mut self, key: &str) {
        self.records
            .push(TxRecord::Wal(WalRecord::DeleteNode { key: key.into() }));
        self.ops.push(BatchOp::DeleteNode { key: key.into() });
        self.touch(key);
    }

    fn edge_prop(&mut self, edge: (String, String, String), field: String, value: Option<Value>) {
        self.touch(&edge.1);
        self.touch(&edge.2);
        let (edge_type, src_key, dst_key) = edge.clone();
        self.ops.push(match value.clone() {
            Some(value) => BatchOp::SetEdgeProp {
                edge_type,
                src_key,
                dst_key,
                field: field.clone(),
                value,
            },
            None => BatchOp::RemoveEdgeProp {
                edge_type,
                src_key,
                dst_key,
                field: field.clone(),
            },
        });
        self.records.push(TxRecord::EdgeProp { edge, field, value });
    }

    fn touch(&mut self, key: &str) {
        if !self.keys.contains(key) {
            self.keys.insert(key.to_string());
        }
    }
}

fn query_err(phase: &str) -> impl Fn(String) -> GraphError + '_ {
    move |e| GraphError::QueryError {
        detail: format!("{phase}: {e}"),
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
        path_var: None,
        continues_match: false,
    }
}

fn run(view: &GraphView<'_>, q: &Query, params: &BTreeMap<String, Value>) -> Result<ResultSet> {
    let ops = plan(q).map_err(query_err("plan"))?;
    execute(view, &ops, &Params(params)).map_err(query_err("execute"))
}

/// Every key bound to one of `vars` in any row of `rs`.
fn keys_in(rs: &ResultSet, vars: &[String], out: &mut HashSet<String>) {
    for row in 0..rs.len() {
        for var in vars {
            if let Some(Value::Str(k)) = rs.get(row, var) {
                if !out.contains(k) {
                    out.insert(k.clone());
                }
            }
        }
    }
}

/// The name a `WITH` item binds: its alias, or the variable it passes on.
fn item_name(item: &RetItem) -> Option<&String> {
    match (&item.alias, &item.value) {
        (Some(alias), _) => Some(alias),
        (None, RetVal::Var(v)) => Some(v),
        _ => None,
    }
}

/// Whether the `WITH` passes the node `name` on unchanged.
fn stage_passes(stage: &core_query::cypher::WithStage, name: &str) -> bool {
    stage.items.iter().any(|item| {
        matches!(&item.value, RetVal::Var(v) if v == name)
            && item.alias.as_deref().is_none_or(|a| a == name)
    })
}

/// Whether carrying more names through the `WITH` would change its rows: it
/// aggregates (extra names would become grouping keys), or it binds a name
/// already carried to something else.
fn stage_is_boundary(stage: &core_query::cypher::WithStage, carried: &[String]) -> bool {
    stage.items.iter().any(|item| {
        matches!(item.value, RetVal::Agg { .. })
            || item_name(item)
                .is_some_and(|name| carried.iter().any(|c| c == name) && !stage_passes(stage, name))
    })
}

/// Every distinct edge incident on `key` in `v`, as `(src_key, dst_key)`;
/// empty when `key` is not live.
fn incident_edges_in(v: &GraphView<'_>, key: &str) -> Vec<(String, String)> {
    let Some(id) = v.node_id(key) else {
        return Vec::new();
    };
    let mut edges = Vec::new();
    for etype in v.topo.etypes() {
        for (dir, flip) in [(Direction::Out, false), (Direction::In, true)] {
            for &other in v.topo.neighbors(etype, dir, id).as_ref() {
                let (src, dst) = if flip { (other, id) } else { (id, other) };
                edges.push((etype, src, dst));
            }
        }
    }
    edges.sort_unstable();
    edges.dedup();
    edges
        .into_iter()
        .map(|(_, src, dst)| (v.key_of(src).to_string(), v.key_of(dst).to_string()))
        .collect()
}

/// The transaction's state — its base snapshot with its earlier statements'
/// writes laid over it — as the [`RowSource`] a statement computes its ops
/// over. A transaction writes with full authority (a role-bound token cannot
/// write in one), so no role-scope check applies.
struct TxRows<'v, 'a> {
    view: &'v GraphView<'a>,
    db: &'v SharedDb,
}

impl RowSource for TxRows<'_, '_> {
    fn row_view<R>(&self, f: impl FnOnce(&GraphView<'_>) -> R) -> R {
        f(self.view)
    }

    fn row_exists(&self, key: &str) -> bool {
        self.view.node_id(key).is_some()
    }

    fn row_id(&self, key: &str) -> Option<u32> {
        self.view.node_id(key)
    }

    fn row_props(&self, key: &str) -> BTreeMap<String, Value> {
        self.view
            .node_id(key)
            .map(|id| self.view.props.props_of(id))
            .unwrap_or_default()
    }

    fn row_prop(&self, key: &str, field: &str) -> Option<Value> {
        let id = self.view.node_id(key)?;
        self.view.prop(id, field).map(|v| v.into_value())
    }

    fn row_label(&self, key: &str) -> Option<String> {
        let id = self.view.node_id(key)?;
        self.view.label_of(id).map(str::to_string)
    }

    fn row_edge(&self, etype: &str, src: &str, dst: &str) -> bool {
        let v = self.view;
        let (Some(src), Some(dst), Some(sym)) = (v.node_id(src), v.node_id(dst), v.sym(etype))
        else {
            return false;
        };
        v.topo.neighbors(sym, Direction::Out, src).contains(&dst)
    }

    fn row_has_edges(&self, key: &str) -> bool {
        let v = self.view;
        let Some(id) = v.node_id(key) else {
            return false;
        };
        v.topo.etypes().any(|et| {
            !v.topo.neighbors(et, Direction::Out, id).is_empty()
                || !v.topo.neighbors(et, Direction::In, id).is_empty()
        })
    }

    fn row_incident(&self, key: &str) -> Vec<String> {
        incident_edges_in(self.view, key)
            .into_iter()
            .map(|(from, to)| if from == key { to } else { from })
            .collect()
    }

    fn row_merge_existed(&self, _label: &str, key: &str) -> Result<bool> {
        Ok(self.row_exists(key))
    }

    fn row_merge_props(
        &self,
        key_field: &str,
        key_value: &Value,
        named_ns: Option<&Value>,
    ) -> Result<Vec<(String, Value)>> {
        Ok(merge_node_props(key_field, key_value, named_ns))
    }

    /// Views are the store's: the live registry answers.
    fn row_view_owns(&self, field: &str) -> bool {
        self.db.read().row_view_owns(field)
    }

    fn row_edge_props(&self, etype: &str, src: &str, dst: &str) -> BTreeMap<String, Value> {
        match self.row_edge_ids(etype, src, dst) {
            Some((et, s, d)) => self.view.edge_props.props_of(et, s, d),
            None => BTreeMap::new(),
        }
    }

    fn row_edge_ids(&self, etype: &str, src: &str, dst: &str) -> Option<(u32, u32, u32)> {
        let v = self.view;
        Some((v.sym(etype)?, v.node_id(src)?, v.node_id(dst)?))
    }

    /// Rules are the store's: the live engine answers.
    fn row_rule_owning(&self, etype: &str) -> Option<String> {
        self.db.read().rule_owning_edge_type(etype)
    }
}

/// A `MATCH … SET` / `REMOVE` or per-row statement's ops, computed over the
/// transaction's state and not yet buffered.
enum Prepared {
    Match(MatchWrite),
    Rows(Box<RowPass>),
}

impl Prepared {
    fn take_ops(&mut self) -> Vec<BatchOp> {
        match self {
            Prepared::Match(w) => std::mem::take(&mut w.ops),
            Prepared::Rows(p) => std::mem::take(&mut p.ops),
        }
    }

    fn result(self, src: &TxRows<'_, '_>, params: &BTreeMap<String, Value>) -> Result<ResultSet> {
        match self {
            Prepared::Match(w) => w.result(src, params),
            Prepared::Rows(p) => p.result(src.view, params),
        }
    }
}

/// `ops` as buffered records, with every key they write in the conflict set.
///
/// A `DETACH DELETE` records both endpoints of every edge it removes (as the
/// transaction's own `DETACH DELETE` does), and an `id` property write is
/// refused here, at the statement, as it is for the other write forms.
fn stage_ops(src: &TxRows<'_, '_>, ops: Vec<BatchOp>) -> Result<Staged> {
    let mut staged = Staged::default();
    // A node's `id` property as the statement's earlier ops left it.
    let mut ids: HashMap<String, Option<Value>> = HashMap::new();
    let stored_id = |ids: &HashMap<String, Option<Value>>, key: &str| match ids.get(key) {
        Some(v) => v.clone(),
        None => src.row_prop(key, "id"),
    };
    for op in ops {
        match op {
            BatchOp::InsertNode { label, key, props } => {
                let id = props
                    .iter()
                    .find(|(f, _)| f == "id")
                    .map(|(_, v)| v.clone());
                ids.insert(key.clone(), id);
                staged.insert_node(&label, &key, props);
            }
            BatchOp::InsertEdge {
                edge_type,
                src_key,
                dst_key,
            } => staged.insert_edge(&edge_type, &src_key, &dst_key),
            BatchOp::SetProp { key, field, value } => {
                if field == "id" {
                    let stored = stored_id(&ids, &key);
                    crate::db::check_key_property_write(&key, stored.as_ref(), Some(&value))?;
                    ids.insert(key.clone(), Some(value.clone()));
                }
                staged.set_prop(&key, &field, value);
            }
            BatchOp::RemoveProp { key, field } => {
                if field == "id" {
                    let stored = stored_id(&ids, &key);
                    crate::db::check_key_property_removal(&key, stored.as_ref())?;
                    ids.insert(key.clone(), None);
                }
                staged.remove_prop(&key, &field);
            }
            BatchOp::DeleteNode { key } => {
                // Edges the statement itself created at `key` already have
                // both endpoints in the set.
                for (from, to) in incident_edges_in(src.view, &key) {
                    staged.touch(&from);
                    staged.touch(&to);
                }
                ids.remove(&key);
                staged.delete_node(&key);
            }
            BatchOp::DeleteEdge {
                edge_type,
                src_key,
                dst_key,
            } => staged.delete_edge(&edge_type, &src_key, &dst_key),
            BatchOp::SetEdgeProp {
                edge_type,
                src_key,
                dst_key,
                field,
                value,
            } => staged.edge_prop((edge_type, src_key, dst_key), field, Some(value)),
            BatchOp::RemoveEdgeProp {
                edge_type,
                src_key,
                dst_key,
                field,
            } => staged.edge_prop((edge_type, src_key, dst_key), field, None),
            // No statement form makes these. Listed, not wildcarded, so a new
            // op is a compile error here rather than a runtime surprise.
            BatchOp::CreateRule(_)
            | BatchOp::DeleteRule { .. }
            | BatchOp::RenameNode { .. }
            | BatchOp::InsertEdgeUpsert { .. }
            | BatchOp::InsertNodeOnConflict { .. } => {
                return Err(GraphError::QueryError {
                    detail: "internal: a statement made an op a transaction cannot buffer".into(),
                })
            }
        }
    }
    Ok(staged)
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
        // The schema is the store's, not the transaction's: a statement that
        // changes it is refused here, and `SHOW` lists it as of BEGIN.
        if let Some(stmt) = crate::db::ddl_statement(&tokens)? {
            if !stmt.is_show() {
                return Err(crate::db::schema_change_in_a_transaction());
            }
            return Ok(self.base_snap.frozen.schema.show(&stmt));
        }
        // `CALL` reads the transaction's state, its own writes included.
        if let Some(call) = crate::db::call_statement(&tokens)? {
            let schema = &self.base_snap.frozen.schema;
            return self.with_view(|v| crate::db::run_call(&call, v, schema, params))?;
        }
        if is_write_tokens(&tokens) {
            let stmt = parse_write(&tokens).map_err(query_err("parse"))?;
            return self.execute_write(stmt, params);
        }
        let union = parse_read(&tokens).map_err(query_err("parse"))?;
        let rs = self
            .with_view(|v| {
                execute_union(v, &union, &Params(params)).map(|rs| crate::db::finish_result(v, rs))
            })?
            .map_err(query_err("execute"))?;
        let keys = self.read_set(&union, params);
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
        // The overlay is dropped before the submit, off the commit's path.
        drop(self.overlay);
        self.db
            .submit_batch_cas_with_seq(preconds, self.buffer)
            .map(|(_, seq)| seq)
    }

    /// Discard the buffered writes. Nothing reaches the WAL.
    pub fn rollback(self) {}

    // ── Internals ────────────────────────────────────────────────────────────

    /// Run `f` over the state statements read: the base snapshot with the
    /// transaction's overlay on top.
    fn with_view<R>(&self, f: impl FnOnce(&GraphView<'_>) -> R) -> Result<R> {
        match &self.overlay {
            None => self.base_snap.with_view(f),
            Some(overlay) => {
                let (state, base, _) = self.base_snap.layer_parts()?;
                Ok(f(&overlay.view(state, base, None)))
            }
        }
    }

    /// Whether `key` is a live node in the transaction's state.
    fn has_key(&self, key: &str) -> Result<bool> {
        self.with_view(|v| v.node_id(key).is_some())
    }

    /// A stored property of `key` in the transaction's state (base snapshot
    /// plus the overlay); `None` when the node or the property is absent.
    fn stored_prop(&self, key: &str, field: &str) -> Result<Option<Value>> {
        self.with_view(|v| {
            let id = v.node_id(key)?;
            v.prop(id, field).map(|vr| vr.into_value())
        })
    }

    /// Whether `src -[edge_type]-> dst` is in the transaction's state.
    fn edge_present(&self, edge_type: &str, src_key: &str, dst_key: &str) -> Result<bool> {
        self.with_view(|v| {
            let (Some(src), Some(dst), Some(sym)) =
                (v.node_id(src_key), v.node_id(dst_key), v.sym(edge_type))
            else {
                return false;
            };
            v.topo.neighbors(sym, Direction::Out, src).contains(&dst)
        })
    }

    /// Every distinct edge incident on `key` in the transaction's state, as
    /// `(src_key, dst_key)`; empty when `key` is not live.
    fn incident_edges(&self, key: &str) -> Result<Vec<(String, String)>> {
        self.with_view(|v| incident_edges_in(v, key))
    }

    /// Run `f` over the transaction's state as a [`RowSource`].
    fn with_rows<R>(&self, f: impl FnOnce(&TxRows<'_, '_>) -> R) -> Result<R> {
        self.with_view(|view| f(&TxRows { view, db: &self.db }))
    }

    /// Add `keys` not yet in the conflict set, each with the `last_change` it
    /// had in the base snapshot.
    fn record_keys(&mut self, keys: &HashSet<String>) {
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

    /// Lay `records` over the overlay, creating it on the first write. On a
    /// failure the overlay is put back to the accepted buffer alone.
    fn apply(&mut self, records: &[TxRecord]) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        if self.overlay.is_none() {
            self.overlay = Some(self.fresh_overlay()?);
        }
        let (state, base, _) = self.base_snap.layer_parts()?;
        let overlay = self.overlay.as_mut().expect("set just above");
        let applied = records
            .iter()
            .try_for_each(|rec| lay(overlay, state, base, rec));
        if applied.is_err() {
            self.revert()?;
        }
        applied
    }

    /// Rebuild the overlay from the accepted records alone, undoing a failed
    /// statement's. Costs the transaction's own buffer; runs only when a
    /// statement fails after its records were laid over the overlay.
    fn revert(&mut self) -> Result<()> {
        self.overlay = None;
        if self.records.is_empty() {
            return Ok(());
        }
        let mut overlay = self.fresh_overlay()?;
        let (state, base, _) = self.base_snap.layer_parts()?;
        for rec in &self.records {
            lay(&mut overlay, state, base, rec)?;
        }
        self.overlay = Some(overlay);
        Ok(())
    }

    /// The overlay before any of this transaction's writes: a copy of the
    /// base snapshot's delta-tail layer (it costs the tail, not the store),
    /// or an empty layer over the fold.
    fn fresh_overlay(&self) -> Result<DeltaLayer> {
        let (state, _, tail) = self.base_snap.layer_parts()?;
        Ok(tail.cloned().unwrap_or_else(|| DeltaLayer::new(state)))
    }

    /// Make a fully successful statement part of the transaction.
    fn accept(&mut self, staged: Staged) {
        self.record_keys(&staged.keys);
        self.buffer.extend(staged.ops);
        self.records.extend(staged.records);
    }

    fn execute_write(
        &mut self,
        stmt: WriteStatement,
        params: &BTreeMap<String, Value>,
    ) -> Result<ResultSet> {
        let mut staged = Staged::default();
        let outcome = match stmt {
            WriteStatement::Create(s) => self.stage_create(s, &mut staged)?,
            stmt @ (WriteStatement::MatchSet(_)
            | WriteStatement::MatchSetMap(_)
            | WriteStatement::MatchRemove(_)
            | WriteStatement::UnwindWrite(_)) => return self.execute_rows(stmt, params),
            WriteStatement::MatchDelete(s) => self.stage_match_delete(s, params, &mut staged)?,
            WriteStatement::MatchDeleteNode(s) => {
                self.stage_match_delete_node(s, params, &mut staged)?
            }
            WriteStatement::Merge(s) => self.stage_merge(s, params, &mut staged)?,
        };
        // Only this statement's records are applied: the overlay already
        // holds every earlier statement's.
        self.apply(&staged.records)?;
        let rs = match outcome {
            Outcome::Counts(rs) => rs,
            Outcome::Return(q, stats) => match self
                .with_view(|v| run(v, &q, params).map(|rs| crate::db::finish_result(v, rs)))
            {
                Ok(Ok(rs)) => rs.with_stats(stats),
                Ok(Err(e)) | Err(e) => {
                    if !staged.records.is_empty() {
                        self.revert()?;
                    }
                    return Err(e);
                }
            },
        };
        self.accept(staged);
        Ok(rs)
    }

    /// `MATCH … SET` / `REMOVE` and the per-row forms: the ops are computed by
    /// the code auto-commit runs, over the transaction's state, then buffered;
    /// a `RETURN` reads the transaction with them laid over it.
    fn execute_rows(
        &mut self,
        stmt: WriteStatement,
        params: &BTreeMap<String, Value>,
    ) -> Result<ResultSet> {
        // The statement's read part, for the conflict set: taken before the
        // statement is consumed, run after it succeeds.
        let read = match &stmt {
            WriteStatement::MatchSet(s) => match_query(s.matches.clone(), s.where_expr.clone()),
            WriteStatement::MatchSetMap(s) => match_query(s.matches.clone(), s.where_expr.clone()),
            WriteStatement::MatchRemove(s) => match_query(s.matches.clone(), s.where_expr.clone()),
            WriteStatement::UnwindWrite(s) => s.read.clone(),
            _ => match_query(Vec::new(), None),
        };
        let (prepared, mut staged) = self.with_rows(|src| -> Result<(Prepared, Staged)> {
            let mut prepared = match stmt {
                WriteStatement::MatchSet(s) => Prepared::Match(match_set_ops(src, s, params)?),
                WriteStatement::MatchSetMap(s) => {
                    Prepared::Match(match_set_map_ops(src, s, params)?)
                }
                WriteStatement::MatchRemove(s) => {
                    Prepared::Match(match_remove_ops(src, s, params)?)
                }
                WriteStatement::UnwindWrite(s) => {
                    Prepared::Rows(Box::new(unwind_rows(src, &s, params)?))
                }
                other => {
                    return Err(GraphError::QueryError {
                        detail: format!("internal: not a per-row statement: {other:?}"),
                    })
                }
            };
            let mut staged = stage_ops(src, prepared.take_ops())?;
            // A key a MERGE found but did not write still guards the commit,
            // as the plain MERGE's does.
            if let Prepared::Rows(pass) = &prepared {
                for key in pass.merge_keys() {
                    staged.touch(key);
                }
            }
            Ok((prepared, staged))
        })??;
        // Every node the statement read, as a plain read records them: a
        // concurrent change to a node it matched or filtered on is a conflict.
        staged.keys.extend(self.statement_read_set(&read, params)?);
        self.apply(&staged.records)?;
        match self.with_rows(|src| prepared.result(src, params)) {
            Ok(Ok(rs)) => {
                self.accept(staged);
                Ok(rs)
            }
            Ok(Err(e)) | Err(e) => {
                if !staged.records.is_empty() {
                    self.revert()?;
                }
                Err(e)
            }
        }
    }

    fn stage_create(&self, stmt: CreateStmt, staged: &mut Staged) -> Result<Outcome> {
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
            if self.has_key(&key)? || var_to_key.values().any(|k| k == &key) {
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
        let stats = crate::db::create_stats(&stmt);
        let Some(returns) = stmt.returns else {
            return Ok(Outcome::Counts(
                write_counts(stmt.nodes.len() as i64, 0, 0, stmt.edges.len() as i64)
                    .with_stats(stats),
            ));
        };
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
        Ok(Outcome::Return(Box::new(q), stats))
    }

    fn stage_match_delete(
        &self,
        stmt: MatchDeleteStmt,
        params: &BTreeMap<String, Value>,
        staged: &mut Staged,
    ) -> Result<Outcome> {
        let mut matches = stmt.matches;
        let mut node_vars = Vec::new();
        bind_node_vars(&mut matches, &mut 0, &mut node_vars);
        for ed in &stmt.deletes {
            add_var(&mut node_vars, &ed.src_var);
            add_var(&mut node_vars, &ed.dst_var);
        }
        let mut q = match_query(matches, stmt.where_expr);
        q.returns = var_items(&node_vars);
        let match_rs = self.with_view(|v| run(v, &q, params))??;
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
        Ok(Outcome::Counts(write_counts(0, 0, deleted, 0).with_stats(
            WriteStats {
                relationships_deleted: deleted,
                ..WriteStats::default()
            },
        )))
    }

    fn stage_match_delete_node(
        &self,
        stmt: MatchDeleteNodeStmt,
        params: &BTreeMap<String, Value>,
        staged: &mut Staged,
    ) -> Result<Outcome> {
        let mut matches = stmt.matches;
        let mut node_vars = Vec::new();
        bind_node_vars(&mut matches, &mut 0, &mut node_vars);
        let mut q = match_query(matches, stmt.where_expr);
        q.returns = var_items(&node_vars);
        let match_rs = self.with_view(|v| run(v, &q, params))??;
        keys_in(&match_rs, &node_vars, &mut staged.keys);
        // In row order, each once: the deletes are buffered in this order.
        let mut doomed: Vec<String> = Vec::new();
        for row in 0..match_rs.len() {
            for var in &stmt.node_vars {
                if let Some(Value::Str(k)) = match_rs.get(row, var) {
                    add_var(&mut doomed, k);
                }
            }
        }
        let mut nodes_deleted = 0i64;
        let mut edges_deleted = 0i64;
        // Auto-commit deletes the nodes one at a time, so an edge between two
        // of them is removed — and counted — once, with the first.
        let mut gone: HashSet<&str> = HashSet::new();
        for key in &doomed {
            let edges = self.incident_edges(key)?;
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
            for (src, dst) in &edges {
                staged.touch(src);
                staged.touch(dst);
            }
            let removed_here = edges
                .iter()
                .filter(|(src, dst)| !gone.contains(src.as_str()) && !gone.contains(dst.as_str()))
                .count();
            nodes_deleted += 1;
            edges_deleted += removed_here as i64;
            gone.insert(key);
            staged.delete_node(key);
        }
        Ok(Outcome::Counts(
            write_counts(0, 0, nodes_deleted + edges_deleted, 0).with_stats(WriteStats {
                nodes_deleted,
                relationships_deleted: edges_deleted,
                ..WriteStats::default()
            }),
        ))
    }

    fn stage_merge(
        &self,
        stmt: MergeStmt,
        params: &BTreeMap<String, Value>,
        staged: &mut Staged,
    ) -> Result<Outcome> {
        if stmt.edge.is_some() {
            return self.stage_merge_edge(stmt, params, staged);
        }
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
        let existed = self.has_key(&key)?;
        staged.touch(&key);
        let mut created = 0i64;
        let mut properties_set = 0i64;
        // The `ON CREATE` / `ON MATCH` items applied: the write row's
        // `properties_set` (ledger row 150).
        let sets_applied = if existed {
            stmt.on_match.len()
        } else {
            stmt.on_create.len()
        } as i64;
        if existed {
            properties_set += stmt.on_match.len() as i64;
            for sc in &stmt.on_match {
                let value = resolve_merge_set_value(&sc.value, params)?;
                if sc.field == "id" {
                    crate::db::check_key_property_write(
                        &key,
                        self.stored_prop(&key, "id")?.as_ref(),
                        Some(&value),
                    )?;
                }
                staged.set_prop(&key, &sc.field, value);
            }
        } else {
            let props = merge_node_props(&stmt.key_field, &stmt.key_value, stmt.ns.as_ref());
            properties_set += (props.len() + stmt.on_create.len()) as i64;
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
        let stats = WriteStats {
            nodes_created: created,
            labels_added: created,
            properties_set,
            ..WriteStats::default()
        };
        let Some(returns) = stmt.returns else {
            return Ok(Outcome::Counts(
                write_counts(created, sets_applied, 0, 0).with_stats(stats),
            ));
        };
        let var = stmt.var.as_deref().unwrap_or("_mn0");
        let mut q = match_query(
            vec![node_by_key(var, &stmt.label, stmt.key_value.clone())],
            None,
        );
        q.returns = returns;
        Ok(Outcome::Return(Box::new(q), stats))
    }

    /// `MERGE (a)-[:T]->(b)`: the same decisions as `GraphDb::exec_merge_edge`,
    /// buffered. Both nodes are found or created by key, then the edge; `ON
    /// MATCH` runs when all three existed, `ON CREATE` otherwise. Existence is
    /// read through the overlay, so an edge an earlier statement buffered is
    /// found. Both end keys join the conflict set either way.
    fn stage_merge_edge(
        &self,
        stmt: MergeStmt,
        params: &BTreeMap<String, Value>,
        staged: &mut Staged,
    ) -> Result<Outcome> {
        let Some(edge) = stmt.edge.clone() else {
            return Err(GraphError::QueryError {
                detail: "MERGE relationship pattern missing".into(),
            });
        };
        let key_of = |v: &Value| match v {
            Value::Str(s) => Ok(s.clone()),
            other => Err(GraphError::QueryError {
                detail: format!("MERGE key value must be a string (got {other:?})"),
            }),
        };
        let start_key = key_of(&stmt.key_value)?;
        let end_key = key_of(&edge.end_key_value)?;
        if start_key == end_key && stmt.label != edge.end_label {
            return Err(GraphError::QueryError {
                detail: format!(
                    "MERGE: key '{start_key}' is given both label '{}' and label '{}'",
                    stmt.label, edge.end_label
                ),
            });
        }
        // A SET item names the start node or the end node, never the relationship.
        let set_target = |var: &str| -> Result<&str> {
            if stmt.var.as_deref() == Some(var) {
                Ok(start_key.as_str())
            } else if edge.end_var.as_deref() == Some(var) {
                Ok(end_key.as_str())
            } else {
                Err(GraphError::QueryError {
                    detail: format!(
                        "SET variable '{var}' does not match a MERGE node or relationship \
                         variable"
                    ),
                })
            }
        };
        let mut create_sets = Vec::new();
        for sc in &stmt.on_create {
            create_sets.push((set_target(&sc.var)?, sc));
        }
        let mut match_sets = Vec::new();
        for sc in &stmt.on_match {
            match_sets.push((set_target(&sc.var)?, sc));
        }
        let start_existed = self.has_key(&start_key)?;
        let end_existed = self.has_key(&end_key)?;
        let (src_key, dst_key) = match edge.dir {
            core_query::cypher::RelDir::Left => (end_key.as_str(), start_key.as_str()),
            _ => (start_key.as_str(), end_key.as_str()),
        };
        let edge_existed =
            start_existed && end_existed && self.edge_present(&edge.etype, src_key, dst_key)?;
        staged.touch(&start_key);
        staged.touch(&end_key);
        let mut created = 0i64;
        let mut relationships_created = 0i64;
        let mut properties_set = 0i64;
        if !start_existed {
            let props = merge_node_props(&stmt.key_field, &stmt.key_value, stmt.ns.as_ref());
            properties_set += props.len() as i64;
            staged.insert_node(&stmt.label, &start_key, props);
            created += 1;
        }
        if !end_existed && end_key != start_key {
            let props = merge_node_props(
                &edge.end_key_field,
                &edge.end_key_value,
                edge.end_ns.as_ref(),
            );
            properties_set += props.len() as i64;
            staged.insert_node(&edge.end_label, &end_key, props);
            created += 1;
        }
        let sets = if edge_existed {
            &match_sets
        } else {
            staged.insert_edge(&edge.etype, src_key, dst_key);
            relationships_created = 1;
            &create_sets
        };
        for (key, sc) in sets {
            let value = resolve_merge_set_value(&sc.value, params)?;
            if sc.field == "id" {
                let stored = if edge_existed
                    || (stmt.var.as_deref() == Some(sc.var.as_str()) && start_existed)
                    || (edge.end_var.as_deref() == Some(sc.var.as_str()) && end_existed)
                {
                    self.stored_prop(key, "id")?
                } else if stmt.var.as_deref() == Some(sc.var.as_str()) {
                    (stmt.key_field == "id").then(|| stmt.key_value.clone())
                } else {
                    (edge.end_key_field == "id").then(|| edge.end_key_value.clone())
                };
                crate::db::check_key_property_write(key, stored.as_ref(), Some(&value))?;
            }
            staged.set_prop(key, &sc.field, value);
        }
        let sets_applied = sets.len() as i64;
        properties_set += sets_applied;
        let stats = WriteStats {
            nodes_created: created,
            labels_added: created,
            relationships_created,
            properties_set,
            ..WriteStats::default()
        };
        let Some(returns) = stmt.returns else {
            return Ok(Outcome::Counts(
                write_counts(created, sets_applied, 0, relationships_created).with_stats(stats),
            ));
        };
        let node =
            |var: Option<&String>, default: &str, label: &str, field: &str, key: &str| NodePat {
                var: Some(var.cloned().unwrap_or_else(|| default.to_string())),
                label: Some(label.to_string()),
                props: vec![(field.to_string(), Operand::Lit(Value::Str(key.to_string())))],
            };
        let mut q = match_query(
            vec![Pattern {
                start: node(
                    stmt.var.as_ref(),
                    "_mn0",
                    &stmt.label,
                    &stmt.key_field,
                    &start_key,
                ),
                chain: vec![(
                    core_query::cypher::RelPat {
                        var: edge.rel_var.clone(),
                        etypes: vec![edge.etype.clone()],
                        dir: edge.dir,
                        hops: None,
                    },
                    node(
                        edge.end_var.as_ref(),
                        "_mn1",
                        &edge.end_label,
                        &edge.end_key_field,
                        &end_key,
                    ),
                )],
                shortest: false,
                path_var: None,
                continues_match: false,
            }],
            None,
        );
        q.returns = returns;
        Ok(Outcome::Return(Box::new(q), stats))
    }

    /// Every node the read part of a write statement binds — named or
    /// anonymous, in its `MATCH` and in every stage after an `UNWIND` or
    /// `WITH` — for the conflict set, as [`Self::read_set`] records a plain
    /// read's.
    ///
    /// The read part is run again over the state before the statement, with
    /// every anonymous node named and every node carried to the end, returning
    /// the distinct node keys. A `WITH` that aggregates, or rebinds a carried
    /// name, would change the rows if more names were carried through it, so
    /// the nodes bound before it are collected by running the read part up to
    /// it instead. Unlike `read_set`, a failure here fails the statement:
    /// a write must not commit with reads it did not record.
    fn statement_read_set(
        &self,
        read: &Query,
        params: &BTreeMap<String, Value>,
    ) -> Result<HashSet<String>> {
        let mut q = read.clone();
        q.returns.clear();
        let mut next = 0;
        let mut carried: Vec<String> = Vec::new();
        bind_node_vars(&mut q.matches, &mut next, &mut carried);
        for oc in &mut q.optional_clauses {
            bind_node_vars(&mut oc.patterns, &mut next, &mut carried);
        }
        let mut keys = HashSet::new();
        for i in 0..q.stages.len() {
            if stage_is_boundary(&q.stages[i], &carried) {
                self.read_set_upto(&q, i, &carried, params, &mut keys)?;
                carried.retain(|name| stage_passes(&q.stages[i], name));
            } else {
                let stage = &mut q.stages[i];
                for name in &carried {
                    if !stage.items.iter().any(|item| item_name(item) == Some(name)) {
                        stage.items.push(RetItem {
                            value: RetVal::Var(name.clone()),
                            alias: None,
                        });
                    }
                }
            }
            let stage = &mut q.stages[i];
            bind_node_vars(&mut stage.matches, &mut next, &mut carried);
            for oc in &mut stage.optional_clauses {
                bind_node_vars(&mut oc.patterns, &mut next, &mut carried);
            }
        }
        self.read_set_upto(&q, q.stages.len(), &carried, params, &mut keys)?;
        Ok(keys)
    }

    /// Add to `keys` the distinct keys of `nodes` over `q`'s first `upto`
    /// stages.
    fn read_set_upto(
        &self,
        q: &Query,
        upto: usize,
        nodes: &[String],
        params: &BTreeMap<String, Value>,
        keys: &mut HashSet<String>,
    ) -> Result<()> {
        if nodes.is_empty() {
            return Ok(());
        }
        let mut prefix = q.clone();
        prefix.stages.truncate(upto);
        prefix.returns = var_items(nodes);
        prefix.distinct = true;
        prefix.order_by.clear();
        prefix.skip = None;
        prefix.limit = None;
        let rs = self.with_view(|v| run(v, &prefix, params))??;
        keys_in(&rs, nodes, keys);
        Ok(())
    }

    /// The node keys a read query matched, for the conflict set.
    ///
    /// Each part's top-level `MATCH` / `OPTIONAL MATCH` patterns and `WHERE`
    /// are re-run returning every node they bind, anonymous ones included, so
    /// both endpoints of every matched edge are recorded. Best effort by
    /// design: if the filtered re-run cannot be planned the unfiltered one is
    /// used (a wider set is only more conservative), and a read whose re-run
    /// fails records nothing — snapshot isolation needs the write set, which
    /// every write statement records exactly.
    fn read_set(&self, union: &UnionQuery, params: &BTreeMap<String, Value>) -> HashSet<String> {
        let mut keys = HashSet::new();
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
            let rs = self
                .with_view(|v| run(v, &q, params))
                .and_then(|r| r)
                .or_else(|_| {
                    q.where_expr = None;
                    self.with_view(|v| run(v, &q, params)).and_then(|r| r)
                });
            if let Ok(rs) = rs {
                keys_in(&rs, &vars, &mut keys);
            }
        }
        keys
    }
}

/// The properties a MERGE create arm inserts: the identifying key, plus `ns`
/// when the pattern named one.
fn merge_node_props(
    key_field: &str,
    key_value: &Value,
    ns: Option<&Value>,
) -> Vec<(String, Value)> {
    let mut props = vec![(key_field.to_string(), key_value.clone())];
    if let Some(ns) = ns {
        props.push((NS_PROP.to_string(), ns.clone()));
    }
    props
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
