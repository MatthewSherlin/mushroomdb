use crate::cow::{CowMap, CowVec, HashIndex};
use crate::types::{GraphError, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};

fn dense_id(len: usize) -> Result<u32> {
    u32::try_from(len).map_err(|_| GraphError::Corrupt {
        detail: "id space exhausted".into(),
    })
}

/// Node keys to dense ids and back.
///
/// Copy-on-write in pieces (`crate::cow`): the write path shares the map with
/// the reader's fold, and the first insert, rename or delete after a fold
/// copies a shard of hashes and a chunk of keys, not every key (ledger row
/// 126). The serialized form is unchanged: [`IdMapWire`].
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(from = "IdMapWire", into = "IdMapWire")]
pub struct IdMap {
    /// The live keys. Answers through `to_key`.
    to_id: HashIndex,
    to_key: CowVec<String>,
    /// Dense ids permanently retired by `delete`. Never reused.
    tombstones: CowMap<u32, ()>,
}

/// How an [`IdMap`] is serialized, as it always has been.
#[derive(Serialize, Deserialize)]
#[serde(rename = "IdMap")]
struct IdMapWire {
    to_id: HashMap<String, u32>,
    to_key: Vec<String>,
    tombstones: BTreeSet<u32>,
}

impl From<IdMap> for IdMapWire {
    fn from(m: IdMap) -> Self {
        let tombstones: BTreeSet<u32> = m.tombstoned_ids().collect();
        let to_id = m
            .to_key
            .iter()
            .enumerate()
            .map(|(i, key)| (key, i as u32))
            .filter(|(key, id)| m.get(key) == Some(*id))
            .map(|(key, id)| (key.clone(), id))
            .collect();
        Self {
            to_id,
            to_key: m.to_key.iter().cloned().collect(),
            tombstones,
        }
    }
}

impl From<IdMapWire> for IdMap {
    fn from(w: IdMapWire) -> Self {
        let mut m = IdMap {
            to_id: HashIndex::new(),
            to_key: CowVec::from_vec(w.to_key),
            tombstones: CowMap::from_sorted(w.tombstones.into_iter().map(|id| (id, ()))),
        };
        let mut live: Vec<(String, u32)> = w.to_id.into_iter().collect();
        live.sort_unstable_by_key(|(_, id)| *id);
        for (key, id) in live {
            m.to_id.insert(&key, id);
        }
        m
    }
}

impl IdMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuild the map from what a snapshot holds: every key slot in dense-id
    /// order, and which of them are retired.
    ///
    /// Slot `i` is id `i`, whatever its key. Rebuilding by inserting the keys
    /// in order instead loses a slot whenever a retired slot's key is one a
    /// lower, live slot now carries — delete `b`, then rename `a` to `b` —
    /// because the insert finds the key present and allocates nothing.
    pub fn from_slots(to_key: Vec<String>, tombstones: BTreeSet<u32>) -> Self {
        let mut to_id = HashIndex::new();
        for (i, key) in to_key.iter().enumerate() {
            if !tombstones.contains(&(i as u32)) && !key.is_empty() {
                to_id.insert(key, i as u32);
            }
        }
        Self {
            to_id,
            to_key: CowVec::from_vec(to_key),
            tombstones: CowMap::from_sorted(tombstones.into_iter().map(|id| (id, ()))),
        }
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
        let h = HashIndex::hash(key);
        let to_key = &self.to_key;
        if let Some(id) = self
            .to_id
            .get_hashed(key, h, |id| to_key.get(id as usize).map(String::as_str))
        {
            return Ok(id);
        }
        let id = dense_id(self.to_key.len())?;
        self.to_key.push(key.to_string());
        self.to_id.insert_hashed(key, h, id);
        Ok(id)
    }

    pub fn get(&self, key: &str) -> Option<u32> {
        // A deleted key is no longer indexed, so this returns None for it.
        let to_key = &self.to_key;
        self.to_id
            .get(key, |id| to_key.get(id as usize).map(String::as_str))
    }

    pub fn key_of(&self, id: u32) -> Option<&str> {
        if self.is_tombstoned(id) {
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

    /// Stop indexing the live key `key`, returning its id.
    fn unindex(&mut self, key: &str) -> Option<u32> {
        let to_key = &self.to_key;
        self.to_id
            .remove(key, |id| to_key.get(id as usize).map(String::as_str))
    }

    /// Rename a live key, keeping its dense id stable.
    ///
    /// Returns `Err(KeyNotFound)` if `old` is unknown or tombstoned.
    /// Returns `Err(DuplicateKey)` if `new` is already a live key.
    /// On success returns the stable id shared by both names.
    pub fn rename(&mut self, old: &str, new: &str) -> Result<u32> {
        if self.get(new).is_some() {
            return Err(GraphError::DuplicateKey { key: new.into() });
        }
        let id = self
            .unindex(old)
            .ok_or_else(|| GraphError::KeyNotFound { key: old.into() })?;
        self.to_key.set(id as usize, new.to_string());
        self.to_id.insert(new, id);
        Ok(id)
    }

    /// Remove `key` from the live map, permanently tombstone its dense id, and
    /// return that id. Returns `None` if the key is not present.
    pub fn delete(&mut self, key: &str) -> Option<u32> {
        let id = self.unindex(key)?;
        self.tombstones.insert(id, ());
        Some(id)
    }

    /// Every retired id, ascending.
    pub fn tombstoned_ids(&self) -> impl Iterator<Item = u32> + '_ {
        self.tombstones.keys().copied()
    }

    /// Returns `true` if `id` has been retired by a prior `delete` call.
    pub fn is_tombstoned(&self, id: u32) -> bool {
        !self.tombstones.is_empty() && self.tombstones.contains_key(&id)
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
    pub(crate) fn all_keys(&self) -> impl Iterator<Item = &String> + '_ {
        self.to_key.iter()
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
    fn from_slots_keeps_a_live_slot_whose_key_a_later_retired_slot_also_held() {
        // delete b (id 1), rename a (id 0) to b: slots ["b", "b"], id 1 retired.
        let mut m = IdMap::new();
        m.get_or_insert("a");
        m.get_or_insert("b");
        m.delete("b");
        m.rename("a", "b").unwrap();
        let back = IdMap::from_slots(m.all_keys().cloned().collect(), BTreeSet::from([1]));
        assert_eq!(back.len(), 2);
        assert_eq!(back.live_len(), 1);
        assert_eq!(back.get("b"), Some(0));
        assert_eq!(back.key_of(0), Some("b"));
        assert!(back.is_tombstoned(1));
        assert!(!back.is_tombstoned(0));
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

    /// Random inserts, deletes and renames answer as a plain map does, both
    /// with real hashes and with every key's hash the same (each key past the
    /// first then lives in the collision table), and survive a serde round
    /// trip and a clone that the original then moves away from.
    #[test]
    fn answers_as_a_plain_map_with_and_without_hash_collisions() {
        for forced in [None, Some(0x5EED_u64)] {
            crate::cow::FORCED_HASH.with(|f| f.set(forced));
            let mut m = IdMap::new();
            let mut live: HashMap<String, u32> = HashMap::new();
            let mut slots: Vec<String> = Vec::new();
            let mut dead: BTreeSet<u32> = BTreeSet::new();
            let mut r: u64 = 0x2545_F491_4F6C_DD1D;
            let mut clone_at_200 = None;
            for step in 0..2_000 {
                r ^= r << 13;
                r ^= r >> 7;
                r ^= r << 17;
                let key = format!("k{}", r % 60);
                match r % 5 {
                    0 | 1 => {
                        let id = m.get_or_insert(&key);
                        let want = *live.entry(key.clone()).or_insert_with(|| {
                            slots.push(key.clone());
                            slots.len() as u32 - 1
                        });
                        assert_eq!(id, want);
                    }
                    2 => {
                        assert_eq!(m.delete(&key), live.remove(&key));
                    }
                    _ => {
                        let new = format!("k{}", (r >> 8) % 60);
                        let got = m.rename(&key, &new).ok();
                        let want = if live.contains_key(&new) || !live.contains_key(&key) {
                            None
                        } else {
                            let id = live.remove(&key).unwrap();
                            live.insert(new.clone(), id);
                            slots[id as usize] = new;
                            Some(id)
                        };
                        assert_eq!(got, want);
                    }
                }
                if step == 200 {
                    clone_at_200 = Some((m.clone(), live.clone()));
                }
            }
            for (id, _) in slots.iter().enumerate() {
                let id = id as u32;
                if !live.values().any(|&v| v == id) {
                    dead.insert(id);
                }
            }
            assert_eq!(m.live_len(), live.len());
            assert_eq!(m.len(), slots.len());
            for (key, &id) in &live {
                assert_eq!(m.get(key), Some(id));
                assert_eq!(m.key_of(id), Some(key.as_str()));
            }
            assert_eq!(m.tombstoned_ids().collect::<BTreeSet<_>>(), dead);
            let back: IdMap = bincode::deserialize(&bincode::serialize(&m).unwrap()).unwrap();
            for (key, &id) in &live {
                assert_eq!(back.get(key), Some(id));
            }
            assert_eq!(back.live_len(), live.len());
            assert_eq!(back.tombstoned_ids().collect::<BTreeSet<_>>(), dead);
            let (old, old_live) = clone_at_200.unwrap();
            assert_eq!(old.live_len(), old_live.len());
            for (key, &id) in &old_live {
                assert_eq!(old.get(key), Some(id));
            }
            crate::cow::FORCED_HASH.with(|f| f.set(None));
        }
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
