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
use core_query::VisibleSet;

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

/// The name prefixes of [`equality_index_name`] and [`fulltext_index_name`].
/// A range index may not take a name beginning with either, so a listed name
/// never belongs to a range index and to another index at once.
const RESERVED_INDEX_PREFIXES: [&str; 2] = ["eq_", "ft_"];

/// The refusal for a range index named with a reserved prefix, if `name` is.
pub(crate) fn reserved_index_name(name: &str) -> Option<GraphError> {
    RESERVED_INDEX_PREFIXES
        .iter()
        .any(|p| name.starts_with(p))
        .then(|| GraphError::QueryError {
            detail: format!(
                "index names beginning 'eq_' or 'ft_' are reserved: SHOW INDEXES lists \
                 equality and full-text indexes under them; choose another name for range \
                 index '{name}'"
            ),
        })
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
        self.show_where(stmt, |_| true)
    }

    /// [`show`](Self::show) for a scoped read: only the rows whose label a
    /// node the caller can see carries. `view` is the unmasked state the
    /// caller's `visible` ids index into. A label none of those nodes carries
    /// is not listed, so a role, a key mask or a namespace learns nothing of
    /// the schema outside it.
    pub(crate) fn show_visible(
        &self,
        stmt: &DdlStatement,
        view: &GraphView<'_>,
        visible: &VisibleSet,
    ) -> ResultSet {
        let mut wanted: HashMap<u32, &str> = self
            .indexes
            .iter()
            .map(|e| e.label.as_str())
            .chain(self.constraints.iter().map(|c| c.label.as_str()))
            .filter_map(|label| view.sym(label).map(|sym| (sym, label)))
            .collect();
        let mut seen: HashSet<&str> = HashSet::new();
        // Stops as soon as every listed label is found, so a wide mask over a
        // store whose indexed labels are common costs a few probes.
        for id in visible.iter() {
            if wanted.is_empty() {
                break;
            }
            if view.try_key_of(id).is_none() {
                continue;
            }
            if let Some(label) = view.label_sym(id).and_then(|sym| wanted.remove(&sym)) {
                seen.insert(label);
            }
        }
        self.show_where(stmt, |label| seen.contains(label))
    }

    fn show_where(&self, stmt: &DdlStatement, keep: impl Fn(&str) -> bool) -> ResultSet {
        let text = |s: &str| Some(Value::Str(s.to_string()));
        match stmt {
            DdlStatement::ShowIndexes => {
                let mut rs = ResultSet::new(INDEX_COLUMNS.iter().map(|c| c.to_string()).collect());
                for e in self.indexes.iter().filter(|e| keep(&e.label)) {
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
                for c in self.constraints.iter().filter(|c| keep(&c.label)) {
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

    /// Every index listed under `name`. More than one only for equality or
    /// full-text indexes whose label and property join to the same text
    /// (`(A_b, c)` and `(A, b_c)` are both `eq_A_b_c`).
    fn indexes_named(&self, name: &str) -> Vec<&IndexEntry> {
        self.indexes.iter().filter(|e| e.name == name).collect()
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
                // Unnamed, an existing range index on the pair is the index
                // asked for. Named, only one of that name is: a pair indexed
                // under another name leaves the requested name missing, which
                // `enable_range_index` reports.
                let anonymous = name.is_none();
                let name = name.unwrap_or_else(|| auto_index_name(&label, &field));
                let exists = self.prop_index.range_index(&name).is_some()
                    || (anonymous && self.prop_index.range_index_on(&label, &field).is_some());
                if !(if_not_exists && exists) {
                    self.enable_range_index(&name, &label, &field)?;
                }
            }
            DdlStatement::DropIndex { name, if_exists } => {
                let listing = self.schema_listing();
                match listing.indexes_named(&name).as_slice() {
                    [e] => match e.kind {
                        IndexKind::Range => self.disable_range_index(&name)?,
                        IndexKind::Equality => self.disable_index(&e.label, &e.field)?,
                        IndexKind::Fulltext => self.disable_fulltext(&e.label, &e.field)?,
                    },
                    [] if if_exists => {}
                    [] => {
                        return Err(GraphError::QueryError {
                            detail: format!("no index named '{name}'"),
                        })
                    }
                    several => {
                        let candidates: Vec<String> = several
                            .iter()
                            .map(|e| format!("{} ({}, {})", e.kind.as_str(), e.label, e.field))
                            .collect();
                        return Err(GraphError::QueryError {
                            detail: format!(
                                "'{name}' names more than one index: {}; drop the one you mean \
                                 with disable_index(label, field) or disable_fulltext(label, \
                                 field)",
                                candidates.join(", ")
                            ),
                        });
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
