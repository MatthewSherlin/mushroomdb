use core_storage::fs::Fs;
use std::collections::HashSet;

use crate::db::GraphDb;

/// Controls how hidden nodes are rendered when a [`NodeMask`] is used in
/// [`GraphDb::node_info_masked`], [`GraphDb::node_edges_masked`], and
/// [`GraphDb::neighborhood_masked`].
///
/// The default is [`MaskMode::Omit`], which preserves byte-identical behaviour
/// with all pre-existing masked paths.  [`MaskMode::Stub`] is an explicit
/// opt-in that discloses node *existence* — suitable only for full-token
/// client masks.  Role-token paths are hard-coded to `Omit`.
///
/// **Existence-disclosure warning**: `Stub` mode intentionally tells the caller
/// whether a node exists, even if its contents are hidden.  Only use this on
/// client-mask (full-token) paths where the caller already has that knowledge
/// implicitly.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum MaskMode {
    /// Hidden nodes are silently omitted from every result — behaviour is
    /// byte-identical to the pre-existing masked-query paths.  This is the
    /// default.
    #[default]
    Omit,
    /// Hidden nodes' existence is acknowledged via a restricted stub:
    /// `{"key": "<key>", "restricted": true}`.  No label, props, or other
    /// fields are included in the stub.
    Stub,
}

/// Query-scoped node visibility filter (ACL primitive).
///
/// When a `NodeMask` is passed to `query_masked`, only nodes whose dense id
/// appears in `visible` will be returned by label scans, key lookups, and
/// neighbor expansions. Edges where either endpoint is hidden are silently
/// dropped from the result.
///
/// Unknown keys in `from_keys` are silently ignored (they resolve to no id).
/// An empty mask hides every node.
#[derive(Clone, Debug)]
pub struct NodeMask {
    pub(crate) visible: HashSet<u32>,
    mode: MaskMode,
}

impl NodeMask {
    /// Resolve string keys to dense ids and build a mask.
    ///
    /// Keys that do not exist in the database are ignored.
    /// The mask mode defaults to [`MaskMode::Omit`]; call [`NodeMask::with_mode`]
    /// to opt into [`MaskMode::Stub`].
    pub fn from_keys<'a, F: Fs>(db: &GraphDb<F>, keys: impl IntoIterator<Item = &'a str>) -> Self {
        let visible = keys.into_iter().filter_map(|k| db.ids().get(k)).collect();
        NodeMask {
            visible,
            mode: MaskMode::default(),
        }
    }

    /// Build a mask from an already-resolved iterator of dense node ids.
    ///
    /// Used by `ReaderSnapshot` handlers that resolve keys against the frozen
    /// state without a `GraphDb` reference.
    pub fn from_ids(ids: impl IntoIterator<Item = u32>) -> Self {
        NodeMask {
            visible: ids.into_iter().collect(),
            mode: MaskMode::default(),
        }
    }

    /// Set the rendering mode, consuming `self` and returning a new mask.
    ///
    /// **SECURITY**: never call with [`MaskMode::Stub`] on role-token paths.
    pub fn with_mode(self, mode: MaskMode) -> Self {
        NodeMask { mode, ..self }
    }

    /// Return the current rendering mode.
    pub fn mode(&self) -> MaskMode {
        self.mode
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
    ///
    /// The result always carries [`MaskMode::Omit`] — the role-path invariant
    /// means stubs must never slip through an intersection.
    pub fn intersect(&self, other: &NodeMask) -> NodeMask {
        NodeMask {
            visible: self.visible.intersection(&other.visible).copied().collect(),
            mode: MaskMode::Omit,
        }
    }

    /// Return `true` if the dense node id is visible in this mask.
    ///
    /// Used by `ReaderSnapshot` handlers where the key has already been resolved
    /// to a dense id (avoids a second lookup into a `GraphDb`).
    pub fn contains_id(&self, id: u32) -> bool {
        self.visible.contains(&id)
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
