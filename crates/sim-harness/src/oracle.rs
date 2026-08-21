use core_api::{Direction, Value};
use core_rules::{evaluate, NodeView, RuleDef, ViewDef};
use core_storage::fulltext::{parse_query, tokenize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// Obviously-correct reference. No ids, no interning, no persistence.
#[derive(Debug, Default, Clone)]
pub struct Oracle {
    nodes: HashMap<String, HashMap<String, Value>>, // key -> props
    labels: HashMap<String, String>,                // key -> label
    node_order: Vec<String>,                        // insertion order = dense id order
    edges: BTreeSet<(String, String, String)>,      // (etype, src, dst) — user-inserted edges
    rules: Vec<RuleDef>,                            // registered rules
    views: Vec<ViewDef>,                            // registered materialized views
    fulltext_enabled: BTreeSet<(String, String)>,   // (label, field) pairs with full-text enabled
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

    /// Register a materialized view. Returns false if a view with the same
    /// name or the same `view_prop` already exists.
    pub fn create_view(&mut self, def: ViewDef) -> bool {
        if self.views.iter().any(|v| v.name == def.name) {
            return false;
        }
        if self.views.iter().any(|v| v.view_prop == def.view_prop) {
            return false;
        }
        self.views.push(def);
        true
    }

    /// Remove a view by name. Returns false if no view with that name exists.
    pub fn delete_view(&mut self, name: &str) -> bool {
        if let Some(pos) = self.views.iter().position(|v| v.name == name) {
            self.views.remove(pos);
            true
        } else {
            false
        }
    }

    /// For a top-k rule (`max_edges: Some(k)`), returns the top-k matching
    /// dst keys for `src_key`, ordered by (score DESC, key ASC).
    ///
    /// This is the oracle invariant for per-source top-k semantics: the derived
    /// set for a source must equal exactly this set at every quiescent point.
    pub fn top_k_dsts_for_src(&self, rule: &RuleDef, k: u64, src_key: &str) -> BTreeSet<String> {
        let src_label = self.labels.get(src_key).map_or("", |l| l.as_str());
        if src_label != rule.src_label {
            return BTreeSet::new();
        }
        let src_props = match self.nodes.get(src_key) {
            Some(p) => p,
            None => return BTreeSet::new(),
        };

        let mut candidates: Vec<(String, f64)> = Vec::new();
        for (dst_key, dst_props) in &self.nodes {
            if dst_key == src_key {
                continue; // no self-edges
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
            if let Some(score) = evaluate(&rule.predicate, &src_view, &dst_view) {
                candidates.push((dst_key.clone(), score));
            }
        }

        // Sort by score DESC, then key ASC for deterministic tiebreak.
        candidates.sort_by(|(ka, sa), (kb, sb)| sb.total_cmp(sa).then_with(|| ka.cmp(kb)));
        candidates
            .into_iter()
            .take(k as usize)
            .map(|(k, _)| k)
            .collect()
    }

    /// Returns user edges ∪ brute-force derived edges as (etype, src_key, dst_key) triples.
    ///
    /// Full O(n²) label-pair scan calling `core_rules::def::evaluate` directly.
    /// Shares nothing with `candidate_spec` / `SideIndex` — incrementality is
    /// the property under test, not scoring.
    ///
    /// For rules with `max_edges: Some(k)`, applies per-source top-k semantics:
    /// each source gets only the top-k matching destinations ordered by
    /// (score DESC, dst_key ASC). For rules with `max_edges: None`, all matching
    /// pairs are included (global-budget not modelled in oracle — oracle assumes
    /// budget is never hit for None rules).
    pub fn all_edges(&self) -> BTreeSet<(String, String, String)> {
        let mut out = self.edges.clone();
        for rule in &self.rules {
            for src_key in &self.node_order {
                let src_label = self.labels.get(src_key).map_or("", |l| l.as_str());
                if src_label != rule.src_label {
                    continue;
                }
                let src_props = match self.nodes.get(src_key) {
                    Some(p) => p,
                    None => continue,
                };

                if let Some(k) = rule.max_edges {
                    // Top-k per-source: filter to best-k dsts.
                    let top_k = self.top_k_dsts_for_src(rule, k, src_key);
                    for dst_key in top_k {
                        out.insert((rule.edge_type.clone(), src_key.clone(), dst_key));
                    }
                } else {
                    // No cap: include all matching dsts.
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
        }
        out
    }

    /// Brute-force scores for rules that store a `weight_prop`. Same O(n²)
    /// `evaluate` scan as `all_edges` — no index sharing.
    pub fn derived_weights(&self) -> BTreeMap<(String, String, String), f64> {
        let mut out = BTreeMap::new();
        for rule in &self.rules {
            if rule.weight_prop.is_none() {
                continue;
            }
            for (src_key, src_props) in &self.nodes {
                let src_label = self.labels.get(src_key).map_or("", |l| l.as_str());
                if src_label != rule.src_label {
                    continue;
                }
                for (dst_key, dst_props) in &self.nodes {
                    if src_key == dst_key {
                        continue;
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
                    if let Some(score) = evaluate(&rule.predicate, &src_view, &dst_view) {
                        out.insert(
                            (rule.edge_type.clone(), src_key.clone(), dst_key.clone()),
                            score,
                        );
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

    // --- Fulltext support ---

    /// Enable full-text indexing for `(label, field)`.
    /// Returns `true` if newly added, `false` if already present.
    pub fn enable_fulltext(&mut self, label: &str, field: &str) -> bool {
        self.fulltext_enabled
            .insert((label.into(), field.into()))
    }

    /// Disable full-text indexing for `(label, field)`.
    /// Returns `true` if it was present and removed, `false` if absent.
    pub fn disable_fulltext(&mut self, label: &str, field: &str) -> bool {
        self.fulltext_enabled
            .remove(&(label.into(), field.into()))
    }

    /// Brute-force full-text search equivalent to `GraphDb::search`.
    ///
    /// Walks all live nodes whose label has `(label, field)` enabled,
    /// tokenizes their `field` value, and counts OR-group matches.
    /// Returns `(key, match_count)` sorted by match_count DESC, key ASC.
    ///
    /// This is the DST oracle: its result must match `db.search(field, query)`
    /// at every quiescent point.
    pub fn scratch_search(&self, field: &str, query: &str) -> Vec<(String, usize)> {
        let groups = parse_query(query);
        if groups.is_empty() {
            return vec![];
        }

        // Labels that currently have `field` indexed.
        let indexed_labels: HashSet<&str> = self
            .fulltext_enabled
            .iter()
            .filter(|(_, f)| f == field)
            .map(|(l, _)| l.as_str())
            .collect();

        if indexed_labels.is_empty() {
            return vec![];
        }

        let mut results: Vec<(String, usize)> = Vec::new();
        for (key, props) in &self.nodes {
            let label = self.labels.get(key).map_or("", |l| l.as_str());
            if !indexed_labels.contains(label) {
                continue;
            }
            // Mirror fulltext.rs value_tokens: tokenize Str, flatten List<Str>,
            // skip everything else.  Both db.search() and oracle.scratch_search
            // must agree on which values produce tokens.
            let doc_tokens: BTreeSet<String> = match props.get(field) {
                Some(Value::Str(s)) => tokenize(s).into_iter().collect(),
                Some(Value::List(items)) => items
                    .iter()
                    .flat_map(|v| {
                        if let Value::Str(s) = v {
                            tokenize(s)
                        } else {
                            vec![]
                        }
                    })
                    .collect(),
                _ => continue,
            };

            // Count how many OR-groups match (mirrors FulltextIndex::search).
            let mut match_count = 0usize;
            for group in &groups {
                let group_matched = group.iter().all(|term| {
                    if term.prefix {
                        doc_tokens.iter().any(|t| t.starts_with(term.token.as_str()))
                    } else {
                        doc_tokens.contains(&term.token)
                    }
                });
                if group_matched {
                    match_count += 1;
                }
            }

            if match_count > 0 {
                results.push((key.clone(), match_count));
            }
        }

        // Sort by match_count DESC, then key ASC (deterministic tiebreak).
        results.sort_by(|(ka, ca), (kb, cb)| cb.cmp(ca).then_with(|| ka.cmp(kb)));
        results
    }

    /// Returns true if (etype, src_key, dst_key) would be derived by any live rule
    /// given current node props and labels.
    ///
    /// For top-k rules (`max_edges: Some(k)`), returns true only if `dst_key` is
    /// within the top-k for `src_key` (score DESC, key ASC tiebreak). A pair that
    /// matches the predicate but is outside the top-k is NOT derived.
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
            if evaluate(&rule.predicate, &src_view, &dst_view).is_none() {
                continue; // predicate doesn't match at all
            }
            if let Some(k) = rule.max_edges {
                // Top-k: check if dst_key is within the top-k for src_key.
                let top_k = self.top_k_dsts_for_src(rule, k, src_key);
                if top_k.contains(dst_key) {
                    return true;
                }
            } else {
                return true; // no cap, any match counts
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
            approximate: false,
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
            approximate: false,
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
            approximate: false,
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
            approximate: false,
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
            approximate: false,
        }));
        let edges = o.all_edges();
        assert!(edges.contains(&("VEC".into(), "a".into(), "b".into())));
        assert!(!edges.contains(&("VEC".into(), "a".into(), "c".into())));
    }
}
