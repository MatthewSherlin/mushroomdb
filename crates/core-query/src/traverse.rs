use crate::view::GraphView;
use core_storage::Direction;
use std::collections::{BTreeSet, VecDeque};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Out,
    In,
    Both,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct EdgeRef {
    pub etype: u32,
    pub src: u32,
    pub dst: u32,
}

/// Typed 1-hop expansion. `etypes=None` → all edge types in the graph (sorted).
/// Deterministic: etype asc, then neighbor asc; `Dir::Both` = Out then In.
/// Dedupe only identical `EdgeRef` triples — Out and In of a pair are distinct.
pub fn expand(view: &GraphView, id: u32, etypes: Option<&[u32]>, dir: Dir) -> Vec<EdgeRef> {
    let types: Vec<u32> = match etypes {
        Some(ts) => {
            let mut v = ts.to_vec();
            v.sort_unstable();
            v.dedup();
            v
        }
        None => view.topo.etypes().collect(),
    };
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for etype in types {
        if matches!(dir, Dir::Out | Dir::Both) {
            for &dst in view.topo.neighbors(etype, Direction::Out, id).as_ref() {
                push_unique(
                    &mut out,
                    &mut seen,
                    EdgeRef {
                        etype,
                        src: id,
                        dst,
                    },
                );
            }
        }
        if matches!(dir, Dir::In | Dir::Both) {
            for &src in view.topo.neighbors(etype, Direction::In, id).as_ref() {
                push_unique(
                    &mut out,
                    &mut seen,
                    EdgeRef {
                        etype,
                        src,
                        dst: id,
                    },
                );
            }
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq)]
pub struct Neighborhood {
    pub nodes: Vec<(u32, u32)>, // (node id, depth), BFS order, start excluded, first-seen depth
    pub edges: Vec<EdgeRef>,    // every edge traversed to reach a node (deduped, sorted)
}

pub fn neighborhood(
    view: &GraphView,
    start: u32,
    depth: u32,
    etypes: Option<&[u32]>,
    dir: Dir,
) -> Neighborhood {
    let mut visited = BTreeSet::new();
    visited.insert(start);
    let mut queue = VecDeque::new();
    queue.push_back((start, 0u32));
    let mut nodes = Vec::new();
    let mut edges = BTreeSet::new();

    while let Some((id, d)) = queue.pop_front() {
        if d >= depth {
            continue;
        }
        for e in expand(view, id, etypes, dir) {
            edges.insert(e);
            let nbr = if e.src == id { e.dst } else { e.src };
            if visited.insert(nbr) {
                let nd = d + 1;
                nodes.push((nbr, nd));
                queue.push_back((nbr, nd));
            }
        }
    }

    Neighborhood {
        nodes,
        edges: edges.into_iter().collect(),
    }
}

fn push_unique(out: &mut Vec<EdgeRef>, seen: &mut BTreeSet<EdgeRef>, e: EdgeRef) {
    if seen.insert(e) {
        out.push(e);
    }
}

#[cfg(test)]
mod tests {
    use super::{expand, neighborhood, Dir, EdgeRef};
    use crate::view::GraphView;
    use core_storage::{ColumnStore, EdgeProps, IdMap, Interner, Topology};

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

        fn add(&mut self, label: &str, key: &str) -> u32 {
            let id = self.ids.get_or_insert(key);
            let sym = self.syms.intern(label);
            self.labels.resize(id as usize + 1, u32::MAX);
            self.labels[id as usize] = sym;
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
                mask: None,
            }
        }
    }

    /// Diamond + shortcut:
    ///   A -KNOWS-> B -KNOWS-> D
    ///   A -KNOWS-> C -KNOWS-> D
    ///   A -LIKES-> D
    struct Diamond {
        fx: Fx,
        a: u32,
        b: u32,
        c: u32,
        d: u32,
        knows: u32,
        likes: u32,
    }

    fn diamond() -> Diamond {
        let mut fx = Fx::new();
        let a = fx.add("Person", "a");
        let b = fx.add("Person", "b");
        let c = fx.add("Person", "c");
        let d = fx.add("Person", "d");
        let knows = fx.syms.intern("KNOWS");
        let likes = fx.syms.intern("LIKES");
        fx.topo.add_edge(knows, a, b);
        fx.topo.add_edge(knows, a, c);
        fx.topo.add_edge(knows, b, d);
        fx.topo.add_edge(knows, c, d);
        fx.topo.add_edge(likes, a, d);
        Diamond {
            fx,
            a,
            b,
            c,
            d,
            knows,
            likes,
        }
    }

    fn e(etype: u32, src: u32, dst: u32) -> EdgeRef {
        EdgeRef { etype, src, dst }
    }

    #[test]
    fn expand_etype_then_neighbor_order_and_both_is_out_then_in() {
        let g = diamond();
        let v = g.fx.view();
        assert_eq!(
            expand(&v, g.a, None, Dir::Out),
            vec![
                e(g.knows, g.a, g.b),
                e(g.knows, g.a, g.c),
                e(g.likes, g.a, g.d)
            ]
        );
        // caller etype order is ignored; result is still etype asc
        assert_eq!(
            expand(&v, g.a, Some(&[g.likes, g.knows]), Dir::Out),
            vec![
                e(g.knows, g.a, g.b),
                e(g.knows, g.a, g.c),
                e(g.likes, g.a, g.d)
            ]
        );
        assert_eq!(
            expand(&v, g.d, Some(&[g.knows]), Dir::In),
            vec![e(g.knows, g.b, g.d), e(g.knows, g.c, g.d)]
        );
        assert_eq!(
            expand(&v, g.b, Some(&[g.knows]), Dir::Both),
            vec![e(g.knows, g.b, g.d), e(g.knows, g.a, g.b)]
        );
        // The same directed triple is visible as Out from src and In from dst.
        assert!(expand(&v, g.a, Some(&[g.knows]), Dir::Out).contains(&e(g.knows, g.a, g.b)));
        assert_eq!(
            expand(&v, g.b, Some(&[g.knows]), Dir::In),
            vec![e(g.knows, g.a, g.b)]
        );
    }

    #[test]
    fn expand_none_uses_topo_etypes_not_interned_symbols() {
        let mut fx = Fx::new();
        // Labels/fields interned between etypes: symbol space ≠ topology etypes.
        let a = fx.add("Person", "a");
        let b = fx.add("Person", "b");
        let knows = fx.syms.intern("KNOWS");
        let _age = fx.syms.intern("age");
        let _company = fx.syms.intern("Company");
        let likes = fx.syms.intern("LIKES");
        fx.topo.add_edge(knows, a, b);
        fx.topo.add_edge(likes, a, b);
        assert!(fx.syms.get("Person").unwrap() < knows);
        assert!(knows < fx.syms.get("age").unwrap());
        assert!(fx.syms.get("age").unwrap() < likes);
        let v = fx.view();
        assert_eq!(
            expand(&v, a, None, Dir::Out),
            vec![e(knows, a, b), e(likes, a, b)]
        );
    }

    #[test]
    fn expand_dedupes_only_identical_triples() {
        let mut g = diamond();
        g.fx.topo.add_edge(g.knows, g.a, g.a); // self-loop: Out and In are the same triple
        let v = g.fx.view();
        let both = expand(&v, g.a, Some(&[g.knows, g.knows]), Dir::Both);
        let self_loop_hits = both.iter().filter(|x| *x == &e(g.knows, g.a, g.a)).count();
        assert_eq!(self_loop_hits, 1);
        assert!(both.contains(&e(g.knows, g.a, g.b)));
        assert!(both.contains(&e(g.knows, g.a, g.c)));
    }

    #[test]
    fn neighborhood_depth0_empty() {
        let g = diamond();
        let v = g.fx.view();
        let n = neighborhood(&v, g.a, 0, None, Dir::Out);
        assert!(n.nodes.is_empty());
        assert!(n.edges.is_empty());
    }

    #[test]
    fn neighborhood_depth1_vs_depth2_and_first_seen() {
        let g = diamond();
        let v = g.fx.view();

        let d1 = neighborhood(&v, g.a, 1, Some(&[g.knows]), Dir::Out);
        assert_eq!(d1.nodes, vec![(g.b, 1), (g.c, 1)]);
        assert_eq!(d1.edges, vec![e(g.knows, g.a, g.b), e(g.knows, g.a, g.c)]);

        let d2 = neighborhood(&v, g.a, 2, Some(&[g.knows]), Dir::Out);
        assert_eq!(d2.nodes, vec![(g.b, 1), (g.c, 1), (g.d, 2)]);
        // every traversed edge, including both diamond legs to D
        assert_eq!(
            d2.edges,
            vec![
                e(g.knows, g.a, g.b),
                e(g.knows, g.a, g.c),
                e(g.knows, g.b, g.d),
                e(g.knows, g.c, g.d),
            ]
        );

        // LIKES shortcut: D is first seen at depth 1, not 2
        let all = neighborhood(&v, g.a, 2, None, Dir::Out);
        assert_eq!(all.nodes, vec![(g.b, 1), (g.c, 1), (g.d, 1)]);
        assert_eq!(
            all.edges,
            vec![
                e(g.knows, g.a, g.b),
                e(g.knows, g.a, g.c),
                e(g.knows, g.b, g.d),
                e(g.knows, g.c, g.d),
                e(g.likes, g.a, g.d),
            ]
        );
    }

    #[test]
    fn neighborhood_dir_in_out_both_and_etype_filter() {
        let g = diamond();
        let v = g.fx.view();

        let inn = neighborhood(&v, g.d, 1, None, Dir::In);
        assert_eq!(inn.nodes, vec![(g.b, 1), (g.c, 1), (g.a, 1)]);
        assert_eq!(
            inn.edges,
            vec![
                e(g.knows, g.b, g.d),
                e(g.knows, g.c, g.d),
                e(g.likes, g.a, g.d),
            ]
        );

        let likes_only = neighborhood(&v, g.a, 2, Some(&[g.likes]), Dir::Out);
        assert_eq!(likes_only.nodes, vec![(g.d, 1)]);
        assert_eq!(likes_only.edges, vec![e(g.likes, g.a, g.d)]);

        let both = neighborhood(&v, g.b, 1, Some(&[g.knows]), Dir::Both);
        assert_eq!(both.nodes, vec![(g.d, 1), (g.a, 1)]);
        assert_eq!(both.edges, vec![e(g.knows, g.a, g.b), e(g.knows, g.b, g.d)]);
    }

    #[test]
    fn neighborhood_is_deterministic() {
        let g = diamond();
        let v = g.fx.view();
        let x = neighborhood(&v, g.a, 2, None, Dir::Both);
        let y = neighborhood(&v, g.a, 2, None, Dir::Both);
        assert_eq!(x, y);
        assert_eq!(
            expand(&v, g.a, None, Dir::Both),
            expand(&v, g.a, None, Dir::Both)
        );
    }
}
