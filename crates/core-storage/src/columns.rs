use crate::types::Value;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ColumnStore {
    // field name -> node id -> value. Columnar layout (Vec + null bitmap)
    // replaces the inner map in Plan 6 behind this same interface.
    cols: HashMap<String, HashMap<u32, Value>>,
}

impl ColumnStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&mut self, node: u32, field: &str, value: Value) {
        self.cols
            .entry(field.to_string())
            .or_default()
            .insert(node, value);
    }

    pub fn get(&self, node: u32, field: &str) -> Option<&Value> {
        self.cols.get(field)?.get(&node)
    }

    pub fn fields(&self) -> impl Iterator<Item = &str> {
        self.cols.keys().map(|s| s.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Value;

    #[test]
    fn set_get_overwrite_and_sparse_nodes() {
        let mut c = ColumnStore::new();
        c.set(0, "name", Value::Str("ada".into()));
        c.set(2, "name", Value::Str("bob".into())); // node 1 skipped: sparse
        c.set(0, "name", Value::Str("ada2".into())); // overwrite
        c.set(0, "age", Value::Int(36));
        assert_eq!(c.get(0, "name"), Some(&Value::Str("ada2".into())));
        assert_eq!(c.get(1, "name"), None);
        assert_eq!(c.get(2, "age"), None);
        let mut fields: Vec<_> = c.fields().collect();
        fields.sort();
        assert_eq!(fields, vec!["age", "name"]);
    }
}
