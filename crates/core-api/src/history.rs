use core_storage::Value;

// ── Edge history types ────────────────────────────────────────────────────────

/// Result wrapper for history queries. Carries the event list and the total
/// number of WAL commits visible in the current horizon window.
///
/// Valid commit indices for `was_linked` are `0..total_commits`. Any index
/// `>= total_commits` is outside the horizon and `was_linked` will return
/// `CommitOutOfRange`.
#[derive(Debug)]
pub struct HistoryResult<T> {
    pub items: Vec<T>,
    /// Exclusive upper bound for valid commit indices (`frames.len()`).
    /// The horizon window is `[0, total_commits)`.
    pub total_commits: u64,
}

/// A single add-or-retract event for an edge between two nodes.
#[derive(Debug, PartialEq)]
pub struct EdgeHistoryEvent {
    pub edge_type: String,
    /// 0-based WAL frame index of the commit that produced this event.
    pub commit: u64,
    pub event: EdgeEvent,
    /// `Some(rule_name)` for rule-derived edges, `None` for manually written
    /// edges. In the current implementation this is always `None` because
    /// derived edges are not WAL-logged (see `derived_edges_are_not_wal_logged`
    /// test in rules.rs).
    pub rule: Option<String>,
}

/// Whether an edge was added or retracted.
#[derive(Debug, PartialEq)]
pub enum EdgeEvent {
    Added,
    Retracted,
}

// ── Node history types ────────────────────────────────────────────────────────

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
