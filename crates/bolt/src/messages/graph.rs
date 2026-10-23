//! Nodes, relationships and paths as Bolt structures, and a write's counters.
//!
//! The engine marks a result column that is a bare node, relationship or path
//! variable, or a list of them (`collect(n)`, `[a, b]`, `p.nodes`), and — because the Bolt layer runs every statement inside
//! [`mushroomdb::with_entity_detail`] — hands over the element behind each
//! cell. This module writes those elements the way the negotiated version
//! spells them:
//!
//! | tag | structure | Bolt 5 fields | Bolt 4.4 fields |
//! |---|---|---|---|
//! | `0x4E` `N` | Node | `id, labels, properties, element_id` | `id, labels, properties` |
//! | `0x52` `R` | Relationship | `id, start_id, end_id, type, properties, element_id, start_element_id, end_element_id` | `id, start_id, end_id, type, properties` |
//! | `0x72` `r` | UnboundRelationship | `id, type, properties, element_id` | `id, type, properties` |
//! | `0x50` `P` | Path | `nodes, relationships, indices` | the same |
//!
//! **Element ids** come from the element's identity in the store, so they are
//! the same for the life of the element, on every connection and across
//! restarts:
//!
//! - a node: `n:` followed by its key (`n:ada`);
//! - a relationship: `r:` followed by its type, its start key and its end key,
//!   joined by `|`, each with `\` and `|` escaped by a backslash
//!   (`r:KNOWS|ada|bob`). The store holds at most one relationship of a type
//!   in each direction between two nodes, so these three name it.
//!
//! Renaming a node gives it a new key and so a new element id.
//!
//! **Integer ids** all fit in 53 bits, so a JavaScript number holds them
//! exactly:
//!
//! - a node's `id` (and a relationship's `start_id` / `end_id`) is the node's
//!   dense id in the store: assigned once, never reused after a delete, kept
//!   across a rename, below 2^32;
//! - a relationship's `id` is a 53-bit FNV-1a hash of its element id: the
//!   same for the same relationship in every result. Two relationships could
//!   share one (about one chance in 2^21 among 65,000 relationships); compare
//!   element ids when that matters. Node and relationship ids are separate
//!   namespaces.
//!
//! **Labels** are the node's one label (0.10 nodes have exactly one).
//!
//! A path lists each distinct node and relationship once; `indices` walks it
//! as the Bolt specification defines: for each hop, the relationship's
//! 1-based position (negative when the hop goes against the relationship's
//! direction), then the position of the node reached.

use mushroomdb::{Entity, NodeEntity, PathEntity, RelationshipEntity, Value, WriteStats};

use crate::error::BoltError;
use crate::packstream::{
    PackStruct, PackValue, TAG_NODE, TAG_PATH, TAG_RELATIONSHIP, TAG_UNBOUND_RELATIONSHIP,
};

use super::run::pack_value;

/// Whether `version` carries element ids in its graph structures.
fn has_element_ids(version: (u8, u8)) -> bool {
    version >= (5, 0)
}

/// The element id of the node `key`.
pub fn node_element_id(key: &str) -> String {
    format!("n:{key}")
}

/// The element id of the relationship `start -[rel_type]-> end`.
pub fn relationship_element_id(rel_type: &str, start: &str, end: &str) -> String {
    let escape = |part: &str| part.replace('\\', "\\\\").replace('|', "\\|");
    format!("r:{}|{}|{}", escape(rel_type), escape(start), escape(end))
}

/// The largest integer a JavaScript number holds exactly: 2^53 - 1.
pub const MAX_SAFE_ID: i64 = (1 << 53) - 1;

/// A relationship's integer id: FNV-1a over its element id, kept to 53 bits.
pub fn relationship_id(element_id: &str) -> i64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0100_0000_01b3;
    let hash = element_id
        .bytes()
        .fold(OFFSET, |h, b| (h ^ u64::from(b)).wrapping_mul(PRIME));
    (hash & MAX_SAFE_ID as u64) as i64
}

/// One result cell: its graph element as a Bolt structure (a list column as a
/// list of them), else its value. `depth` is the nesting the cell sits at, for
/// the depth check.
pub fn pack_cell(
    entity: Option<&Entity>,
    value: Option<&Value>,
    version: (u8, u8),
    depth: usize,
) -> Result<PackValue, BoltError> {
    Ok(match (entity, value) {
        (Some(Entity::Node(node)), _) => pack_node(node, version, depth)?,
        (Some(Entity::Relationship(rel)), _) => pack_relationship(rel, version, depth)?,
        (Some(Entity::Path(path)), _) => pack_path(path, version, depth)?,
        (Some(Entity::List(items)), Some(Value::List(values))) if items.len() == values.len() => {
            PackValue::List(
                items
                    .iter()
                    .zip(values)
                    .map(|(item, v)| pack_cell(item.as_ref(), Some(v), version, depth + 1))
                    .collect::<Result<_, _>>()?,
            )
        }
        (_, Some(value)) => pack_value(value, version, depth)?,
        (_, None) => PackValue::Null,
    })
}

fn properties(
    props: &std::collections::BTreeMap<String, Value>,
    version: (u8, u8),
    depth: usize,
) -> Result<PackValue, BoltError> {
    Ok(PackValue::Map(
        props
            .iter()
            .map(|(k, v)| Ok((k.clone(), pack_value(v, version, depth + 1)?)))
            .collect::<Result<_, BoltError>>()?,
    ))
}

fn pack_node(node: &NodeEntity, version: (u8, u8), depth: usize) -> Result<PackValue, BoltError> {
    let element_id = node_element_id(&node.key);
    let mut fields = vec![
        PackValue::Int(i64::from(node.id)),
        PackValue::List(vec![PackValue::String(node.label.clone())]),
        properties(&node.properties, version, depth + 1)?,
    ];
    if has_element_ids(version) {
        fields.push(PackValue::String(element_id));
    }
    Ok(PackValue::Struct(PackStruct::new(TAG_NODE, fields)))
}

fn pack_relationship(
    rel: &RelationshipEntity,
    version: (u8, u8),
    depth: usize,
) -> Result<PackValue, BoltError> {
    let element_id = relationship_element_id(&rel.rel_type, &rel.start, &rel.end);
    let (start, end) = (node_element_id(&rel.start), node_element_id(&rel.end));
    let mut fields = vec![
        PackValue::Int(relationship_id(&element_id)),
        PackValue::Int(i64::from(rel.start_id)),
        PackValue::Int(i64::from(rel.end_id)),
        PackValue::String(rel.rel_type.clone()),
        properties(&rel.properties, version, depth + 1)?,
    ];
    if has_element_ids(version) {
        fields.extend([
            PackValue::String(element_id),
            PackValue::String(start),
            PackValue::String(end),
        ]);
    }
    Ok(PackValue::Struct(PackStruct::new(TAG_RELATIONSHIP, fields)))
}

fn pack_unbound(
    rel: &RelationshipEntity,
    version: (u8, u8),
    depth: usize,
) -> Result<PackValue, BoltError> {
    let element_id = relationship_element_id(&rel.rel_type, &rel.start, &rel.end);
    let mut fields = vec![
        PackValue::Int(relationship_id(&element_id)),
        PackValue::String(rel.rel_type.clone()),
        properties(&rel.properties, version, depth + 1)?,
    ];
    if has_element_ids(version) {
        fields.push(PackValue::String(element_id));
    }
    Ok(PackValue::Struct(PackStruct::new(
        TAG_UNBOUND_RELATIONSHIP,
        fields,
    )))
}

fn pack_path(path: &PathEntity, version: (u8, u8), depth: usize) -> Result<PackValue, BoltError> {
    // Distinct nodes and relationships, each once, in order of first use.
    let mut nodes: Vec<&NodeEntity> = Vec::new();
    let mut rels: Vec<&RelationshipEntity> = Vec::new();
    let same_node = |a: &NodeEntity, b: &NodeEntity| a.key == b.key;
    if let Some(first) = path.nodes.first() {
        nodes.push(first);
    }
    let mut indices = Vec::with_capacity(2 * path.relationships.len());
    for (hop, rel) in path.relationships.iter().enumerate() {
        let (Some(from), Some(to)) = (path.nodes.get(hop), path.nodes.get(hop + 1)) else {
            break;
        };
        let rel_index = position_or_push(&mut rels, rel, |a, b| {
            (&a.rel_type, &a.start, &a.end) == (&b.rel_type, &b.start, &b.end)
        }) as i64
            + 1;
        let forward = rel.start == from.key && rel.end == to.key;
        indices.push(if forward { rel_index } else { -rel_index });
        indices.push(position_or_push(&mut nodes, to, same_node) as i64);
    }
    let nodes = nodes
        .into_iter()
        .map(|n| pack_node(n, version, depth + 2))
        .collect::<Result<_, _>>()?;
    let rels = rels
        .into_iter()
        .map(|r| pack_unbound(r, version, depth + 2))
        .collect::<Result<_, _>>()?;
    Ok(PackValue::Struct(PackStruct::new(
        TAG_PATH,
        vec![
            PackValue::List(nodes),
            PackValue::List(rels),
            PackValue::List(indices.into_iter().map(PackValue::Int).collect()),
        ],
    )))
}

fn position_or_push<'a, T>(
    items: &mut Vec<&'a T>,
    item: &'a T,
    same: impl Fn(&T, &T) -> bool,
) -> usize {
    match items.iter().position(|x| same(x, item)) {
        Some(at) => at,
        None => {
            items.push(item);
            items.len() - 1
        }
    }
}

/// A write's counters as the `stats` map of its closing `SUCCESS`, under the
/// names drivers read. Only the counters that moved are listed (a driver
/// reads a missing one as zero); `None` when nothing changed.
pub fn stats_map(stats: &WriteStats) -> Option<PackValue> {
    if !stats.contains_updates() {
        return None;
    }
    let counters = [
        ("nodes-created", stats.nodes_created),
        ("nodes-deleted", stats.nodes_deleted),
        ("relationships-created", stats.relationships_created),
        ("relationships-deleted", stats.relationships_deleted),
        ("properties-set", stats.properties_set),
        ("labels-added", stats.labels_added),
        ("indexes-added", stats.indexes_added),
        ("indexes-removed", stats.indexes_removed),
        ("constraints-added", stats.constraints_added),
        ("constraints-removed", stats.constraints_removed),
    ];
    Some(PackValue::Map(
        counters
            .into_iter()
            .filter(|(_, n)| *n != 0)
            .map(|(name, n)| (name.to_string(), PackValue::Int(n)))
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// The dense id the fixtures give a key.
    fn id_of(key: &str) -> u32 {
        ["ada", "bob", "cam"]
            .iter()
            .position(|k| *k == key)
            .unwrap() as u32
    }

    fn node(key: &str) -> NodeEntity {
        NodeEntity {
            id: id_of(key),
            key: key.into(),
            label: "P".into(),
            properties: BTreeMap::from([("name".to_string(), Value::Str(key.to_uppercase()))]),
        }
    }

    fn rel(t: &str, start: &str, end: &str) -> RelationshipEntity {
        RelationshipEntity {
            rel_type: t.into(),
            start: start.into(),
            end: end.into(),
            start_id: id_of(start),
            end_id: id_of(end),
            properties: BTreeMap::from([("w".to_string(), Value::Int(1))]),
        }
    }

    fn pack_entity(
        entity: &Entity,
        version: (u8, u8),
        depth: usize,
    ) -> Result<PackValue, BoltError> {
        pack_cell(Some(entity), None, version, depth)
    }

    fn fields(v: &PackValue) -> (u8, &[PackValue]) {
        match v {
            PackValue::Struct(s) => (s.tag, &s.fields),
            other => panic!("not a structure: {other:?}"),
        }
    }

    #[test]
    fn element_ids_are_unambiguous_and_relationship_ids_are_stable_and_js_safe() {
        assert_eq!(node_element_id("ada"), "n:ada");
        assert_eq!(
            relationship_element_id("KNOWS", "ada", "bob"),
            "r:KNOWS|ada|bob"
        );
        // A key holding the separator cannot be read as another relationship.
        assert_ne!(
            relationship_element_id("T", "a|b", "c"),
            relationship_element_id("T", "a", "b|c")
        );
        assert_eq!(
            relationship_element_id("T", "a|b\\", "c"),
            "r:T|a\\|b\\\\|c"
        );
        assert_eq!(relationship_id("r:K|a|b"), relationship_id("r:K|a|b"));
        assert_ne!(relationship_id("r:K|a|b"), relationship_id("r:K|b|a"));
        // Every id fits a JavaScript number exactly.
        for i in 0..10_000 {
            let id = relationship_id(&relationship_element_id("T", &format!("k{i}"), "z"));
            assert!((0..=MAX_SAFE_ID).contains(&id), "{id}");
        }
        assert_eq!(MAX_SAFE_ID, 9_007_199_254_740_991);
        let top = NodeEntity {
            id: u32::MAX,
            ..node("ada")
        };
        let packed = pack_entity(&Entity::Node(top), (5, 0), 1).unwrap();
        assert!(matches!(fields(&packed).1[0], PackValue::Int(n) if n <= MAX_SAFE_ID));
    }

    #[test]
    fn a_node_has_four_fields_in_bolt_5_and_three_in_4_4() {
        let packed = pack_entity(&Entity::Node(node("ada")), (5, 0), 1).unwrap();
        let (tag, f) = fields(&packed);
        assert_eq!(tag, 0x4E);
        assert_eq!(f.len(), 4);
        assert_eq!(f[0], PackValue::Int(0), "the dense id");
        assert_eq!(f[1], PackValue::List(vec![PackValue::String("P".into())]));
        assert_eq!(
            f[2],
            PackValue::Map(vec![("name".into(), PackValue::String("ADA".into()))])
        );
        assert_eq!(f[3], PackValue::String("n:ada".into()));
        let old = pack_entity(&Entity::Node(node("ada")), (4, 4), 1).unwrap();
        assert_eq!(fields(&old).1.len(), 3);
    }

    #[test]
    fn a_relationship_names_its_ends_by_the_ids_their_nodes_carry() {
        let packed = pack_entity(&Entity::Relationship(rel("K", "ada", "bob")), (5, 0), 1).unwrap();
        let (tag, f) = fields(&packed);
        assert_eq!(tag, 0x52);
        assert_eq!(f.len(), 8);
        assert_eq!(f[0], PackValue::Int(relationship_id("r:K|ada|bob")));
        assert_eq!(f[1], PackValue::Int(0), "ada's dense id");
        assert_eq!(f[2], PackValue::Int(1), "bob's dense id");
        assert_eq!(f[3], PackValue::String("K".into()));
        assert_eq!(f[5], PackValue::String("r:K|ada|bob".into()));
        assert_eq!(f[6], PackValue::String("n:ada".into()));
        assert_eq!(f[7], PackValue::String("n:bob".into()));
        let old = pack_entity(&Entity::Relationship(rel("K", "ada", "bob")), (4, 4), 1).unwrap();
        assert_eq!(fields(&old).1.len(), 5);
    }

    #[test]
    fn a_path_lists_each_element_once_and_walks_with_signed_indices() {
        // ada -K-> bob <-K- cam, then back to ada over the first K again.
        let path = PathEntity {
            nodes: vec![
                node("ada"),
                node("bob"),
                node("cam"),
                node("bob"),
                node("ada"),
            ],
            relationships: vec![
                rel("K", "ada", "bob"),
                rel("K", "cam", "bob"),
                rel("K", "cam", "bob"),
                rel("K", "ada", "bob"),
            ],
        };
        let packed = pack_entity(&Entity::Path(path), (5, 0), 1).unwrap();
        let (tag, f) = fields(&packed);
        assert_eq!(tag, 0x50);
        let PackValue::List(nodes) = &f[0] else {
            panic!()
        };
        let PackValue::List(rels) = &f[1] else {
            panic!()
        };
        assert_eq!(nodes.len(), 3);
        assert_eq!(rels.len(), 2);
        assert_eq!(fields(&rels[0]).0, 0x72);
        assert_eq!(fields(&rels[0]).1.len(), 4);
        let ints = |v: &[i64]| PackValue::List(v.iter().map(|i| PackValue::Int(*i)).collect());
        assert_eq!(f[2], ints(&[1, 1, -2, 2, 2, 1, -1, 0]));
        // A path of one node has no indices.
        let single = PathEntity {
            nodes: vec![node("ada")],
            relationships: vec![],
        };
        let packed = pack_entity(&Entity::Path(single), (4, 4), 1).unwrap();
        assert_eq!(fields(&packed).1[2], ints(&[]));
    }

    #[test]
    fn stats_list_only_the_counters_that_moved() {
        assert_eq!(stats_map(&WriteStats::default()), None);
        let map = stats_map(&WriteStats {
            nodes_created: 2,
            properties_set: 3,
            labels_added: 2,
            ..WriteStats::default()
        })
        .unwrap();
        assert_eq!(
            map,
            PackValue::Map(vec![
                ("nodes-created".into(), PackValue::Int(2)),
                ("properties-set".into(), PackValue::Int(3)),
                ("labels-added".into(), PackValue::Int(2)),
            ])
        );
    }

    #[test]
    fn a_list_cell_packs_each_element_and_keeps_a_null_element() {
        let entity = Entity::List(vec![Some(Entity::Node(node("ada"))), None]);
        let value = Value::List(vec![Value::Str("ada".into()), Value::Null]);
        let packed = pack_cell(Some(&entity), Some(&value), (5, 0), 1).unwrap();
        let PackValue::List(items) = packed else {
            panic!("{packed:?}")
        };
        assert_eq!(fields(&items[0]).0, 0x4E);
        assert_eq!(items[1], PackValue::Null);
        // A cell with no entity is its value.
        assert_eq!(
            pack_cell(None, Some(&Value::Int(3)), (5, 0), 1).unwrap(),
            PackValue::Int(3)
        );
    }
}
