//! The changes since a fold, as one layer over it: what a reader snapshot
//! reads its unfolded delta tail through, and what a transaction lays its own
//! writes on.
//!
//! A [`DeltaLayer`] holds only what changed — the nodes inserted (with the
//! dense ids the writer gave or will give them), deleted, relabelled or
//! renamed, the property writes and removals, the edge inserts and deletes,
//! the insert counts — in structures that cost what they hold: [`NodeLayer`],
//! [`PropLayer`], and a [`Topology`] and [`EdgeProps`] of the changes alone. A
//! [`GraphView`] built by [`DeltaLayer::view`] reads it before the fold's
//! state, so nothing below it is copied, re-applied or re-indexed: building
//! the layer costs the delta tail's records, a transaction's statement costs
//! its own records, and neither depends on the store's size.
//!
//! **Full text.** The layer keeps no full-text index. A reader answers no
//! full-text search (`GraphDb::search` runs on the writer's own index); inside
//! a reader query or a transaction, full text is matched by Cypher's
//! `textMatches()`, which reads property values, and those are layered. The
//! writer's own index stays the writer's; a fold does not share it.

use std::sync::Arc;

use core_query::view::NodeLayer;
use core_query::GraphView;
use core_storage::v8::seam::PropLayer;
use core_storage::v8::MappedBase;
use core_storage::wal::WalRecord;
use core_storage::{Direction, EdgeProps, GraphError, Result, Topology, Value};

use crate::reader::{build_cv, build_epv, build_tv, CommitDelta, FrozenOverlay};

/// See the [module docs](self).
#[derive(Clone)]
pub(crate) struct DeltaLayer {
    nodes: NodeLayer,
    /// Edges inserted (adjacency) and deleted from below (tombstones).
    topo: Topology,
    /// Property writes and removals, by dense node id.
    props: PropLayer,
    /// Edge-property writes (insert counts) and the tombstones of edges a
    /// node delete swept.
    edge_props: EdgeProps,
}

fn corrupt(detail: String) -> GraphError {
    GraphError::Corrupt { detail }
}

impl DeltaLayer {
    /// An empty layer over `state`.
    pub(crate) fn new(state: &FrozenOverlay) -> Self {
        Self {
            nodes: NodeLayer::new(&state.ids, &state.syms),
            topo: Topology::new(),
            props: PropLayer::new(),
            edge_props: EdgeProps::new(),
        }
    }

    /// `deltas` laid over `state`, in order — a reader snapshot's effective
    /// state over its fold.
    pub(crate) fn from_deltas(
        state: &FrozenOverlay,
        base: &Option<Arc<MappedBase>>,
        deltas: &[Arc<CommitDelta>],
    ) -> Result<Self> {
        let mut layer = Self::new(state);
        for delta in deltas {
            layer.apply_delta(state, base, delta)?;
        }
        Ok(layer)
    }

    /// This layer with `deltas` laid over it too — the next snapshot's layer
    /// when its tail extends this one's (same fold, this tail a prefix). Costs
    /// the new deltas' records.
    pub(crate) fn extend(
        &mut self,
        state: &FrozenOverlay,
        base: &Option<Arc<MappedBase>>,
        deltas: &[Arc<CommitDelta>],
    ) -> Result<()> {
        for delta in deltas {
            self.apply_delta(state, base, delta)?;
        }
        Ok(())
    }

    /// A view of `state` (over `base`) with this layer on top.
    pub(crate) fn view<'a>(
        &'a self,
        state: &'a FrozenOverlay,
        base: &'a Option<Arc<MappedBase>>,
        mask: Option<&'a core_query::visible::VisibleSet>,
    ) -> GraphView<'a> {
        GraphView {
            ids: &state.ids,
            syms: &state.syms,
            labels: &state.labels,
            props: build_cv(&state.props, base).with_layer(&self.props),
            topo: build_tv(&state.topo, base).with_layer(&self.topo),
            edge_props: build_epv(&state.edge_props, base).with_layer(&self.edge_props),
            mask,
            // As for every reader snapshot: no equality index; `IndexScan`
            // falls back to a correct scan + filter.
            prop_index: None,
            layer: Some(&self.nodes),
        }
    }

    /// Lay one committed delta over the layer: its records, then the
    /// rule-derived edges and the view values the writer computed for it.
    fn apply_delta(
        &mut self,
        state: &FrozenOverlay,
        base: &Option<Arc<MappedBase>>,
        delta: &CommitDelta,
    ) -> Result<()> {
        for rec in &delta.records {
            self.apply(state, base, rec)?;
        }
        for &(etype, src, dst) in &delta.derived_inserts {
            self.topo.add_edge(etype, src, dst);
        }
        for &(etype, src, dst) in &delta.derived_deletes {
            self.remove_edge(etype, src, dst);
        }
        for (node, view_prop, value) in &delta.view_values {
            match value {
                Some(v) => self.props.set(*node, view_prop, v.clone()),
                // Removed as well as hidden, or the fold's value reads through.
                None => self.props.remove(*node, view_prop),
            }
        }
        // The edge properties the commit wrote with no record of their own —
        // a rule's weights — exactly as the writer's overlay holds them
        // (ledger row 124). Last, so the commit's final state wins.
        for &((etype, src, dst), ref entry) in &delta.edge_props {
            self.edge_props.restore_entry(etype, src, dst, entry);
        }
        Ok(())
    }

    /// Lay one record over the layer, exactly as the writer applied it.
    pub(crate) fn apply(
        &mut self,
        state: &FrozenOverlay,
        base: &Option<Arc<MappedBase>>,
        rec: &WalRecord,
    ) -> Result<()> {
        match rec {
            WalRecord::Intern { id, text } => {
                let got = self.nodes.intern(&state.syms, text);
                if got != *id {
                    return Err(corrupt(format!(
                        "mvcc delta intern mismatch for {text:?}: expected {id}, got {got}"
                    )));
                }
            }
            WalRecord::InsertNodeId { label, key, props } => {
                if self.view(state, base, None).sym_name(*label).is_none() {
                    return Err(corrupt(format!("mvcc delta: unknown label sym {label}")));
                }
                let id = self.insert_node(state, base, key, *label);
                for (field_sym, value) in props {
                    let field = self
                        .view(state, base, None)
                        .sym_name(*field_sym)
                        .ok_or_else(|| {
                            corrupt(format!("mvcc delta: unknown field sym {field_sym}"))
                        })?
                        .to_string();
                    self.props.set(id, &field, value.clone());
                }
            }
            WalRecord::InsertNode { label, key, props } => {
                let label_sym = self.nodes.intern(&state.syms, label);
                let id = self.insert_node(state, base, key, label_sym);
                for (field, value) in props {
                    self.props.set(id, field, value.clone());
                }
            }
            WalRecord::SetPropId { id, field, value } => {
                let field = self
                    .view(state, base, None)
                    .sym_name(*field)
                    .map(str::to_string);
                if let Some(field) = field {
                    self.props.set(*id, &field, value.clone());
                }
            }
            WalRecord::SetProp { key, field, value } => {
                if let Some(id) = self.view(state, base, None).node_id(key) {
                    self.props.set(id, field, value.clone());
                }
            }
            WalRecord::RemoveProp { key, field } => {
                if let Some(id) = self.view(state, base, None).node_id(key) {
                    self.props.remove(id, field);
                }
            }
            WalRecord::DeleteNode { key } => self.delete_node(state, base, key),
            WalRecord::InsertEdgeId { etype, src, dst } => {
                self.topo.add_edge(*etype, *src, *dst);
            }
            WalRecord::InsertEdge {
                edge_type,
                src_key,
                dst_key,
            } => {
                let etype = self.nodes.intern(&state.syms, edge_type);
                let view = self.view(state, base, None);
                if let (Some(src), Some(dst)) = (view.node_id(src_key), view.node_id(dst_key)) {
                    self.topo.add_edge(etype, src, dst);
                }
            }
            WalRecord::DeleteEdge {
                edge_type,
                src_key,
                dst_key,
            } => {
                let view = self.view(state, base, None);
                let edge = view.sym(edge_type).and_then(|etype| {
                    Some((etype, view.node_id(src_key)?, view.node_id(dst_key)?))
                });
                if let Some((etype, src, dst)) = edge {
                    self.remove_edge(etype, src, dst);
                }
            }
            WalRecord::SetEdgeCount {
                etype,
                src,
                dst,
                count,
            } if *count > 0 => {
                // An insert count is an edge property: absolute, last write wins.
                self.edge_props.set(
                    *etype,
                    *src,
                    *dst,
                    crate::db::EDGE_COUNT_PROP,
                    Value::Int(*count as i64),
                );
            }
            WalRecord::SetEdgePropId {
                etype,
                src,
                dst,
                field,
                value,
            } => {
                let name = self.field_name(state, base, *field)?;
                self.edge_props
                    .set(*etype, *src, *dst, &name, value.clone());
            }
            WalRecord::RemoveEdgePropId {
                etype,
                src,
                dst,
                field,
            } => {
                let name = self.field_name(state, base, *field)?;
                self.remove_edge_prop(state, base, (*etype, *src, *dst), &name);
            }
            WalRecord::RenameNode { old_key, new_key } => {
                // Recovery-safe: an `old_key` already gone is skipped cleanly.
                let view = self.view(state, base, None);
                if let Some(id) = view.node_id(old_key) {
                    if view.node_id(new_key).is_some() {
                        return Err(corrupt(format!(
                            "mvcc delta RenameNode {old_key}→{new_key} failed"
                        )));
                    }
                    self.nodes.rename(id, old_key, new_key);
                }
            }
            WalRecord::Batch(inner) => {
                for r in inner {
                    self.apply(state, base, r)?;
                }
            }
            // Read-path no-ops. Rules and views: their effects arrive as the
            // delta's derived edges and view values. Property-index, range-index
            // and constraint declarations: the reader carries no equality or
            // range index (`IndexScan` falls back to a scan) and never writes. The edge-count opt-in (count 0): a writer-side
            // gate. History markers: they serve WAL scans only. Full-text
            // declarations: the layer keeps no full-text index (module docs).
            WalRecord::CreateRule { .. }
            | WalRecord::DeleteRule { .. }
            | WalRecord::RebuildRule { .. }
            | WalRecord::CreateView { .. }
            | WalRecord::DeleteView { .. }
            | WalRecord::EnableIndex { .. }
            | WalRecord::DisableIndex { .. }
            | WalRecord::EnableRangeIndex { .. }
            | WalRecord::DisableRangeIndex { .. }
            | WalRecord::CreateConstraint { .. }
            | WalRecord::DropConstraint { .. }
            | WalRecord::SetEdgeCount { .. }
            | WalRecord::DerivedEdgeAdded { .. }
            | WalRecord::DerivedEdgeRetracted { .. }
            | WalRecord::EnableFulltext { .. }
            | WalRecord::DisableFulltext { .. } => {}
        }
        Ok(())
    }

    /// The name an edge-property record's interned `field` stands for.
    fn field_name(
        &self,
        state: &FrozenOverlay,
        base: &Option<Arc<MappedBase>>,
        field: u32,
    ) -> Result<String> {
        self.view(state, base, None)
            .sym_name(field)
            .map(str::to_string)
            .ok_or_else(|| corrupt(format!("mvcc delta: unknown field sym {field}")))
    }

    /// Remove one property of an edge as every layer below shows it: the
    /// edge's whole map, without the field, hides what lies below.
    fn remove_edge_prop(
        &mut self,
        state: &FrozenOverlay,
        base: &Option<Arc<MappedBase>>,
        (etype, src, dst): (u32, u32, u32),
        field: &str,
    ) {
        let mut props = self
            .view(state, base, None)
            .edge_props
            .props_of(etype, src, dst);
        if props.remove(field).is_some() {
            self.edge_props.replace(etype, src, dst, props);
        }
    }

    /// Lay a transaction's relationship-property write over the layer: set
    /// (`Some`) or remove (`None`) `field` of the edge named by its type and
    /// end keys. A transaction buffers these by key, not as WAL records: the
    /// records name the edge by dense ids, which only the commit assigns.
    pub(crate) fn write_edge_prop(
        &mut self,
        state: &FrozenOverlay,
        base: &Option<Arc<MappedBase>>,
        (edge_type, src_key, dst_key): (&str, &str, &str),
        field: &str,
        value: Option<&Value>,
    ) {
        let view = self.view(state, base, None);
        let (Some(etype), Some(src), Some(dst)) = (
            view.sym(edge_type),
            view.node_id(src_key),
            view.node_id(dst_key),
        ) else {
            return;
        };
        match value {
            Some(v) => self.edge_props.set(etype, src, dst, field, v.clone()),
            None => self.remove_edge_prop(state, base, (etype, src, dst), field),
        }
    }

    /// `IdMap::try_insert` over the layer: a live `key` keeps its id and takes
    /// the new label; any other gets the next dense id.
    fn insert_node(
        &mut self,
        state: &FrozenOverlay,
        base: &Option<Arc<MappedBase>>,
        key: &str,
        label_sym: u32,
    ) -> u32 {
        match self.view(state, base, None).node_id(key) {
            Some(id) => {
                self.nodes.relabel(id, label_sym);
                id
            }
            None => self.nodes.insert(key, label_sym),
        }
    }

    fn delete_node(&mut self, state: &FrozenOverlay, base: &Option<Arc<MappedBase>>, key: &str) {
        let view = self.view(state, base, None);
        let Some(id) = view.node_id(key) else {
            return;
        };
        // The edges incident on the node go with it, from every layer — the
        // writer sweeps them inline, with no DeleteEdge record.
        let mut doomed = Vec::new();
        for etype in view.topo.etypes() {
            for &dst in view.topo.neighbors(etype, Direction::Out, id).as_ref() {
                doomed.push((etype, id, dst));
            }
            for &src in view.topo.neighbors(etype, Direction::In, id).as_ref() {
                if src != id {
                    doomed.push((etype, src, id));
                }
            }
        }
        self.nodes.delete(key, id);
        self.props.remove_all(id);
        for (etype, src, dst) in doomed {
            self.remove_edge(etype, src, dst);
            self.edge_props.remove_edge(etype, src, dst);
        }
    }

    /// Remove an edge from every layer: from this layer's own adjacency if it
    /// is there, and by a tombstone over whatever lies below.
    fn remove_edge(&mut self, etype: u32, src: u32, dst: u32) {
        self.topo.remove_overlay_edge(etype, src, dst);
        self.topo.remove_edge(etype, src, dst);
    }
}
