use core_storage::Value;
use std::collections::BTreeMap;

/// What a result column holds when it is a bare graph variable (`RETURN n`,
/// `RETURN r`, `RETURN p`) or a list of them (`collect(n)`, `[a, b]`,
/// `p.nodes`, `p.relationships`). Any other column (a property, an
/// expression, any other aggregate, a value alias) has no kind.
///
/// The column's values are rendered as every surface has always seen them —
/// a node as its key string, a relationship as a `{type, start, end}` map, a
/// path as a `{nodes, relationships, length}` map — so a caller that ignores
/// the kind reads exactly what it read before. A protocol that has graph
/// structures of its own (Bolt) reads the kind, and the [`Entity`] details
/// when they were asked for (see [`crate::cypher::with_entity_detail`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntityKind {
    Node,
    Relationship,
    Path,
    /// A list of nodes, each rendered as its key.
    NodeList,
    /// A list of relationships, each rendered as its map.
    RelationshipList,
    /// A list of paths, each rendered as its map.
    PathList,
}

impl EntityKind {
    /// The kind of a list of elements of this kind; `None` for a list of
    /// lists, which has no kind.
    pub fn list_of(self) -> Option<EntityKind> {
        match self {
            EntityKind::Node => Some(EntityKind::NodeList),
            EntityKind::Relationship => Some(EntityKind::RelationshipList),
            EntityKind::Path => Some(EntityKind::PathList),
            _ => None,
        }
    }

    /// The kind of one element, for a list kind.
    pub fn element(self) -> Option<EntityKind> {
        match self {
            EntityKind::NodeList => Some(EntityKind::Node),
            EntityKind::RelationshipList => Some(EntityKind::Relationship),
            EntityKind::PathList => Some(EntityKind::Path),
            _ => None,
        }
    }
}

/// A node as it stood when the query read it.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeEntity {
    /// The node's dense id: assigned once, never reused after a delete, and
    /// kept across a rename.
    pub id: u32,
    pub key: String,
    pub label: String,
    /// The node's properties as the query's view saw them (a role-scoped
    /// view sees a node only when the role may, and then all of it).
    pub properties: BTreeMap<String, Value>,
}

/// A relationship as it stood when the query read it. `start` and `end` are
/// the node keys of its stored direction.
#[derive(Debug, Clone, PartialEq)]
pub struct RelationshipEntity {
    pub rel_type: String,
    pub start: String,
    pub end: String,
    /// The dense ids of the start and end nodes.
    pub start_id: u32,
    pub end_id: u32,
    pub properties: BTreeMap<String, Value>,
}

/// A path: its nodes in walk order (`nodes.len() == relationships.len() + 1`)
/// and its relationships, each in its stored direction.
#[derive(Debug, Clone, PartialEq)]
pub struct PathEntity {
    pub nodes: Vec<NodeEntity>,
    pub relationships: Vec<RelationshipEntity>,
}

/// The full graph element behind one cell of an [`EntityKind`] column.
#[derive(Debug, Clone, PartialEq)]
pub enum Entity {
    Node(NodeEntity),
    Relationship(RelationshipEntity),
    Path(PathEntity),
    /// The elements of a list column, one per list element; `None` for an
    /// element that is not one (a null).
    List(Vec<Option<Entity>>),
}

/// What a write statement changed, counted per effect: the numbers a client
/// reads as the statement's summary. Every field counts this statement only.
///
/// The one-row write result (`created`, `properties_set`, `deleted`,
/// `relationships_created`) keeps its documented meaning; these counters are
/// finer: deletes are split into nodes and relationships, and
/// `properties_set` includes the properties a `CREATE` or a `MERGE` create
/// arm writes with the node.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WriteStats {
    pub nodes_created: i64,
    pub nodes_deleted: i64,
    pub relationships_created: i64,
    pub relationships_deleted: i64,
    pub properties_set: i64,
    /// One per node created: a node is created with its one label.
    pub labels_added: i64,
    pub indexes_added: i64,
    pub indexes_removed: i64,
    pub constraints_added: i64,
    pub constraints_removed: i64,
}

impl WriteStats {
    /// Whether the statement changed anything.
    pub fn contains_updates(&self) -> bool {
        *self != Self::default()
    }
}

/// A query result: named columns and rows of values.
///
/// Beside the rows it may carry three things no renderer has to read: the
/// [`EntityKind`] of each bare-variable column, the [`Entity`] behind each of
/// their cells (only when asked for), and a write's [`WriteStats`]. Equality
/// compares the columns and rows alone, the table every surface renders.
#[derive(Debug, Clone)]
pub struct ResultSet {
    columns: Vec<String>,
    rows: Vec<Vec<Option<Value>>>,
    kinds: Vec<Option<EntityKind>>,
    /// Row-major, `columns.len()` cells per row; `None` until attached.
    entities: Option<Vec<Option<Entity>>>,
    /// Boxed: most results are reads, and a `ResultSet` is held inline in
    /// other types.
    stats: Option<Box<WriteStats>>,
}

impl PartialEq for ResultSet {
    fn eq(&self, other: &Self) -> bool {
        self.columns == other.columns && self.rows == other.rows
    }
}

impl ResultSet {
    pub fn new(columns: Vec<String>) -> Self {
        let kinds = vec![None; columns.len()];
        Self {
            columns,
            rows: Vec::new(),
            kinds,
            entities: None,
            stats: None,
        }
    }

    pub fn push_row(&mut self, row: Vec<Option<Value>>) {
        assert_eq!(
            row.len(),
            self.columns.len(),
            "row arity does not match ResultSet columns"
        );
        self.rows.push(row);
        if let Some(entities) = self.entities.as_mut() {
            entities.resize(self.rows.len() * self.columns.len(), None);
        }
    }

    pub fn columns(&self) -> &[String] {
        &self.columns
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// # Panics — if `i >= self.len()`.
    pub fn row(&self, i: usize) -> &[Option<Value>] {
        &self.rows[i]
    }

    pub fn get(&self, i: usize, col: &str) -> Option<&Value> {
        let idx = self.columns.iter().position(|c| c == col)?;
        self.rows.get(i)?.get(idx)?.as_ref()
    }

    /// The kind of column `col`, when it is a bare graph variable.
    pub fn column_kind(&self, col: usize) -> Option<EntityKind> {
        self.kinds.get(col).copied().flatten()
    }

    /// The kind of every column, in column order.
    pub fn column_kinds(&self) -> &[Option<EntityKind>] {
        &self.kinds
    }

    /// Set the kind of every column. Extra entries are dropped and missing
    /// ones are `None`, so the list always matches the columns.
    pub fn set_column_kinds(&mut self, mut kinds: Vec<Option<EntityKind>>) {
        kinds.resize(self.columns.len(), None);
        self.kinds = kinds;
    }

    /// The graph element behind cell (`row`, `col`), when entity details were
    /// attached and the cell holds one.
    pub fn entity(&self, row: usize, col: usize) -> Option<&Entity> {
        if col >= self.columns.len() {
            return None;
        }
        self.entities
            .as_ref()?
            .get(row * self.columns.len() + col)?
            .as_ref()
    }

    /// Whether entity details are attached.
    pub fn has_entities(&self) -> bool {
        self.entities.is_some()
    }

    /// Attach the graph element behind each cell, row-major.
    ///
    /// # Panics — if `entities.len()` is not `len() * columns().len()`.
    pub fn set_entities(&mut self, entities: Vec<Option<Entity>>) {
        assert_eq!(
            entities.len(),
            self.rows.len() * self.columns.len(),
            "entity table does not match the ResultSet"
        );
        self.entities = Some(entities);
    }

    /// What the write statement that produced this result changed.
    pub fn stats(&self) -> Option<&WriteStats> {
        self.stats.as_deref()
    }

    /// Record what the write statement that produced this result changed.
    pub fn with_stats(mut self, stats: WriteStats) -> Self {
        self.stats = Some(Box::new(stats));
        self
    }
}

#[cfg(test)]
mod tests {
    use super::{Entity, EntityKind, NodeEntity, ResultSet, WriteStats};
    use core_storage::Value;

    #[test]
    fn resultset_basics() {
        let mut rs = ResultSet::new(vec!["a".into(), "b".into()]);
        rs.push_row(vec![Some(Value::Int(1)), None]);
        assert_eq!(rs.len(), 1);
        assert_eq!(rs.get(0, "a"), Some(&Value::Int(1)));
        assert_eq!(rs.get(0, "b"), None);
        assert_eq!(rs.get(0, "zz"), None);
        assert_eq!(rs.row(0)[1], None);
    }

    #[test]
    #[should_panic]
    fn resultset_arity_mismatch_panics() {
        let mut rs = ResultSet::new(vec!["a".into()]);
        rs.push_row(vec![]);
    }

    #[test]
    fn resultset_empty_and_columns() {
        let rs = ResultSet::new(vec!["a".into()]);
        assert!(rs.is_empty());
        assert_eq!(rs.len(), 0);
        assert_eq!(rs.columns(), &["a".to_string()]);
    }

    #[test]
    fn kinds_entities_and_stats_ride_beside_the_table_and_equality_ignores_them() {
        let mut rs = ResultSet::new(vec!["n".into(), "x".into()]);
        rs.push_row(vec![Some(Value::Str("k".into())), Some(Value::Int(1))]);
        let plain = rs.clone();
        rs.set_column_kinds(vec![Some(EntityKind::Node)]);
        assert_eq!(rs.column_kinds(), &[Some(EntityKind::Node), None]);
        let node = Entity::Node(NodeEntity {
            id: 0,
            key: "k".into(),
            label: "L".into(),
            properties: Default::default(),
        });
        rs.set_entities(vec![Some(node.clone()), None]);
        assert_eq!(rs.entity(0, 0), Some(&node));
        assert_eq!(rs.entity(0, 1), None);
        assert_eq!(rs.entity(0, 2), None);
        let rs = rs.with_stats(WriteStats {
            nodes_created: 1,
            ..Default::default()
        });
        assert!(rs.stats().unwrap().contains_updates());
        assert!(!WriteStats::default().contains_updates());
        assert_eq!(rs, plain);
    }
}
