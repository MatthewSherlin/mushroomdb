//! Materialized property views — incremental per-node derived properties.
//!
//! ## Storage choice
//!
//! View values are stored as regular entries in the `ColumnStore` under their
//! `view_prop` name, updated in place on each triggering event.
//!
//! **Why ColumnStore in-place**:
//! - Zero query-layer changes: every read path (scan, filter, project, group)
//!   already reads from `ColumnStore` — view props appear automatically.
//! - `ColumnStore::set` / `remove` handle cleanup; `remove_all` clears a
//!   deleted node's view values as part of the normal tombstone sweep.
//! - Rebuild-on-open recomputes values from persisted graph state (topo + props)
//!   without touching the snapshot format.
//! - No virtual-column overlay, no side-table, no special query path.
//!
//! **Trade-off**: view props appear in `node_info()` alongside real props.
//! That is intentional — "they're column values" per the brief.
//!
//! ## MIN/MAX retraction cost
//!
//! Retracting an edge whose endpoint held the current MIN or MAX requires
//! rescanning all remaining neighbors (O(degree)) because there is no
//! sorted structure tracking the second-best value.  This is documented
//! as the v1 cost; no auxiliary structures are maintained.
//!
//! ## Subscriptions interplay (v1)
//!
//! View-value updates do **not** emit subscription events.  Only user-write
//! WAL records and rule fire/retract deltas generate `DbEvent`s.  View
//! maintenance writes to `ColumnStore` directly and is invisible to the
//! subscription layer.  This preserves T1's `pending_deltas` discipline:
//! the `debug_assert!(pending_delta_count == 0)` at `log_then_apply_with`
//! entry remains green through view-heavy workloads because view updates
//! bypass the engine's delta path entirely.

use core_storage::{ColumnStore, Direction, IdMap, Interner, Topology, Value};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AggFn {
    Sum,
    Avg,
    Min,
    Max,
    Count,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ViewSource {
    Degree {
        edge_type: String,
        direction: Direction,
    },
    NeighborAgg {
        edge_type: String,
        direction: Direction,
        agg: AggFn,
        /// Neighbor property to aggregate.
        prop: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ViewDef {
    pub name: String,
    /// Node label this view applies to.
    pub label: String,
    /// The synthetic property name written into `ColumnStore`.
    pub view_prop: String,
    pub source: ViewSource,
}

impl ViewDef {
    pub fn validate(&self) -> Result<(), String> {
        if self.name.is_empty() {
            return Err("view name must not be empty".into());
        }
        if self.label.is_empty() {
            return Err("view label must not be empty".into());
        }
        if self.view_prop.is_empty() {
            return Err("view_prop must not be empty".into());
        }
        match &self.source {
            ViewSource::Degree { edge_type, .. } => {
                if edge_type.is_empty() {
                    return Err("Degree edge_type must not be empty".into());
                }
            }
            ViewSource::NeighborAgg {
                edge_type, prop, ..
            } => {
                if edge_type.is_empty() {
                    return Err("NeighborAgg edge_type must not be empty".into());
                }
                if prop.is_empty() {
                    return Err("NeighborAgg prop must not be empty".into());
                }
            }
        }
        Ok(())
    }

    fn edge_type(&self) -> &str {
        match &self.source {
            ViewSource::Degree { edge_type, .. } => edge_type,
            ViewSource::NeighborAgg { edge_type, .. } => edge_type,
        }
    }

    fn direction(&self) -> Direction {
        match &self.source {
            ViewSource::Degree { direction, .. } => *direction,
            ViewSource::NeighborAgg { direction, .. } => *direction,
        }
    }
}

// ---------------------------------------------------------------------------
// ViewStore
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
pub struct ViewStore {
    /// Ordered by name.
    views: BTreeMap<String, ViewDef>,
}

impl ViewStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn views(&self) -> impl Iterator<Item = &ViewDef> {
        self.views.values()
    }

    pub fn has_view(&self, name: &str) -> bool {
        self.views.contains_key(name)
    }

    /// If `prop_name` is managed by any view, return that view's name.
    pub fn view_for_prop(&self, prop_name: &str) -> Option<&str> {
        self.views
            .values()
            .find(|v| v.view_prop == prop_name)
            .map(|v| v.name.as_str())
    }

    // -----------------------------------------------------------------------
    // DDL
    // -----------------------------------------------------------------------

    /// Register a view and backfill values for all existing nodes.
    ///
    /// # Errors
    /// - Duplicate view name.
    /// - `view_prop` already claimed by another view.
    /// - `view_prop` already present as a real node property in `ColumnStore`.
    pub fn create_view(
        &mut self,
        def: ViewDef,
        props: &mut ColumnStore,
        topo: &Topology,
        ids: &IdMap,
        syms: &Interner,
        labels: &[u32],
    ) -> Result<(), String> {
        def.validate()?;

        if self.views.contains_key(&def.name) {
            return Err(format!("view {:?} already exists", def.name));
        }
        if let Some(existing) = self.views.values().find(|v| v.view_prop == def.view_prop) {
            return Err(format!(
                "view_prop {:?} is already used by view {:?}",
                def.view_prop, existing.name
            ));
        }
        // Real-prop collision: any ColumnStore field not owned by a view.
        let view_props: std::collections::HashSet<&str> =
            self.views.values().map(|v| v.view_prop.as_str()).collect();
        if props
            .fields()
            .any(|f| f == def.view_prop && !view_props.contains(f))
        {
            return Err(format!(
                "view_prop {:?} conflicts with an existing node property",
                def.view_prop
            ));
        }

        // Backfill: compute initial values for all existing nodes.
        backfill_view(&def, props, topo, ids, syms, labels);

        self.views.insert(def.name.clone(), def);
        Ok(())
    }

    /// Restore a view definition from a snapshot without collision checking or
    /// backfilling.  The snapshot's `ColumnStore` already contains view values;
    /// this method simply registers the definition so the store is aware of it.
    ///
    /// Called only from `open_with` during snapshot restore.  Callers must then
    /// call `rebuild_all` to ensure values are correct after WAL replay.
    pub fn restore_view(&mut self, def: ViewDef) -> Result<(), String> {
        def.validate()?;
        if self.views.contains_key(&def.name) {
            return Ok(()); // idempotent during snapshot restore
        }
        self.views.insert(def.name.clone(), def);
        Ok(())
    }

    /// Remove a view and delete its values from every node.
    ///
    /// # Errors
    /// - View not found.
    pub fn delete_view(
        &mut self,
        name: &str,
        props: &mut ColumnStore,
        ids: &IdMap,
        labels: &[u32],
        syms: &Interner,
    ) -> Result<(), String> {
        let def = self
            .views
            .remove(name)
            .ok_or_else(|| format!("view {:?} not found", name))?;

        // Remove the view-prop value from every matching node.
        if let Some(label_sym) = syms.get(&def.label) {
            for id in 0..ids.len() as u32 {
                if labels.get(id as usize).copied() == Some(label_sym) {
                    props.remove(id, &def.view_prop);
                }
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Incremental maintenance
    // -----------------------------------------------------------------------

    /// Called when an edge of type `etype` (symbol) between `src` and `dst` is
    /// inserted (`inserted=true`) or deleted (`inserted=false`).
    ///
    /// Covers both manual user edges and derived engine edges.  The topo must
    /// already reflect the new state (edge added before this call on insert;
    /// edge removed before this call on delete) so that full-recompute paths
    /// (Avg, Min, Max) see correct neighbor sets.
    #[allow(clippy::too_many_arguments)]
    pub fn on_edge_changed(
        &self,
        etype: u32,
        src: u32,
        dst: u32,
        inserted: bool,
        props: &mut ColumnStore,
        topo: &Topology,
        ids: &IdMap,
        syms: &Interner,
        labels: &[u32],
    ) {
        for def in self.views.values() {
            let Some(et_sym) = syms.get(def.edge_type()) else {
                continue;
            };
            if et_sym != etype {
                continue;
            }
            let direction = def.direction();
            // Determine which node is the "subject" whose view value changes.
            // direction=Out: subject is src (counts/aggregates its out-neighbors)
            // direction=In:  subject is dst (counts/aggregates its in-neighbors)
            let subject = match direction {
                Direction::Out => src,
                Direction::In => dst,
            };
            // The "neighbor" whose property is read for NeighborAgg.
            let neighbor = match direction {
                Direction::Out => dst,
                Direction::In => src,
            };
            update_node_view(
                def,
                subject,
                neighbor,
                inserted,
                props,
                topo,
                ids,
                syms,
                labels,
            );
        }
    }

    /// Called after `SetProp` / `RemoveProp` — finds NeighborAgg views that
    /// read `field` from neighbors and recomputes values for all subject nodes.
    #[allow(clippy::too_many_arguments)]
    pub fn on_prop_changed(
        &self,
        changed_node: u32,
        field: &str,
        props: &mut ColumnStore,
        topo: &Topology,
        ids: &IdMap,
        syms: &Interner,
        labels: &[u32],
    ) {
        for def in self.views.values() {
            let ViewSource::NeighborAgg {
                edge_type,
                direction,
                prop,
                ..
            } = &def.source
            else {
                continue;
            };
            if prop != field {
                continue;
            }
            let Some(et_sym) = syms.get(edge_type) else {
                continue;
            };
            // Find subject nodes that have changed_node as a neighbor via et_sym/direction.
            // direction=Out: subjects X where edge X→changed_node exists → In-neighbors of changed_node
            // direction=In:  subjects X where edge changed_node→X exists → Out-neighbors of changed_node
            let reverse_dir = match direction {
                Direction::Out => Direction::In,
                Direction::In => Direction::Out,
            };
            let subjects: Vec<u32> = topo
                .neighbors(et_sym, reverse_dir, changed_node)
                .to_vec();
            for subject in subjects {
                // Full recompute for the subject's view value.
                if let Some(val) = compute_view_value(def, subject, props, topo, ids, syms, labels)
                {
                    props.set(subject, &def.view_prop, val);
                } else {
                    props.remove(subject, &def.view_prop);
                }
            }
        }
    }

    /// Initialize view values for a newly inserted node.
    ///
    /// Sets Degree, Count, and Sum views to their zero values (0 / 0.0).
    /// Avg / Min / Max are left absent — there is no sensible neutral value
    /// when no neighbors have been observed yet.
    ///
    /// Call this BEFORE the rule engine's `on_node_changed` for the new node so
    /// that subsequent `on_edge_changed` calls (from derived-edge deltas) can
    /// increment correctly from a known baseline.
    pub fn init_node_views(&self, node: u32, props: &mut ColumnStore, syms: &Interner, labels: &[u32]) {
        for def in self.views.values() {
            let Some(label_sym) = syms.get(&def.label) else { continue; };
            if labels.get(node as usize).copied() != Some(label_sym) { continue; }
            match &def.source {
                ViewSource::Degree { .. } => {
                    if props.get(node, &def.view_prop).is_none() {
                        props.set(node, &def.view_prop, Value::Int(0));
                    }
                }
                ViewSource::NeighborAgg { agg: AggFn::Count, .. } => {
                    if props.get(node, &def.view_prop).is_none() {
                        props.set(node, &def.view_prop, Value::Int(0));
                    }
                }
                ViewSource::NeighborAgg { agg: AggFn::Sum, .. } => {
                    // Always set to 0.0 — init is only called for freshly-inserted
                    // nodes whose view_prop does not yet exist.
                    props.set(node, &def.view_prop, Value::Float(0.0));
                }
                _ => {} // Avg / Min / Max: absent until neighbors exist
            }
        }
    }

    // -----------------------------------------------------------------------
    // Rebuild
    // -----------------------------------------------------------------------

    /// Recompute all view values from scratch.  Called on open after WAL
    /// replay so values are consistent with persisted graph state.
    pub fn rebuild_all(
        &self,
        props: &mut ColumnStore,
        topo: &Topology,
        ids: &IdMap,
        syms: &Interner,
        labels: &[u32],
    ) {
        for def in self.views.values() {
            backfill_view(def, props, topo, ids, syms, labels);
        }
    }
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Compute and store the view value for every node matching `def.label`.
fn backfill_view(
    def: &ViewDef,
    props: &mut ColumnStore,
    topo: &Topology,
    ids: &IdMap,
    syms: &Interner,
    labels: &[u32],
) {
    let Some(label_sym) = syms.get(&def.label) else {
        return;
    };
    let Some(et_sym) = syms.get(def.edge_type()) else {
        // Edge type not yet interned → no such edges exist.
        // Set zero for Degree / Count / Sum; leave absent for Avg / Min / Max.
        for id in 0..ids.len() as u32 {
            if labels.get(id as usize).copied() == Some(label_sym) {
                match &def.source {
                    ViewSource::Degree { .. } => {
                        props.set(id, &def.view_prop, Value::Int(0));
                    }
                    ViewSource::NeighborAgg {
                        agg: AggFn::Count, ..
                    } => {
                        props.set(id, &def.view_prop, Value::Int(0));
                    }
                    ViewSource::NeighborAgg { agg: AggFn::Sum, .. } => {
                        props.set(id, &def.view_prop, Value::Float(0.0));
                    }
                    _ => {}
                }
            }
        }
        return;
    };
    for id in 0..ids.len() as u32 {
        if labels.get(id as usize).copied() != Some(label_sym) {
            continue;
        }
        match compute_view_value(def, id, props, topo, ids, syms, labels) {
            Some(val) => props.set(id, &def.view_prop, val),
            None => {
                props.remove(id, &def.view_prop);
                // Degree and Count should always yield Some; only Avg/Min/Max
                // return None when there are no qualifying neighbors.
            }
        }
        // Ensure Degree and Count always have a value (even 0).
        // compute_view_value already returns Some(Int(0)) for empty neighbor sets
        // in Degree and Count, so no extra step needed here.
        let _ = et_sym;
    }
}

/// Compute the view value for a single node.  Returns `None` when there are
/// no qualifying neighbors for Avg/Min/Max (no sensible neutral value).
pub fn compute_view_value(
    def: &ViewDef,
    node: u32,
    props: &ColumnStore,
    topo: &Topology,
    _ids: &IdMap,
    syms: &Interner,
    labels: &[u32],
) -> Option<Value> {
    // Label check.
    let label_sym = syms.get(&def.label)?;
    if labels.get(node as usize).copied() != Some(label_sym) {
        return None;
    }

    // If the edge_type has never been used it is not in the symbol table.
    // There are zero such edges, so Degree/Count return 0, Sum returns 0.0,
    // and Avg/Min/Max are absent.
    let et_sym = match syms.get(def.edge_type()) {
        Some(s) => s,
        None => {
            return match &def.source {
                ViewSource::Degree { .. } => Some(Value::Int(0)),
                ViewSource::NeighborAgg {
                    agg: AggFn::Count, ..
                } => Some(Value::Int(0)),
                ViewSource::NeighborAgg { agg: AggFn::Sum, .. } => Some(Value::Float(0.0)),
                ViewSource::NeighborAgg { .. } => None,
            };
        }
    };
    let direction = def.direction();
    let neighbors = topo.neighbors(et_sym, direction, node);

    match &def.source {
        ViewSource::Degree { .. } => Some(Value::Int(neighbors.len() as i64)),
        ViewSource::NeighborAgg { agg, prop, .. } => {
            match agg {
                AggFn::Count => Some(Value::Int(neighbors.len() as i64)),
                AggFn::Sum => {
                    let mut sum = 0.0f64;
                    for &nbr in neighbors {
                        if let Some(v) = props.get(nbr, prop) {
                            if let Some(n) = as_float(v) {
                                sum += n;
                            }
                        }
                    }
                    Some(Value::Float(sum))
                }
                AggFn::Avg => {
                    let mut sum = 0.0f64;
                    let mut count = 0usize;
                    for &nbr in neighbors {
                        if let Some(v) = props.get(nbr, prop) {
                            if let Some(n) = as_float(v) {
                                sum += n;
                                count += 1;
                            }
                        }
                    }
                    if count == 0 {
                        None
                    } else {
                        Some(Value::Float(sum / count as f64))
                    }
                }
                AggFn::Min => {
                    let mut best: Option<f64> = None;
                    for &nbr in neighbors {
                        if let Some(v) = props.get(nbr, prop) {
                            if let Some(n) = as_float(v) {
                                best = Some(best.map_or(n, |m: f64| m.min(n)));
                            }
                        }
                    }
                    best.map(Value::Float)
                }
                AggFn::Max => {
                    let mut best: Option<f64> = None;
                    for &nbr in neighbors {
                        if let Some(v) = props.get(nbr, prop) {
                            if let Some(n) = as_float(v) {
                                best = Some(best.map_or(n, |m: f64| m.max(n)));
                            }
                        }
                    }
                    best.map(Value::Float)
                }
            }
        }
    }
}

/// Update `subject`'s view value incrementally after one edge change.
/// `neighbor` is the endpoint whose property is read for NeighborAgg.
/// `inserted`: true = edge was added, false = edge was removed.
///
/// The topo must already reflect the new state before this is called.
#[allow(clippy::too_many_arguments)]
fn update_node_view(
    def: &ViewDef,
    subject: u32,
    neighbor: u32,
    inserted: bool,
    props: &mut ColumnStore,
    topo: &Topology,
    ids: &IdMap,
    syms: &Interner,
    labels: &[u32],
) {
    // Label check: only subjects with the matching label get this view.
    let Some(label_sym) = syms.get(&def.label) else {
        return;
    };
    if labels.get(subject as usize).copied() != Some(label_sym) {
        return;
    }

    match &def.source {
        ViewSource::Degree { .. } => {
            // Degree: increment or decrement the stored count.
            let current = match props.get(subject, &def.view_prop) {
                Some(Value::Int(n)) => *n,
                _ => 0,
            };
            let new_val = if inserted { current + 1 } else { (current - 1).max(0) };
            props.set(subject, &def.view_prop, Value::Int(new_val));
        }
        ViewSource::NeighborAgg { agg, prop, .. } => {
            match agg {
                AggFn::Count => {
                    let current = match props.get(subject, &def.view_prop) {
                        Some(Value::Int(n)) => *n,
                        _ => 0,
                    };
                    let new_val = if inserted { current + 1 } else { (current - 1).max(0) };
                    props.set(subject, &def.view_prop, Value::Int(new_val));
                }
                AggFn::Sum => {
                    let delta = match props.get(neighbor, prop) {
                        Some(v) => as_float(v).unwrap_or(0.0),
                        None => 0.0,
                    };
                    let current = match props.get(subject, &def.view_prop) {
                        Some(Value::Float(f)) => *f,
                        Some(Value::Int(n)) => *n as f64,
                        _ => 0.0,
                    };
                    let new_val = if inserted { current + delta } else { current - delta };
                    props.set(subject, &def.view_prop, Value::Float(new_val));
                }
                // Avg, Min, Max: full recompute from topo (O(degree)).
                AggFn::Avg | AggFn::Min | AggFn::Max => {
                    match compute_view_value(def, subject, props, topo, ids, syms, labels) {
                        Some(val) => props.set(subject, &def.view_prop, val),
                        None => {
                            props.remove(subject, &def.view_prop);
                        }
                    }
                }
            }
        }
    }
}

fn as_float(v: &Value) -> Option<f64> {
    match v {
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) if f.is_finite() => Some(*f),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use core_storage::{IdMap, Interner, Topology};

    fn make_setup() -> (ViewStore, ColumnStore, Topology, IdMap, Interner, Vec<u32>) {
        let mut ids = IdMap::new();
        let mut syms = Interner::new();
        let mut labels = Vec::new();
        let mut topo = Topology::new();
        let mut props = ColumnStore::new();

        // Insert nodes: p0, p1, p2 (label Person), c0 (label City)
        let person_sym = syms.intern("Person");
        let city_sym = syms.intern("City");
        let edge_sym = syms.intern("LIVES_IN");

        for key in &["p0", "p1", "p2"] {
            let id = ids.get_or_insert(key);
            if labels.len() <= id as usize {
                labels.resize(id as usize + 1, u32::MAX);
            }
            labels[id as usize] = person_sym;
        }
        let c0 = ids.get_or_insert("c0");
        if labels.len() <= c0 as usize {
            labels.resize(c0 as usize + 1, u32::MAX);
        }
        labels[c0 as usize] = city_sym;

        // p0 and p1 LIVES_IN c0
        let p0 = ids.get("p0").unwrap();
        let p1 = ids.get("p1").unwrap();
        topo.add_edge(edge_sym, p0, c0);
        topo.add_edge(edge_sym, p1, c0);

        // Set a numeric prop on p0 and p1
        props.set(p0, "score", Value::Float(3.0));
        props.set(p1, "score", Value::Float(7.0));

        (ViewStore::new(), props, topo, ids, syms, labels)
    }

    #[test]
    fn degree_view_basic() {
        let (mut vs, mut props, topo, ids, syms, labels) = make_setup();
        let def = ViewDef {
            name: "city_pop".into(),
            label: "City".into(),
            view_prop: "in_deg".into(),
            source: ViewSource::Degree {
                edge_type: "LIVES_IN".into(),
                direction: Direction::In,
            },
        };
        vs.create_view(def, &mut props, &topo, &ids, &syms, &labels)
            .unwrap();
        let c0 = ids.get("c0").unwrap();
        assert_eq!(props.get(c0, "in_deg"), Some(&Value::Int(2)));
    }

    #[test]
    fn neighbor_agg_sum() {
        let (mut vs, mut props, topo, ids, syms, labels) = make_setup();
        let def = ViewDef {
            name: "city_score_sum".into(),
            label: "City".into(),
            view_prop: "score_sum".into(),
            source: ViewSource::NeighborAgg {
                edge_type: "LIVES_IN".into(),
                direction: Direction::In,
                agg: AggFn::Sum,
                prop: "score".into(),
            },
        };
        vs.create_view(def, &mut props, &topo, &ids, &syms, &labels)
            .unwrap();
        let c0 = ids.get("c0").unwrap();
        // p0=3.0 + p1=7.0
        assert_eq!(props.get(c0, "score_sum"), Some(&Value::Float(10.0)));
    }

    #[test]
    fn view_prop_collision_rejected() {
        let (mut vs, mut props, topo, ids, syms, labels) = make_setup();
        // Insert a real prop with the same name
        let p0 = ids.get("p0").unwrap();
        props.set(p0, "collision_prop", Value::Int(1));
        let def = ViewDef {
            name: "test_view".into(),
            label: "Person".into(),
            view_prop: "collision_prop".into(),
            source: ViewSource::Degree {
                edge_type: "LIVES_IN".into(),
                direction: Direction::Out,
            },
        };
        let err = vs
            .create_view(def, &mut props, &topo, &ids, &syms, &labels)
            .unwrap_err();
        assert!(err.contains("conflicts with an existing node property"), "{err}");
    }

    #[test]
    fn delete_view_removes_values() {
        let (mut vs, mut props, topo, ids, syms, labels) = make_setup();
        let def = ViewDef {
            name: "city_pop".into(),
            label: "City".into(),
            view_prop: "in_deg".into(),
            source: ViewSource::Degree {
                edge_type: "LIVES_IN".into(),
                direction: Direction::In,
            },
        };
        vs.create_view(def, &mut props, &topo, &ids, &syms, &labels)
            .unwrap();
        let c0 = ids.get("c0").unwrap();
        assert!(props.get(c0, "in_deg").is_some());
        vs.delete_view("city_pop", &mut props, &ids, &labels, &syms)
            .unwrap();
        assert!(props.get(c0, "in_deg").is_none());
    }
}
