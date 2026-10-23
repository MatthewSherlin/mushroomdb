use crate::types::Value;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Edge-property overlay.
///
/// After a V8 snapshot open the base properties live in the mmap'd section 5
/// (`ArchivedEdgeProps`).  Only post-snapshot changes land here.  Deletions of
/// base-only edges are recorded in `tombstones` so the view layer can mask them
/// without materialising the full base into RAM.
///
/// For one key the overlay is in one of three states, and every reader (the
/// views in `v8::seam`, the snapshot merge in `v8::encode`) reads them alike:
///
/// - **no entry, no tombstone**: the layer below answers;
/// - **an entry, no tombstone**: the entry's fields win over the layer below,
///   whose other fields still show (a field-wise merge);
/// - **tombstoned** (with or without an entry): the layer below is hidden for
///   the key, and the entry, if any, is all of the edge's properties. This is
///   a deleted edge (no entry), a deleted edge written again, or an edge one of
///   whose properties was removed ([`EdgeProps::replace`]).
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
    /// Every key `set` or `remove_edge` has written since the last
    /// [`take_touched`](Self::take_touched): the journal a reader's delta tail
    /// is built from, since a rule writes edge properties (its weights) with no
    /// WAL record of its own. In-memory only, like `tombstones`.
    #[serde(skip)]
    touched: BTreeSet<(u32, u32, u32)>,
}

/// One edge's overlay entry as it stands: its properties, if the overlay holds
/// any, and whether it is tombstoned. See [`EdgeProps::entry`].
pub type EdgePropsEntry = (Option<BTreeMap<String, Value>>, bool);

impl EdgeProps {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set one field. A tombstone stays: after a delete (or a
    /// [`replace`](Self::replace)) the entry is the whole of the edge's
    /// properties, so a field written now must not bring back the fields the
    /// layer below still holds for the key.
    pub fn set(&mut self, etype: u32, src: u32, dst: u32, field: &str, value: Value) {
        self.touched.insert((etype, src, dst));
        self.map
            .entry((etype, src, dst))
            .or_default()
            .insert(field.to_owned(), value);
    }

    pub fn get(&self, etype: u32, src: u32, dst: u32, field: &str) -> Option<&Value> {
        self.map.get(&(etype, src, dst))?.get(field)
    }

    /// The overlay's own properties for one edge, if it has any.
    pub fn props_of(
        &self,
        etype: u32,
        src: u32,
        dst: u32,
    ) -> Option<&std::collections::BTreeMap<String, Value>> {
        self.map.get(&(etype, src, dst))
    }

    /// Remove an edge's props from the overlay and record a tombstone so that
    /// archive lookups for this key are also masked.
    pub fn remove_edge(&mut self, etype: u32, src: u32, dst: u32) {
        self.map.remove(&(etype, src, dst));
        self.tombstones.insert((etype, src, dst));
        self.touched.insert((etype, src, dst));
    }

    /// Make `props` the whole of the edge's properties, hiding whatever the
    /// layer below holds for it: how one property is removed when the layer
    /// below may still hold it. `props` is the edge's full map as a read of it
    /// sees it, minus what the caller removes.
    pub fn replace(&mut self, etype: u32, src: u32, dst: u32, props: BTreeMap<String, Value>) {
        let key = (etype, src, dst);
        if props.is_empty() {
            self.map.remove(&key);
        } else {
            self.map.insert(key, props);
        }
        self.tombstones.insert(key);
        self.touched.insert(key);
    }

    /// Whether any key was written since the last [`take_touched`](Self::take_touched).
    pub fn has_touched(&self) -> bool {
        !self.touched.is_empty()
    }

    /// The keys written since the last call, emptying the journal.
    pub fn take_touched(&mut self) -> BTreeSet<(u32, u32, u32)> {
        std::mem::take(&mut self.touched)
    }

    /// One edge's overlay entry exactly as it stands.
    pub fn entry(&self, etype: u32, src: u32, dst: u32) -> EdgePropsEntry {
        let key = (etype, src, dst);
        (self.map.get(&key).cloned(), self.tombstones.contains(&key))
    }

    /// Make one edge's overlay entry exactly `entry`, as [`Self::entry`] read
    /// it elsewhere. Not journalled: it copies a state, it does not write one.
    pub fn restore_entry(&mut self, etype: u32, src: u32, dst: u32, entry: &EdgePropsEntry) {
        let key = (etype, src, dst);
        match &entry.0 {
            Some(props) => {
                self.map.insert(key, props.clone());
            }
            None => {
                self.map.remove(&key);
            }
        }
        if entry.1 {
            self.tombstones.insert(key);
        } else {
            self.tombstones.remove(&key);
        }
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
    fn writes_are_journalled_and_an_entry_restores_exactly() {
        let mut e = EdgeProps::new();
        assert!(!e.has_touched());
        e.set(0, 1, 2, "score", Value::Float(1.0));
        e.set(0, 3, 4, "score", Value::Float(2.0));
        e.remove_edge(0, 3, 4);
        assert!(e.has_touched());
        let touched = e.take_touched();
        assert_eq!(
            touched.into_iter().collect::<Vec<_>>(),
            [(0, 1, 2), (0, 3, 4)]
        );
        assert!(!e.has_touched());

        let mut copy = EdgeProps::new();
        copy.set(0, 3, 4, "stale", Value::Int(9));
        for (et, s, d) in [(0, 1, 2), (0, 3, 4)] {
            copy.restore_entry(et, s, d, &e.entry(et, s, d));
        }
        assert_eq!(copy.get(0, 1, 2, "score"), Some(&Value::Float(1.0)));
        assert_eq!(copy.get(0, 3, 4, "stale"), None);
        assert!(copy.is_tombstoned(0, 3, 4));
        assert!(!copy.is_tombstoned(0, 1, 2));
    }

    /// A field written after a delete is visible, and the tombstone stays, so
    /// the fields the layer below holds for the deleted edge stay hidden.
    #[test]
    fn set_after_a_delete_keeps_the_tombstone() {
        let mut e = EdgeProps::new();
        e.set(0, 1, 2, "score", Value::Float(1.0));
        e.remove_edge(0, 1, 2);
        assert!(e.is_tombstoned(0, 1, 2));
        e.set(0, 1, 2, "score", Value::Float(2.0));
        assert!(e.is_tombstoned(0, 1, 2));
        assert_eq!(e.get(0, 1, 2, "score"), Some(&Value::Float(2.0)));
    }

    #[test]
    fn replace_is_the_whole_map_and_hides_the_layer_below() {
        let mut e = EdgeProps::new();
        let props: BTreeMap<String, Value> = [("a".to_string(), Value::Int(1))].into();
        e.replace(0, 1, 2, props.clone());
        assert!(e.is_tombstoned(0, 1, 2));
        assert_eq!(e.props_of(0, 1, 2), Some(&props));
        e.replace(0, 1, 2, BTreeMap::new());
        assert!(e.is_tombstoned(0, 1, 2));
        assert_eq!(e.props_of(0, 1, 2), None);
        assert_eq!(e.take_touched().len(), 1);
    }
}
