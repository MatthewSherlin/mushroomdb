use std::collections::{HashMap, HashSet};

use core_storage::property_index::{PropertyIndex, UnifiedIndex};
use core_storage::v8::seam::{ColumnsView, EdgePropsView, TopologyView, ValueRef};
use core_storage::{IdMap, Interner, Value};

use crate::visible::VisibleSet;

/// Read-only twin of `GraphMut`. Holds only borrowed graph state.
///
/// **Read node identity through the methods** ([`node_id`](Self::node_id),
/// [`try_key_of`](Self::try_key_of), [`label_syms`](Self::label_syms),
/// [`sym`](Self::sym), …), not the `ids` / `syms` / `labels` fields: under a
/// [`layer`](Self::layer) those fields are the state *below* it — a reader
/// snapshot's fold, under its delta tail and any transaction's writes — and
/// only the methods lay the layer over them. `props`, `topo` and `edge_props`
/// carry their own layer and read correctly either way.
pub struct GraphView<'a> {
    pub ids: &'a IdMap,
    pub syms: &'a Interner,
    pub labels: &'a [u32],
    /// Overlay-over-base column store view.  For V5–V7 snapshots and fresh
    /// databases, `base` is `None` and all column reads go to the overlay.
    /// For V8 snapshots, `base` holds the archived columns from the mmap.
    pub props: ColumnsView<'a>,
    /// Overlay-over-base topology view. For V5–V7 snapshots and fresh
    /// databases, `base` is `None` and all topology reads go to the owned
    /// overlay. For V8 snapshots, `base` holds the archived CSR from the
    /// mmap, and WAL-replayed edges accumulate in the overlay.
    pub topo: TopologyView<'a>,
    /// Overlay-over-base edge-property view.  For V8 snapshots the base
    /// section is consulted zero-copy; the overlay holds only post-snapshot
    /// changes.  Tombstones in the overlay mask deleted-from-base entries.
    pub edge_props: EdgePropsView<'a>,
    /// Optional query-scoped node visibility set. `None` = all nodes visible.
    /// When `Some(set)`, only dense ids present in `set` are accessible.
    ///
    /// [`VisibleSet`] carries its own representation choice, so this field is
    /// one probe whichever shape the mask took; see that type for the rule.
    pub mask: Option<&'a VisibleSet>,
    /// Optional equality index over scalar properties. `Some` on the primary
    /// locked read path; `None` for MVCC reader snapshots (which fall back to a
    /// scan). `IndexScan` consults it only when the `(label, field)` is declared.
    pub prop_index: Option<&'a PropertyIndex>,
    /// Node inserts, deletes, relabels and renames, and interned symbols,
    /// laid over `ids`, `labels` and `syms`: a reader snapshot's unfolded
    /// delta tail, plus an open transaction's own writes. `None` reads the
    /// fields alone.
    pub layer: Option<&'a NodeLayer>,
}

/// Changes to node identity laid over a fold's `IdMap`, labels and
/// interner: the nodes inserted since (dense ids from `id_floor` up, exactly
/// as the writer allocated or will allocate them), the nodes deleted,
/// relabelled or renamed, and the symbols interned (from `sym_floor` up).
///
/// It is a layer, not a copy: it holds only what changed, and a [`GraphView`]
/// reads it before the state below. Building one and reading through it cost
/// nothing proportional to the store.
#[derive(Debug, Clone)]
pub struct NodeLayer {
    /// `ids.len()` below: the first dense id this layer allocates.
    id_floor: u32,
    /// Key of each id from `id_floor`, in allocation order.
    keys: Vec<String>,
    /// Label symbol of each id from `id_floor`.
    labels: Vec<u32>,
    /// Live keys this layer resolves itself: its own ids, and ids from below
    /// it renamed.
    by_key: HashMap<String, u32>,
    /// Ids deleted, from below or from this layer.
    deleted: HashSet<u32>,
    /// Ids from below whose label this layer replaced.
    relabeled: HashMap<u32, u32>,
    /// Ids from below whose key this layer replaced.
    renamed: HashMap<u32, String>,
    /// `syms.len()` below: the first symbol this layer allocates.
    sym_floor: u32,
    /// Text of each symbol from `sym_floor`.
    syms: Vec<String>,
    by_sym: HashMap<String, u32>,
}

impl NodeLayer {
    /// An empty layer over the state whose ids and symbols are `ids` and
    /// `syms`.
    pub fn new(ids: &IdMap, syms: &Interner) -> Self {
        Self {
            id_floor: ids.len() as u32,
            keys: Vec::new(),
            labels: Vec::new(),
            by_key: HashMap::new(),
            deleted: HashSet::new(),
            relabeled: HashMap::new(),
            renamed: HashMap::new(),
            sym_floor: syms.len() as u32,
            syms: Vec::new(),
            by_sym: HashMap::new(),
        }
    }

    /// The symbol for `name`: the one below if it exists there, else this
    /// layer's, allocated on first use — the id `Interner::intern` would give.
    pub fn intern(&mut self, below: &Interner, name: &str) -> u32 {
        if let Some(sym) = below.get(name) {
            return sym;
        }
        if let Some(&sym) = self.by_sym.get(name) {
            return sym;
        }
        let sym = self.sym_floor + self.syms.len() as u32;
        self.syms.push(name.to_string());
        self.by_sym.insert(name.to_string(), sym);
        sym
    }

    /// Allocate the next dense id for a node `key` with label `label_sym` —
    /// the id `IdMap::try_insert` would give. The caller has checked that
    /// `key` is not live.
    pub fn insert(&mut self, key: &str, label_sym: u32) -> u32 {
        let id = self.id_floor + self.keys.len() as u32;
        self.keys.push(key.to_string());
        self.labels.push(label_sym);
        self.by_key.insert(key.to_string(), id);
        id
    }

    /// Give the live node `id` the label `label_sym`.
    pub fn relabel(&mut self, id: u32, label_sym: u32) {
        match id.checked_sub(self.id_floor) {
            Some(i) => self.labels[i as usize] = label_sym,
            None => {
                self.relabeled.insert(id, label_sym);
            }
        }
    }

    /// Rename the live node `id` from `old` to `new`, keeping its id. The
    /// caller has checked that `new` is not live.
    pub fn rename(&mut self, id: u32, old: &str, new: &str) {
        if self.by_key.get(old) == Some(&id) {
            self.by_key.remove(old);
        }
        match id.checked_sub(self.id_floor) {
            Some(i) => self.keys[i as usize] = new.to_string(),
            None => {
                self.renamed.insert(id, new.to_string());
            }
        }
        self.by_key.insert(new.to_string(), id);
    }

    /// Retire the live node `key`, whose id is `id`.
    pub fn delete(&mut self, key: &str, id: u32) {
        if self.by_key.get(key) == Some(&id) {
            self.by_key.remove(key);
        }
        self.deleted.insert(id);
    }

    #[inline]
    fn is_deleted(&self, id: u32) -> bool {
        !self.deleted.is_empty() && self.deleted.contains(&id)
    }

    /// The label of an id from below, as this layer leaves it.
    #[inline]
    fn below_label(&self, id: u32, below: u32) -> u32 {
        if self.is_deleted(id) {
            return u32::MAX;
        }
        if self.relabeled.is_empty() {
            return below;
        }
        self.relabeled.get(&id).copied().unwrap_or(below)
    }
}

/// Every node slot's label symbol in dense-id order, `u32::MAX` for a deleted
/// or unlabelled slot. See [`GraphView::label_syms`].
pub struct LabelSyms<'a> {
    below: std::slice::Iter<'a, u32>,
    layer: Option<&'a NodeLayer>,
    /// The id of the next slot.
    next: usize,
}

impl Iterator for LabelSyms<'_> {
    type Item = u32;

    #[inline]
    fn next(&mut self) -> Option<u32> {
        let i = self.next;
        self.next += 1;
        if let Some(&sym) = self.below.next() {
            return Some(match self.layer {
                None => sym,
                Some(layer) => layer.below_label(i as u32, sym),
            });
        }
        let layer = self.layer?;
        let floor = layer.id_floor as usize;
        if i < floor {
            // Ids below allocated with no label slot (a gap).
            return Some(u32::MAX);
        }
        let sym = *layer.labels.get(i - floor)?;
        Some(if layer.is_deleted(i as u32) {
            u32::MAX
        } else {
            sym
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let below = self.below.len();
        match self.layer {
            None => (below, Some(below)),
            Some(_) => (below, None),
        }
    }
}

impl<'a> GraphView<'a> {
    /// Returns `true` if `id` is visible under the current mask.
    /// Always `true` when no mask is set.
    #[inline]
    pub fn visible(&self, id: u32) -> bool {
        self.mask.is_none_or(|m| m.contains(id))
    }

    pub fn node_id(&self, key: &str) -> Option<u32> {
        match self.layer {
            None => self.ids.get(key),
            Some(layer) => {
                if let Some(&id) = layer.by_key.get(key) {
                    return Some(id);
                }
                self.ids
                    .get(key)
                    .filter(|id| !layer.is_deleted(*id) && !layer.renamed.contains_key(id))
            }
        }
    }

    pub fn key_of(&self, id: u32) -> &str {
        self.try_key_of(id).expect("dense ids")
    }

    /// The key of `id`, or `None` for an id that is retired or was never
    /// allocated.
    pub fn try_key_of(&self, id: u32) -> Option<&str> {
        match self.layer {
            None => self.ids.key_of(id),
            Some(layer) => {
                if layer.is_deleted(id) {
                    return None;
                }
                match id.checked_sub(layer.id_floor) {
                    Some(i) => layer.keys.get(i as usize).map(String::as_str),
                    None => match layer.renamed.get(&id) {
                        Some(key) => Some(key),
                        None => self.ids.key_of(id),
                    },
                }
            }
        }
    }

    /// One past the highest dense id allocated, live or retired.
    pub fn id_bound(&self) -> u32 {
        match self.layer {
            None => self.ids.len() as u32,
            Some(layer) => layer.id_floor + layer.keys.len() as u32,
        }
    }

    /// The symbol interned for `name`, if any.
    pub fn sym(&self, name: &str) -> Option<u32> {
        self.syms
            .get(name)
            .or_else(|| self.layer.and_then(|l| l.by_sym.get(name).copied()))
    }

    /// The text of symbol `sym`, if it is interned.
    pub fn sym_name(&self, sym: u32) -> Option<&str> {
        match self
            .layer
            .and_then(|l| sym.checked_sub(l.sym_floor).map(|i| (l, i)))
        {
            Some((layer, i)) => layer.syms.get(i as usize).map(String::as_str),
            None => self.syms.resolve(sym),
        }
    }

    /// The label symbol of every node slot, in dense-id order (`u32::MAX` for
    /// a deleted or unlabelled slot) — what a label scan walks.
    #[inline]
    pub fn label_syms(&self) -> LabelSyms<'_> {
        LabelSyms {
            below: self.labels.iter(),
            layer: self.layer,
            next: 0,
        }
    }

    /// The label symbol of node `id`, `None` for a deleted or unlabelled slot.
    pub fn label_sym(&self, id: u32) -> Option<u32> {
        let sym = match self.layer {
            None => *self.labels.get(id as usize)?,
            Some(layer) => match id.checked_sub(layer.id_floor) {
                Some(i) if !layer.is_deleted(id) => *layer.labels.get(i as usize)?,
                Some(_) => return None,
                None => layer.below_label(id, *self.labels.get(id as usize)?),
            },
        };
        (sym != u32::MAX).then_some(sym)
    }

    pub fn label_of(&self, id: u32) -> Option<&str> {
        self.sym_name(self.label_sym(id)?)
    }

    /// Indexed lookup of node ids of `label` whose scalar `field` equals
    /// `value`, filtered by the active mask. Returns `None` when no equality
    /// index covers `(label, field)` (the caller should fall back to a scan);
    /// `Some(ids)` — possibly empty — when the index answers the query.
    pub fn nodes_with_prop(&self, label: &str, field: &str, value: &Value) -> Option<Vec<u32>> {
        let index = self.prop_index?;
        // An operand with no index key (a list, a map, a duration) can still
        // equal a stored value, which the index does not hold: scan for it.
        if !index.is_enabled(label, field) || !UnifiedIndex::answers_eq(value) {
            return None;
        }
        let ids = index
            .lookup(label, field, value)
            .into_iter()
            .filter(|&id| self.visible(id))
            .collect();
        Some(ids)
    }

    /// The range index a range or ordered scan of `(label, field)` may read:
    /// `Some` only when a range index is declared on the pair, and the index
    /// describes exactly the nodes this view reads: it is not a reader's
    /// layered index (whose range lookups see only the layer's own entries),
    /// and no node layer (a delta tail, a transaction's writes) lies over the
    /// view. `None` means scan. The caller applies the mask.
    pub fn range_index(&self, label: &str, field: &str) -> Option<&UnifiedIndex> {
        let index = self.prop_index?;
        let usable = self.layer.is_none()
            && !index.is_layered()
            && index.kind_of(label, field) == Some(core_storage::IndexKind::Range);
        usable.then_some(index)
    }

    pub fn nodes_with_label(&self, label: &str) -> Vec<u32> {
        let Some(sym) = self.sym(label) else {
            return Vec::new();
        };
        let matching = |(i, s): (usize, u32)| if s == sym { Some(i as u32) } else { None };
        match self.layer {
            None => self
                .labels
                .iter()
                .copied()
                .enumerate()
                .filter_map(matching)
                .collect(),
            Some(_) => self.label_syms().enumerate().filter_map(matching).collect(),
        }
    }

    /// All non-tombstoned node ids regardless of label.
    ///
    /// Respects the query-scoped mask: when a mask is active only ids present
    /// in the mask are returned, consistent with every other node accessor.
    pub fn nodes_all(&self) -> Vec<u32> {
        let live = |(i, s): (usize, u32)| {
            if s != u32::MAX && self.visible(i as u32) {
                Some(i as u32)
            } else {
                None
            }
        };
        match self.layer {
            None => self
                .labels
                .iter()
                .copied()
                .enumerate()
                .filter_map(live)
                .collect(),
            Some(_) => self.label_syms().enumerate().filter_map(live).collect(),
        }
    }

    /// Look up the property `field` for node `id`.
    ///
    /// Returns `ValueRef::Borrowed` for overlay hits (zero allocation) and
    /// `ValueRef::Owned` for base-section hits (value materialised from
    /// archived data).  Returns `None` when neither overlay nor base has a
    /// value for `(id, field)`.
    pub fn prop(&self, id: u32, field: &str) -> Option<ValueRef<'_>> {
        self.props.get(id, field)
    }
}

#[cfg(test)]
mod tests {
    use super::{GraphView, VisibleSet};
    use core_storage::v8::seam::{ColumnsView, EdgePropsView, TopologyView};
    use core_storage::{ColumnStore, EdgeProps, IdMap, Interner, Topology, Value};

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

        fn view(&self) -> GraphView<'_> {
            GraphView {
                ids: &self.ids,
                syms: &self.syms,
                labels: &self.labels,
                props: ColumnsView::owned(&self.props),
                topo: TopologyView::owned(&self.topo),
                edge_props: EdgePropsView::owned(&self.eprops),
                mask: None,
                prop_index: None,
                layer: None,
            }
        }
    }

    #[test]
    fn prop_returns_none_for_missing() {
        let mut fx = Fx::new();
        let id = fx.add("N", "alice", vec![("age", Value::Int(36))]);
        let v = fx.view();
        assert_eq!(
            v.prop(id, "age").map(|vr| vr.into_value()),
            Some(Value::Int(36))
        );
        assert!(v.prop(id, "missing").is_none());
    }

    #[test]
    fn graph_view_lookups() {
        let mut fx = Fx::new();
        let id = fx.add("Person", "ada", vec![("age", Value::Int(36))]);
        let v = fx.view();
        assert_eq!(v.node_id("ada"), Some(id));
        assert_eq!(v.node_id("zzz"), None);
        assert_eq!(v.key_of(id), "ada");
        assert_eq!(v.label_of(id), Some("Person"));
        assert_eq!(v.label_of(99), None);
        assert_eq!(
            v.prop(id, "age").map(|vr| vr.into_value()),
            Some(Value::Int(36))
        );
        assert_eq!(v.prop(id, "missing"), None);
    }

    #[test]
    fn gap_sentinel_is_not_a_label() {
        let mut fx = Fx::new();
        let kept = fx.add("Person", "ada", vec![]);
        fx.ids.get_or_insert("ghost");
        fx.labels.resize(2, u32::MAX);
        let later = fx.add("Person", "bob", vec![]);
        let v = fx.view();
        assert_eq!(v.label_of(1), None);
        assert_eq!(v.nodes_with_label("Person"), vec![kept, later]);
    }

    #[test]
    fn nodes_with_label_skips_tombstoned_id() {
        let mut fx = Fx::new();
        let ada = fx.add("Person", "ada", vec![]);
        let bob = fx.add("Person", "bob", vec![]);
        fx.ids.delete("ada");
        fx.labels[ada as usize] = u32::MAX;
        let v = fx.view();
        assert_eq!(v.node_id("ada"), None);
        assert_eq!(v.label_of(ada), None);
        assert_eq!(v.nodes_with_label("Person"), vec![bob]);
    }

    #[test]
    fn nodes_with_label_dense_id_order_and_unknown_empty() {
        let mut fx = Fx::new();
        let bob = fx.add("Person", "bob", vec![]);
        let ada = fx.add("Person", "ada", vec![]);
        let _acme = fx.add("Company", "acme", vec![]);
        let v = fx.view();
        assert_eq!(v.nodes_with_label("Person"), vec![bob, ada]);
        assert_eq!(v.nodes_with_label("Person"), vec![0, 1]);
        assert!(v.nodes_with_label("Nope").is_empty());
    }

    #[test]
    fn nodes_all_respects_mask() {
        let mut fx = Fx::new();
        let alice = fx.add("Person", "alice", vec![]);
        let bob = fx.add("Person", "bob", vec![]);
        let carol = fx.add("Person", "carol", vec![]);

        // Unmask: all three visible.
        let v_full = fx.view();
        let all = v_full.nodes_all();
        assert!(all.contains(&alice));
        assert!(all.contains(&bob));
        assert!(all.contains(&carol));

        // Masked: only alice and carol visible.
        let visible: VisibleSet = [alice, carol].into_iter().collect();
        let v_masked = GraphView {
            ids: &fx.ids,
            syms: &fx.syms,
            labels: &fx.labels,
            props: ColumnsView::owned(&fx.props),
            topo: TopologyView::owned(&fx.topo),
            edge_props: EdgePropsView::owned(&fx.eprops),
            mask: Some(&visible),
            prop_index: None,
            layer: None,
        };
        let masked = v_masked.nodes_all();
        assert!(masked.contains(&alice));
        assert!(!masked.contains(&bob), "masked node must be excluded");
        assert!(masked.contains(&carol));
    }

    #[test]
    fn layer_inserts_deletes_and_resurrects_without_touching_below() {
        let mut fx = Fx::new();
        let ada = fx.add("Person", "ada", vec![]);
        let bob = fx.add("Person", "bob", vec![]);
        let mut tx = super::NodeLayer::new(&fx.ids, &fx.syms);
        let company = tx.intern(&fx.syms, "Company");
        let person = tx.intern(&fx.syms, "Person");
        let acme = tx.insert("acme", company);
        tx.delete("ada", ada);
        let ada2 = tx.insert("ada", person);
        let base_view = fx.view();
        let v = GraphView {
            layer: Some(&tx),
            ..fx.view()
        };
        assert_eq!(acme, 2);
        assert_eq!(
            ada2, 3,
            "a re-created key gets a fresh id, as the commit gives it"
        );
        assert_eq!(v.node_id("ada"), Some(ada2));
        assert_eq!(v.try_key_of(ada), None);
        assert_eq!(v.key_of(acme), "acme");
        assert_eq!(v.label_of(acme), Some("Company"));
        assert_eq!(v.id_bound(), 4);
        assert_eq!(v.nodes_with_label("Person"), vec![bob, ada2]);
        assert_eq!(v.nodes_with_label("Company"), vec![acme]);
        assert_eq!(v.nodes_all(), vec![bob, acme, ada2]);
        // Below is untouched.
        assert_eq!(base_view.node_id("ada"), Some(ada));
        assert_eq!(base_view.sym("Company"), None);
        assert_eq!(base_view.nodes_all(), vec![ada, bob]);
    }
}
