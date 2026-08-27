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

use crate::{GraphDb, Result, RuleDef, ViewDef};
use core_storage::fs::Fs;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A declarative description of the schema that should exist in a database.
///
/// All three lists default to empty when absent from JSON, so a partial schema
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
    /// # Update cost
    ///
    /// Updating a rule (definition differs) triggers `delete_rule` +
    /// `create_rule`.  The `create_rule` call runs a full backfill of derived
    /// edges.  This can be expensive for large graphs; prefer stable rule
    /// definitions in production.
    pub fn apply_schema(&mut self, schema: &Schema) -> Result<SchemaDiff> {
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
        let live_views = self.views();
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
        let live_rules = self.rules();
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

        Ok(SchemaDiff {
            created,
            updated,
            unchanged,
        })
    }
}
