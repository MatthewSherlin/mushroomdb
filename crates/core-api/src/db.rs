use core_rules::{GraphMut, RuleDef, RuleEngine};
use core_storage::fs::{FileId, Fs, FsIntrospect, RealFs};
use core_storage::wal::{decode_all, encode_record, WalRecord};
use core_storage::{ColumnStore, Direction, EdgeProps, GraphError, IdMap, Interner, Result, Topology, Value};

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
    GraphMut { ids, syms, labels, props, topo, edge_props }
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
            db.engine.reindex_all(&db.ids, &db.syms, &db.labels, &db.props);
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
                    let mut gm = make_graph_mut(&self.ids, &mut self.syms, &self.labels, &self.props, &mut self.topo, &mut self.edge_props);
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
                    let mut gm = make_graph_mut(&self.ids, &mut self.syms, &self.labels, &self.props, &mut self.topo, &mut self.edge_props);
                    eng.on_node_changed(id, Some((field, old_value)), &mut gm);
                }
                self.engine = eng;
            }
            WalRecord::CreateRule { def_bytes } => {
                let def: RuleDef = bincode::deserialize(def_bytes).map_err(|e| {
                    GraphError::Corrupt {
                        detail: format!("CreateRule def_bytes deserialize failed: {e}"),
                    }
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
                    let mut gm = make_graph_mut(&self.ids, &mut self.syms, &self.labels, &self.props, &mut self.topo, &mut self.edge_props);
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
                    let mut gm = make_graph_mut(&self.ids, &mut self.syms, &self.labels, &self.props, &mut self.topo, &mut self.edge_props);
                    eng.delete_rule(name, &mut gm)
                };
                self.engine = eng;
                result.map_err(|_| GraphError::RuleNotFound { name: name.clone() })?;
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
            let mut gm = make_graph_mut(&self.ids, &mut self.syms, &self.labels, &self.props, &mut self.topo, &mut self.edge_props);
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
