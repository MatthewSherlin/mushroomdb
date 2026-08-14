use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct IdMap {
    to_id: HashMap<String, u32>,
    to_key: Vec<String>,
}

impl IdMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get_or_insert(&mut self, key: &str) -> u32 {
        if let Some(&id) = self.to_id.get(key) {
            return id;
        }
        let id = self.to_key.len() as u32;
        self.to_id.insert(key.to_string(), id);
        self.to_key.push(key.to_string());
        id
    }

    pub fn get(&self, key: &str) -> Option<u32> {
        self.to_id.get(key).copied()
    }

    pub fn key_of(&self, id: u32) -> Option<&str> {
        self.to_key.get(id as usize).map(|s| s.as_str())
    }

    pub fn len(&self) -> usize {
        self.to_key.len()
    }

    pub fn is_empty(&self) -> bool {
        self.to_key.is_empty()
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
}
