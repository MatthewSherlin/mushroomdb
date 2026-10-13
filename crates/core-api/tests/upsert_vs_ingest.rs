//! `upsert_entity`'s create path writes through `insert_node`; before 0.7 it
//! wrote through `ingest`. Nothing compared the two (ledger row 47). This
//! does: the same record through each, and everything a reader can observe
//! about the node afterwards — stored values and their types, the full-text
//! index, a rule's derived edges, and a property filter.
//!
//! Found: they agree on all of it. The upsert stores one field more, the
//! `aliases` it derives from the name, and that is by design.
use core_api::memory::identity::ALIASES_FIELD;
use core_api::memory::remember::upsert_entity;
use core_api::memory_schema::memory_defaults;
use core_api::{AutoFk, Direction, GraphDb, IngestOptions, Predicate, RuleDef, Value};
use std::collections::BTreeMap;

type Db = GraphDb<core_storage::fs::RealFs>;

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "mdb-upsert-vs-ingest-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn s(x: &str) -> Value {
    Value::Str(x.into())
}

/// One record carrying every kind of value a property can hold.
fn record() -> BTreeMap<String, Value> {
    let mut nested = BTreeMap::new();
    nested.insert("city".to_string(), s("Austin"));
    nested.insert("floor".to_string(), Value::Int(3));
    BTreeMap::from([
        ("name".to_string(), s("Ada Lovelace")),
        ("age".to_string(), Value::Int(36)),
        ("score".to_string(), Value::Float(0.5)),
        ("active".to_string(), Value::Bool(true)),
        (
            "tags".to_string(),
            Value::List(vec![s("math"), s("engines")]),
        ),
        ("empty".to_string(), Value::List(vec![])),
        (
            "mixed".to_string(),
            Value::List(vec![Value::Int(1), s("two")]),
        ),
        ("blank".to_string(), s("")),
        ("office".to_string(), Value::Map(nested)),
    ])
}

/// A store with the memory schema, a peer node to derive an edge to, and a
/// rule over `tags`.
fn store(name: &str) -> Db {
    let mut db = GraphDb::open(&tmp(name)).unwrap();
    db.apply_schema(&memory_defaults()).unwrap();
    db.insert_node(
        "Person",
        "peer",
        vec![("tags".into(), Value::List(vec![s("math"), s("engines")]))],
    )
    .unwrap();
    db.create_rule(RuleDef {
        name: "shared_tags".into(),
        src_label: "Person".into(),
        dst_label: "Person".into(),
        predicate: Predicate::Overlap {
            field: "tags".into(),
            min: 0.5,
        },
        edge_type: "SHARES".into(),
        weight_prop: Some("score".into()),
        max_edges: None,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
        namespace: None,
    })
    .unwrap();
    db
}

fn through_ingest(name: &str) -> Db {
    let mut db = store(name);
    let mut row = record();
    row.insert("id".to_string(), s("ada"));
    let opts = IngestOptions {
        key_field: "id".into(),
        auto_fk: AutoFk::Off,
    };
    db.ingest("Person", vec![row], &opts).unwrap();
    db
}

fn through_upsert(name: &str) -> Db {
    let mut db = store(name);
    upsert_entity(&mut db, "ada", Some("Person"), record(), &[]).unwrap();
    db
}

#[test]
fn both_paths_store_the_same_values_with_the_same_types() {
    let ingested = through_ingest("props-ingest");
    let upserted = through_upsert("props-upsert");
    let mut upserted_props = upserted.node_info("ada").unwrap().props;
    // The one thing an entity upsert adds that a plain ingest does not: the
    // name's derived aliases, which identity linking reads.
    assert_eq!(
        upserted_props.remove(ALIASES_FIELD),
        Some(Value::List(vec![
            s("ada"),
            s("ada lovelace"),
            s("lovelace")
        ]))
    );
    assert_eq!(upserted_props, ingested.node_info("ada").unwrap().props);
    assert_eq!(
        upserted.node_info("ada").unwrap().label,
        ingested.node_info("ada").unwrap().label
    );
}

#[test]
fn both_paths_leave_the_node_in_the_full_text_index() {
    let ingested = through_ingest("fts-ingest");
    let upserted = through_upsert("fts-upsert");
    let hit = ingested.search("name", "lovelace");
    assert_eq!(hit.len(), 1, "the schema declares Person.name: {hit:?}");
    assert_eq!(upserted.search("name", "lovelace"), hit);
}

#[test]
fn both_paths_fire_the_same_rule() {
    let ingested = through_ingest("rule-ingest");
    let upserted = through_upsert("rule-upsert");
    for dir in [Direction::Out, Direction::In] {
        let edges = ingested.neighbors("ada", "SHARES", dir).unwrap();
        assert_eq!(edges, vec!["peer".to_string()]);
        assert_eq!(upserted.neighbors("ada", "SHARES", dir).unwrap(), edges);
    }
    assert_eq!(
        format!("{:?}", upserted.explain("ada", "peer").unwrap()),
        format!("{:?}", ingested.explain("ada", "peer").unwrap())
    );
}

#[test]
fn both_paths_answer_the_same_property_filter() {
    let ingested = through_ingest("query-ingest");
    let upserted = through_upsert("query-upsert");
    let no_params = BTreeMap::new();
    for cypher in [
        "MATCH (n:Person) WHERE n.age = 36 RETURN n",
        "MATCH (n:Person) WHERE n.active = true RETURN n",
        "MATCH (n:Person) WHERE n.score < 1 RETURN n",
        "MATCH (n:Person {name: 'Ada Lovelace'}) RETURN n, n.tags[0], n.id",
    ] {
        let want = ingested.query(cypher, &no_params).unwrap();
        let got = upserted.query(cypher, &no_params).unwrap();
        assert_eq!(want.len(), 1, "{cypher}");
        assert_eq!(
            (0..got.len())
                .map(|i| got.row(i).to_vec())
                .collect::<Vec<_>>(),
            (0..want.len())
                .map(|i| want.row(i).to_vec())
                .collect::<Vec<_>>(),
            "{cypher}"
        );
    }
}
