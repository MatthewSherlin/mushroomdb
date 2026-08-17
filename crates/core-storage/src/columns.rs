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

    /// Remove the value stored at `(node, field)` and return it, or `None` if absent.
    /// Prunes the column's inner map entry when it becomes empty.
    pub fn remove(&mut self, node: u32, field: &str) -> Option<Value> {
        let col = self.cols.get_mut(field)?;
        let old = col.remove(&node)?;
        if col.is_empty() {
            self.cols.remove(field);
        }
        Some(old)
    }

    /// Drop every field stored for `node`. Idempotent: a node with no remaining
    /// props (or an id that was never written) is a no-op. Used by `DeleteNode`
    /// after rule retraction and user-edge sweep. Field iteration order is not
    /// observable — the resulting store is identical regardless of HashMap order.
    pub fn remove_all(&mut self, node: u32) {
        self.cols.retain(|_, col| {
            col.remove(&node);
            !col.is_empty()
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Value;

    #[test]
    fn remove_returns_old_value_and_none_on_absent() {
        let mut c = ColumnStore::new();
        c.set(0, "name", Value::Str("ada".into()));
        assert_eq!(c.remove(0, "name"), Some(Value::Str("ada".into())));
        assert_eq!(c.get(0, "name"), None);
        // second remove: absent → None
        assert_eq!(c.remove(0, "name"), None);
        // completely absent field
        assert_eq!(c.remove(99, "absent"), None);
    }

    #[test]
    fn remove_prunes_empty_column_entry() {
        let mut c = ColumnStore::new();
        c.set(0, "x", Value::Int(1));
        c.set(1, "x", Value::Int(2));
        c.remove(0, "x");
        // one entry still present → column not pruned
        assert!(c.fields().any(|f| f == "x"));
        c.remove(1, "x");
        // now empty → column pruned
        assert!(!c.fields().any(|f| f == "x"));
    }

    #[test]
    fn remove_all_clears_every_field_and_is_noop_on_absent() {
        let mut c = ColumnStore::new();
        c.set(0, "name", Value::Str("ada".into()));
        c.set(0, "age", Value::Int(36));
        c.set(1, "name", Value::Str("bob".into()));
        c.remove_all(0);
        assert_eq!(c.get(0, "name"), None);
        assert_eq!(c.get(0, "age"), None);
        assert_eq!(c.get(1, "name"), Some(&Value::Str("bob".into())));
        // second call is a clean no-op (crash-window / already-cleared node)
        c.remove_all(0);
        assert_eq!(c.get(1, "name"), Some(&Value::Str("bob".into())));
        c.remove_all(99);
        assert_eq!(c.get(1, "name"), Some(&Value::Str("bob".into())));
    }

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
