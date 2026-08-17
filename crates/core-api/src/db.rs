use core_query::cypher::{execute, lex, parse, plan, Params};
use core_query::{eval_filter, expand, neighborhood, Dir, Filter, GraphView, ResultSet};
use core_rules::{GraphMut, RuleDef, RuleEngine};
use core_storage::fs::{FileId, Fs, FsIntrospect, RealFs};
use core_storage::wal::{decode_all, encode_record, WalRecord};
use core_storage::{
    ColumnStore, Direction, EdgeProps, GraphError, IdMap, Interner, Result, Topology, Value,
};
use std::collections::{BTreeMap, BTreeSet};

/// One rule-owned edge between two nodes, with the rule name, edge type,
/// direction (src_key → dst_key), and weight if the rule stores one.
#[derive(Debug, PartialEq)]
pub struct Explanation {
    pub rule: String,
    pub edge_type: String,
    pub src_key: String,
    pub dst_key: String,
    pub weight: Option<f64>,
}

/// Single construction point for a `GraphMut` view over the split-borrowed graph fields.
/// Callers use `std::mem::take` on the engine before calling this, then restore it after.
fn make_graph_mut<'a>(
    ids: &'a IdMap,
    syms: &'a mut Interner,
    labels: &'a [u32],
    props: &'a ColumnStore,
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

pub struct GraphDb<F: Fs> {
    fs: F,
    ids: IdMap,
    syms: Interner,
    topo: Topology,
    props: ColumnStore,
    labels: Vec<u32>, // node id -> label symbol
    edge_props: EdgeProps,
    engine: RuleEngine,
}

impl GraphDb<RealFs> {
    pub fn open(dir: &std::path::Path) -> Result<Self> {
        Self::open_with(RealFs::new(dir)?)
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
        };
        let snap_bytes = db.fs.read(FileId::Snapshot)?;
        if let Some(state) = core_storage::snapshot::decode(&snap_bytes)? {
            db.ids = state.ids;
            db.syms = state.syms;
            db.topo = state.topo;
            db.props = state.props;
            db.labels = state.labels;
            db.edge_props = state.edge_props;
            let defs: Vec<RuleDef> = state
                .rule_defs
                .iter()
                .map(|b| {
                    bincode::deserialize(b).map_err(|e| GraphError::Corrupt {
                        detail: format!("snapshot rule_def deserialize: {e}"),
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            db.engine = RuleEngine::from_persist(defs, state.provenance);
            db.engine
                .reindex_all(&db.ids, &db.syms, &db.labels, &db.props);
        }
        let bytes = db.fs.read(FileId::Wal)?;
        let (records, valid_len) = decode_all(&bytes);
        if valid_len < bytes.len() {
            db.fs.write_atomic(FileId::Wal, &bytes[..valid_len])?;
        }
        for rec in records {
            db.apply(&rec)?;
        }
        Ok(db)
    }

    /// Apply a record to in-memory state. Used by both live writes and replay,
    /// so replay is definitionally identical to the original execution.
    fn apply(&mut self, rec: &WalRecord) -> Result<()> {
        match rec {
            WalRecord::InsertNode { label, key, props } => {
                let id = self.ids.get_or_insert(key);
                let sym = self.syms.intern(label);
                if self.labels.len() <= id as usize {
                    // gap slots are sentinels, never valid label symbols
                    self.labels.resize(id as usize + 1, u32::MAX);
                }
                self.labels[id as usize] = sym;
                for (field, value) in props {
                    self.props.set(id, field, value.clone());
                }
                // Fire rules for the newly inserted node.
                let mut eng = std::mem::take(&mut self.engine);
                {
                    let mut gm = make_graph_mut(
                        &self.ids,
                        &mut self.syms,
                        &self.labels,
                        &self.props,
                        &mut self.topo,
                        &mut self.edge_props,
                    );
                    eng.on_node_changed(id, None, &mut gm);
                }
                self.engine = eng;
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
                self.topo.add_edge(etype, src, dst);
            }
            WalRecord::SetProp { key, field, value } => {
                let id = self.ids.get(key).ok_or_else(|| GraphError::Corrupt {
                    detail: format!("wal replay references unknown key {key}"),
                })?;
                let old_value = self.props.get(id, field).cloned();
                self.props.set(id, field, value.clone());
                // Fire rules for the changed field.
                let mut eng = std::mem::take(&mut self.engine);
                {
                    let mut gm = make_graph_mut(
                        &self.ids,
                        &mut self.syms,
                        &self.labels,
                        &self.props,
                        &mut self.topo,
                        &mut self.edge_props,
                    );
                    eng.on_node_changed(id, Some((field, old_value)), &mut gm);
                }
                self.engine = eng;
            }
            WalRecord::CreateRule { def_bytes } => {
                let def: RuleDef =
                    bincode::deserialize(def_bytes).map_err(|e| GraphError::Corrupt {
                        detail: format!("CreateRule def_bytes deserialize failed: {e}"),
                    })?;
                // Replay-over-snapshot idempotency: the rule was captured in the snapshot
                // so the engine already has it; silently skip to avoid a spurious
                // RuleInvalid error in the crash window between snapshot write and WAL
                // truncation.
                if self.engine.rules().any(|r| r.name == def.name) {
                    return Ok(());
                }
                let mut eng = std::mem::take(&mut self.engine);
                let result = {
                    let mut gm = make_graph_mut(
                        &self.ids,
                        &mut self.syms,
                        &self.labels,
                        &self.props,
                        &mut self.topo,
                        &mut self.edge_props,
                    );
                    eng.create_rule(def, &mut gm)
                };
                self.engine = eng;
                result.map_err(|e| GraphError::RuleInvalid { detail: e })?;
            }
            WalRecord::DeleteRule { name } => {
                // Replay-over-snapshot idempotency: the snapshot already captured the
                // post-delete state so the rule is absent; silently skip to avoid a
                // spurious RuleNotFound error in the crash window between snapshot write
                // and WAL truncation.
                if !self.engine.rules().any(|r| r.name == *name) {
                    return Ok(());
                }
                let mut eng = std::mem::take(&mut self.engine);
                let result = {
                    let mut gm = make_graph_mut(
                        &self.ids,
                        &mut self.syms,
                        &self.labels,
                        &self.props,
                        &mut self.topo,
                        &mut self.edge_props,
                    );
                    eng.delete_rule(name, &mut gm)
                };
                self.engine = eng;
                result.map_err(|_| GraphError::RuleNotFound { name: name.clone() })?;
            }
            WalRecord::RemoveProp { key, field } => {
                // Recovery-safe: unknown key or already-absent field is a
                // clean no-op. Crash-window replay over a snapshot that
                // already applied this record must not Err.
                let Some(id) = self.ids.get(key) else {
                    return Ok(());
                };
                let old = self.props.get(id, field).cloned();
                self.props.remove(id, field);
                let mut eng = std::mem::take(&mut self.engine);
                {
                    let mut gm = make_graph_mut(
                        &self.ids,
                        &mut self.syms,
                        &self.labels,
                        &self.props,
                        &mut self.topo,
                        &mut self.edge_props,
                    );
                    eng.on_node_changed(id, Some((field, old)), &mut gm);
                }
                self.engine = eng;
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
                self.topo.remove_edge(etype, src, dst);
                self.edge_props.remove_edge(etype, src, dst);
            }
            WalRecord::DeleteNode { .. } => {
                // Stub: implemented in Task 4.
            }
            WalRecord::Batch(inner) => {
                // Apply each inner record in order through the same apply path.
                // Inner records are validated free of nested Batch by encode_record.
                for rec in inner {
                    self.apply(rec)?;
                }
            }
        }
        Ok(())
    }

    fn log_then_apply(&mut self, rec: WalRecord) -> Result<()> {
        self.fs.append(FileId::Wal, &encode_record(&rec))?;
        self.fs.sync(FileId::Wal)?; // strict policy in plan 1
        self.apply(&rec)
    }

    pub fn insert_node(
        &mut self,
        label: &str,
        key: &str,
        props: Vec<(String, Value)>,
    ) -> Result<()> {
        if self.ids.get(key).is_some() {
            return Err(GraphError::DuplicateKey { key: key.into() });
        }
        self.log_then_apply(WalRecord::InsertNode {
            label: label.into(),
            key: key.into(),
            props,
        })
    }

    pub fn insert_edge(&mut self, edge_type: &str, src_key: &str, dst_key: &str) -> Result<bool> {
        for k in [src_key, dst_key] {
            if self.ids.get(k).is_none() {
                return Err(GraphError::KeyNotFound { key: k.into() });
            }
        }
        let src = self.ids.get(src_key).unwrap();
        let dst = self.ids.get(dst_key).unwrap();
        if let Some(sym) = self.syms.get(edge_type) {
            // Rule-owned guard: reject user edges that conflict with derived edges.
            if self.engine.is_owned(sym, src, dst) {
                return Err(GraphError::RuleOwned {
                    detail: format!("edge {edge_type} {src_key}→{dst_key} is rule-owned"),
                });
            }
            if self
                .topo
                .neighbors(sym, Direction::Out, src)
                .binary_search(&dst)
                .is_ok()
            {
                return Ok(false); // duplicate: don't log
            }
        }
        self.log_then_apply(WalRecord::InsertEdge {
            edge_type: edge_type.into(),
            src_key: src_key.into(),
            dst_key: dst_key.into(),
        })?;
        Ok(true)
    }

    pub fn set_prop(&mut self, key: &str, field: &str, value: Value) -> Result<()> {
        if self.ids.get(key).is_none() {
            return Err(GraphError::KeyNotFound { key: key.into() });
        }
        self.log_then_apply(WalRecord::SetProp {
            key: key.into(),
            field: field.into(),
            value,
        })
    }

    /// Remove a property. Returns `Ok(false)` (and does not log) if the field
    /// is already absent. Unknown or tombstoned keys are `Err(KeyNotFound)`.
    pub fn remove_prop(&mut self, key: &str, field: &str) -> Result<bool> {
        let Some(id) = self.ids.get(key) else {
            return Err(GraphError::KeyNotFound { key: key.into() });
        };
        if self.props.get(id, field).is_none() {
            return Ok(false);
        }
        self.log_then_apply(WalRecord::RemoveProp {
            key: key.into(),
            field: field.into(),
        })?;
        Ok(true)
    }

    /// Delete a user edge. Returns `Ok(false)` (and does not log) if the edge
    /// is absent. Unknown keys are `Err(KeyNotFound)`. Rule-owned edges are
    /// `Err(RuleOwned)` — delete or change the owning rule instead.
    pub fn delete_edge(&mut self, edge_type: &str, src_key: &str, dst_key: &str) -> Result<bool> {
        for k in [src_key, dst_key] {
            if self.ids.get(k).is_none() {
                return Err(GraphError::KeyNotFound { key: k.into() });
            }
        }
        let src = self.ids.get(src_key).unwrap();
        let dst = self.ids.get(dst_key).unwrap();
        if let Some(sym) = self.syms.get(edge_type) {
            if self.engine.is_owned(sym, src, dst) {
                return Err(GraphError::RuleOwned {
                    detail: format!(
                        "edge {edge_type} {src_key}→{dst_key} is rule-owned; \
                         delete or change the owning rule"
                    ),
                });
            }
            if self
                .topo
                .neighbors(sym, Direction::Out, src)
                .binary_search(&dst)
                .is_err()
            {
                return Ok(false);
            }
        } else {
            return Ok(false);
        }
        self.log_then_apply(WalRecord::DeleteEdge {
            edge_type: edge_type.into(),
            src_key: src_key.into(),
            dst_key: dst_key.into(),
        })?;
        Ok(true)
    }

    /// Validate and WAL-log a new rule, then backfill derived edges inside apply.
    /// Validation and duplicate-name check run before logging so invalid rules
    /// never enter the WAL.
    pub fn create_rule(&mut self, def: RuleDef) -> Result<()> {
        def.validate()
            .map_err(|e| GraphError::RuleInvalid { detail: e })?;
        if self.engine.rules().any(|r| r.name == def.name) {
            return Err(GraphError::RuleInvalid {
                detail: format!("rule {:?} already exists", def.name),
            });
        }
        let def_bytes = bincode::serialize(&def).map_err(|e| GraphError::Corrupt {
            detail: format!("serialize rule: {e}"),
        })?;
        self.log_then_apply(WalRecord::CreateRule { def_bytes })
    }

    /// WAL-log rule deletion. Returns RuleNotFound if the rule does not exist.
    pub fn delete_rule(&mut self, name: &str) -> Result<()> {
        if !self.engine.rules().any(|r| r.name == name) {
            return Err(GraphError::RuleNotFound { name: name.into() });
        }
        self.log_then_apply(WalRecord::DeleteRule { name: name.into() })
    }

    /// Return a snapshot of all registered rules.
    pub fn rules(&self) -> Vec<RuleDef> {
        self.engine.rules().cloned().collect()
    }

    /// Recompute a rule's derived edges from scratch (repair tool; NOT WAL-logged).
    pub fn rebuild_rule(&mut self, name: &str) -> Result<()> {
        let mut eng = std::mem::take(&mut self.engine);
        let result = {
            let mut gm = make_graph_mut(
                &self.ids,
                &mut self.syms,
                &self.labels,
                &self.props,
                &mut self.topo,
                &mut self.edge_props,
            );
            eng.rebuild(name, &mut gm)
        };
        self.engine = eng;
        result.map_err(|_| GraphError::RuleNotFound { name: name.into() })
    }

    pub fn get_prop(&self, key: &str, field: &str) -> Option<&Value> {
        self.props.get(self.ids.get(key)?, field)
    }

    pub fn has_node(&self, key: &str) -> bool {
        self.ids.get(key).is_some()
    }

    fn view(&self) -> GraphView<'_> {
        GraphView {
            ids: &self.ids,
            syms: &self.syms,
            labels: &self.labels,
            props: &self.props,
            topo: &self.topo,
            edge_props: &self.edge_props,
        }
    }

    pub fn node_ref(&self, key: &str) -> Option<NodeRef<'_, F>> {
        let id = self.ids.get(key)?;
        Some(NodeRef { db: self, id })
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
            .filter(|&id| eval_filter(filter, &|field| view.prop(id, field).cloned()))
            .map(|id| NodeRef { db: self, id })
            .collect()
    }

    /// Lex → parse → plan → execute `cypher` over a read-only view.
    /// Every pipeline `Err(String)` becomes `GraphError::QueryError`.
    pub fn query(&self, cypher: &str, params: &BTreeMap<String, Value>) -> Result<ResultSet> {
        let tokens = lex(cypher).map_err(|e| GraphError::QueryError { detail: e })?;
        let ast = parse(&tokens).map_err(|e| GraphError::QueryError { detail: e })?;
        let ops = plan(&ast).map_err(|e| GraphError::QueryError { detail: e })?;
        execute(&self.view(), &ops, &Params(params))
            .map_err(|e| GraphError::QueryError { detail: e })
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

        for (rule_name, prov_set) in self.engine.provenance() {
            let Some(rule_def) = self.engine.rules().find(|r| &r.name == rule_name) else {
                continue;
            };
            for &(etype, src, dst) in prov_set {
                if !((src == id_a && dst == id_b) || (src == id_b && dst == id_a)) {
                    continue;
                }
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
                    rule: rule_name.clone(),
                    edge_type,
                    src_key,
                    dst_key,
                    weight,
                });
            }
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
        self.topo
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

    pub fn edge_count(&self) -> u64 {
        self.topo.edge_count()
    }

    /// Test-support: total bytes appended (SimFs only usage).
    pub fn fs_total_appended(&self) -> usize
    where
        F: FsIntrospect,
    {
        self.fs.total_appended()
    }

    /// Consume the db, returning its fs (for crash simulation).
    pub fn into_fs(self) -> F {
        self.fs
    }

    pub fn snapshot(&mut self) -> Result<()> {
        let (rule_defs_typed, provenance) = self.engine.to_persist();
        let rule_defs = rule_defs_typed
            .iter()
            .map(|r| bincode::serialize(r).expect("RuleDef serialize cannot fail"))
            .collect();
        let state = core_storage::snapshot::SnapshotState {
            ids: self.ids.clone(),
            syms: self.syms.clone(),
            topo: self.topo.clone(),
            props: self.props.clone(),
            labels: self.labels.clone(),
            edge_props: self.edge_props.clone(),
            rule_defs,
            provenance,
        };
        self.fs
            .write_atomic(FileId::Snapshot, &core_storage::snapshot::encode(&state))?;
        self.fs.write_atomic(FileId::Wal, b"")?; // wal tail now starts empty
        Ok(())
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

    pub fn prop(&self, field: &str) -> Option<&Value> {
        self.db.props.get(self.id, field)
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
