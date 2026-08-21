//! Post-commit subscription API.
//!
//! [`GraphDb`] exposes three subscription entry-points:
//! - [`GraphDb::subscribe_rule`] — edge-fire / retract events for one named rule.
//! - [`GraphDb::subscribe_all_rules`] — edge events for every rule.
//! - [`GraphDb::subscribe_writes`] — node and property mutations.
//!
//! All three return a [`Subscription`] handle. Dropping it unregisters the
//! subscriber on the next commit (via [`Weak`] upgrade failure in the distribution
//! loop).
//!
//! # Ordering invariant
//!
//! Events are pushed inside `log_then_apply_with` **after** the WAL fsync and
//! the in-memory `apply` have both completed. A subscriber that queries the db
//! immediately after receiving an event therefore observes the state that
//! produced it.
//!
//! # Bounded queue / Lagged
//!
//! Each subscription has a fixed-capacity queue (default [`DEFAULT_SUB_CAPACITY`]).
//! When the queue is full, events are dropped and a missed count is incremented.
//! The next [`Subscription::try_recv`] / [`Subscription::recv_timeout`] call
//! that finds an empty queue and a non-zero miss count returns
//! [`DbEvent::Lagged { missed }`] before continuing with queued events.
//!
//! # v1 scope
//!
//! Rule-edge events and write-mutation events only. Incremental query
//! subscriptions (differential dataflow) are roadmap (Plan-15 T3+).

use serde::Serialize;
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::Duration;

/// Default per-subscriber queue capacity.
pub const DEFAULT_SUB_CAPACITY: usize = 65_536;

/// A post-commit event delivered to subscribers.
///
/// Serialises as internally-tagged JSON (`"type"` discriminant, snake_case).
///
/// ```json
/// {"type":"edge_fired","rule":"skill_fit","src_key":"p1","dst_key":"proj-01",
///  "edge_type":"FIT","weight":0.87,"commit_seq":42}
/// {"type":"lagged","missed":3}
/// ```
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DbEvent {
    /// A rule derived a new edge.
    EdgeFired {
        rule: String,
        src_key: String,
        dst_key: String,
        edge_type: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        weight: Option<f64>,
        commit_seq: u64,
    },
    /// A rule retracted a previously derived edge.
    EdgeRetracted {
        rule: String,
        src_key: String,
        dst_key: String,
        edge_type: String,
        commit_seq: u64,
    },
    /// A node was inserted.
    NodeInserted {
        label: String,
        key: String,
        commit_seq: u64,
    },
    /// A node was deleted.
    NodeDeleted { key: String, commit_seq: u64 },
    /// A user-inserted edge was added.
    EdgeInserted {
        edge_type: String,
        src: String,
        dst: String,
        commit_seq: u64,
    },
    /// A user-inserted edge was deleted.
    EdgeDeleted {
        edge_type: String,
        src: String,
        dst: String,
        commit_seq: u64,
    },
    /// A property was set on a node.
    PropSet {
        key: String,
        field: String,
        commit_seq: u64,
    },
    /// A property was removed from a node.
    PropRemoved {
        key: String,
        field: String,
        commit_seq: u64,
    },
    /// One or more events were dropped due to a full queue.
    ///
    /// The subscriber must re-read graph state to recover consistency for
    /// lossless consumers.  `missed` is the count of dropped events.
    Lagged { missed: u64 },
}

// ---------------------------------------------------------------------------
// Internal queue
// ---------------------------------------------------------------------------

pub(crate) struct SubInner {
    mu: Mutex<SubQueue>,
    condvar: Condvar,
}

impl std::fmt::Debug for SubInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubInner").finish_non_exhaustive()
    }
}

struct SubQueue {
    items: VecDeque<DbEvent>,
    missed: u64,
    capacity: usize,
}

impl SubInner {
    pub(crate) fn new(capacity: usize) -> Arc<Self> {
        Arc::new(SubInner {
            mu: Mutex::new(SubQueue {
                items: VecDeque::new(),
                missed: 0,
                capacity,
            }),
            condvar: Condvar::new(),
        })
    }

    /// Push an event, dropping it (incrementing `missed`) if the queue is full.
    ///
    /// `notify_one` is only called when an item is actually enqueued.  On
    /// overflow we increment `missed` but skip the notification: no waiter can
    /// consume a dropped event, and the spurious wakeup just wastes a syscall.
    pub(crate) fn push(&self, event: DbEvent) {
        let mut q = self.mu.lock().unwrap();
        if q.items.len() >= q.capacity {
            q.missed += 1;
            // No notify: the dropped event cannot be consumed.
            return;
        }
        q.items.push_back(event);
        drop(q);
        self.condvar.notify_one();
    }

    fn pop_one(q: &mut SubQueue) -> Option<DbEvent> {
        if let Some(item) = q.items.pop_front() {
            return Some(item);
        }
        if q.missed > 0 {
            let missed = std::mem::take(&mut q.missed);
            return Some(DbEvent::Lagged { missed });
        }
        None
    }

    /// Non-blocking read.
    pub(crate) fn try_recv(&self) -> Option<DbEvent> {
        let mut q = self.mu.lock().unwrap();
        Self::pop_one(&mut q)
    }

    /// Blocking read with deadline.  Returns `None` on timeout.
    pub(crate) fn recv_timeout(&self, timeout: Duration) -> Option<DbEvent> {
        let mut q = self.mu.lock().unwrap();
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(item) = Self::pop_one(&mut q) {
                return Some(item);
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return None;
            }
            let remaining = deadline - now;
            let (q2, timed_out) = self.condvar.wait_timeout(q, remaining).unwrap();
            q = q2;
            if timed_out.timed_out() {
                // One last try in case a push arrived just as we timed out.
                return Self::pop_one(&mut q);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Filter
// ---------------------------------------------------------------------------

/// What events a subscriber receives.
pub(crate) enum SubFilter {
    /// Only edge events for the named rule.
    Rule(String),
    /// All rule edge events.
    AllRules,
    /// Write events only (node/prop mutations; no edge-fire/retract).
    Writes,
}

pub(crate) fn event_matches(event: &DbEvent, filter: &SubFilter) -> bool {
    match filter {
        SubFilter::Rule(name) => match event {
            DbEvent::EdgeFired { rule, .. } | DbEvent::EdgeRetracted { rule, .. } => rule == name,
            _ => false,
        },
        SubFilter::AllRules => {
            matches!(
                event,
                DbEvent::EdgeFired { .. } | DbEvent::EdgeRetracted { .. }
            )
        }
        SubFilter::Writes => matches!(
            event,
            DbEvent::NodeInserted { .. }
                | DbEvent::NodeDeleted { .. }
                | DbEvent::EdgeInserted { .. }
                | DbEvent::EdgeDeleted { .. }
                | DbEvent::PropSet { .. }
                | DbEvent::PropRemoved { .. }
        ),
    }
}

// ---------------------------------------------------------------------------
// Registry entry
// ---------------------------------------------------------------------------

pub(crate) struct SubEntry {
    pub(crate) filter: SubFilter,
    pub(crate) inner: Weak<SubInner>,
}

// ---------------------------------------------------------------------------
// Public handle
// ---------------------------------------------------------------------------

/// A live subscription handle returned by [`GraphDb::subscribe_rule`] etc.
///
/// Dropping this value unregisters the subscriber: the next commit will detect
/// the dead [`Weak`] reference and prune the entry, so no resources leak.
#[derive(Clone, Debug)]
pub struct Subscription(pub(crate) Arc<SubInner>);

impl Subscription {
    /// Non-blocking read.  Returns `None` if the queue is empty.
    pub fn try_recv(&self) -> Option<DbEvent> {
        self.0.try_recv()
    }

    /// Blocking read with timeout.  Returns `None` on timeout.
    ///
    /// Use `tokio::task::spawn_blocking` to bridge into an async context.
    pub fn recv_timeout(&self, timeout: Duration) -> Option<DbEvent> {
        self.0.recv_timeout(timeout)
    }
}
