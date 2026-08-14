use core_api::{Direction, Value};
use std::collections::{BTreeSet, HashMap};

/// Obviously-correct reference. No ids, no interning, no persistence.
#[derive(Debug, Default)]
pub struct Oracle {
    nodes: HashMap<String, HashMap<String, Value>>, // key -> props
    node_order: Vec<String>,                        // insertion order = dense id order
    edges: BTreeSet<(String, String, String)>,      // (etype, src, dst)
}

impl Oracle {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert_node(&mut self, key: &str, props: &[(String, Value)]) -> bool {
        if self.nodes.contains_key(key) {
            return false;
        }
        self.nodes
            .insert(key.into(), props.iter().cloned().collect());
        self.node_order.push(key.into());
        true
    }

    pub fn insert_edge(&mut self, etype: &str, src: &str, dst: &str) -> Option<bool> {
        if !self.nodes.contains_key(src) || !self.nodes.contains_key(dst) {
            return None; // key-not-found
        }
        Some(self.edges.insert((etype.into(), src.into(), dst.into())))
    }

    pub fn set_prop(&mut self, key: &str, field: &str, value: Value) -> bool {
        match self.nodes.get_mut(key) {
            Some(p) => {
                p.insert(field.into(), value);
                true
            }
            None => false,
        }
    }

    pub fn get_prop(&self, key: &str, field: &str) -> Option<&Value> {
        self.nodes.get(key)?.get(field)
    }

    pub fn neighbors(&self, key: &str, etype: &str, dir: Direction) -> Vec<String> {
        let mut out: Vec<String> = self
            .edges
            .iter()
            .filter(|(t, s, d)| {
                t == etype
                    && match dir {
                        Direction::Out => s == key,
                        Direction::In => d == key,
                    }
            })
            .map(|(_, s, d)| match dir {
                Direction::Out => d.clone(),
                Direction::In => s.clone(),
            })
            .collect();
        // GraphDb returns neighbors sorted by dense internal id == insertion order.
        let rank: HashMap<&str, usize> = self
            .node_order
            .iter()
            .enumerate()
            .map(|(i, k)| (k.as_str(), i))
            .collect();
        out.sort_by_key(|k| rank[k.as_str()]);
        out
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn edge_count(&self) -> u64 {
        self.edges.len() as u64
    }
}
