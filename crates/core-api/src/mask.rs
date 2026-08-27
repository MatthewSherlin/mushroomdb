use core_storage::fs::Fs;
use std::collections::HashSet;

use crate::db::GraphDb;

/// Query-scoped node visibility filter (ACL primitive).
///
/// When a `NodeMask` is passed to `query_masked`, only nodes whose dense id
/// appears in `visible` will be returned by label scans, key lookups, and
/// neighbor expansions. Edges where either endpoint is hidden are silently
/// dropped from the result.
///
/// Unknown keys in `from_keys` are silently ignored (they resolve to no id).
/// An empty mask hides every node.
pub struct NodeMask {
    pub(crate) visible: HashSet<u32>,
}

impl NodeMask {
    /// Resolve string keys to dense ids and build a mask.
    ///
    /// Keys that do not exist in the database are ignored.
    pub fn from_keys<'a, F: Fs>(db: &GraphDb<F>, keys: impl IntoIterator<Item = &'a str>) -> Self {
        let visible = keys.into_iter().filter_map(|k| db.ids().get(k)).collect();
        NodeMask { visible }
    }

    pub fn len(&self) -> usize {
        self.visible.len()
    }

    pub fn is_empty(&self) -> bool {
        self.visible.is_empty()
    }

    /// Return a new mask that is the intersection of `self` and `other`.
    ///
    /// The result contains only nodes visible in both masks.  Used to enforce
    /// the never-widen rule when a role token also supplies a client mask:
    /// `effective = role_mask.intersect(&client_mask)`.
    pub fn intersect(&self, other: &NodeMask) -> NodeMask {
        NodeMask {
            visible: self.visible.intersection(&other.visible).copied().collect(),
        }
    }

    /// Return `true` if the node identified by `key` is visible in this mask.
    ///
    /// Returns `false` for keys that do not exist in the database (unknown keys
    /// are never visible), as well as for keys that exist but are not in the
    /// visible set.  Used by node-endpoint handlers to produce the same
    /// absent-key response for both missing and hidden nodes.
    pub fn contains_node<F: core_storage::fs::Fs>(
        &self,
        db: &crate::db::GraphDb<F>,
        key: &str,
    ) -> bool {
        db.ids()
            .get(key)
            .is_some_and(|id| self.visible.contains(&id))
    }
}
