//! TX-NS (defect-ledger rows 148 and 149): a bare node, relationship or path
//! variable in a RETURN is a column of that kind, with the graph element behind
//! each cell when the caller asks for it; a bare relationship variable may be
//! returned and carried by WITH.
//!
//! The rendered values do not change for a node (its key) or a path (its map);
//! a relationship, refused before, renders as the map a path's relationship
//! already is.

use super::tests::{run, Fx};
use crate::cypher::{
    attach_entities, attach_requested_entities, execute_union, lex, parse, parse_read,
    with_entity_detail, Params,
};
use crate::result::{Entity, EntityKind, NodeEntity, RelationshipEntity, ResultSet};
use crate::visible::VisibleSet;
use crate::GraphView;
use core_storage::Value;
use std::collections::BTreeMap;

fn s(v: &str) -> Value {
    Value::Str(v.into())
}

fn i(v: i64) -> Value {
    Value::Int(v)
}

/// ada -KNOWS{since: 2001}-> bob -KNOWS-> cam; ada -OWNS-> sec (a Secret).
fn graph() -> Fx {
    let mut fx = Fx::new();
    let ada = fx.add("Person", "ada", vec![("name", s("Ada")), ("i", i(1))]);
    let bob = fx.add("Person", "bob", vec![("name", s("Bob")), ("i", i(2))]);
    let cam = fx.add("Person", "cam", vec![("name", s("Cam")), ("i", i(3))]);
    let sec = fx.add("Secret", "sec", vec![("code", s("x"))]);
    fx.edge("KNOWS", ada, bob, vec![("since", i(2001))]);
    fx.edge("KNOWS", bob, cam, vec![]);
    fx.edge("OWNS", ada, sec, vec![]);
    fx
}

fn query(view: &GraphView, q: &str) -> ResultSet {
    run(view, q, &BTreeMap::new()).unwrap_or_else(|e| panic!("`{q}` failed: {e}"))
}

fn detailed(view: &GraphView, q: &str) -> ResultSet {
    let mut rs = query(view, q);
    attach_entities(view, &mut rs);
    rs
}

fn props(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

/// The dense id `graph()` gives a key.
fn id_of(key: &str) -> u32 {
    ["ada", "bob", "cam", "sec"]
        .iter()
        .position(|k| *k == key)
        .unwrap() as u32
}

fn node(key: &str, label: &str, pairs: &[(&str, Value)]) -> NodeEntity {
    NodeEntity {
        id: id_of(key),
        key: key.into(),
        label: label.into(),
        properties: props(pairs),
    }
}

fn rel(t: &str, start: &str, end: &str, pairs: &[(&str, Value)]) -> RelationshipEntity {
    RelationshipEntity {
        rel_type: t.into(),
        start: start.into(),
        end: end.into(),
        start_id: id_of(start),
        end_id: id_of(end),
        properties: props(pairs),
    }
}

fn rel_map(t: &str, start: &str, end: &str) -> Value {
    Value::Map(BTreeMap::from([
        ("type".to_string(), s(t)),
        ("start".to_string(), s(start)),
        ("end".to_string(), s(end)),
    ]))
}

#[test]
fn a_bare_node_column_keeps_its_key_and_carries_the_node_when_asked() {
    let fx = graph();
    let view = fx.view();
    let q = "MATCH (n:Person {id: 'ada'}) RETURN n, n.name AS name";
    let plain = query(&view, q);
    assert_eq!(plain.row(0)[0], Some(s("ada")));
    assert_eq!(plain.column_kinds(), &[Some(EntityKind::Node), None]);
    assert!(!plain.has_entities(), "details only when asked for");
    let rs = detailed(&view, q);
    assert_eq!(rs, plain, "the rendered rows do not change");
    assert_eq!(
        rs.entity(0, 0),
        Some(&Entity::Node(node(
            "ada",
            "Person",
            &[("name", s("Ada")), ("i", i(1))]
        )))
    );
    assert_eq!(rs.entity(0, 1), None);
}

#[test]
fn a_bare_relationship_is_returned_as_a_map_and_carries_its_properties() {
    let fx = graph();
    let view = fx.view();
    let rs = detailed(
        &view,
        "MATCH (a {id: 'ada'})-[r:KNOWS]->(b) RETURN r, b.name",
    );
    assert_eq!(rs.row(0)[0], Some(rel_map("KNOWS", "ada", "bob")));
    assert_eq!(rs.column_kind(0), Some(EntityKind::Relationship));
    assert_eq!(
        rs.entity(0, 0),
        Some(&Entity::Relationship(rel(
            "KNOWS",
            "ada",
            "bob",
            &[("since", i(2001))]
        )))
    );
    // Walked against its direction it is still the stored relationship.
    let back = detailed(&view, "MATCH (b {id: 'bob'})<-[r:KNOWS]-(a) RETURN r");
    assert_eq!(back.row(0)[0], Some(rel_map("KNOWS", "ada", "bob")));
}

#[test]
fn a_relationship_survives_with_aggregation_star_order_and_distinct() {
    let fx = graph();
    let view = fx.view();
    let carried = query(&view, "MATCH ()-[r:KNOWS]->() WITH r RETURN r");
    assert_eq!(carried.len(), 2);
    assert_eq!(carried.column_kind(0), Some(EntityKind::Relationship));
    let grouped = detailed(
        &view,
        "MATCH (a)-[r:KNOWS]->(b) WITH r, count(*) AS c RETURN r, c ORDER BY c",
    );
    assert_eq!(grouped.column_kinds()[0], Some(EntityKind::Relationship));
    assert!(matches!(
        grouped.entity(0, 0),
        Some(Entity::Relationship(_))
    ));
    let star = query(&view, "MATCH (a {id: 'ada'})-[r:KNOWS]->(b) RETURN *");
    assert_eq!(star.columns(), ["a", "b", "r"]);
    assert_eq!(
        star.column_kinds(),
        &[
            Some(EntityKind::Node),
            Some(EntityKind::Node),
            Some(EntityKind::Relationship)
        ]
    );
    let ordered = query(&view, "MATCH ()-[r:KNOWS]->() RETURN r ORDER BY r DESC");
    assert_eq!(ordered.row(0)[0], Some(rel_map("KNOWS", "bob", "cam")));
    let distinct = query(&view, "MATCH (x)-[r:KNOWS]-(y) RETURN DISTINCT r");
    assert_eq!(distinct.len(), 2);
}

#[test]
fn a_variable_length_relationship_is_still_refused_bare() {
    let fx = graph();
    let q = parse(&lex("MATCH (a)-[rs:KNOWS*1..2]->(b) RETURN rs").unwrap()).unwrap();
    let err = crate::cypher::plan(&q).unwrap_err();
    assert!(err.contains("variable-length relationship"), "{err}");
    let star = query(
        &fx.view(),
        "MATCH (a {id: 'ada'})-[rs:KNOWS*1..2]->(b) RETURN *",
    );
    assert_eq!(star.columns(), ["a", "b"]);
}

#[test]
fn a_path_column_carries_its_nodes_and_relationships() {
    let fx = graph();
    let view = fx.view();
    let rs = detailed(
        &view,
        "MATCH p = (a {id: 'ada'})-[:KNOWS]->(b)-[:KNOWS]->(c) RETURN p",
    );
    assert_eq!(rs.column_kind(0), Some(EntityKind::Path));
    let Some(Entity::Path(path)) = rs.entity(0, 0) else {
        panic!("{:?}", rs.entity(0, 0))
    };
    let keys: Vec<&str> = path.nodes.iter().map(|n| n.key.as_str()).collect();
    assert_eq!(keys, ["ada", "bob", "cam"]);
    assert_eq!(
        path.relationships[0],
        rel("KNOWS", "ada", "bob", &[("since", i(2001))])
    );
    assert_eq!(path.relationships[1], rel("KNOWS", "bob", "cam", &[]));
    // Walked backwards: nodes in walk order, relationships in stored direction.
    let back = detailed(&view, "MATCH p = (c {id: 'cam'})<-[:KNOWS]-(b) RETURN p");
    let Some(Entity::Path(path)) = back.entity(0, 0) else {
        panic!()
    };
    assert_eq!(path.nodes[0].key, "cam");
    assert_eq!(path.relationships[0].start, "bob");
    // A path carried through WITH keeps its kind; a WITH alias of a property does not.
    let carried = query(
        &view,
        "MATCH p = (a {id: 'ada'})-[:OWNS]->(s) WITH p AS q RETURN q",
    );
    assert_eq!(carried.column_kind(0), Some(EntityKind::Path));
}

#[test]
fn a_value_that_reuses_a_node_name_has_no_kind() {
    let fx = graph();
    let view = fx.view();
    let rs = detailed(&view, "MATCH (n {id: 'ada'}) WITH n.name AS n RETURN n");
    assert_eq!(rs.row(0)[0], Some(s("Ada")));
    assert_eq!(rs.column_kind(0), None);
    assert_eq!(rs.entity(0, 0), None);
    let unwound = query(&view, "MATCH (n {id: 'ada'}) UNWIND [1] AS n RETURN n");
    assert_eq!(unwound.column_kind(0), None);
}

#[test]
fn entities_follow_their_rows_through_order_skip_limit_and_grouping() {
    let fx = graph();
    let view = fx.view();
    for q in [
        "MATCH (n:Person) RETURN n ORDER BY n.i DESC LIMIT 2",
        "MATCH (n:Person) RETURN n SKIP 1 LIMIT 5",
        "MATCH (n:Person) RETURN DISTINCT n",
        "MATCH (n:Person)-[:KNOWS]->(m) RETURN n, count(m) AS c",
        "MATCH (n:Person) WITH n ORDER BY n.i RETURN n",
        "MATCH (n:Person) WITH n, count(*) AS c WHERE c > 0 RETURN n",
    ] {
        let rs = detailed(&view, q);
        assert!(!rs.is_empty(), "{q}");
        assert_eq!(rs.column_kind(0), Some(EntityKind::Node), "{q}");
        for row in 0..rs.len() {
            let Some(Value::Str(key)) = &rs.row(row)[0] else {
                panic!("{q}: {:?}", rs.row(row))
            };
            let Some(Entity::Node(n)) = rs.entity(row, 0) else {
                panic!("{q}: row {row} has no node")
            };
            assert_eq!(&n.key, key, "{q}");
            assert!(n.properties.contains_key("name"), "{q}");
        }
    }
}

#[test]
fn a_masked_view_never_shows_a_relationship_to_a_hidden_node() {
    let fx = graph();
    let visible: VisibleSet = ["ada", "bob", "cam"]
        .iter()
        .map(|k| fx.ids.get(k).unwrap())
        .collect();
    let view = GraphView {
        mask: Some(&visible),
        ..fx.view()
    };
    let rs = detailed(&view, "MATCH (a {id: 'ada'})-[r]->(b) RETURN r");
    let types: Vec<&Value> = (0..rs.len())
        .map(|row| rs.row(row)[0].as_ref().unwrap())
        .collect();
    assert_eq!(types, [&rel_map("KNOWS", "ada", "bob")]);
    let paths = detailed(&view, "MATCH p = (a {id: 'ada'})-[]->(b) RETURN p");
    assert_eq!(paths.len(), 1);
}

#[test]
fn a_union_keeps_a_kind_only_where_every_part_agrees() {
    let fx = graph();
    let view = fx.view();
    let run_union = |q: &str| {
        let union = parse_read(&lex(q).unwrap()).unwrap();
        execute_union(&view, &union, &Params(&BTreeMap::new())).unwrap()
    };
    let same = run_union("MATCH (n {id: 'ada'}) RETURN n UNION MATCH (n {id: 'bob'}) RETURN n");
    assert_eq!(same.column_kind(0), Some(EntityKind::Node));
    let mixed =
        run_union("MATCH (n {id: 'ada'}) RETURN n UNION MATCH (m {id: 'bob'}) RETURN m.name AS n");
    assert_eq!(mixed.column_kind(0), None);
}

#[test]
fn details_are_attached_only_inside_with_entity_detail() {
    let fx = graph();
    let view = fx.view();
    let mut outside = query(&view, "MATCH (n {id: 'ada'}) RETURN n");
    attach_requested_entities(&view, &mut outside);
    assert!(!outside.has_entities());
    let inside = with_entity_detail(|| {
        let mut rs = query(&view, "MATCH (n {id: 'ada'}) RETURN n");
        attach_requested_entities(&view, &mut rs);
        rs
    });
    assert!(inside.has_entities());
    // The switch is put back afterwards, and after a panic inside it.
    let _ = std::panic::catch_unwind(|| with_entity_detail(|| panic!("inside")));
    let mut after = query(&view, "MATCH (n {id: 'ada'}) RETURN n");
    attach_requested_entities(&view, &mut after);
    assert!(!after.has_entities());
}

#[test]
fn star_lists_fixed_relationships_but_not_variable_length_ones() {
    let names = |q: &str| -> Vec<String> {
        parse(&lex(q).unwrap())
            .unwrap()
            .returns
            .iter()
            .map(|item| crate::cypher::ret_val_label(&item.value).unwrap())
            .collect()
    };
    assert_eq!(
        names("MATCH p = (b)-[r]->(a) UNWIND [1] AS x RETURN *"),
        ["a", "b", "p", "r", "x"]
    );
    assert_eq!(names("MATCH (x)-[rs*1..2]->(y) RETURN *"), ["x", "y"]);
}

#[test]
fn collect_of_a_relationship_returns_its_maps() {
    // Row I-1 of the TX-NS review: `collect(r)` returned [] silently.
    let fx = graph();
    let view = fx.view();
    let rs = detailed(&view, "MATCH (a)-[r:KNOWS]->(b) RETURN collect(r) AS rs");
    let Some(Value::List(items)) = &rs.row(0)[0] else {
        panic!("{:?}", rs.row(0))
    };
    assert_eq!(items.len(), 2);
    assert!(items.contains(&rel_map("KNOWS", "ada", "bob")));
    assert_eq!(rs.column_kind(0), Some(EntityKind::RelationshipList));
    let Some(Entity::List(rels)) = rs.entity(0, 0) else {
        panic!("{:?}", rs.entity(0, 0))
    };
    assert!(rels
        .iter()
        .all(|r| matches!(r, Some(Entity::Relationship(_)))));
    let distinct = query(
        &view,
        "MATCH (x)-[r:KNOWS]-(y) RETURN collect(DISTINCT r) AS rs",
    );
    let Some(Value::List(items)) = &distinct.row(0)[0] else {
        panic!()
    };
    assert_eq!(items.len(), 2);
}

#[test]
fn lists_of_nodes_relationships_and_paths_carry_their_elements() {
    let fx = graph();
    let view = fx.view();
    for (q, kind) in [
        (
            "MATCH (a:Person)-[:KNOWS]->(b) RETURN a, collect(b) AS friends",
            EntityKind::NodeList,
        ),
        (
            "MATCH (a {id: 'ada'}), (b {id: 'bob'}) RETURN [a, b] AS pair",
            EntityKind::NodeList,
        ),
        (
            "MATCH p = (a {id: 'ada'})-[:KNOWS]->(b) RETURN p.nodes AS ns",
            EntityKind::NodeList,
        ),
        (
            "MATCH p = (a {id: 'ada'})-[:KNOWS]->(b) RETURN p.relationships AS rs",
            EntityKind::RelationshipList,
        ),
        (
            "MATCH p = (a {id: 'ada'})-[:KNOWS]->(b) RETURN collect(p) AS ps",
            EntityKind::PathList,
        ),
        (
            "MATCH (n:Person) WITH collect(n) AS people RETURN people",
            EntityKind::NodeList,
        ),
    ] {
        let rs = detailed(&view, q);
        let col = rs.columns().len() - 1;
        assert_eq!(rs.column_kind(col), Some(kind), "{q}");
        let Some(Value::List(values)) = &rs.row(0)[col] else {
            panic!("{q}: {:?}", rs.row(0))
        };
        let Some(Entity::List(items)) = rs.entity(0, col) else {
            panic!("{q}: {:?}", rs.entity(0, col))
        };
        assert_eq!(items.len(), values.len(), "{q}");
        assert!(items.iter().all(Option::is_some), "{q}: {items:?}");
    }
    let rels = detailed(
        &view,
        "MATCH (a)-[r:KNOWS]->(b)-[s:KNOWS]->(c) RETURN [r, s] AS l",
    );
    assert_eq!(rels.column_kind(0), Some(EntityKind::RelationshipList));
    // A list that mixes kinds, or holds values, has no kind.
    let mixed = query(
        &view,
        "MATCH (a {id: 'ada'})-[r:KNOWS]->(b) RETURN [a, r] AS l, [1, 2] AS v",
    );
    assert_eq!(mixed.column_kinds(), &[None, None]);
    // `collect` of a property is a value list.
    let names = query(&view, "MATCH (n:Person) RETURN collect(n.name) AS names");
    assert_eq!(names.column_kind(0), None);
}
