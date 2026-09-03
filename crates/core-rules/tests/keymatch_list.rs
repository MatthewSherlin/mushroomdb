//! `KeyMatch` over a list-valued field: one edge per string element that names
//! a live destination node, retracted per element, back-filled when the target
//! appears later.
//!
//! The fixture mirrors the in-crate engine tests: a hand-built `GraphMut` over
//! the storage primitives, driven through `create_rule` / `on_node_changed` /
//! `on_node_removed`.  No `GraphDb` is involved — that lives in `core-api`,
//! which depends on this crate.

use core_rules::{GraphMut, Predicate, RuleDef, RuleEngine, MAX_KEYMATCH_LIST};
use core_storage::v8::seam::ColumnsView;
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
            props: ColumnsView::owned(&self.props),
            topo: &mut self.topo,
            edge_props: &mut self.eprops,
        }
    }
}

fn strs(items: &[&str]) -> Value {
    Value::List(items.iter().map(|s| Value::Str((*s).into())).collect())
}

/// `File.imports` (a list of module keys) → `IMPORTS` edges onto `Mod` nodes.
fn imports_rule() -> RuleDef {
    RuleDef {
        name: "imports".into(),
        src_label: "File".into(),
        dst_label: "Mod".into(),
        predicate: Predicate::KeyMatch {
            field: "imports".into(),
        },
        edge_type: "IMPORTS".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
    }
}

#[test]
fn fires_one_edge_per_live_target() {
    let mut fx = Fx::new();
    let a = fx.add("Mod", "a.rs", vec![]);
    let b = fx.add("Mod", "b.rs", vec![]);
    // "ghost.rs" has no node: it must contribute nothing.
    let f = fx.add(
        "File",
        "main.rs",
        vec![("imports", strs(&["a.rs", "ghost.rs", "b.rs"]))],
    );
    let mut eng = RuleEngine::new();
    let mut g = fx.g();
    eng.create_rule(imports_rule(), &mut g).unwrap();

    let et = g.syms.get("IMPORTS").unwrap();
    let mut out = g.topo.neighbors(et, Direction::Out, f).to_vec();
    out.sort_unstable();
    assert_eq!(out, vec![a, b], "one edge per live target, ghost excluded");
    assert_eq!(g.topo.edge_count(), 2, "two derived edges");
    assert!(eng.is_owned(et, f, a) && eng.is_owned(et, f, b));
}

#[test]
fn retracts_removed_element_in_same_commit() {
    let mut fx = Fx::new();
    let a = fx.add("Mod", "a.rs", vec![]);
    let b = fx.add("Mod", "b.rs", vec![]);
    let f = fx.add(
        "File",
        "main.rs",
        vec![("imports", strs(&["a.rs", "b.rs"]))],
    );
    let mut eng = RuleEngine::new();
    {
        let mut g = fx.g();
        eng.create_rule(imports_rule(), &mut g).unwrap();
        assert_eq!(g.topo.edge_count(), 2, "two derived edges");
    }

    // Drop "b.rs" from the list — one prop write, one engine notification.
    let old = fx.props.get(f, "imports").cloned();
    fx.props.set(f, "imports", strs(&["a.rs"]));
    let mut g = fx.g();
    eng.on_node_changed(f, Some(("imports", old)), &mut g);

    let et = g.syms.get("IMPORTS").unwrap();
    assert_eq!(
        g.topo.neighbors(et, Direction::Out, f).to_vec(),
        vec![a],
        "the surviving element keeps its edge"
    );
    assert!(
        !eng.is_owned(et, f, b),
        "removed element's edge is retracted"
    );
    assert_eq!(g.topo.edge_count(), 1);
}

#[test]
fn ignores_non_string_and_duplicate_elements() {
    let mut fx = Fx::new();
    let a = fx.add("Mod", "a.rs", vec![]);
    let f = fx.add(
        "File",
        "main.rs",
        vec![(
            "imports",
            Value::List(vec![
                Value::Str("a.rs".into()),
                Value::Int(7),
                Value::Bool(true),
                Value::Float(1.5),
                Value::List(vec![Value::Str("a.rs".into())]),
                Value::Str("a.rs".into()),
            ]),
        )],
    );
    let mut eng = RuleEngine::new();
    let mut g = fx.g();
    eng.create_rule(imports_rule(), &mut g).unwrap();

    let et = g.syms.get("IMPORTS").unwrap();
    assert_eq!(g.topo.neighbors(et, Direction::Out, f).to_vec(), vec![a]);
    assert_eq!(
        g.topo.edge_count(),
        1,
        "non-strings ignored, duplicates collapse to one edge"
    );
}

#[test]
fn target_created_later_backfills() {
    let mut fx = Fx::new();
    let f = fx.add("File", "main.rs", vec![("imports", strs(&["late.rs"]))]);
    let mut eng = RuleEngine::new();
    {
        let mut g = fx.g();
        eng.create_rule(imports_rule(), &mut g).unwrap();
        assert_eq!(g.topo.edge_count(), 0, "no target node yet → no edge");
    }

    let late = fx.add("Mod", "late.rs", vec![]);
    let mut g = fx.g();
    eng.on_node_changed(late, None, &mut g);

    let et = g.syms.get("IMPORTS").unwrap();
    assert_eq!(
        g.topo.neighbors(et, Direction::Out, f).to_vec(),
        vec![late],
        "dst-side probe finds the src whose list names this key"
    );
    assert!(eng.is_owned(et, f, late));
}

#[test]
fn deleting_a_target_retracts_only_its_edge() {
    let mut fx = Fx::new();
    let a = fx.add("Mod", "a.rs", vec![]);
    let b = fx.add("Mod", "b.rs", vec![]);
    let f = fx.add(
        "File",
        "main.rs",
        vec![("imports", strs(&["a.rs", "b.rs"]))],
    );
    let mut eng = RuleEngine::new();
    {
        let mut g = fx.g();
        eng.create_rule(imports_rule(), &mut g).unwrap();
        assert_eq!(g.topo.edge_count(), 2, "two derived edges");
    }
    let mut g = fx.g();
    eng.on_node_removed(b, &mut g);

    let et = g.syms.get("IMPORTS").unwrap();
    assert_eq!(g.topo.neighbors(et, Direction::Out, f).to_vec(), vec![a]);
    assert!(!eng.is_owned(et, f, b));
}

/// Copy of the in-crate `dst_side_keymatch_links_when_c_node_inserted_after_t`:
/// a scalar `KeyMatch` field must behave exactly as it did before lists were
/// understood — index, dst-side probe, ownership.
#[test]
fn scalar_behaviour_is_unchanged() {
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
                approximate: false,
                via_label: None,
                via_edge: None,
                via_dir: None,
            },
            &mut g,
        )
        .unwrap();
        assert_eq!(g.topo.edge_count(), 0, "no C node yet → no edge");
    }
    // Now insert C node "c9" and notify the engine.
    let c9 = fx.add("C", "c9", vec![]);
    {
        let mut g = fx.g();
        eng.on_node_changed(c9, None, &mut g);
        let at = g.syms.get("AT").unwrap();
        assert!(
            g.topo.neighbors(at, Direction::Out, t).contains(&c9),
            "T→C edge must appear when C node is inserted"
        );
        assert!(eng.is_owned(at, t, c9));
        assert_eq!(g.topo.edge_count(), 1, "a scalar FK still yields one edge");
    }
    // Repointing the scalar retracts the old edge and links the new one.
    let c8 = fx.add("C", "c8", vec![]);
    let old = fx.props.get(t, "cid").cloned();
    fx.props.set(t, "cid", Value::Str("c8".into()));
    let mut g = fx.g();
    eng.on_node_changed(t, Some(("cid", old)), &mut g);
    let at = g.syms.get("AT").unwrap();
    assert_eq!(g.topo.neighbors(at, Direction::Out, t).to_vec(), vec![c8]);
    assert_eq!(g.topo.edge_count(), 1);
}

#[test]
fn list_cap_is_deterministic() {
    let over = MAX_KEYMATCH_LIST + 8;
    let mut fx = Fx::new();
    let keys: Vec<String> = (0..over).map(|i| format!("m{i:04}")).collect();
    let ids: Vec<u32> = keys.iter().map(|k| fx.add("Mod", k, vec![])).collect();
    let list = Value::List(keys.iter().map(|k| Value::Str(k.clone())).collect());
    let f = fx.add("File", "main.rs", vec![("imports", list)]);

    let mut eng = RuleEngine::new();
    let mut g = fx.g();
    eng.create_rule(imports_rule(), &mut g).unwrap();

    let et = g.syms.get("IMPORTS").unwrap();
    let out = g.topo.neighbors(et, Direction::Out, f).to_vec();
    assert_eq!(
        out.len(),
        MAX_KEYMATCH_LIST,
        "only the first {MAX_KEYMATCH_LIST} elements are considered"
    );
    assert!(out.contains(&ids[0]), "first element is in stored order");
    assert!(
        out.contains(&ids[MAX_KEYMATCH_LIST - 1]),
        "last element under the cap is considered"
    );
    assert!(
        !out.contains(&ids[MAX_KEYMATCH_LIST]),
        "the first element past the cap is dropped"
    );
}
