use core_storage::{ColumnStore, EdgeProps, IdMap, Interner, Topology, Value};

/// Read-only twin of `GraphMut`. Holds only borrowed graph state.
pub struct GraphView<'a> {
    pub ids: &'a IdMap,
    pub syms: &'a Interner,
    pub labels: &'a [u32],
    pub props: &'a ColumnStore,
    pub topo: &'a Topology,
    pub edge_props: &'a EdgeProps,
}

impl<'a> GraphView<'a> {
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

    pub fn prop(&self, id: u32, field: &str) -> Option<&Value> {
        self.props.get(id, field)
    }
}

#[cfg(test)]
mod tests {
    use super::GraphView;
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
                props: &self.props,
                topo: &self.topo,
                edge_props: &self.eprops,
            }
        }
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
    fn graph_view_lookups() {
        let mut fx = Fx::new();
        let id = fx.add("Person", "ada", vec![("age", Value::Int(36))]);
        let v = fx.view();
        assert_eq!(v.node_id("ada"), Some(id));
        assert_eq!(v.node_id("zzz"), None);
        assert_eq!(v.key_of(id), "ada");
        assert_eq!(v.label_of(id), Some("Person"));
        assert_eq!(v.label_of(99), None);
        assert_eq!(v.prop(id, "age"), Some(&Value::Int(36)));
        assert_eq!(v.prop(id, "missing"), None);
    }

    #[test]
    fn gap_sentinel_is_not_a_label() {
        let mut fx = Fx::new();
        let kept = fx.add("Person", "ada", vec![]);
        // Simulate a dense-id hole: next slot exists but holds the gap sentinel.
        fx.ids.get_or_insert("ghost");
        fx.labels.resize(2, u32::MAX);
        let later = fx.add("Person", "bob", vec![]);
        let v = fx.view();
        assert_eq!(v.label_of(1), None);
        assert_eq!(v.nodes_with_label("Person"), vec![kept, later]);
    }
}
