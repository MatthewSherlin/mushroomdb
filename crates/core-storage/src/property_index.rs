//! Opt-in equality index over scalar node properties.
//!
//! Mirrors [`crate::fulltext::FulltextIndex`]: a set of declared `(label,
//! field)` pairs whose values are indexed for exact-match lookup, maintained
//! incrementally on insert/set/delete and rebuilt on open. It answers
//! "which nodes of label `L` have `field = value`?" in `O(matches)` instead of
//! an `O(N_label)` scan — the hot path behind `WHERE n.field = $x`.
//!
//! Only scalar values are indexable (`Int`, `Float`, `Str`, `Bool` via
//! [`crate::types::ValueKey`]); list- and map-valued properties are skipped (a list has no
//! single equality key). A per-node reverse map records the current key so an
//! update or delete removes the stale entry in `O(log n)` without knowing the
//! previous value.
//!
//! Since 0.10 the equality index is one kind of the unified index in
//! [`crate::range_index`]; `PropertyIndex` is its old name, kept so existing
//! callers compile unchanged.

pub use crate::range_index::{IndexKind, UnifiedIndex};

/// The equality index's name before it became [`UnifiedIndex`].
pub type PropertyIndex = UnifiedIndex;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Value;

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
