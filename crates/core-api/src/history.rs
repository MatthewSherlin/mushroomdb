use core_storage::Value;

/// A single change event in a node's history, paired with the WAL commit that produced it.
///
/// ## Horizon
///
/// History reaches back only to the last WAL-truncating snapshot, exactly like `open_at`.
/// Snapshots written with `keep_wal: true` preserve deeper history. This is the honest,
/// zero-cost contract; a durable history log is out of scope.
///
/// ## Derived edges
///
/// Rule-created (derived) edges are **not** in the WAL and therefore do not appear in
/// history. Only edges written directly by the application are recorded.
#[derive(Debug, PartialEq)]
pub struct HistoryEntry {
    /// 0-based WAL frame index of the commit that produced this change.
    pub commit: u64,
    pub change: HistoryChange,
}

#[derive(Debug, PartialEq)]
pub enum HistoryChange {
    NodeInserted {
        label: String,
    },
    PropSet {
        field: String,
        value: Value,
    },
    PropRemoved {
        field: String,
    },
    /// An edge involving this node was added.
    ///
    /// `outgoing` is `true` if this node is the source, `false` if it is the destination.
    ///
    /// Self-edges (src == dst == this node) produce a single entry with `outgoing: true`.
    EdgeAdded {
        edge_type: String,
        other: String,
        outgoing: bool,
    },
    /// An edge involving this node was removed.
    ///
    /// `outgoing` is `true` if this node is the source, `false` if it is the destination.
    ///
    /// Self-edges (src == dst == this node) produce a single entry with `outgoing: true`.
    EdgeRemoved {
        edge_type: String,
        other: String,
        outgoing: bool,
    },
    NodeDeleted,
}
