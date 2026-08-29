use core_storage::v8::seam::{ColumnsView, EdgePropsView, TopologyView, ValueRef};
use core_storage::{IdMap, Interner};
use std::collections::HashSet;

/// Read-only twin of `GraphMut`. Holds only borrowed graph state.
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
    pub mask: Option<&'a HashSet<u32>>,
}

impl<'a> GraphView<'a> {
    /// Returns `true` if `id` is visible under the current mask.
    /// Always `true` when no mask is set.
    #[inline]
    pub fn visible(&self, id: u32) -> bool {
        self.mask.is_none_or(|m| m.contains(&id))
    }

    pub fn node_id(&self, key: &str) -> Option<u32> {
        self.ids.get(key)
    }

    pub fn key_of(&self, id: u32) -> &str {
        self.ids.key_of(id).expect("dense ids")
    }

    pub fn label_of(&self, id: u32) -> Option<&str> {
        let sym = *self.labels.get(id as usize)?;
        if sym == u32::MAX {
            return None;
        }
        self.syms.resolve(sym)
    }

    pub fn nodes_with_label(&self, label: &str) -> Vec<u32> {
        let Some(sym) = self.syms.get(label) else {
            return Vec::new();
        };
        self.labels
            .iter()
            .enumerate()
            .filter_map(|(i, &s)| if s == sym { Some(i as u32) } else { None })
            .collect()
    }

    /// All non-tombstoned node ids regardless of label.
    pub fn nodes_all(&self) -> Vec<u32> {
        self.labels
            .iter()
            .enumerate()
            .filter_map(|(i, &s)| if s != u32::MAX { Some(i as u32) } else { None })
            .collect()
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
    use super::GraphView;
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
}
