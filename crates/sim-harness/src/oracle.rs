use core_api::{Direction, Value};
use core_rules::{evaluate, NodeView, RuleDef};
use std::collections::{BTreeSet, HashMap};

/// Obviously-correct reference. No ids, no interning, no persistence.
#[derive(Debug, Default)]
pub struct Oracle {
    nodes: HashMap<String, HashMap<String, Value>>, // key -> props
    labels: HashMap<String, String>,                // key -> label
    node_order: Vec<String>,                        // insertion order = dense id order
    edges: BTreeSet<(String, String, String)>,      // (etype, src, dst) — user-inserted edges
    rules: Vec<RuleDef>,                            // registered rules
}

impl Oracle {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert_node(&mut self, label: &str, key: &str, props: &[(String, Value)]) -> bool {
        if self.nodes.contains_key(key) {
            return false;
        }
        self.nodes
            .insert(key.into(), props.iter().cloned().collect());
        self.labels.insert(key.into(), label.into());
        self.node_order.push(key.into());
        true
    }

    pub fn has_node(&self, key: &str) -> bool {
        self.nodes.contains_key(key)
    }

    pub fn has_user_edge(&self, etype: &str, src: &str, dst: &str) -> bool {
        self.edges
            .contains(&(etype.to_string(), src.to_string(), dst.to_string()))
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

    // --- Rule support ---

    /// Register a rule. Returns false if a rule with the same name already exists
    /// or if the rule definition is invalid.
    pub fn create_rule(&mut self, def: RuleDef) -> bool {
        if def.validate().is_err() {
            return false;
        }
        if self.rules.iter().any(|r| r.name == def.name) {
            return false;
        }
        self.rules.push(def);
        true
    }

    /// Remove a rule by name. Returns false if no rule with that name exists.
    pub fn delete_rule(&mut self, name: &str) -> bool {
        if let Some(pos) = self.rules.iter().position(|r| r.name == name) {
            self.rules.remove(pos);
            true
        } else {
            false
        }
    }

    /// Returns user edges ∪ brute-force derived edges as (etype, src_key, dst_key) triples.
    ///
    /// The oracle reuses `core_rules::evaluate` as shared scoring truth because
    /// INCREMENTALITY is the property under test, not scoring. The oracle's independence
    /// comes from recomputing all derived edges from scratch on every call, making it
    /// immune to any incremental-update bugs in the engine.
    pub fn all_edges(&self) -> BTreeSet<(String, String, String)> {
        let mut out = self.edges.clone();
        for rule in &self.rules {
            for (src_key, src_props) in &self.nodes {
                let src_label = self.labels.get(src_key).map_or("", |l| l.as_str());
                if src_label != rule.src_label {
                    continue;
                }
                for (dst_key, dst_props) in &self.nodes {
                    if src_key == dst_key {
                        continue; // skip self-pairs
                    }
                    let dst_label = self.labels.get(dst_key).map_or("", |l| l.as_str());
                    if dst_label != rule.dst_label {
                        continue;
                    }
                    let sp = |f: &str| src_props.get(f).cloned();
                    let dp = |f: &str| dst_props.get(f).cloned();
                    let src_view = NodeView {
                        key: src_key,
                        props: &sp,
                    };
                    let dst_view = NodeView {
                        key: dst_key,
                        props: &dp,
                    };
                    if evaluate(&rule.predicate, &src_view, &dst_view).is_some() {
                        out.insert((
                            rule.edge_type.clone(),
                            src_key.clone(),
                            dst_key.clone(),
                        ));
                    }
                }
            }
        }
        out
    }

    /// Returns true if (etype, src_key, dst_key) would be derived by any live rule
    /// given current node props and labels.
    pub fn is_derived_edge(&self, etype: &str, src_key: &str, dst_key: &str) -> bool {
        if src_key == dst_key {
            return false;
        }
        for rule in &self.rules {
            if rule.edge_type != etype {
                continue;
            }
            let src_label = self.labels.get(src_key).map_or("", |l| l.as_str());
            if src_label != rule.src_label {
                continue;
            }
            let dst_label = self.labels.get(dst_key).map_or("", |l| l.as_str());
            if dst_label != rule.dst_label {
                continue;
            }
            let Some(src_props) = self.nodes.get(src_key) else {
                continue;
            };
            let Some(dst_props) = self.nodes.get(dst_key) else {
                continue;
            };
            let sp = |f: &str| src_props.get(f).cloned();
            let dp = |f: &str| dst_props.get(f).cloned();
            let src_view = NodeView {
                key: src_key,
                props: &sp,
            };
            let dst_view = NodeView {
                key: dst_key,
                props: &dp,
            };
            if evaluate(&rule.predicate, &src_view, &dst_view).is_some() {
                return true;
            }
        }
        false
    }
}
