//! Declarative schema-as-code for mushroomdb.
//!
//! [`Schema`] is a serde-JSON-round-trippable description of the fulltext
//! indexes, materialized views, and linking rules that should exist in a
//! database.  [`GraphDb::apply_schema`] applies the schema idempotently:
//! items already matching the live database are left untouched (no WAL write),
//! items that differ are replaced (delete + create), and items absent from the
//! schema are left in place (no pruning — destructive removal is out of scope
//! for this plan).
//!
//! # Application order
//!
//! 1. **Fulltext** indexes are applied first: rules may later benefit from
//!    freshly-enabled fulltext state during backfill, even though the current
//!    rule predicates do not require it.
//! 2. **Views** are applied second: they are cheaper to backfill than rules
//!    and logically independent of rules.
//! 3. **Rules** are applied last.  Creating a rule triggers a full backfill
//!    of derived edges, which can be expensive for large graphs — document the
//!    cost at the call site.
//!
//! # Update semantics
//!
//! When a schema item (rule or view) exists in the database but its definition
//! differs from the one in the schema, it is replaced via `delete_X` +
//! `create_X`.  For rules, the `create_rule` call triggers a full backfill of
//! derived edges — this can be expensive for large graphs.  The re-backfill
//! cost is inherent to any definition change (the old edge set may not be
//! valid under the new predicate), so there is no cheaper path in v1.
//!
//! # No pruning
//!
//! Items that live in the database but are absent from the schema are left
//! untouched.  Destructive removal ("prune items not in the schema") waits for
//! explicit demand — YAGNI for this plan.

use crate::roles::RoleDef;
use crate::{GraphDb, Result, RuleDef, ViewDef};
use core_storage::{fs::Fs, GraphError};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A declarative description of the schema that should exist in a database.
///
/// All lists default to empty when absent from JSON, so a partial schema
/// that names only rules (for example) is valid.
#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct Schema {
    /// Fulltext index declarations as `(label, field)` pairs.
    #[serde(default)]
    pub fulltext: Vec<(String, String)>,
    /// Linking rule definitions.
    #[serde(default)]
    pub rules: Vec<RuleDef>,
    /// Materialized view definitions.
    #[serde(default)]
    pub views: Vec<ViewDef>,
    /// RBAC role definitions. Persisted as `roles.json` sidecar on apply.
    #[serde(default)]
    pub roles: Vec<RoleDef>,
}

/// The outcome of a single [`GraphDb::apply_schema`] call.
///
/// Entry names are namespaced: `"rule:NAME"`, `"view:NAME"`,
/// `"fulltext:LABEL.FIELD"`.
#[derive(Debug, PartialEq)]
pub struct SchemaDiff {
    /// Items that did not exist and were created.
    pub created: Vec<String>,
    /// Items that existed but whose definition differed; replaced via
    /// delete + create.
    pub updated: Vec<String>,
    /// Items that already matched the live database; no WAL writes made.
    pub unchanged: Vec<String>,
}

// ---------------------------------------------------------------------------
// apply_schema implementation
// ---------------------------------------------------------------------------

impl<F: Fs> GraphDb<F> {
    /// Apply `schema` to the database idempotently.
    ///
    /// Returns a [`SchemaDiff`] describing what was created, updated, or left
    /// unchanged.  The diff is in application order: fulltext, then views,
    /// then rules.
    ///
    /// Items absent from `schema` but present in the database are left
    /// untouched (no pruning).
    ///
    /// # Atomicity of validation
    ///
    /// All rules and views that would be created or updated are validated
    /// **before** any mutation is made.  If any definition is invalid, the
    /// function returns `Err` without touching the database.  This prevents
    /// the partial-application hazard where the old item is already deleted
    /// before the invalid replacement fails.
    ///
    /// # Update cost
    ///
    /// Updating a rule (definition differs) triggers `delete_rule` +
    /// `create_rule`.  The `create_rule` call runs a full backfill of derived
    /// edges.  This can be expensive for large graphs; prefer stable rule
    /// definitions in production.
    pub fn apply_schema(&mut self, schema: &Schema) -> Result<SchemaDiff> {
        // Pre-validation pass: validate every rule, view, and role that would be
        // created or updated, before touching the database.  Unchanged items
        // are already valid (they passed validation when first created).
        let live_views = self.views();
        let live_rules = self.rules();

        // Reject duplicate names within the submitted schema before any mutation.
        {
            let mut seen_rules = std::collections::HashSet::new();
            for rule_def in &schema.rules {
                if !seen_rules.insert(rule_def.name.as_str()) {
                    return Err(GraphError::RuleInvalid {
                        detail: format!("duplicate rule name in schema: {}", rule_def.name),
                    });
                }
            }
            let mut seen_views = std::collections::HashSet::new();
            for view_def in &schema.views {
                if !seen_views.insert(view_def.name.as_str()) {
                    return Err(GraphError::RuleInvalid {
                        detail: format!("duplicate view name in schema: {}", view_def.name),
                    });
                }
            }
        }

        for view_def in &schema.views {
            let would_mutate = live_views
                .iter()
                .find(|v| v.name == view_def.name)
                .is_none_or(|live| live != view_def);
            if would_mutate {
                view_def
                    .validate()
                    .map_err(|e| GraphError::RuleInvalid { detail: e })?;
            }
        }

        for rule_def in &schema.rules {
            let would_mutate = live_rules
                .iter()
                .find(|r| r.name == rule_def.name)
                .is_none_or(|live| live != rule_def);
            if would_mutate {
                rule_def
                    .validate()
                    .map_err(|e| GraphError::RuleInvalid { detail: e })?;
            }
        }

        // Validate roles: non-empty names, unique names within the schema,
        // and write-scope subset rule (§7.1 ruling: create/update/delete_labels
        // must each be a subset of the role's read labels).
        {
            let mut seen = std::collections::HashSet::new();
            for role_def in &schema.roles {
                if role_def.name.is_empty() {
                    return Err(GraphError::RuleInvalid {
                        detail: "role name must not be empty".into(),
                    });
                }
                if !seen.insert(role_def.name.as_str()) {
                    return Err(GraphError::RuleInvalid {
                        detail: format!("duplicate role name: {}", role_def.name),
                    });
                }
                if let Some(write) = &role_def.write {
                    let read_labels: std::collections::HashSet<&str> =
                        role_def.labels.iter().map(String::as_str).collect();
                    // Check create_labels, update_labels, delete_labels — each must
                    // be a subset of the role's read labels (§7.1 subset ruling).
                    // create_edge_types and delete_edge_types are exempt (no read-scope
                    // analog for edge types).
                    for (field, labels) in [
                        ("create_labels", &write.create_labels),
                        ("update_labels", &write.update_labels),
                        ("delete_labels", &write.delete_labels),
                    ] {
                        for label in labels {
                            if !read_labels.contains(label.as_str()) {
                                return Err(GraphError::RuleInvalid {
                                    detail: format!(
                                        "role '{}': write scope {field} contains label '{}' \
                                         that is not in the role's read labels (subset rule)",
                                        role_def.name, label
                                    ),
                                });
                            }
                        }
                    }
                }
            }
        }

        // Mutation pass — all definitions are known-valid from here.
        let mut created = Vec::new();
        let mut updated = Vec::new();
        let mut unchanged = Vec::new();

        // 1. Fulltext indexes.
        for (label, field) in &schema.fulltext {
            let key = format!("fulltext:{label}.{field}");
            if self.is_fulltext_enabled(label, field) {
                unchanged.push(key);
            } else {
                self.enable_fulltext(label, field)?;
                created.push(key);
            }
        }

        // 2. Views.
        for view_def in &schema.views {
            let key = format!("view:{}", view_def.name);
            if let Some(live) = live_views.iter().find(|v| v.name == view_def.name) {
                if live == view_def {
                    unchanged.push(key);
                } else {
                    // Delete + create to pick up the new definition.
                    self.delete_view(&view_def.name)?;
                    self.create_view(view_def.clone())?;
                    updated.push(key);
                }
            } else {
                self.create_view(view_def.clone())?;
                created.push(key);
            }
        }

        // 3. Rules — creating a rule triggers a full backfill (see module doc).
        for rule_def in &schema.rules {
            let key = format!("rule:{}", rule_def.name);
            if let Some(live) = live_rules.iter().find(|r| r.name == rule_def.name) {
                if live == rule_def {
                    unchanged.push(key);
                } else {
                    // Delete + create; create_rule backfills all derived edges.
                    self.delete_rule(&rule_def.name)?;
                    self.create_rule(rule_def.clone())?;
                    updated.push(key);
                }
            } else {
                self.create_rule(rule_def.clone())?;
                created.push(key);
            }
        }

        // 4. Roles — sidecar only (no WAL records). Written atomically when
        // any role is new or changed; unchanged roles leave the file untouched
        // (byte-identical idempotency guarantee).
        {
            let live_roles = self.roles();
            let mut new_roles: Vec<RoleDef> = live_roles.clone();
            let mut roles_changed = false;

            for role_def in &schema.roles {
                let key = format!("role:{}", role_def.name);
                if let Some(live) = live_roles.iter().find(|r| r.name == role_def.name) {
                    if live == role_def {
                        unchanged.push(key);
                    } else {
                        // Update the entry in new_roles.
                        if let Some(slot) = new_roles.iter_mut().find(|r| r.name == role_def.name) {
                            *slot = role_def.clone();
                        }
                        roles_changed = true;
                        updated.push(key);
                    }
                } else {
                    new_roles.push(role_def.clone());
                    roles_changed = true;
                    created.push(key);
                }
            }

            if roles_changed {
                self.commit_roles(new_roles)?;
            }
        }

        Ok(SchemaDiff {
            created,
            updated,
            unchanged,
        })
    }
}
