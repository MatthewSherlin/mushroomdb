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
//! or `{ "version": 2, "roles": [...] }` (at least one role has a write scope)
//! or `{ "version": 3, "roles": [...] }` (at least one role has a
//! [`visible_where`](RoleDef::visible_where) predicate).
//! The highest applicable version is written and no higher: version 2 is
//! written only when a write scope is present, version 3 only when a predicate
//! is. Version 1 is kept for forward-compat honesty — a v0.2 server can load v1
//! safely and the `write` field (absent from v1) is ignored by serde's
//! `#[serde(default)]` when a v2 sidecar is loaded by an older binary.
//! Version 3 is deliberately *not* loadable by an older binary: a binary that
//! does not know `visible_where` would resolve a narrowed role to its full
//! label set, so an unrecognised version poisons instead, which denies rather
//! than over-grants.
//! Files are written atomically (temp → fsync → rename → dir-sync); a no-change
//! re-apply leaves the file byte-identical.

use core_storage::Value;
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

/// One property test a role's visibility may carry, beside `labels`.
///
/// Equality and membership only: no ranges, no negation, no nesting. A mask
/// that can express arbitrary predicates is a query language with a security
/// boundary attached — every operator added is another shape the resolver has
/// to be right about, on the deny side, forever. Two operators are enough for
/// the case that motivates them (`status in ["published"]`) and small enough to
/// be obviously correct.
///
/// Exactly one of `eq` and `in` is set; `validate` enforces it.
///
/// # Value shapes accepted on the way in
///
/// A predicate is usually hand-written, so both spellings of a value parse:
/// the plain JSON scalar (`"published"`, `3`, `true`) and the tagged form the
/// graph's own [`Value`] serializes as (`{"Str": "published"}`, `{"Int": 3}`).
/// They mean the same thing. Serialization always writes the tagged form, so a
/// sidecar this binary rewrote is unambiguous no matter which one was typed.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct PropPredicate {
    /// The property to test. Never empty.
    pub field: String,
    /// Single-value form: the property must equal this value.
    #[serde(
        default,
        deserialize_with = "de_value_opt",
        skip_serializing_if = "Option::is_none"
    )]
    pub eq: Option<Value>,
    /// Membership form: the property must equal one of these values. An empty
    /// list matches nothing — it is a valid, fully-closed predicate.
    #[serde(
        default,
        rename = "in",
        deserialize_with = "de_value_vec_opt",
        skip_serializing_if = "Option::is_none"
    )]
    pub in_: Option<Vec<Value>>,
}

/// Read one predicate value, accepting the tagged form or a plain JSON scalar.
///
/// The tagged form is tried first, so `{"Str": "x"}` never falls through to the
/// scalar branch and is never mistaken for a map-valued property.
fn value_from_json(j: serde_json::Value) -> std::result::Result<Value, String> {
    if let Ok(v) = serde_json::from_value::<Value>(j.clone()) {
        return Ok(v);
    }
    match j {
        serde_json::Value::String(s) => Ok(Value::Str(s)),
        serde_json::Value::Bool(b) => Ok(Value::Bool(b)),
        serde_json::Value::Number(n) => n
            .as_i64()
            .map(Value::Int)
            .or_else(|| n.as_f64().map(Value::Float))
            .ok_or_else(|| format!("visible_where: {n} is not a representable number")),
        other => Err(format!(
            "visible_where: {other} is not a value — use a string, number or boolean, \
             or the tagged form such as {{\"Str\": \"published\"}}"
        )),
    }
}

fn de_value_opt<'de, D>(d: D) -> std::result::Result<Option<Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    match Option::<serde_json::Value>::deserialize(d)? {
        None => Ok(None),
        Some(j) => value_from_json(j)
            .map(Some)
            .map_err(serde::de::Error::custom),
    }
}

fn de_value_vec_opt<'de, D>(d: D) -> std::result::Result<Option<Vec<Value>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    match Option::<Vec<serde_json::Value>>::deserialize(d)? {
        None => Ok(None),
        // Element-wise, so one list may mix the two spellings.
        Some(items) => items
            .into_iter()
            .map(value_from_json)
            .collect::<std::result::Result<Vec<_>, _>>()
            .map(Some)
            .map_err(serde::de::Error::custom),
    }
}

impl PropPredicate {
    /// Test one node's property value against the predicate.
    ///
    /// `visible = keys ∪ { n : label(n) ∈ labels ∧ holds(n) }`.
    ///
    /// A missing property does **not** hold: absent is not a match. A node that
    /// never carried the field is outside a narrowed role, which is the
    /// deny-side answer — a role narrowed to `status in ["published"]` must not
    /// see a document that has no status at all.
    pub fn holds(&self, value: Option<&Value>) -> bool {
        let Some(value) = value else {
            return false;
        };
        match (&self.eq, &self.in_) {
            (Some(expected), None) => value == expected,
            (None, Some(allowed)) => allowed.iter().any(|a| a == value),
            // Neither or both is refused by `validate`; hold nothing if a
            // hand-edited sidecar slips one through.
            _ => false,
        }
    }

    /// Reject a predicate that does not name exactly one test of one field.
    pub fn validate(&self) -> std::result::Result<(), String> {
        if self.field.is_empty() {
            return Err("visible_where.field must not be empty".into());
        }
        match (&self.eq, &self.in_) {
            (Some(_), None) | (None, Some(_)) => Ok(()),
            (None, None) => Err(format!(
                "visible_where on field '{}' sets neither 'eq' nor 'in'",
                self.field
            )),
            (Some(_), Some(_)) => Err(format!(
                "visible_where on field '{}' sets both 'eq' and 'in'; use one",
                self.field
            )),
        }
    }
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
    /// Optional property test that narrows the **label leg only**.
    ///
    /// Absent = the role is exactly what it was before version 3: every node of
    /// an allowed label. Present = a node of an allowed label is visible only
    /// when it also passes the predicate. `keys` is an administrative grant and
    /// is never narrowed by it.
    ///
    /// A predicate with no labels is refused at `apply_schema`: it would narrow
    /// nothing, and silently granting the key leg under a name that reads like
    /// a restriction is the wrong way to be wrong.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible_where: Option<PropPredicate>,
    /// Absent or null = read-only role (v1 behavior, backward compatible).
    #[serde(default)]
    pub write: Option<WriteScope>,
}

/// On-disk wrapper for `roles.json`.  Version field allows future format bumps.
///
/// Version 1: no write scopes (all roles read-only, v0.2 compatible).
/// Version 2: at least one role carries a `write` field.
/// Version 3: at least one role carries a `visible_where` predicate.
/// Version >3: unrecognised — roles state is poisoned on load.
#[derive(Serialize, Deserialize)]
pub(crate) struct RolesFile {
    pub version: u32,
    pub roles: Vec<RoleDef>,
}

impl RolesFile {
    /// Build a `RolesFile` choosing the correct version automatically.
    ///
    /// Picks 3 > 2 > 1, the highest the content actually needs: version 3 iff
    /// any role carries a `visible_where` predicate, else version 2 iff any
    /// role carries a write scope, else version 1. This preserves
    /// forward-compatibility where it is safe to: a v0.2 server loading a v1
    /// sidecar sees no behavioral change, and a v0.2 server loading a v2
    /// sidecar silently ignores the `write` field (serde default) and treats
    /// all roles as read-only — safe because v0.2 denies all writes from role
    /// tokens anyway.
    ///
    /// A predicate is different: an older binary ignoring `visible_where` would
    /// resolve a narrowed role to its whole label set, which widens. Version 3
    /// is therefore unrecognised by every binary that predates it, and an
    /// unrecognised version poisons the roles state rather than loading it.
    pub(crate) fn new_versioned(roles: Vec<RoleDef>) -> Self {
        let version = if roles.iter().any(|r| r.visible_where.is_some()) {
            3
        } else if roles.iter().any(|r| r.write.is_some()) {
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
