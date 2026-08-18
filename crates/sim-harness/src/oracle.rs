use core_api::{Direction, Value};
use core_rules::{evaluate, NodeView, RuleDef};
use std::collections::{BTreeSet, HashMap};

/// Obviously-correct reference. No ids, no interning, no persistence.
#[derive(Debug, Default, Clone)]
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
    /// Full O(n²) label-pair scan calling `core_rules::def::evaluate` directly.
    /// Shares nothing with `candidate_spec` / `SideIndex` — incrementality is
    /// the property under test, not scoring. New Plan-7 predicates
    /// (`NumericWithin`, `GeoRadius`, `VectorSimilar`) are covered automatically
    /// because `evaluate` is the sole match authority.
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
                        out.insert((rule.edge_type.clone(), src_key.clone(), dst_key.clone()));
                    }
                }
            }
        }
        out
    }

    /// Remove a live node and every user edge touching it. The key is gone
    /// (`has_node` is false). Re-inserting the same key is a fresh identity:
    /// a new slot is appended to `node_order` so remaining nodes keep their
    /// dense-id ranks (the vacated slot is a tombstone). Derived edges are
    /// not stored — `all_edges` recomputes from live nodes, so retraction is
    /// automatic.
    pub fn delete_node(&mut self, key: &str) -> bool {
        if self.nodes.remove(key).is_none() {
            return false;
        }
        self.labels.remove(key);
        self.edges.retain(|(_, s, d)| s != key && d != key);
        true
    }

    /// Delete a user edge. `None` = a key is missing (`KeyNotFound`).
    /// `Some(None)` = a live rule would derive this pair (`RuleOwned`) —
    /// mirrors the engine: the rule would just put the edge back.
    /// `Some(Some(removed))` = user-edge outcome (`true` deleted, `false` absent).
    pub fn delete_edge(&mut self, etype: &str, src: &str, dst: &str) -> Option<Option<bool>> {
        if !self.nodes.contains_key(src) || !self.nodes.contains_key(dst) {
            return None;
        }
        if self.is_derived_edge(etype, src, dst) {
            return Some(None);
        }
        Some(Some(self.edges.remove(&(
            etype.into(),
            src.into(),
            dst.into(),
        ))))
    }

    /// Remove a property. `None` = unknown key; `Some(false)` = field already
    /// absent; `Some(true)` = removed. Retraction falls out of `all_edges`.
    pub fn remove_prop(&mut self, key: &str, field: &str) -> Option<bool> {
        Some(self.nodes.get_mut(key)?.remove(field).is_some())
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

#[cfg(test)]
mod tests {
    use super::*;
    use core_rules::{Predicate, RuleDef};

    fn fe_rule() -> RuleDef {
        RuleDef {
            name: "r".into(),
            src_label: "L".into(),
            dst_label: "L".into(),
            predicate: Predicate::FieldEqual { field: "f".into() },
            edge_type: "FE".into(),
            weight_prop: None,
            max_edges: None,
        }
    }

    #[test]
    fn delete_node_drops_edges_and_reinsert_is_fresh() {
        let mut o = Oracle::new();
        assert!(o.insert_node("L", "a", &[]));
        assert!(o.insert_node("L", "b", &[]));
        assert_eq!(o.insert_edge("E", "a", "b"), Some(true));
        assert!(o.delete_node("a"));
        assert!(!o.has_node("a"));
        assert!(o.has_node("b"));
        assert!(!o.has_user_edge("E", "a", "b"));
        assert!(o.all_edges().is_empty());
        // key gone → re-insert is a new identity; old user edges do not return
        assert!(o.insert_node("L", "a", &[]));
        assert!(o.has_node("a"));
        assert!(!o.has_user_edge("E", "a", "b"));
        assert_eq!(o.node_count(), 2);
        assert_eq!(o.node_order.len(), 3);
    }

    #[test]
    fn delete_edge_rule_owned_when_live_rule_would_derive() {
        let mut o = Oracle::new();
        let props = vec![("f".into(), Value::Int(1))];
        assert!(o.insert_node("L", "a", &props));
        assert!(o.insert_node("L", "b", &props));
        assert!(o.create_rule(fe_rule()));
        assert!(o.is_derived_edge("FE", "a", "b"));
        assert_eq!(o.delete_edge("FE", "a", "b"), Some(None));
        assert!(o
            .all_edges()
            .contains(&("FE".into(), "a".into(), "b".into())));
    }

    #[test]
    fn remove_prop_retracts_via_recompute() {
        let mut o = Oracle::new();
        let props = vec![("f".into(), Value::Int(1))];
        assert!(o.insert_node("L", "a", &props));
        assert!(o.insert_node("L", "b", &props));
        assert!(o.create_rule(fe_rule()));
        assert_eq!(o.all_edges().len(), 2);
        assert_eq!(o.remove_prop("a", "f"), Some(true));
        assert_eq!(o.get_prop("a", "f"), None);
        assert!(o.all_edges().is_empty());
        assert_eq!(o.remove_prop("a", "f"), Some(false));
        assert_eq!(o.remove_prop("missing", "f"), None);
    }

    fn loc(lat: f64, lon: f64) -> Value {
        Value::List(vec![Value::Float(lat), Value::Float(lon)])
    }

    fn emb(xs: &[f64]) -> Value {
        Value::List(xs.iter().copied().map(Value::Float).collect())
    }

    #[test]
    fn numeric_within_cross_type_and_signed_zero() {
        let mut o = Oracle::new();
        o.insert_node("Y", "a", &[("year".into(), Value::Int(1998))]);
        o.insert_node("Y", "b", &[("year".into(), Value::Float(2000.0))]);
        o.insert_node("Y", "z0", &[("year".into(), Value::Float(-0.0))]);
        o.insert_node("Y", "z1", &[("year".into(), Value::Float(0.0))]);
        assert!(o.create_rule(RuleDef {
            name: "nw".into(),
            src_label: "Y".into(),
            dst_label: "Y".into(),
            predicate: Predicate::NumericWithin {
                field: "year".into(),
                tolerance: 3.0,
            },
            edge_type: "NW".into(),
            weight_prop: None,
            max_edges: None,
        }));
        assert!(o.create_rule(RuleDef {
            name: "nz".into(),
            src_label: "Y".into(),
            dst_label: "Y".into(),
            predicate: Predicate::NumericWithin {
                field: "year".into(),
                tolerance: 0.0,
            },
            edge_type: "NZ".into(),
            weight_prop: None,
            max_edges: None,
        }));
        let edges = o.all_edges();
        assert!(edges.contains(&("NW".into(), "a".into(), "b".into())));
        assert!(edges.contains(&("NZ".into(), "z0".into(), "z1".into())));
        assert!(edges.contains(&("NZ".into(), "z1".into(), "z0".into())));
        assert!(!edges.contains(&("NZ".into(), "a".into(), "b".into())));
    }

    #[test]
    fn geo_radius_cell_straddle_and_antimeridian() {
        let mut o = Oracle::new();
        o.insert_node("G", "paris", &[("loc".into(), loc(48.8566, 2.3522))]);
        o.insert_node("G", "london", &[("loc".into(), loc(51.5074, -0.1278))]);
        o.insert_node("G", "east", &[("loc".into(), loc(70.0, 179.9))]);
        o.insert_node("G", "west", &[("loc".into(), loc(70.0, -179.9))]);
        o.insert_node("G", "nyc", &[("loc".into(), loc(40.7128, -74.0060))]);
        assert!(o.create_rule(RuleDef {
            name: "geo".into(),
            src_label: "G".into(),
            dst_label: "G".into(),
            predicate: Predicate::GeoRadius {
                field: "loc".into(),
                km: 400.0,
            },
            edge_type: "GEO".into(),
            weight_prop: None,
            max_edges: None,
        }));
        let edges = o.all_edges();
        assert!(edges.contains(&("GEO".into(), "paris".into(), "london".into())));
        assert!(edges.contains(&("GEO".into(), "east".into(), "west".into())));
        assert!(!edges.contains(&("GEO".into(), "paris".into(), "nyc".into())));
    }

    #[test]
    fn vector_similar_near_threshold() {
        let mut o = Oracle::new();
        o.insert_node("V", "a", &[("emb".into(), emb(&[1.0, 0.0]))]);
        o.insert_node(
            "V",
            "b",
            &[("emb".into(), emb(&[0.95, (1.0_f64 - 0.95 * 0.95).sqrt()]))],
        );
        o.insert_node("V", "c", &[("emb".into(), emb(&[0.0, 1.0]))]);
        assert!(o.create_rule(RuleDef {
            name: "vec".into(),
            src_label: "V".into(),
            dst_label: "V".into(),
            predicate: Predicate::VectorSimilar {
                field: "emb".into(),
                min: 0.9,
            },
            edge_type: "VEC".into(),
            weight_prop: None,
            max_edges: None,
        }));
        let edges = o.all_edges();
        assert!(edges.contains(&("VEC".into(), "a".into(), "b".into())));
        assert!(!edges.contains(&("VEC".into(), "a".into(), "c".into())));
    }
}
