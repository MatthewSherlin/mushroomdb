use crate::def::{evaluate, NodeView, Predicate, RuleDef};
use crate::index::{candidate_spec, CandidateSpec, RuleIndex};
use core_storage::{ColumnStore, EdgeProps, IdMap, Interner, Topology, Value};
use std::collections::{BTreeMap, BTreeSet};

/// Borrowed mutable view of graph state the engine writes derived edges into.
pub struct GraphMut<'a> {
    pub ids: &'a IdMap,
    pub syms: &'a mut Interner,
    pub labels: &'a [u32],
    pub props: &'a ColumnStore,
    pub topo: &'a mut Topology,
    pub edge_props: &'a mut EdgeProps,
}

/// Used when `RuleDef.max_edges` is `None`.
pub const DEFAULT_MAX_EDGES: u64 = 1_000_000;

#[derive(Debug, Default)]
pub struct RuleEngine {
    rules: BTreeMap<String, RuleDef>,
    indexes: BTreeMap<String, RuleIndex>,
    provenance: BTreeMap<String, BTreeSet<(u32, u32, u32)>>,
    owned: BTreeSet<(u32, u32, u32)>,
    tripped: BTreeMap<String, bool>,
    fires: BTreeMap<String, u64>,
}

// ---------------------------------------------------------------------------
// Private helpers (free functions, not methods, to avoid whole-struct borrows)
// ---------------------------------------------------------------------------

/// For KeyMatch, src side is indexed as Scalar (FK field value → node bucket).
/// For all other predicates, returns the same as candidate_spec.
/// Index maintenance uses this for the src side; candidate_spec for the dst side.
fn src_lookup_spec(p: &Predicate) -> CandidateSpec<'_> {
    match p {
        Predicate::KeyMatch { field } => CandidateSpec::Scalar { field },
        Predicate::All(parts) => {
            debug_assert!(!parts.is_empty(), "validated predicate required");
            src_lookup_spec(&parts[0])
        }
        other => candidate_spec(other),
    }
}

/// Returns true if the predicate (or its leading All part) is KeyMatch.
fn predicate_is_keymatch(p: &Predicate) -> bool {
    match p {
        Predicate::KeyMatch { .. } => true,
        Predicate::All(parts) => !parts.is_empty() && predicate_is_keymatch(&parts[0]),
        _ => false,
    }
}

/// Extract the FK field name from a KeyMatch (or All-leading-KeyMatch) predicate.
fn keymatch_field(p: &Predicate) -> Option<&str> {
    match p {
        Predicate::KeyMatch { field } => Some(field),
        Predicate::All(parts) => parts.first().and_then(keymatch_field),
        _ => None,
    }
}

/// Compute the set of desired (src, dst) → score edges involving node `n` on
/// the given side.  Returns an empty map if `n`'s label doesn't match the rule.
fn compute_desired(
    def: &RuleDef,
    index: &RuleIndex,
    n: u32,
    on_src_side: bool,
    g: &GraphMut<'_>,
) -> BTreeMap<(u32, u32), f64> {
    let (my_label, other_label) = if on_src_side {
        (&def.src_label, &def.dst_label)
    } else {
        (&def.dst_label, &def.src_label)
    };

    let Some(my_sym) = g.syms.get(my_label) else {
        return BTreeMap::new();
    };
    if g.labels.get(n as usize).copied() != Some(my_sym) {
        return BTreeMap::new();
    }
    let other_sym = g.syms.get(other_label);

    let n_key = match g.ids.key_of(n) {
        Some(k) => k,
        None => return BTreeMap::new(),
    };
    let n_get = |f: &str| g.props.get(n, f).cloned();

    let spec = candidate_spec(&def.predicate);
    let candidates: BTreeSet<u32> = if on_src_side {
        match &spec {
            CandidateSpec::ByKey => {
                // KeyMatch src→dst: look up the dst node directly by FK field value.
                let field =
                    keymatch_field(&def.predicate).expect("ByKey always comes from KeyMatch");
                match n_get(field) {
                    Some(Value::Str(ref target_key)) => match g.ids.get(target_key) {
                        Some(dst_id) => std::iter::once(dst_id).collect(),
                        None => BTreeSet::new(),
                    },
                    _ => BTreeSet::new(),
                }
            }
            _ => index.dst_side.candidates(&spec, &n_get),
        }
    } else {
        // n is dst: probe src_side to find src candidates.
        let src_spec = src_lookup_spec(&def.predicate);
        if predicate_is_keymatch(&def.predicate) {
            // Synthetic getter: returns n's key for the FK field so we find
            // src nodes whose FK value points to n.
            let key_getter = |_: &str| Some(Value::Str(n_key.to_string()));
            index.src_side.candidates(&src_spec, &key_getter)
        } else {
            index.src_side.candidates(&src_spec, &n_get)
        }
    };

    let mut out = BTreeMap::new();
    for m in candidates {
        if m == n {
            continue; // never self-edges
        }
        if g.labels.get(m as usize).copied() != other_sym {
            continue; // label filter
        }
        let m_key = match g.ids.key_of(m) {
            Some(k) => k,
            None => continue,
        };
        let m_get = |f: &str| g.props.get(m, f).cloned();
        let (s_view, d_view, s_id, d_id) = if on_src_side {
            (
                NodeView {
                    key: n_key,
                    props: &n_get,
                },
                NodeView {
                    key: m_key,
                    props: &m_get,
                },
                n,
                m,
            )
        } else {
            (
                NodeView {
                    key: m_key,
                    props: &m_get,
                },
                NodeView {
                    key: n_key,
                    props: &n_get,
                },
                m,
                n,
            )
        };
        if let Some(score) = evaluate(&def.predicate, &s_view, &d_view) {
            out.insert((s_id, d_id), score);
        }
    }
    out
}

fn edge_budget(def: &RuleDef) -> u64 {
    def.max_edges.unwrap_or(DEFAULT_MAX_EDGES)
}

/// Diff-apply `desired` against provenance. `retract_touching = Some(n)` only
/// retracts triples that involve `n` (incremental fire). `None` retracts any
/// current provenance triple not in `desired` (backfill / rebuild).
///
/// `tripped` is a one-way latch: once set, no new provenance edges are added
/// (gate on the flag itself, not `prov.len()`), even if retracts have brought
/// the set below budget. Retracts and weight refreshes on already-owned edges
/// still run. Crossing the budget on a not-yet-tripped rule sets the latch
/// and skips that add and every later add in this call. Never an error.
/// First-N (pre-trip) is BTree `(src, dst)` order of `desired` after retract.
fn apply_desired(
    def: &RuleDef,
    desired: BTreeMap<(u32, u32), f64>,
    retract_touching: Option<u32>,
    prov: &mut BTreeSet<(u32, u32, u32)>,
    owned: &mut BTreeSet<(u32, u32, u32)>,
    tripped: &mut bool,
    g: &mut GraphMut<'_>,
) {
    let budget = edge_budget(def);
    let et = g.syms.intern(&def.edge_type);

    let current: Vec<(u32, u32, u32)> = prov
        .iter()
        .filter(|(t, s, d)| {
            *t == et
                && match retract_touching {
                    None => true,
                    Some(n) => *s == n || *d == n,
                }
        })
        .copied()
        .collect();

    for (t, s, d) in current {
        if !desired.contains_key(&(s, d)) {
            g.topo.remove_edge(t, s, d);
            g.edge_props.remove_edge(t, s, d);
            prov.remove(&(t, s, d));
            owned.remove(&(t, s, d));
        }
    }

    for ((s, d), score) in desired {
        let triple = (et, s, d);
        let already = prov.contains(&triple);
        if !already {
            if *tripped || prov.len() as u64 >= budget {
                *tripped = true;
                continue;
            }
            let newly = g.topo.add_edge(et, s, d);
            if newly {
                prov.insert(triple);
                owned.insert(triple);
            }
        }
        // Only set weight_prop on edges this rule owns (newly added now, or
        // already in provenance). Pre-existing user edges are never owned, so
        // writing a weight to them would leave a ghost property after deletion.
        let is_owned_here = already || prov.contains(&triple);
        if is_owned_here {
            if let Some(p) = &def.weight_prop {
                g.edge_props.set(et, s, d, p, Value::Float(score));
            }
        }
    }
}

/// Union of `compute_desired(..., as_src)` over every live src-label node.
/// BTree iteration of the result is the engine's deterministic first-N order
/// for backfill and rebuild.
fn compute_full_desired(
    def: &RuleDef,
    index: &RuleIndex,
    g: &GraphMut<'_>,
) -> BTreeMap<(u32, u32), f64> {
    let mut desired = BTreeMap::new();
    let src_sym = g.syms.get(&def.src_label);
    for id in 0..g.ids.len() as u32 {
        let label_sym = match g.labels.get(id as usize).copied() {
            Some(s) if s != u32::MAX => s,
            _ => continue,
        };
        if src_sym == Some(label_sym) {
            desired.extend(compute_desired(def, index, id, true, g));
        }
    }
    desired
}

/// Increment `fires` once per live node whose label matches either side.
/// Backfill / rebuild counting: one tick per participating node evaluated.
fn bump_fires_for_participants(def: &RuleDef, g: &GraphMut<'_>, fires: &mut u64) {
    let src_sym = g.syms.get(&def.src_label);
    let dst_sym = g.syms.get(&def.dst_label);
    for id in 0..g.ids.len() as u32 {
        let label_sym = match g.labels.get(id as usize).copied() {
            Some(s) if s != u32::MAX => s,
            _ => continue,
        };
        if src_sym == Some(label_sym) || dst_sym == Some(label_sym) {
            *fires += 1;
        }
    }
}

/// Insert node `id` into the rule's src/dst indexes where its label matches.
fn index_node_for_rule(
    id: u32,
    label_sym: u32,
    def: &RuleDef,
    index: &mut RuleIndex,
    syms: &Interner,
    props: &ColumnStore,
) {
    let get = |f: &str| props.get(id, f).cloned();
    if syms.get(&def.src_label) == Some(label_sym) {
        let spec = src_lookup_spec(&def.predicate);
        index.src_side.insert(&spec, id, &get);
    }
    if syms.get(&def.dst_label) == Some(label_sym) {
        let spec = candidate_spec(&def.predicate);
        index.dst_side.insert(&spec, id, &get);
    }
}

// ---------------------------------------------------------------------------
// RuleEngine
// ---------------------------------------------------------------------------

impl RuleEngine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn rules(&self) -> impl Iterator<Item = &RuleDef> {
        self.rules.values()
    }

    pub fn is_owned(&self, etype: u32, src: u32, dst: u32) -> bool {
        self.owned.contains(&(etype, src, dst))
    }

    /// Read-only view of the provenance map: rule name → set of (etype_sym, src, dst).
    pub fn provenance(&self) -> &BTreeMap<String, BTreeSet<(u32, u32, u32)>> {
        &self.provenance
    }

    /// One-way latch: `true` after a budget breach until [`Self::rebuild`]
    /// is the only exit (and only if the full desired set then fits).
    pub fn is_tripped(&self, name: &str) -> bool {
        self.tripped.get(name).copied().unwrap_or(false)
    }

    /// Evaluations of this rule: one tick per `on_node_changed` fire, and
    /// one tick per participating node on backfill **and rebuild** (even
    /// when rebuild is a provenance no-op).
    pub fn fire_count(&self, name: &str) -> u64 {
        self.fires.get(name).copied().unwrap_or(0)
    }

    /// Snapshot support: definitions + provenance + tripped/fires. Indexes
    /// are NOT included (they are rebuilt on open via `reindex_all`).
    #[allow(clippy::type_complexity)]
    pub fn to_persist(
        &self,
    ) -> (
        Vec<RuleDef>,
        BTreeMap<String, BTreeSet<(u32, u32, u32)>>,
        BTreeMap<String, bool>,
        BTreeMap<String, u64>,
    ) {
        (
            self.rules.values().cloned().collect(),
            self.provenance.clone(),
            self.tripped.clone(),
            self.fires.clone(),
        )
    }

    /// Reconstruct engine from a snapshot.  Caller must call `reindex_all` after.
    pub fn from_persist(
        rules: Vec<RuleDef>,
        prov: BTreeMap<String, BTreeSet<(u32, u32, u32)>>,
        tripped: BTreeMap<String, bool>,
        fires: BTreeMap<String, u64>,
    ) -> Self {
        let mut owned = BTreeSet::new();
        for set in prov.values() {
            owned.extend(set.iter().copied());
        }
        let indexes = rules
            .iter()
            .map(|r| (r.name.clone(), RuleIndex::default()))
            .collect();
        let rules: BTreeMap<String, RuleDef> =
            rules.into_iter().map(|r| (r.name.clone(), r)).collect();
        // Fill any missing keys so live rules always have entries.
        let mut tripped = tripped;
        let mut fires = fires;
        for name in rules.keys() {
            tripped.entry(name.clone()).or_insert(false);
            fires.entry(name.clone()).or_insert(0);
        }
        Self {
            rules,
            indexes,
            provenance: prov,
            owned,
            tripped,
            fires,
        }
    }

    /// Rebuild all candidate indexes by scanning every node.  Call on open.
    pub fn reindex_all(
        &mut self,
        ids: &IdMap,
        syms: &Interner,
        labels: &[u32],
        props: &ColumnStore,
    ) {
        for idx in self.indexes.values_mut() {
            *idx = RuleIndex::default();
        }
        // Collect rule names once outside the per-node loop to avoid repeated
        // allocation and to satisfy the borrow checker without cloning inside.
        let rule_names: Vec<String> = self.rules.keys().cloned().collect();
        for id in 0..ids.len() as u32 {
            let label_sym = match labels.get(id as usize).copied() {
                Some(s) if s != u32::MAX => s,
                _ => continue,
            };
            for name in &rule_names {
                let def = self.rules[name].clone();
                let idx = self.indexes.get_mut(name).unwrap();
                index_node_for_rule(id, label_sym, &def, idx, syms, props);
            }
        }
    }

    /// Register a rule and backfill existing nodes.
    /// Returns Err on failed validate() or duplicate name.
    pub fn create_rule(&mut self, def: RuleDef, g: &mut GraphMut<'_>) -> Result<(), String> {
        def.validate()?;
        if self.rules.contains_key(&def.name) {
            return Err(format!("rule {:?} already exists", def.name));
        }
        let name = def.name.clone();
        self.rules.insert(name.clone(), def);
        self.indexes.insert(name.clone(), RuleIndex::default());
        self.provenance.entry(name.clone()).or_default();
        self.tripped.insert(name.clone(), false);
        self.fires.insert(name.clone(), 0);

        // Phase 1: index all existing nodes for this rule.
        let n_total = g.ids.len() as u32;
        let def = self.rules[&name].clone();
        for id in 0..n_total {
            let label_sym = match g.labels.get(id as usize).copied() {
                Some(s) if s != u32::MAX => s,
                _ => continue,
            };
            let idx = self.indexes.get_mut(&name).unwrap();
            index_node_for_rule(id, label_sym, &def, idx, g.syms, g.props);
        }

        // Phase 2: full desired set in deterministic BTree order, cap at budget.
        let desired = compute_full_desired(&def, &self.indexes[&name], g);
        let prov = self.provenance.get_mut(&name).unwrap();
        let tripped = self.tripped.get_mut(&name).unwrap();
        apply_desired(&def, desired, None, prov, &mut self.owned, tripped, g);
        // Fires: one tick per participating node evaluated (same unit as
        // on_node_changed). Empty-graph create_rule therefore leaves fires=0.
        let fires = self.fires.get_mut(&name).unwrap();
        bump_fires_for_participants(&def, g, fires);

        Ok(())
    }

    /// Remove the rule and exactly its owned edges.  Returns Err if unknown.
    pub fn delete_rule(&mut self, name: &str, g: &mut GraphMut<'_>) -> Result<(), String> {
        if !self.rules.contains_key(name) {
            return Err(format!("rule {:?} not found", name));
        }
        let def = self.rules.remove(name).unwrap();
        self.indexes.remove(name);
        self.tripped.remove(name);
        self.fires.remove(name);
        let prov = self.provenance.remove(name).unwrap_or_default();
        // intern so the symbol exists; edge_type was already interned at create time.
        let _et = g.syms.intern(&def.edge_type);
        for (t, s, d) in prov {
            g.topo.remove_edge(t, s, d);
            g.edge_props.remove_edge(t, s, d);
            self.owned.remove(&(t, s, d));
        }
        // Surviving rules that share the same edge_type may derive edges that
        // were previously blocked (add_edge returned false because the deleted
        // rule already owned them, so their provenance never recorded them).
        // Rebuilding each such rule lets it claim those edges now that the
        // deleted rule's entries have been removed from the topology.
        let same_etype_survivors: Vec<String> = self
            .rules
            .values()
            .filter(|r| r.edge_type == def.edge_type)
            .map(|r| r.name.clone())
            .collect();
        for survivor in same_etype_survivors {
            // rebuild returns Err only for unknown rules; survivor is live.
            let _ = self.rebuild(&survivor, g);
        }
        Ok(())
    }

    /// Called when node `n` is inserted (changed=None) or a field is updated.
    /// - None: all rules where n's label matches either side fire; index gains n.
    /// - Some((field, old_value)): only rules watching `field` fire; index is
    ///   updated using old_value for removal so stale buckets are cleaned.
    pub fn on_node_changed(
        &mut self,
        n: u32,
        changed: Option<(&str, Option<Value>)>,
        g: &mut GraphMut<'_>,
    ) {
        let n_label = g.labels.get(n as usize).copied();
        let rule_names: Vec<String> = self.rules.keys().cloned().collect();

        for rule_name in rule_names {
            let def = self.rules[&rule_name].clone();
            let src_sym = g.syms.get(&def.src_label);
            let dst_sym = g.syms.get(&def.dst_label);
            let as_src = src_sym.is_some() && n_label == src_sym;
            let as_dst = dst_sym.is_some() && n_label == dst_sym;

            let fires = match changed {
                None => as_src || as_dst,
                Some((field, _)) => def.watched_fields().contains(field) && (as_src || as_dst),
            };
            if !fires {
                continue;
            }
            *self.fires.entry(rule_name.clone()).or_default() += 1;

            // --- Index maintenance ---
            if let Some((field, ref old_val)) = changed {
                // Remove using the OLD value so stale buckets are cleared.
                let old_val_cloned = old_val.clone();
                let old_getter = |f: &str| {
                    if f == field {
                        old_val_cloned.clone()
                    } else {
                        g.props.get(n, f).cloned()
                    }
                };
                let idx = self.indexes.get_mut(&rule_name).unwrap();
                if as_src {
                    let spec = src_lookup_spec(&def.predicate);
                    idx.src_side.remove(&spec, n, &old_getter);
                }
                if as_dst {
                    let spec = candidate_spec(&def.predicate);
                    idx.dst_side.remove(&spec, n, &old_getter);
                }
            }

            // Insert with current props (idempotent on new-node path).
            {
                let cur_getter = |f: &str| g.props.get(n, f).cloned();
                let idx = self.indexes.get_mut(&rule_name).unwrap();
                if as_src {
                    let spec = src_lookup_spec(&def.predicate);
                    idx.src_side.insert(&spec, n, &cur_getter);
                }
                if as_dst {
                    let spec = candidate_spec(&def.predicate);
                    idx.dst_side.insert(&spec, n, &cur_getter);
                }
            }

            // --- Desired set + diff-apply ---
            let mut desired = BTreeMap::new();
            if as_src {
                desired.extend(compute_desired(&def, &self.indexes[&rule_name], n, true, g));
            }
            if as_dst {
                desired.extend(compute_desired(
                    &def,
                    &self.indexes[&rule_name],
                    n,
                    false,
                    g,
                ));
            }
            let prov = self.provenance.entry(rule_name.clone()).or_default();
            let tripped = self.tripped.entry(rule_name).or_default();
            apply_desired(&def, desired, Some(n), prov, &mut self.owned, tripped, g);
        }
    }

    /// Retract every provenance edge touching `n` across all rules and drop
    /// `n` from every rule index using its *current* props.
    ///
    /// Caller must invoke this while labels/props are still intact (before
    /// tombstone). Rules are walked in BTree name order; touching edges in
    /// BTree triple order. A second call on an already-retracted node is a
    /// no-op (crash-window replay / absent state).
    pub fn on_node_removed(&mut self, n: u32, g: &mut GraphMut<'_>) {
        // O(R x P) provenance scan: acceptable pre-alpha; at scale, index
        // provenance by node (tracked for the performance plan).
        let n_label = g.labels.get(n as usize).copied();
        let rule_names: Vec<String> = self.rules.keys().cloned().collect();

        for rule_name in rule_names {
            let def = self.rules[&rule_name].clone();
            let src_sym = g.syms.get(&def.src_label);
            let dst_sym = g.syms.get(&def.dst_label);
            let as_src = src_sym.is_some() && n_label == src_sym;
            let as_dst = dst_sym.is_some() && n_label == dst_sym;

            {
                let cur_getter = |f: &str| g.props.get(n, f).cloned();
                let idx = self.indexes.get_mut(&rule_name).unwrap();
                if as_src {
                    let spec = src_lookup_spec(&def.predicate);
                    idx.src_side.remove(&spec, n, &cur_getter);
                }
                if as_dst {
                    let spec = candidate_spec(&def.predicate);
                    idx.dst_side.remove(&spec, n, &cur_getter);
                }
            }

            let Some(prov) = self.provenance.get_mut(&rule_name) else {
                continue;
            };
            let touching: Vec<(u32, u32, u32)> = prov
                .iter()
                .filter(|(_, s, d)| *s == n || *d == n)
                .copied()
                .collect();
            for (t, s, d) in touching {
                g.topo.remove_edge(t, s, d);
                g.edge_props.remove_edge(t, s, d);
                prov.remove(&(t, s, d));
                self.owned.remove(&(t, s, d));
            }
        }
    }

    /// Recompute one rule from scratch. Only exit from the tripped latch.
    ///
    /// If the full desired set fits in the budget, it is applied completely
    /// and `tripped` is cleared. If it still exceeds the budget, existing
    /// provenance is left completely untouched and `tripped` stays true
    /// (rebuild-is-noop for at/over-cap rules). Always counts as a fire
    /// evaluation per participating node. Returns Err if unknown.
    pub fn rebuild(&mut self, name: &str, g: &mut GraphMut<'_>) -> Result<(), String> {
        if !self.rules.contains_key(name) {
            return Err(format!("rule {:?} not found", name));
        }
        let def = self.rules[name].clone();

        // Reindex this rule from scratch (indexes only).
        *self.indexes.get_mut(name).unwrap() = RuleIndex::default();
        let n_total = g.ids.len() as u32;
        for id in 0..n_total {
            let label_sym = match g.labels.get(id as usize).copied() {
                Some(s) if s != u32::MAX => s,
                _ => continue,
            };
            let idx = self.indexes.get_mut(name).unwrap();
            index_node_for_rule(id, label_sym, &def, idx, g.syms, g.props);
        }

        let desired = compute_full_desired(&def, &self.indexes[name], g);
        if desired.len() as u64 > edge_budget(&def) {
            // Still over cap: true no-op on provenance; latch stays set.
            self.tripped.insert(name.to_string(), true);
        } else {
            self.tripped.insert(name.to_string(), false);
            let prov = self.provenance.get_mut(name).unwrap();
            let tripped = self.tripped.get_mut(name).unwrap();
            apply_desired(&def, desired, None, prov, &mut self.owned, tripped, g);
        }
        let fires = self.fires.entry(name.to_string()).or_default();
        bump_fires_for_participants(&def, g, fires);

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::def::{Predicate, RuleDef};
    use core_storage::{ColumnStore, Direction, EdgeProps, IdMap, Interner, Topology, Value};

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
        fn g(&mut self) -> GraphMut<'_> {
            GraphMut {
                ids: &self.ids,
                syms: &mut self.syms,
                labels: &self.labels,
                props: &self.props,
                topo: &mut self.topo,
                edge_props: &mut self.eprops,
            }
        }
    }

    fn tags(items: &[&str]) -> Value {
        Value::List(items.iter().map(|s| Value::Str((*s).into())).collect())
    }

    fn overlap_rule() -> RuleDef {
        RuleDef {
            name: "rel".into(),
            src_label: "A".into(),
            dst_label: "A".into(),
            predicate: Predicate::Overlap {
                field: "tags".into(),
                min: 0.4,
            },
            edge_type: "REL".into(),
            weight_prop: Some("score".into()),
            max_edges: None,
        }
    }

    #[test]
    fn backfill_creates_edges_with_scores_and_delete_removes_exactly_them() {
        let mut fx = Fx::new();
        let a = fx.add("A", "a", vec![("tags", tags(&["x", "y"]))]);
        let b = fx.add("A", "b", vec![("tags", tags(&["x", "y"]))]);
        let _c = fx.add("A", "c", vec![("tags", tags(&["q"]))]);
        // pre-existing user edge with same type: must survive rule delete
        let et = fx.syms.intern("REL");
        fx.topo.add_edge(et, a, b);
        let mut eng = RuleEngine::new();
        let mut g = fx.g();
        eng.create_rule(overlap_rule(), &mut g).unwrap();
        // a↔b jaccard 1.0 both directions; user edge a→b pre-existed so only b→a is owned
        assert!(g.topo.neighbors(et, Direction::Out, b).contains(&a));
        assert_eq!(
            g.edge_props.get(et, b, a, "score"),
            Some(&Value::Float(1.0))
        );
        assert!(!eng.is_owned(et, a, b));
        assert!(eng.is_owned(et, b, a));
        eng.delete_rule("rel", &mut g).unwrap();
        assert!(g.topo.neighbors(et, Direction::Out, a).contains(&b)); // user edge kept
        assert!(!g.topo.neighbors(et, Direction::Out, b).contains(&a)); // derived removed
        assert_eq!(g.edge_props.get(et, b, a, "score"), None);
    }

    #[test]
    fn incremental_update_adds_and_removes_edges() {
        let mut fx = Fx::new();
        let a = fx.add("A", "a", vec![("tags", tags(&["x", "y"]))]);
        let b = fx.add("A", "b", vec![("tags", tags(&["y", "z"]))]);
        let et = fx.syms.intern("REL");
        let mut eng = RuleEngine::new();
        {
            let mut g = fx.g();
            eng.create_rule(overlap_rule(), &mut g).unwrap(); // jaccard 1/3 < 0.4 → no edges
            assert_eq!(g.topo.edge_count(), 0);
        }
        // b's tags change to overlap strongly
        let old = fx.props.get(b, "tags").cloned();
        fx.props.set(b, "tags", tags(&["x", "y"]));
        {
            let mut g = fx.g();
            eng.on_node_changed(b, Some(("tags", old)), &mut g);
            assert!(g.topo.neighbors(et, Direction::Out, a).contains(&b));
            assert!(g.topo.neighbors(et, Direction::Out, b).contains(&a));
        }
        // and change away again → edges retract
        let old = fx.props.get(b, "tags").cloned();
        fx.props.set(b, "tags", tags(&["qqq"]));
        let mut g = fx.g();
        eng.on_node_changed(b, Some(("tags", old)), &mut g);
        assert_eq!(g.topo.edge_count(), 0);
        assert_eq!(g.edge_props.get(et, a, b, "score"), None);
    }

    #[test]
    fn key_match_new_node_links_and_rebuild_is_noop() {
        let mut fx = Fx::new();
        fx.add("C", "c1", vec![]);
        let mut eng = RuleEngine::new();
        {
            let mut g = fx.g();
            eng.create_rule(
                RuleDef {
                    name: "fk".into(),
                    src_label: "T".into(),
                    dst_label: "C".into(),
                    predicate: Predicate::KeyMatch {
                        field: "cid".into(),
                    },
                    edge_type: "AT".into(),
                    weight_prop: None,
                    max_edges: None,
                },
                &mut g,
            )
            .unwrap();
        }
        let t = fx.add("T", "t1", vec![("cid", Value::Str("c1".into()))]);
        let (at, c1, count_before) = {
            let mut g = fx.g();
            eng.on_node_changed(t, None, &mut g);
            let at = g.syms.get("AT").unwrap();
            let c1 = g.ids.get("c1").unwrap();
            assert!(g.topo.neighbors(at, Direction::Out, t).contains(&c1));
            (at, c1, g.topo.edge_count())
        };
        let mut g = fx.g();
        eng.rebuild("fk", &mut g).unwrap();
        assert_eq!(g.topo.edge_count(), count_before); // rebuild is a no-op on consistent state
        assert!(g.topo.neighbors(at, Direction::Out, t).contains(&c1));
    }

    #[test]
    fn score_refresh_on_persisting_owned_edge() {
        // Pins: weight set unconditionally even when add_edge returns false (edge persists).
        // jaccard({x,y,z},{x,y,q}) = |{x,y}|/|{x,y,z,q}| = 2/4 = 0.5 ≥ 0.2 → edges both ways.
        let mut fx = Fx::new();
        let a = fx.add("A", "a", vec![("tags", tags(&["x", "y", "z"]))]);
        let b = fx.add("A", "b", vec![("tags", tags(&["x", "y", "q"]))]);
        let et = fx.syms.intern("SIM");
        let mut eng = RuleEngine::new();
        {
            let mut g = fx.g();
            eng.create_rule(
                RuleDef {
                    name: "sim".into(),
                    src_label: "A".into(),
                    dst_label: "A".into(),
                    predicate: Predicate::Overlap {
                        field: "tags".into(),
                        min: 0.2,
                    },
                    edge_type: "SIM".into(),
                    weight_prop: Some("score".into()),
                    max_edges: None,
                },
                &mut g,
            )
            .unwrap();
            // Both directions present and owned with score ≈ 0.5.
            assert!(g.topo.neighbors(et, Direction::Out, a).contains(&b));
            assert!(g.topo.neighbors(et, Direction::Out, b).contains(&a));
            assert!(eng.is_owned(et, a, b) || eng.is_owned(et, b, a));
            let check = |v: Option<&Value>| {
                if let Some(Value::Float(f)) = v {
                    assert!(
                        (f - 0.5).abs() < 1e-9,
                        "initial score should be 0.5, got {f}"
                    );
                }
            };
            check(g.edge_props.get(et, a, b, "score"));
            check(g.edge_props.get(et, b, a, "score"));
        }
        // Change b's tags to match a exactly → jaccard = 1.0.
        let old = fx.props.get(b, "tags").cloned();
        fx.props.set(b, "tags", tags(&["x", "y", "z"]));
        {
            let mut g = fx.g();
            eng.on_node_changed(b, Some(("tags", old)), &mut g);
            // Both directions still present.
            assert!(g.topo.neighbors(et, Direction::Out, a).contains(&b));
            assert!(g.topo.neighbors(et, Direction::Out, b).contains(&a));
            // Scores must now be 1.0 on both directions.
            assert_eq!(
                g.edge_props.get(et, a, b, "score"),
                Some(&Value::Float(1.0)),
                "score on a→b must refresh to 1.0"
            );
            assert_eq!(
                g.edge_props.get(et, b, a, "score"),
                Some(&Value::Float(1.0)),
                "score on b→a must refresh to 1.0"
            );
        }
    }

    #[test]
    fn dst_side_keymatch_links_when_c_node_inserted_after_t() {
        // Exercises the synthetic key-probe on src_side Scalar index (dst-side KeyMatch path).
        let mut fx = Fx::new();
        // Insert T node first with cid="c9" — no C node yet → no edge.
        let t = fx.add("T", "t1", vec![("cid", Value::Str("c9".into()))]);
        let mut eng = RuleEngine::new();
        {
            let mut g = fx.g();
            eng.create_rule(
                RuleDef {
                    name: "fk".into(),
                    src_label: "T".into(),
                    dst_label: "C".into(),
                    predicate: Predicate::KeyMatch {
                        field: "cid".into(),
                    },
                    edge_type: "AT".into(),
                    weight_prop: None,
                    max_edges: None,
                },
                &mut g,
            )
            .unwrap();
            // No C node → no edge.
            let at = g.syms.intern("AT");
            assert_eq!(g.topo.edge_count(), 0, "no C node yet → no edge");
            // t is indexed in src_side with Scalar{cid}="c9"
            let _ = at;
        }
        // Now insert C node "c9" and notify the engine.
        let c9 = fx.add("C", "c9", vec![]);
        {
            let mut g = fx.g();
            eng.on_node_changed(c9, None, &mut g);
            let at = g.syms.get("AT").unwrap();
            // The dst-side path must have probed src_side with key="c9" and found t.
            assert!(
                g.topo.neighbors(at, Direction::Out, t).contains(&c9),
                "T→C edge must appear when C node is inserted"
            );
            assert!(eng.is_owned(at, t, c9));
        }
    }

    #[test]
    fn on_node_removed_retracts_both_sides_and_deindexes() {
        let mut fx = Fx::new();
        let a = fx.add("A", "a", vec![("tags", tags(&["x", "y"]))]);
        let b = fx.add("A", "b", vec![("tags", tags(&["x", "y"]))]);
        let et = fx.syms.intern("REL");
        let mut eng = RuleEngine::new();
        {
            let mut g = fx.g();
            eng.create_rule(overlap_rule(), &mut g).unwrap();
            assert!(g.topo.neighbors(et, Direction::Out, a).contains(&b));
            assert!(g.topo.neighbors(et, Direction::Out, b).contains(&a));
        }
        {
            let mut g = fx.g();
            eng.on_node_removed(a, &mut g);
            assert!(!g.topo.neighbors(et, Direction::Out, a).contains(&b));
            assert!(!g.topo.neighbors(et, Direction::Out, b).contains(&a));
            assert_eq!(g.edge_props.get(et, a, b, "score"), None);
            assert_eq!(g.edge_props.get(et, b, a, "score"), None);
            assert!(!eng.is_owned(et, a, b));
            assert!(!eng.is_owned(et, b, a));
        }
        // Partner re-links to a NEW matching node; de-indexed a is not a candidate.
        let c = fx.add("A", "c", vec![("tags", tags(&["x", "y"]))]);
        {
            let mut g = fx.g();
            eng.on_node_changed(c, None, &mut g);
            assert!(g.topo.neighbors(et, Direction::Out, b).contains(&c));
            assert!(g.topo.neighbors(et, Direction::Out, c).contains(&b));
            assert!(!g.topo.neighbors(et, Direction::Out, c).contains(&a));
            assert!(!g.topo.neighbors(et, Direction::Out, a).contains(&c));
        }
        // Second remove is a no-op (crash-window / already-retracted).
        {
            let mut g = fx.g();
            eng.on_node_removed(a, &mut g);
            assert!(g.topo.neighbors(et, Direction::Out, b).contains(&c));
        }
    }

    #[test]
    fn duplicate_name_and_unknown_delete_error() {
        let mut fx = Fx::new();
        let mut eng = RuleEngine::new();
        let mut g = fx.g();
        eng.create_rule(overlap_rule(), &mut g).unwrap();
        assert!(eng.create_rule(overlap_rule(), &mut g).is_err());
        assert!(eng.delete_rule("nope", &mut g).is_err());
    }

    /// C1: two rules sharing the same edge_type both match a pair of nodes.
    /// During backfill of R2, add_edge returns false for edges R1 already owns,
    /// so R2's provenance lacks them.  Deleting R1 removes those edges from the
    /// topology — but the rebuild-survivors step must then re-run R2 so it claims
    /// them.  Deleting R2 afterward must actually remove the edge.
    #[test]
    fn coowned_edge_type_survives_first_delete_gone_after_second() {
        let mut fx = Fx::new();
        let a = fx.add("A", "a", vec![("tags", tags(&["x", "y"]))]);
        let b = fx.add("A", "b", vec![("tags", tags(&["x", "y"]))]);
        let mut eng = RuleEngine::new();
        {
            let mut g = fx.g();
            // R1: Overlap min=0.1 — derives a↔b (jaccard 1.0 ≥ 0.1).
            eng.create_rule(
                RuleDef {
                    name: "r1".into(),
                    src_label: "A".into(),
                    dst_label: "A".into(),
                    predicate: Predicate::Overlap {
                        field: "tags".into(),
                        min: 0.1,
                    },
                    edge_type: "REL2".into(),
                    weight_prop: None,
                    max_edges: None,
                },
                &mut g,
            )
            .unwrap();
            // R2: same edge_type, Overlap min=0.2 — also derives a↔b.
            eng.create_rule(
                RuleDef {
                    name: "r2".into(),
                    src_label: "A".into(),
                    dst_label: "A".into(),
                    predicate: Predicate::Overlap {
                        field: "tags".into(),
                        min: 0.2,
                    },
                    edge_type: "REL2".into(),
                    weight_prop: None,
                    max_edges: None,
                },
                &mut g,
            )
            .unwrap();

            let et = g.syms.intern("REL2");
            // Both directions must exist (either rule claims them).
            assert!(
                g.topo.neighbors(et, Direction::Out, a).contains(&b),
                "a→b must exist after both rules created"
            );
            assert!(
                g.topo.neighbors(et, Direction::Out, b).contains(&a),
                "b→a must exist after both rules created"
            );

            // Delete R1 — rebuild-survivors re-runs R2 which must reclaim the edges.
            eng.delete_rule("r1", &mut g).unwrap();
            assert!(
                g.topo.neighbors(et, Direction::Out, a).contains(&b),
                "a→b must survive R1 deletion (R2 rebuilds and claims it)"
            );
            assert!(
                g.topo.neighbors(et, Direction::Out, b).contains(&a),
                "b→a must survive R1 deletion (R2 rebuilds and claims it)"
            );
            // R2 now owns both directions.
            assert!(
                eng.is_owned(et, a, b),
                "a→b must be owned by R2 after rebuild"
            );
            assert!(
                eng.is_owned(et, b, a),
                "b→a must be owned by R2 after rebuild"
            );

            // Delete R2 — no survivor left, edges must be gone.
            eng.delete_rule("r2", &mut g).unwrap();
            assert!(
                !g.topo.neighbors(et, Direction::Out, a).contains(&b),
                "a→b must be gone after both rules deleted"
            );
            assert!(
                !g.topo.neighbors(et, Direction::Out, b).contains(&a),
                "b→a must be gone after both rules deleted"
            );
        }
    }

    fn const_eq_rule(max_edges: u64) -> RuleDef {
        RuleDef {
            name: "eq".into(),
            src_label: "N".into(),
            dst_label: "N".into(),
            predicate: Predicate::FieldEqual { field: "k".into() },
            edge_type: "EQ".into(),
            weight_prop: None,
            max_edges: Some(max_edges),
        }
    }

    /// 4 nodes sharing field k="const" want 12 directed FieldEqual edges.
    /// Budget 10 keeps the first 10 in deterministic (src,dst) order and trips.
    /// A fifth insert still applies (no Err). Rebuild re-trips with the same
    /// first-10 set. delete_rule drops tripped/fires.
    #[test]
    fn field_equal_budget_trips_at_exactly_10() {
        let mut fx = Fx::new();
        let mut eng = RuleEngine::new();
        {
            let mut g = fx.g();
            eng.create_rule(const_eq_rule(10), &mut g).unwrap();
        }
        let mut ids = Vec::new();
        for i in 0..4 {
            let id = fx.add(
                "N",
                &format!("n{i}"),
                vec![("k", Value::Str("const".into()))],
            );
            ids.push(id);
            let mut g = fx.g();
            eng.on_node_changed(id, None, &mut g);
        }
        let et = fx.syms.get("EQ").unwrap();
        let mut kept = BTreeSet::new();
        for &s in &ids {
            for &d in fx.topo.neighbors(et, Direction::Out, s) {
                kept.insert((s, d));
            }
        }
        assert_eq!(kept.len(), 10);
        assert!(eng.is_tripped("eq"));
        assert_eq!(eng.fire_count("eq"), 4);
        assert_eq!(eng.provenance()["eq"].len(), 10);

        // Fifth node: fire succeeds, no new edges.
        let n4 = fx.add("N", "n4", vec![("k", Value::Str("const".into()))]);
        {
            let mut g = fx.g();
            eng.on_node_changed(n4, None, &mut g);
        }
        assert_eq!(fx.topo.edge_count(), 10);
        assert!(eng.is_tripped("eq"));
        assert_eq!(eng.fire_count("eq"), 5);

        {
            let mut g = fx.g();
            eng.delete_rule("eq", &mut g).unwrap();
        }
        assert!(!eng.is_tripped("eq"));
        assert_eq!(eng.fire_count("eq"), 0);
        assert!(!eng.provenance().contains_key("eq"));
    }

    #[test]
    fn rebuild_over_budget_keeps_deterministic_first_10() {
        let mut fx = Fx::new();
        let mut ids = Vec::new();
        for i in 0..4 {
            ids.push(fx.add(
                "N",
                &format!("n{i}"),
                vec![("k", Value::Str("const".into()))],
            ));
        }
        let mut eng = RuleEngine::new();
        {
            let mut g = fx.g();
            eng.create_rule(const_eq_rule(10), &mut g).unwrap();
        }
        let et = fx.syms.get("EQ").unwrap();
        let mut before = BTreeSet::new();
        for &s in &ids {
            for &d in fx.topo.neighbors(et, Direction::Out, s) {
                before.insert((s, d));
            }
        }
        assert_eq!(before.len(), 10);
        assert!(eng.is_tripped("eq"));

        {
            let mut g = fx.g();
            eng.rebuild("eq", &mut g).unwrap();
        }
        let mut after = BTreeSet::new();
        for &s in &ids {
            for &d in fx.topo.neighbors(et, Direction::Out, s) {
                after.insert((s, d));
            }
        }
        assert_eq!(after, before);
        assert!(eng.is_tripped("eq"));
        assert_eq!(eng.provenance()["eq"].len(), 10);
    }

    fn prov_pairs(eng: &RuleEngine, name: &str) -> BTreeSet<(u32, u32)> {
        eng.provenance()
            .get(name)
            .map(|s| s.iter().map(|&(_, a, b)| (a, b)).collect())
            .unwrap_or_default()
    }

    /// Extra matching nodes after the trip must not change the frozen set,
    /// and rebuild while still over budget is a true provenance no-op.
    #[test]
    fn rebuild_while_over_budget_is_provenance_noop() {
        let mut fx = Fx::new();
        let mut eng = RuleEngine::new();
        {
            let mut g = fx.g();
            eng.create_rule(const_eq_rule(10), &mut g).unwrap();
        }
        for i in 0..4 {
            let id = fx.add(
                "N",
                &format!("n{i}"),
                vec![("k", Value::Str("const".into()))],
            );
            let mut g = fx.g();
            eng.on_node_changed(id, None, &mut g);
        }
        assert!(eng.is_tripped("eq"));
        assert_eq!(eng.provenance()["eq"].len(), 10);

        let n4 = fx.add("N", "n4", vec![("k", Value::Str("const".into()))]);
        {
            let mut g = fx.g();
            eng.on_node_changed(n4, None, &mut g);
        }
        let before = prov_pairs(&eng, "eq");
        assert_eq!(before.len(), 10);
        assert!(eng.is_tripped("eq"));

        {
            let mut g = fx.g();
            eng.rebuild("eq", &mut g).unwrap();
        }
        assert_eq!(prov_pairs(&eng, "eq"), before);
        assert!(eng.is_tripped("eq"));
    }

    /// Once tripped, retracts may drop provenance below budget, but new
    /// matching nodes must not grow the set. Rebuild is the only exit: when
    /// the full desired set now fits it is applied completely and tripped
    /// clears; later inserts derive again.
    #[test]
    fn tripped_freeze_blocks_adds_until_rebuild_fits() {
        let mut fx = Fx::new();
        let mut eng = RuleEngine::new();
        {
            let mut g = fx.g();
            eng.create_rule(const_eq_rule(10), &mut g).unwrap();
        }
        let mut ids = Vec::new();
        for i in 0..4 {
            let id = fx.add(
                "N",
                &format!("n{i}"),
                vec![("k", Value::Str("const".into()))],
            );
            ids.push(id);
            let mut g = fx.g();
            eng.on_node_changed(id, None, &mut g);
        }
        assert_eq!(eng.provenance()["eq"].len(), 10);
        assert!(eng.is_tripped("eq"));

        // Retract n2 and n3 below budget. Distinct values so they do not
        // FieldEqual each other.
        for (id, val) in [(ids[2], "x2"), (ids[3], "x3")] {
            let old = fx.props.get(id, "k").cloned();
            fx.props.set(id, "k", Value::Str(val.into()));
            let mut g = fx.g();
            eng.on_node_changed(id, Some(("k", old)), &mut g);
        }
        let after_retract = eng.provenance()["eq"].len();
        assert!(after_retract < 10, "retracts must still run while tripped");
        assert!(eng.is_tripped("eq"));

        let n4 = fx.add("N", "n4", vec![("k", Value::Str("const".into()))]);
        {
            let mut g = fx.g();
            eng.on_node_changed(n4, None, &mut g);
        }
        assert_eq!(
            eng.provenance()["eq"].len(),
            after_retract,
            "freeze: no new edges while tripped, even below budget"
        );
        assert!(eng.is_tripped("eq"));

        // Drop n4 from the match set so remaining desired is n0↔n1 (2 edges).
        let old = fx.props.get(n4, "k").cloned();
        fx.props.set(n4, "k", Value::Str("x4".into()));
        {
            let mut g = fx.g();
            eng.on_node_changed(n4, Some(("k", old)), &mut g);
        }

        {
            let mut g = fx.g();
            eng.rebuild("eq", &mut g).unwrap();
        }
        let et = fx.syms.get("EQ").unwrap();
        assert!(
            !eng.is_tripped("eq"),
            "rebuild must un-trip when desired fits"
        );
        assert_eq!(eng.provenance()["eq"].len(), 2);
        assert!(fx
            .topo
            .neighbors(et, Direction::Out, ids[0])
            .contains(&ids[1]));
        assert!(fx
            .topo
            .neighbors(et, Direction::Out, ids[1])
            .contains(&ids[0]));

        // Subsequent inserts derive normally.
        let n5 = fx.add("N", "n5", vec![("k", Value::Str("const".into()))]);
        {
            let mut g = fx.g();
            eng.on_node_changed(n5, None, &mut g);
        }
        assert!(!eng.is_tripped("eq"));
        assert_eq!(eng.provenance()["eq"].len(), 6); // {n0,n1,n5} pairwise
        assert!(fx.topo.neighbors(et, Direction::Out, n5).contains(&ids[0]));
        assert!(fx.topo.neighbors(et, Direction::Out, ids[0]).contains(&n5));
    }
}
