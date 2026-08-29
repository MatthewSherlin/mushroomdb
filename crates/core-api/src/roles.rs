//! RBAC role definitions and sidecar I/O.
//!
//! [`RoleDef`] is the public unit of role configuration. Roles are declared in
//! [`Schema::roles`](crate::schema::Schema) and persisted as `roles.json` in
//! the database directory via [`GraphDb::apply_schema`].
//!
//! # Never-widen rule
//!
//! - Empty role (no keys, no labels) = empty mask = sees nothing.
//! - Unknown role on a request = `Err` (never silently grant full access).
//! - Corrupt `roles.json` at open = roles poisoned; [`GraphDb::mask_for_role`]
//!   returns `Err` for any role name until the file is fixed and the DB
//!   re-opened.
//!
//! # Persistence
//!
//! `roles.json` format: `{ "version": 1, "roles": [...] }` (no write scopes)
//! or `{ "version": 2, "roles": [...] }` (at least one role has a write scope).
//! Version 2 is written only when a write scope is present; version 1 is kept
//! for forward-compat honesty — a v0.2 server can load v1 safely and the
//! `write` field (absent from v1) is ignored by serde's `#[serde(default)]`
//! when a v2 sidecar is loaded by an older binary.
//! Files are written atomically (temp → fsync → rename → dir-sync); a no-change
//! re-apply leaves the file byte-identical.

use serde::{Deserialize, Serialize};

/// Write permissions granted to a role.
///
/// All fields default to empty (absent from JSON = no write permission for that
/// operation). `write: None` on `RoleDef` is equivalent to all fields empty —
/// the role is read-only, identical to v0.2 behavior.
///
/// Subset rule (enforced at `apply_schema` time):
/// - `create_labels`, `update_labels`, and `delete_labels` must each be a
///   subset of the role's read `labels`.
/// - `create_edge_types` and `delete_edge_types` have no subset requirement
///   (edge types are not read-scoped).
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
pub struct WriteScope {
    /// Labels the role may CREATE nodes under.
    #[serde(default)]
    pub create_labels: Vec<String>,
    /// Labels whose nodes the role may SET properties on or MERGE.
    /// Only nodes already in the role's read mask are reachable.
    #[serde(default)]
    pub update_labels: Vec<String>,
    /// Labels whose nodes the role may DELETE (DETACH DELETE included).
    #[serde(default)]
    pub delete_labels: Vec<String>,
    /// Edge types the role may insert via INSERT EDGE / Cypher CREATE
    /// or /ingest edges field.  Both endpoints must be read-visible.
    #[serde(default)]
    pub create_edge_types: Vec<String>,
    /// Edge types the role may DELETE (user-owned edges only; derived
    /// edges cannot be directly deleted by any token, including Full).
    #[serde(default)]
    pub delete_edge_types: Vec<String>,
}

/// A named RBAC role: resolves to a node-visibility mask at query time.
///
/// `keys` and `labels` both default to empty when absent from JSON, so a
/// schema snippet that names only labels is valid.
///
/// The resolved mask is the union of:
/// - all nodes whose key appears in `keys` (unknown keys silently ignored), and
/// - all nodes carrying any label in `labels` (resolved live against the current
///   graph — new nodes of an allowed label are immediately visible without
///   re-applying the schema).
///
/// An empty union (no keys, no matching label nodes) = empty mask = sees nothing.
///
/// `write: None` (or absent from JSON) = read-only role, v1 behavior, backward
/// compatible with any client that does not know about write scopes.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct RoleDef {
    pub name: String,
    /// Explicit node keys always visible to the role.
    #[serde(default)]
    pub keys: Vec<String>,
    /// All nodes carrying any of these labels are visible (resolved live).
    #[serde(default)]
    pub labels: Vec<String>,
    /// Absent or null = read-only role (v1 behavior, backward compatible).
    #[serde(default)]
    pub write: Option<WriteScope>,
}

/// On-disk wrapper for `roles.json`.  Version field allows future format bumps.
///
/// Version 1: no write scopes (all roles read-only, v0.2 compatible).
/// Version 2: at least one role carries a `write` field.
/// Version >2: unrecognised — roles state is poisoned on load.
#[derive(Serialize, Deserialize)]
pub(crate) struct RolesFile {
    pub version: u32,
    pub roles: Vec<RoleDef>,
}

impl RolesFile {
    /// Build a `RolesFile` choosing the correct version automatically.
    ///
    /// Writes version 2 iff any role carries a write scope; otherwise writes
    /// version 1. This preserves forward-compatibility: a v0.2 server loading
    /// a v1 sidecar sees no behavioral change, and a v0.2 server loading a v2
    /// sidecar silently ignores the `write` field (serde default) and treats
    /// all roles as read-only — safe because v0.2 denies all writes from role
    /// tokens anyway.
    pub(crate) fn new_versioned(roles: Vec<RoleDef>) -> Self {
        let version = if roles.iter().any(|r| r.write.is_some()) {
            2
        } else {
            1
        };
        RolesFile { version, roles }
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn v1(roles: Vec<RoleDef>) -> Self {
        RolesFile { version: 1, roles }
    }
}
