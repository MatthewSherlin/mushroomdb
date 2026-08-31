//! Opt-in equality index over scalar node properties.
//!
//! Mirrors [`crate::fulltext::FulltextIndex`]: a set of declared `(label,
//! field)` pairs whose values are indexed for exact-match lookup, maintained
//! incrementally on insert/set/delete and rebuilt on open. It answers
//! "which nodes of label `L` have `field = value`?" in `O(matches)` instead of
//! an `O(N_label)` scan — the hot path behind `WHERE n.field = $x`.
//!
//! Only scalar values are indexable (`Int`, `Float`, `Str`, `Bool` via
//! [`ValueKey`]); list- and map-valued properties are skipped (a list has no
//! single equality key). A per-node reverse map records the current key so an
//! update or delete removes the stale entry in `O(log n)` without knowing the
//! previous value.

use crate::idmap::IdMap;
use crate::interner::Interner;
use crate::types::{Value, ValueKey};
use crate::v8::seam::ColumnsView;
use std::collections::{BTreeMap, BTreeSet};

/// Equality index over declared `(label, field)` pairs.
#[derive(Debug, Clone, Default)]
pub struct PropertyIndex {
    /// Declared `(label, field)` pairs. A pair not present here is not indexed.
    enabled: BTreeSet<(String, String)>,
    /// `(label, field)` → value → set of node ids holding that value.
    forward: BTreeMap<(String, String), BTreeMap<ValueKey, BTreeSet<u32>>>,
    /// `(label, field, id)` → the node's current indexed key, so an update or
    /// delete can drop the stale forward entry without the old value.
    reverse: BTreeMap<(String, String, u32), ValueKey>,
}

impl PropertyIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether `(label, field)` is declared as an index.
    pub fn is_enabled(&self, label: &str, field: &str) -> bool {
        self.enabled
            .contains(&(label.to_string(), field.to_string()))
    }

    /// Whether any declared index covers `field` (any label). Cheap guard for
    /// the maintenance hooks, matching `FulltextIndex::field_indexed`.
    pub fn field_indexed(&self, field: &str) -> bool {
        self.enabled.iter().any(|(_, f)| f == field)
    }

    /// Whether any field is declared for `label`.
    pub fn has_label(&self, label: &str) -> bool {
        self.enabled.iter().any(|(l, _)| l == label)
    }

    /// Iterate all declared `(label, field)` pairs.
    pub fn enabled_pairs(&self) -> impl Iterator<Item = &(String, String)> {
        self.enabled.iter()
    }

    /// Declare `(label, field)` as an index. Returns `true` if newly added.
    pub fn enable(&mut self, label: &str, field: &str) -> bool {
        self.enabled.insert((label.to_string(), field.to_string()))
    }

    /// Remove the `(label, field)` index and all its entries. Returns `true`
    /// if it had been declared.
    pub fn disable(&mut self, label: &str, field: &str) -> bool {
        let key = (label.to_string(), field.to_string());
        let was = self.enabled.remove(&key);
        self.forward.remove(&key);
        self.reverse
            .retain(|(l, f, _), _| !(l == label && f == field));
        was
    }

    /// Upsert node `id`'s value for `(label, field)`. Removes any prior entry
    /// first, then indexes `value` if it is a scalar. A no-op when the pair is
    /// not declared or `value` is a list/map.
    pub fn set(&mut self, label: &str, field: &str, id: u32, value: &Value) {
        if !self.is_enabled(label, field) {
            return;
        }
        self.remove_node(label, field, id);
        if let Some(vk) = ValueKey::from_value(value) {
            self.forward
                .entry((label.to_string(), field.to_string()))
                .or_default()
                .entry(vk.clone())
                .or_default()
                .insert(id);
            self.reverse
                .insert((label.to_string(), field.to_string(), id), vk);
        }
    }

    /// Remove every entry for node `id` across all declared pairs. Used when a
    /// node is deleted (mirrors [`crate::fulltext::FulltextIndex::remove_node`]).
    pub fn remove_node_all(&mut self, id: u32) {
        let pairs: Vec<(String, String)> = self
            .reverse
            .keys()
            .filter(|(_, _, i)| *i == id)
            .map(|(l, f, _)| (l.clone(), f.clone()))
            .collect();
        for (l, f) in pairs {
            self.remove_node(&l, &f, id);
        }
    }

    /// Remove node `id`'s entry for `(label, field)` if present.
    pub fn remove_node(&mut self, label: &str, field: &str, id: u32) {
        let rkey = (label.to_string(), field.to_string(), id);
        if let Some(old) = self.reverse.remove(&rkey) {
            let fkey = (label.to_string(), field.to_string());
            if let Some(by_value) = self.forward.get_mut(&fkey) {
                if let Some(ids) = by_value.get_mut(&old) {
                    ids.remove(&id);
                    if ids.is_empty() {
                        by_value.remove(&old);
                    }
                }
            }
        }
    }

    /// Node ids of `label` whose `field` equals `value`, ascending. Empty when
    /// the pair is not declared, `value` is non-scalar, or nothing matches.
    pub fn lookup(&self, label: &str, field: &str, value: &Value) -> Vec<u32> {
        let Some(vk) = ValueKey::from_value(value) else {
            return Vec::new();
        };
        self.forward
            .get(&(label.to_string(), field.to_string()))
            .and_then(|by_value| by_value.get(&vk))
            .map(|ids| ids.iter().copied().collect())
            .unwrap_or_default()
    }

    /// Rebuild all postings for the declared pairs from live column data.
    /// Called at open end to correct any drift from per-record replay; a no-op
    /// when nothing is declared. Mirrors [`crate::fulltext::FulltextIndex::rebuild_all`].
    pub fn rebuild_all(
        &mut self,
        ids: &IdMap,
        labels: &[u32],
        syms: &Interner,
        props: ColumnsView<'_>,
    ) {
        if self.enabled.is_empty() {
            return;
        }
        let enabled_vec: Vec<(String, String)> = self.enabled.iter().cloned().collect();
        for pair in &enabled_vec {
            self.forward.remove(pair);
        }
        self.reverse
            .retain(|(l, f, _), _| !enabled_vec.iter().any(|(el, ef)| el == l && ef == f));
        let n = ids.len() as u32;
        for id in 0..n {
            let Some(&sym) = labels.get(id as usize) else {
                continue;
            };
            if sym == u32::MAX {
                continue;
            }
            let Some(label) = syms.resolve(sym) else {
                continue;
            };
            for (lbl, field) in &enabled_vec {
                if lbl == label {
                    if let Some(vr) = props.get(id, field) {
                        let value = vr.into_value();
                        self.set(lbl, field, id, &value);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(x: &str) -> Value {
        Value::Str(x.into())
    }

    #[test]
    fn lookup_returns_only_matching_ids_ascending() {
        let mut ix = PropertyIndex::new();
        ix.enable("Person", "city");
        ix.set("Person", "city", 3, &s("austin"));
        ix.set("Person", "city", 1, &s("austin"));
        ix.set("Person", "city", 2, &s("boston"));
        assert_eq!(ix.lookup("Person", "city", &s("austin")), vec![1, 3]);
        assert_eq!(ix.lookup("Person", "city", &s("boston")), vec![2]);
        assert_eq!(
            ix.lookup("Person", "city", &s("nowhere")),
            Vec::<u32>::new()
        );
    }

    #[test]
    fn undeclared_pair_indexes_nothing() {
        let mut ix = PropertyIndex::new();
        // not enabled
        ix.set("Person", "city", 1, &s("austin"));
        assert!(!ix.is_enabled("Person", "city"));
        assert_eq!(ix.lookup("Person", "city", &s("austin")), Vec::<u32>::new());
    }

    #[test]
    fn update_moves_id_to_new_value_bucket() {
        let mut ix = PropertyIndex::new();
        ix.enable("Person", "city");
        ix.set("Person", "city", 1, &s("austin"));
        // change the value
        ix.set("Person", "city", 1, &s("boston"));
        assert_eq!(ix.lookup("Person", "city", &s("austin")), Vec::<u32>::new());
        assert_eq!(ix.lookup("Person", "city", &s("boston")), vec![1]);
    }

    #[test]
    fn remove_node_drops_entry() {
        let mut ix = PropertyIndex::new();
        ix.enable("Person", "city");
        ix.set("Person", "city", 1, &s("austin"));
        ix.set("Person", "city", 2, &s("austin"));
        ix.remove_node("Person", "city", 1);
        assert_eq!(ix.lookup("Person", "city", &s("austin")), vec![2]);
    }

    #[test]
    fn non_scalar_values_are_skipped() {
        let mut ix = PropertyIndex::new();
        ix.enable("Post", "tags");
        ix.set("Post", "tags", 1, &Value::List(vec![s("a"), s("b")]));
        assert_eq!(
            ix.lookup("Post", "tags", &Value::List(vec![s("a")])),
            Vec::<u32>::new()
        );
    }

    #[test]
    fn int_and_bool_keys_index() {
        let mut ix = PropertyIndex::new();
        ix.enable("N", "age");
        ix.enable("N", "active");
        ix.set("N", "age", 1, &Value::Int(30));
        ix.set("N", "age", 2, &Value::Int(30));
        ix.set("N", "active", 1, &Value::Bool(true));
        assert_eq!(ix.lookup("N", "age", &Value::Int(30)), vec![1, 2]);
        assert_eq!(ix.lookup("N", "active", &Value::Bool(true)), vec![1]);
    }

    #[test]
    fn remove_node_all_drops_every_field_for_id() {
        let mut ix = PropertyIndex::new();
        ix.enable("Person", "city");
        ix.enable("Person", "team");
        ix.set("Person", "city", 1, &s("austin"));
        ix.set("Person", "team", 1, &s("blue"));
        ix.set("Person", "city", 2, &s("austin"));
        ix.remove_node_all(1);
        assert_eq!(ix.lookup("Person", "city", &s("austin")), vec![2]);
        assert_eq!(ix.lookup("Person", "team", &s("blue")), Vec::<u32>::new());
    }

    #[test]
    fn disable_clears_entries_and_declaration() {
        let mut ix = PropertyIndex::new();
        ix.enable("Person", "city");
        ix.set("Person", "city", 1, &s("austin"));
        assert!(ix.disable("Person", "city"));
        assert!(!ix.is_enabled("Person", "city"));
        assert_eq!(ix.lookup("Person", "city", &s("austin")), Vec::<u32>::new());
    }
}
