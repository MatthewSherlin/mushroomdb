use core_storage::v8::seam::{ColumnsView, TopologyView, ValueRef};
use core_storage::{EdgeProps, IdMap, Interner};
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
    pub edge_props: &'a EdgeProps,
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
    use core_storage::v8::seam::{ColumnsView, TopologyView};
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
                edge_props: &self.eprops,
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
}
