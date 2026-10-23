//! `CALL ns.name(args) [YIELD …] [WHERE …] [RETURN …]` (QL-12, contract C1).
//!
//! A procedure call is a read. Every read entry point — the handle, a masked
//! or role-scoped read, the lock-free reader, a transaction, an as-of handle
//! — answers it the way it answers `SHOW INDEXES`: it hands
//! [`run_call`] the view it reads and the schema listing it shows. The
//! procedures see the store through a [`ViewCatalog`] over that view, so a
//! masked view lists only what its mask lets the caller see.

use super::*;
use core_query::cypher::{
    execute_call, is_call_tokens, parse_call, DdlStatement, ProcCallStmt, ProcCatalog, SchemaRow,
    Tok,
};

/// The `CALL` statement in `tokens`, when they begin one; a parse error is a
/// `parse:` query error like any other.
pub(crate) fn call_statement(tokens: &[Tok]) -> Result<Option<ProcCallStmt>> {
    if !is_call_tokens(tokens) {
        return Ok(None);
    }
    parse_call(tokens)
        .map(Some)
        .map_err(|e| GraphError::QueryError {
            detail: format!("parse: {e}"),
        })
}

/// Run `stmt` against `view`, with `schema` as the view's declared indexes
/// and constraints. A failure is an `execute:` query error.
pub(crate) fn run_call(
    stmt: &ProcCallStmt,
    view: &GraphView<'_>,
    schema: &SchemaListing,
    params: &BTreeMap<String, Value>,
) -> Result<ResultSet> {
    let catalog = ViewCatalog { view, schema };
    execute_call(view, stmt, &catalog, &Params(params)).map_err(|e| GraphError::QueryError {
        detail: format!("execute: {e}"),
    })
}

/// The store as a view sees it: the nodes its mask (if any) lets through, the
/// relationships between them, and the schema rows `SHOW` gives the same
/// caller.
pub(crate) struct ViewCatalog<'v, 'a> {
    pub(crate) view: &'v GraphView<'a>,
    pub(crate) schema: &'v SchemaListing,
}

impl ViewCatalog<'_, '_> {
    /// The live nodes the view can see.
    fn nodes(&self) -> Vec<u32> {
        self.view.nodes_all()
    }

    /// Every relationship between two nodes the view can see, as
    /// `(type, source, target)`, calling `f` on each until it returns `false`.
    fn for_each_edge(&self, nodes: &[u32], mut f: impl FnMut(u32, u32, u32) -> bool) {
        let live = |id: u32| self.view.visible(id) && self.view.label_sym(id).is_some();
        for etype in self.view.topo.etypes() {
            for &src in nodes {
                for &dst in self.view.topo.neighbors(etype, Direction::Out, src).iter() {
                    if live(dst) && !f(etype, src, dst) {
                        return;
                    }
                }
            }
        }
    }

    fn schema_rows(&self, stmt: &DdlStatement) -> Vec<SchemaRow> {
        let rs = match self.view.mask {
            Some(visible) => self.schema.show_visible(stmt, self.view, visible),
            None => self.schema.show(stmt),
        };
        (0..rs.len())
            .map(|i| {
                rs.row(i)
                    .iter()
                    .map(|v| v.clone().unwrap_or(Value::Null))
                    .collect()
            })
            .collect()
    }
}

impl ProcCatalog for ViewCatalog<'_, '_> {
    fn labels(&self) -> Vec<String> {
        // One pass over the label column: no id list is built, and only the
        // distinct label symbols are resolved to names.
        let syms: BTreeSet<u32> = self
            .view
            .label_syms()
            .enumerate()
            .filter(|&(id, sym)| sym != u32::MAX && self.view.visible(id as u32))
            .map(|(_, sym)| sym)
            .collect();
        let names: BTreeSet<String> = syms
            .into_iter()
            .filter_map(|sym| self.view.sym_name(sym).map(str::to_string))
            .collect();
        names.into_iter().collect()
    }

    fn relationship_types(&self) -> Vec<String> {
        // A type is listed at its first edge between two visible nodes, so
        // the scan stops there for each type.
        let nodes = self.nodes();
        let live = |id: u32| self.view.visible(id) && self.view.label_sym(id).is_some();
        let names: BTreeSet<String> = self
            .view
            .topo
            .etypes()
            .filter(|&etype| {
                nodes.iter().any(|&src| {
                    self.view
                        .topo
                        .neighbors(etype, Direction::Out, src)
                        .iter()
                        .any(|&dst| live(dst))
                })
            })
            .filter_map(|sym| self.view.sym_name(sym).map(str::to_string))
            .collect();
        names.into_iter().collect()
    }

    fn property_keys(&self) -> Vec<String> {
        // One pass over the visible nodes, testing only the fields not yet
        // found, so the pass ends as soon as every field has been seen.
        let nodes = self.nodes();
        let mut missing: Vec<String> = self.view.props.field_names();
        let mut keys: BTreeSet<String> = BTreeSet::new();
        for &id in &nodes {
            if missing.is_empty() {
                break;
            }
            missing.retain(|field| {
                let found = self.view.prop(id, field).is_some();
                if found {
                    keys.insert(field.clone());
                }
                !found
            });
        }
        self.for_each_edge(&nodes, |etype, src, dst| {
            keys.extend(self.view.edge_props.props_of(etype, src, dst).into_keys());
            true
        });
        keys.into_iter().collect()
    }

    fn relationship_patterns(&self) -> Vec<(String, String, String)> {
        let nodes = self.nodes();
        let mut seen: BTreeSet<(u32, u32, u32)> = BTreeSet::new();
        self.for_each_edge(&nodes, |etype, src, dst| {
            if let (Some(from), Some(to)) = (self.view.label_sym(src), self.view.label_sym(dst)) {
                seen.insert((etype, from, to));
            }
            true
        });
        let name = |sym: u32| self.view.sym_name(sym).map(str::to_string);
        let patterns: BTreeSet<(String, String, String)> = seen
            .into_iter()
            .filter_map(|(etype, from, to)| Some((name(etype)?, name(from)?, name(to)?)))
            .collect();
        patterns.into_iter().collect()
    }

    fn indexes(&self) -> Vec<SchemaRow> {
        self.schema_rows(&DdlStatement::ShowIndexes)
    }

    fn constraints(&self) -> Vec<SchemaRow> {
        self.schema_rows(&DdlStatement::ShowConstraints)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mask::NodeMask;

    fn temp_db() -> GraphDb<RealFs> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "graphdb-call-catalog-{}-{n}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        GraphDb::open(&dir).unwrap()
    }

    /// `a:Person {name}` -KNOWS-> `b:Person {age}`, and `s:Secret {code}`
    /// -OWNS-> `a`, inserted twice so it carries the multiplicity property
    /// `count`; an index on one label and a constraint on the other.
    fn store() -> GraphDb<RealFs> {
        let mut db = temp_db();
        db.enable_multiplicity().unwrap();
        db.insert_node("Person", "a", vec![("name".into(), Value::Str("A".into()))])
            .unwrap();
        db.insert_node("Person", "b", vec![("age".into(), Value::Int(3))])
            .unwrap();
        db.insert_node("Secret", "s", vec![("code".into(), Value::Int(7))])
            .unwrap();
        db.insert_edge("KNOWS", "a", "b").unwrap();
        db.insert_edge("OWNS", "s", "a").unwrap();
        db.insert_edge("OWNS", "s", "a").unwrap();
        for q in [
            "CREATE INDEX person_name FOR (n:Person) ON (n.name)",
            "CREATE CONSTRAINT secret_code FOR (n:Secret) REQUIRE n.code IS UNIQUE",
        ] {
            db.query_write(q, &BTreeMap::new()).unwrap();
        }
        db
    }

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn first_column(rows: &[SchemaRow]) -> Vec<String> {
        rows.iter()
            .map(|r| match &r[0] {
                Value::Str(s) => s.clone(),
                v => panic!("not a name: {v:?}"),
            })
            .collect()
    }

    #[test]
    fn an_unmasked_view_lists_everything() {
        let db = store();
        let schema = db.schema_listing();
        let view = db.view();
        let cat = ViewCatalog {
            view: &view,
            schema: &schema,
        };
        assert_eq!(cat.labels(), strings(&["Person", "Secret"]));
        assert_eq!(cat.relationship_types(), strings(&["KNOWS", "OWNS"]));
        let keys = cat.property_keys();
        for key in ["age", "code", "count", "name"] {
            assert!(keys.contains(&key.to_string()), "{key} in {keys:?}");
        }
        assert_eq!(
            first_column(&cat.indexes()).len(),
            2,
            "range + constraint's"
        );
        assert_eq!(first_column(&cat.constraints()), strings(&["secret_code"]));
    }

    #[test]
    fn a_masked_view_lists_only_what_its_mask_lets_through() {
        let db = store();
        let schema = db.schema_listing();
        let mask = NodeMask::from_keys(&db, ["a", "b"]);
        let view = db.view_masked(&mask);
        let cat = ViewCatalog {
            view: &view,
            schema: &schema,
        };
        assert_eq!(cat.labels(), strings(&["Person"]));
        // OWNS starts at the hidden node, so it is not listed.
        assert_eq!(cat.relationship_types(), strings(&["KNOWS"]));
        let keys = cat.property_keys();
        assert!(keys.contains(&"name".to_string()), "{keys:?}");
        for hidden in ["code", "count"] {
            assert!(!keys.contains(&hidden.to_string()), "{hidden} in {keys:?}");
        }
        assert_eq!(first_column(&cat.indexes()), strings(&["person_name"]));
        assert!(cat.constraints().is_empty());
    }
}
