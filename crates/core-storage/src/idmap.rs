use crate::types::{GraphError, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};

fn dense_id(len: usize) -> Result<u32> {
    u32::try_from(len).map_err(|_| GraphError::Corrupt {
        detail: "id space exhausted".into(),
    })
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct IdMap {
    to_id: HashMap<String, u32>,
    to_key: Vec<String>,
    /// Dense ids permanently retired by `delete`. Never reused.
    tombstones: BTreeSet<u32>,
}

impl IdMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Convenience wrapper around [`Self::try_insert`] for call-sites that do not
    /// return `Result`.  Panics only when the u32 id space is exhausted (> 4 billion
    /// distinct node keys inserted without restart).  Converting this to return
    /// `Result<u32>` requires a public API change — tracked as a TODO-0.4.2 task.
    pub fn get_or_insert(&mut self, key: &str) -> u32 {
        self.try_insert(key).expect("id space exhausted")
    }

    /// Allocate a dense id for `key`, or return the existing live id.
    /// Fails before wrap when the next id would not fit in `u32`.
    pub fn try_insert(&mut self, key: &str) -> Result<u32> {
        if let Some(&id) = self.to_id.get(key) {
            return Ok(id);
        }
        let id = dense_id(self.to_key.len())?;
        self.to_id.insert(key.to_string(), id);
        self.to_key.push(key.to_string());
        Ok(id)
    }

    pub fn get(&self, key: &str) -> Option<u32> {
        // to_id is cleared on delete so this naturally returns None for deleted keys.
        self.to_id.get(key).copied()
    }

    pub fn key_of(&self, id: u32) -> Option<&str> {
        if self.tombstones.contains(&id) {
            return None;
        }
        self.to_key.get(id as usize).map(|s| s.as_str())
    }

    /// Like `key_of`, but also resolves tombstoned ids.
    ///
    /// Use only for historical WAL scan paths (e.g. `edge_history`) where the
    /// goal is to reconstruct what existed in the past, not the current live
    /// state. All other callers should use `key_of`.
    pub fn key_of_historical(&self, id: u32) -> Option<&str> {
        self.to_key.get(id as usize).map(|s| s.as_str())
    }

    /// Rename a live key, keeping its dense id stable.
    ///
    /// Returns `Err(KeyNotFound)` if `old` is unknown or tombstoned.
    /// Returns `Err(DuplicateKey)` if `new` is already a live key.
    /// On success returns the stable id shared by both names.
    pub fn rename(&mut self, old: &str, new: &str) -> Result<u32> {
        if self.to_id.contains_key(new) {
            return Err(GraphError::DuplicateKey { key: new.into() });
        }
        let id = self
            .to_id
            .remove(old)
            .ok_or_else(|| GraphError::KeyNotFound { key: old.into() })?;
        self.to_id.insert(new.to_string(), id);
        self.to_key[id as usize] = new.to_string();
        Ok(id)
    }

    /// Remove `key` from the live map, permanently tombstone its dense id, and
    /// return that id. Returns `None` if the key is not present.
    pub fn delete(&mut self, key: &str) -> Option<u32> {
        let id = self.to_id.remove(key)?;
        self.tombstones.insert(id);
        Some(id)
    }

    /// Returns `true` if `id` has been retired by a prior `delete` call.
    pub fn is_tombstoned(&self, id: u32) -> bool {
        self.tombstones.contains(&id)
    }

    /// Number of total id slots ever allocated (live + tombstoned). Stable across
    /// deletes and re-inserts — use `live_len` for the live count.
    pub fn len(&self) -> usize {
        self.to_key.len()
    }

    /// All allocated key slots in dense-id order (index = id).
    ///
    /// Tombstoned slots retain their original key string so the V8 encoder
    /// can round-trip the full allocation history.  Callers must check
    /// `is_tombstoned(id)` to distinguish live from retired slots.
    pub(crate) fn all_keys(&self) -> &[String] {
        &self.to_key
    }

    pub fn is_empty(&self) -> bool {
        self.to_key.is_empty()
    }

    /// Number of currently live (non-tombstoned) entries.
    pub fn live_len(&self) -> usize {
        self.to_id.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_dense_and_stable() {
        let mut m = IdMap::new();
        assert_eq!(m.get_or_insert("a"), 0);
        assert_eq!(m.get_or_insert("b"), 1);
        assert_eq!(m.get_or_insert("a"), 0); // idempotent
        assert_eq!(m.get("b"), Some(1));
        assert_eq!(m.get("zzz"), None);
        assert_eq!(m.key_of(1), Some("b"));
        assert_eq!(m.key_of(9), None);
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn survives_serde_roundtrip() {
        let mut m = IdMap::new();
        m.get_or_insert("x");
        let back: IdMap = bincode::deserialize(&bincode::serialize(&m).unwrap()).unwrap();
        assert_eq!(back.get("x"), Some(0));
        assert_eq!(back.len(), 1);
    }

    #[test]
    fn delete_makes_key_invisible_and_id_tombstoned() {
        let mut m = IdMap::new();
        let id = m.get_or_insert("alice");
        // delete returns the dead id
        assert_eq!(m.delete("alice"), Some(id));
        // key is gone
        assert_eq!(m.get("alice"), None);
        // id is tombstoned
        assert!(m.is_tombstoned(id));
        assert_eq!(m.key_of(id), None);
        // deleting absent key → None
        assert_eq!(m.delete("nobody"), None);
    }

    #[test]
    fn reinsert_after_delete_gets_fresh_id() {
        let mut m = IdMap::new();
        let dead_id = m.get_or_insert("alice");
        m.delete("alice");
        let new_id = m.get_or_insert("alice");
        assert_ne!(new_id, dead_id);
        // old id still tombstoned
        assert!(m.is_tombstoned(dead_id));
        // new id is live
        assert!(!m.is_tombstoned(new_id));
        assert_eq!(m.get("alice"), Some(new_id));
        assert_eq!(m.key_of(new_id), Some("alice"));
    }

    #[test]
    fn live_len_tracks_live_entries() {
        let mut m = IdMap::new();
        m.get_or_insert("a");
        m.get_or_insert("b");
        assert_eq!(m.live_len(), 2);
        m.delete("a");
        assert_eq!(m.live_len(), 1);
        // len() is total slots ever allocated
        assert_eq!(m.len(), 2);
        // re-insert "a" → new slot, live_len back to 2, len = 3
        m.get_or_insert("a");
        assert_eq!(m.live_len(), 2);
        assert_eq!(m.len(), 3);
    }

    #[test]
    fn serde_roundtrip_preserves_tombstones() {
        let mut m = IdMap::new();
        m.get_or_insert("x");
        let dead = m.get_or_insert("y");
        m.delete("y");
        let bytes = bincode::serialize(&m).unwrap();
        let back: IdMap = bincode::deserialize(&bytes).unwrap();
        assert!(back.is_tombstoned(dead));
        assert_eq!(back.get("y"), None);
        assert_eq!(back.key_of(dead), None);
        assert_eq!(back.live_len(), 1);
        assert_eq!(back.len(), 2);
    }

    #[test]
    fn try_insert_fails_when_u32_space_exhausted() {
        assert!(dense_id(u32::MAX as usize + 1).is_err());
        assert_eq!(dense_id(0).unwrap(), 0);
        assert_eq!(dense_id(u32::MAX as usize).unwrap(), u32::MAX);
        let mut m = IdMap::new();
        assert_eq!(m.try_insert("a").unwrap(), 0);
        assert_eq!(m.try_insert("a").unwrap(), 0);
    }
}
