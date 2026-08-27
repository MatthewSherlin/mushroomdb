use crate::types::Value;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct EdgeProps {
    map: BTreeMap<(u32, u32, u32), BTreeMap<String, Value>>,
}

impl EdgeProps {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&mut self, etype: u32, src: u32, dst: u32, field: &str, value: Value) {
        self.map
            .entry((etype, src, dst))
            .or_default()
            .insert(field.to_owned(), value);
    }

    pub fn get(&self, etype: u32, src: u32, dst: u32, field: &str) -> Option<&Value> {
        self.map.get(&(etype, src, dst))?.get(field)
    }

    pub fn remove_edge(&mut self, etype: u32, src: u32, dst: u32) {
        self.map.remove(&(etype, src, dst));
    }

    /// Return all entries as a sorted Vec of (etype, src, dst, &BTreeMap<String, Value>).
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
}
