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
//! `roles.json` format: `{ "version": 1, "roles": [...] }`. Written
//! atomically (temp → fsync → rename → dir-sync) only when roles change;
//! a no-change re-apply leaves the file byte-identical.

use serde::{Deserialize, Serialize};

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
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct RoleDef {
    pub name: String,
    /// Explicit node keys always visible to the role.
    #[serde(default)]
    pub keys: Vec<String>,
    /// All nodes carrying any of these labels are visible (resolved live).
    #[serde(default)]
    pub labels: Vec<String>,
}

/// On-disk wrapper for `roles.json`.  Version field allows future format bumps.
#[derive(Serialize, Deserialize)]
pub(crate) struct RolesFile {
    pub version: u32,
    pub roles: Vec<RoleDef>,
}

impl RolesFile {
    pub(crate) fn v1(roles: Vec<RoleDef>) -> Self {
        RolesFile { version: 1, roles }
    }
}
