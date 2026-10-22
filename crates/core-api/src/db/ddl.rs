//! Cypher schema statements (TY-9): `CREATE INDEX`, `DROP INDEX`,
//! `SHOW INDEXES`, `CREATE CONSTRAINT`, `DROP CONSTRAINT`, `SHOW CONSTRAINTS`.
//!
//! A statement that changes the schema runs through the API calls the Rust
//! surface has — [`enable_range_index`](GraphDb::enable_range_index),
//! [`disable_range_index`](GraphDb::disable_range_index),
//! [`disable_index`](GraphDb::disable_index),
//! [`disable_fulltext`](GraphDb::disable_fulltext),
//! [`create_constraint`](GraphDb::create_constraint) and
//! [`drop_constraint`](GraphDb::drop_constraint) — so it logs the same records,
//! takes the same V11 upgrade and fails with the same errors. It is a write:
//! only [`query_write`](GraphDb::query_write) runs it. `SHOW …` is a read and
//! answers from a [`SchemaListing`], which every read entry point can reach:
//! the handle builds one from its live state, and each fold carries one for
//! the lock-free reader and the transactions begun on it.

use super::*;
use core_query::cypher::{parse_ddl, DdlStatement, Tok};

/// `SHOW INDEXES` columns.
const INDEX_COLUMNS: [&str; 5] = ["name", "type", "label", "property", "state"];
/// `SHOW CONSTRAINTS` columns.
const CONSTRAINT_COLUMNS: [&str; 4] = ["name", "type", "label", "property"];

/// The name `CREATE INDEX FOR …` gives a range index it is not given one for.
fn auto_index_name(label: &str, field: &str) -> String {
    format!("idx_{label}_{field}")
}

/// The name `SHOW INDEXES` lists an equality index under, and `DROP INDEX`
/// takes it by. An equality index has no name of its own.
fn equality_index_name(label: &str, field: &str) -> String {
    format!("eq_{label}_{field}")
}

/// As [`equality_index_name`], for a full-text index.
fn fulltext_index_name(label: &str, field: &str) -> String {
    format!("ft_{label}_{field}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IndexKind {
    Range,
    Equality,
    Fulltext,
}

impl IndexKind {
    fn as_str(self) -> &'static str {
        match self {
            IndexKind::Range => "range",
            IndexKind::Equality => "equality",
            IndexKind::Fulltext => "fulltext",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct IndexEntry {
    name: String,
    kind: IndexKind,
    label: String,
    field: String,
}

/// The declared indexes and unique constraints, as `SHOW` lists them: each
/// sorted by name.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct SchemaListing {
    indexes: Vec<IndexEntry>,
    constraints: Vec<UniqueConstraint>,
}

impl SchemaListing {
    pub(crate) fn of(
        prop_index: &PropertyIndex,
        fulltext: &FulltextIndex,
        constraints: &[UniqueConstraint],
    ) -> Self {
        let entry = |name: String, kind, (label, field): &(String, String)| IndexEntry {
            name,
            kind,
            label: label.clone(),
            field: field.clone(),
        };
        let mut indexes: Vec<IndexEntry> = prop_index
            .range_indexes()
            .map(|(name, pair)| entry(name.clone(), IndexKind::Range, pair))
            .chain(prop_index.equality_pairs().map(|pair| {
                entry(
                    equality_index_name(&pair.0, &pair.1),
                    IndexKind::Equality,
                    pair,
                )
            }))
            .chain(fulltext.enabled_pairs().map(|pair| {
                entry(
                    fulltext_index_name(&pair.0, &pair.1),
                    IndexKind::Fulltext,
                    pair,
                )
            }))
            .collect();
        indexes.sort_by(|a, b| (&a.name, a.kind.as_str()).cmp(&(&b.name, b.kind.as_str())));
        let mut constraints = constraints.to_vec();
        constraints.sort_by(|a, b| a.name.cmp(&b.name));
        SchemaListing {
            indexes,
            constraints,
        }
    }

    /// The answer to `SHOW INDEXES` or `SHOW CONSTRAINTS`; an empty result for
    /// any other statement.
    pub(crate) fn show(&self, stmt: &DdlStatement) -> ResultSet {
        let text = |s: &str| Some(Value::Str(s.to_string()));
        match stmt {
            DdlStatement::ShowIndexes => {
                let mut rs = ResultSet::new(INDEX_COLUMNS.iter().map(|c| c.to_string()).collect());
                for e in &self.indexes {
                    rs.push_row(vec![
                        text(&e.name),
                        text(e.kind.as_str()),
                        text(&e.label),
                        text(&e.field),
                        // Every index is built in full by the call that
                        // declares it, so none is ever still populating.
                        text("online"),
                    ]);
                }
                rs
            }
            DdlStatement::ShowConstraints => {
                let mut rs =
                    ResultSet::new(CONSTRAINT_COLUMNS.iter().map(|c| c.to_string()).collect());
                for c in &self.constraints {
                    rs.push_row(vec![
                        text(&c.name),
                        text("unique"),
                        text(&c.label),
                        text(&c.field),
                    ]);
                }
                rs
            }
            _ => ResultSet::new(Vec::new()),
        }
    }

    /// The range index named `name` if there is one, else the equality or
    /// full-text index listed under it.
    fn index_named(&self, name: &str) -> Option<&IndexEntry> {
        self.indexes
            .iter()
            .find(|e| e.name == name && e.kind == IndexKind::Range)
            .or_else(|| self.indexes.iter().find(|e| e.name == name))
    }
}

/// The DDL statement in `tokens`, when they begin one; a parse error is a
/// `parse:` query error like any other.
pub(crate) fn ddl_statement(tokens: &[Tok]) -> Result<Option<DdlStatement>> {
    if !core_query::cypher::is_ddl_tokens(tokens) {
        return Ok(None);
    }
    parse_ddl(tokens)
        .map(Some)
        .map_err(|e| GraphError::QueryError {
            detail: format!("parse: {e}"),
        })
}

/// The error a read entry point gives a DDL statement that changes the schema.
pub(crate) fn schema_change_on_a_read() -> GraphError {
    GraphError::QueryError {
        detail: "a schema statement (CREATE/DROP INDEX, CREATE/DROP CONSTRAINT) changes the \
                 store; run it with query_write"
            .into(),
    }
}

/// The error a transaction gives a DDL statement that changes the schema.
pub(crate) fn schema_change_in_a_transaction() -> GraphError {
    GraphError::QueryError {
        detail: "a schema statement (CREATE/DROP INDEX, CREATE/DROP CONSTRAINT) cannot run inside \
                 a transaction; run it on its own, outside BEGIN … COMMIT"
            .into(),
    }
}

/// The error a role-scoped write gives a DDL statement that changes the schema.
pub(crate) fn schema_change_for_a_role() -> GraphError {
    GraphError::RoleWriteDenied {
        reason: "role-bound token: schema statements (CREATE/DROP INDEX, CREATE/DROP \
                 CONSTRAINT) are not permitted"
            .into(),
    }
}

impl<F: Fs> GraphDb<F> {
    /// This handle's declared indexes and constraints, as `SHOW` lists them.
    pub(crate) fn schema_listing(&self) -> SchemaListing {
        SchemaListing::of(&self.prop_index, &self.fulltext, &self.constraints)
    }

    /// Run a parsed DDL statement. `SHOW …` answers from the live state; the
    /// rest change the schema and are refused on a read-only handle before
    /// anything else, `IF [NOT] EXISTS` included.
    pub(crate) fn exec_ddl(&mut self, stmt: DdlStatement) -> Result<ResultSet> {
        if stmt.is_show() {
            return Ok(self.schema_listing().show(&stmt));
        }
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        match stmt {
            DdlStatement::CreateIndex {
                name,
                label,
                field,
                if_not_exists,
            } => {
                let name = name.unwrap_or_else(|| auto_index_name(&label, &field));
                let exists = self.prop_index.range_index(&name).is_some()
                    || self.prop_index.range_index_on(&label, &field).is_some();
                if !(if_not_exists && exists) {
                    self.enable_range_index(&name, &label, &field)?;
                }
            }
            DdlStatement::DropIndex { name, if_exists } => {
                let listing = self.schema_listing();
                match listing.index_named(&name) {
                    Some(e) => match e.kind {
                        IndexKind::Range => self.disable_range_index(&name)?,
                        IndexKind::Equality => self.disable_index(&e.label, &e.field)?,
                        IndexKind::Fulltext => self.disable_fulltext(&e.label, &e.field)?,
                    },
                    None if if_exists => {}
                    None => {
                        return Err(GraphError::QueryError {
                            detail: format!("no index named '{name}'"),
                        })
                    }
                }
            }
            DdlStatement::CreateConstraint {
                name,
                label,
                field,
                if_not_exists,
            } => {
                let exists = self
                    .constraints
                    .iter()
                    .any(|c| c.name == name || (c.label == label && c.field == field));
                if !(if_not_exists && exists) {
                    self.create_constraint(&name, &label, &field)?;
                }
            }
            DdlStatement::DropConstraint { name, if_exists } => {
                let exists = self.constraints.iter().any(|c| c.name == name);
                if exists || !if_exists {
                    self.drop_constraint(&name)?;
                }
            }
            // Answered above.
            DdlStatement::ShowIndexes | DdlStatement::ShowConstraints => {}
        }
        Ok(ResultSet::new(Vec::new()))
    }
}
