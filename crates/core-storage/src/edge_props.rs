use crate::types::Value;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Edge-property overlay.
///
/// After a V8 snapshot open the base properties live in the mmap'd section 5
/// (`ArchivedEdgeProps`).  Only post-snapshot changes land here.  Deletions of
/// base-only edges are recorded in `tombstones` so the view layer can mask them
/// without materialising the full base into RAM.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct EdgeProps {
    map: BTreeMap<(u32, u32, u32), BTreeMap<String, Value>>,
    /// (etype, src, dst) tuples that have been deleted from the overlay *or* the
    /// base.  An entry here masks any archived data for the same key.
    /// Not persisted via serde (bincode): tombstones are ephemeral in-memory
    /// overlay state used only during encode_v8 merge; they are never written
    /// to disk by themselves.  Skipping preserves V5–V7 bincode wire shapes.
    #[serde(skip)]
    tombstones: BTreeSet<(u32, u32, u32)>,
}

impl EdgeProps {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&mut self, etype: u32, src: u32, dst: u32, field: &str, value: Value) {
        // A set un-tombstones the edge (it is being (re-)created).
        self.tombstones.remove(&(etype, src, dst));
        self.map
            .entry((etype, src, dst))
            .or_default()
            .insert(field.to_owned(), value);
    }

    pub fn get(&self, etype: u32, src: u32, dst: u32, field: &str) -> Option<&Value> {
        self.map.get(&(etype, src, dst))?.get(field)
    }

    /// Remove an edge's props from the overlay and record a tombstone so that
    /// archive lookups for this key are also masked.
    pub fn remove_edge(&mut self, etype: u32, src: u32, dst: u32) {
        self.map.remove(&(etype, src, dst));
        self.tombstones.insert((etype, src, dst));
    }

    /// True if `(etype, src, dst)` is tombstoned (deleted from overlay or base).
    pub fn is_tombstoned(&self, etype: u32, src: u32, dst: u32) -> bool {
        self.tombstones.contains(&(etype, src, dst))
    }

    /// True when this overlay has no entries and no tombstones (i.e. nothing
    /// has changed since the last snapshot).  Used to short-circuit the
    /// edge-props section passthrough during snapshot merging.
    pub fn is_clean(&self) -> bool {
        self.map.is_empty() && self.tombstones.is_empty()
    }

    /// Return all overlay entries as a sorted Vec of (etype, src, dst, &props).
    /// Sorted by (etype, src, dst) ascending — BTreeMap iteration is already sorted.
    pub fn sorted_entries(
        &self,
    ) -> Vec<(
        u32,
        u32,
        u32,
        &std::collections::BTreeMap<String, crate::types::Value>,
    )> {
        self.map
            .iter()
            .map(|((et, s, d), props)| (*et, *s, *d, props))
            .collect()
    }

    /// Iterate tombstoned keys, used when merging with a base section.
    pub fn tombstoned_keys(&self) -> impl Iterator<Item = (u32, u32, u32)> + '_ {
        self.tombstones.iter().copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get_remove_edge_props() {
        let mut e = EdgeProps::new();
        e.set(0, 1, 2, "score", Value::Float(0.5));
        e.set(0, 1, 2, "score", Value::Float(0.7)); // overwrite
        assert_eq!(e.get(0, 1, 2, "score"), Some(&Value::Float(0.7)));
        assert_eq!(e.get(0, 2, 1, "score"), None); // directed
        e.remove_edge(0, 1, 2);
        assert_eq!(e.get(0, 1, 2, "score"), None);
    }

    #[test]
    fn tombstone_masks_base_check() {
        let mut e = EdgeProps::new();
        // Initially clean.
        assert!(e.is_clean());
        // Add an entry — no longer clean.
        e.set(0, 1, 2, "score", Value::Float(1.0));
        assert!(!e.is_clean());
        // Remove it — tombstone remains, still not clean.
        e.remove_edge(0, 1, 2);
        assert!(!e.is_clean());
        assert!(e.is_tombstoned(0, 1, 2));
        assert!(!e.is_tombstoned(0, 2, 1));
    }

    #[test]
    fn set_clears_tombstone() {
        let mut e = EdgeProps::new();
        e.set(0, 1, 2, "score", Value::Float(1.0));
        e.remove_edge(0, 1, 2);
        assert!(e.is_tombstoned(0, 1, 2));
        // Re-set: tombstone cleared, value visible again.
        e.set(0, 1, 2, "score", Value::Float(2.0));
        assert!(!e.is_tombstoned(0, 1, 2));
        assert_eq!(e.get(0, 1, 2, "score"), Some(&Value::Float(2.0)));
    }
}
