//! TX-NS (defect-ledger rows 148 and 149): over Bolt, a bare node,
//! relationship or path variable comes back as the Node, Relationship or Path
//! structure, in the form the negotiated version spells it, and a write's
//! closing `SUCCESS` carries its counters as `stats`. A role-scoped connection
//! sees only the nodes its role may see, and no relationship to a hidden one.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use bolt::connection::Connection;
use bolt::messages::graph::{integer_id, node_element_id, relationship_element_id};
use bolt::messages::hello::{AuthToken, Hello, TokenAuthenticator};
use bolt::messages::{ClientMessage, MessageKind, ServerMessage};
use bolt::packstream::{PackStruct, PackValue};
use mushroomdb::{RoleDef, Schema, SharedDb};

const FULL: &str = "full-secret";
const ROLE: &str = "people-secret";

fn authenticator() -> Arc<TokenAuthenticator> {
    let mut roles = HashMap::new();
    roles.insert(ROLE.to_string(), "people".to_string());
    Arc::new(TokenAuthenticator::new(Some(FULL.into()), roles))
}

/// ada -KNOWS-> bob; ada -OWNS-> sec, a Secret the `people`
/// role cannot see.
fn store() -> (SharedDb, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path()).unwrap();
    db.write()
        .apply_schema(&Schema {
            roles: vec![RoleDef {
                name: "people".into(),
                keys: vec![],
                labels: vec!["Person".into()],
                visible_where: None,
                namespaces: None,
                write: None,
            }],
            ..Default::default()
        })
        .unwrap();
    let none = Default::default();
    for q in [
        "CREATE (:Person {id: 'ada', name: 'Ada'})",
        "CREATE (:Person {id: 'bob', name: 'Bob'})",
        "CREATE (:Secret {id: 'sec'})",
        "MATCH (a {id: 'ada'}), (b {id: 'bob'}) CREATE (a)-[:KNOWS]->(b)",
        "MATCH (a {id: 'ada'}) SET a.age = 36",
        "MATCH (a {id: 'ada'}), (b {id: 'sec'}) CREATE (a)-[:OWNS]->(b)",
    ] {
        db.write().query_write(q, &none).unwrap();
    }
    (db, dir)
}

fn connected(db: &SharedDb, token: &str, version: (u8, u8)) -> Connection {
    let mut conn = Connection::new(
        authenticator(),
        Duration::from_secs(30),
        "c".into(),
        version,
    )
    .with_database(db.clone(), Duration::from_millis(100));
    let out = conn.handle(ClientMessage::Hello(Hello {
        user_agent: None,
        auth: AuthToken::Basic {
            principal: None,
            credentials: token.into(),
        },
    }));
    assert!(
        matches!(out.as_slice(), [ServerMessage::Success(_)]),
        "{out:?}"
    );
    conn
}

fn run(query: &str) -> ClientMessage {
    ClientMessage::Other(
        MessageKind::Run,
        vec![
            PackValue::String(query.into()),
            PackValue::Map(vec![]),
            PackValue::Map(vec![]),
        ],
    )
}

fn pull_all() -> ClientMessage {
    ClientMessage::Other(
        MessageKind::Pull,
        vec![PackValue::Map(vec![("n".into(), PackValue::Int(-1))])],
    )
}

/// `RUN` + `PULL`: the rows and the closing `SUCCESS` metadata.
fn query(conn: &mut Connection, q: &str) -> (Vec<Vec<PackValue>>, Vec<(String, PackValue)>) {
    let out = conn.handle(run(q));
    assert!(
        matches!(out.as_slice(), [ServerMessage::Success(_)]),
        "{q}: {out:?}"
    );
    let out = conn.handle(pull_all());
    let (end, records) = out.split_last().expect("a reply");
    let rows = records
        .iter()
        .map(|m| match m {
            ServerMessage::Record(row) => row.clone(),
            other => panic!("{q}: {other:?}"),
        })
        .collect();
    match end {
        ServerMessage::Success(meta) => (rows, meta.clone()),
        other => panic!("{q}: {other:?}"),
    }
}

fn structure(v: &PackValue) -> &PackStruct {
    match v {
        PackValue::Struct(s) => s,
        other => panic!("not a structure: {other:?}"),
    }
}

fn s(text: &str) -> PackValue {
    PackValue::String(text.into())
}

fn map_get<'a>(v: &'a PackValue, key: &str) -> Option<&'a PackValue> {
    match v {
        PackValue::Map(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
        other => panic!("not a map: {other:?}"),
    }
}

fn stats(meta: &[(String, PackValue)]) -> Option<&PackValue> {
    meta.iter().find(|(k, _)| k == "stats").map(|(_, v)| v)
}

#[test]
fn return_n_is_a_node_structure_in_bolt_5_and_4_4() {
    let (db, _dir) = store();
    let mut conn = connected(&db, FULL, (5, 0));
    let (rows, _) = query(
        &mut conn,
        "MATCH (n:Person {id: 'ada'}) RETURN n, n.name AS name",
    );
    let node = structure(&rows[0][0]);
    assert_eq!(node.tag, 0x4E);
    assert_eq!(node.fields.len(), 4);
    assert_eq!(node.fields[0], PackValue::Int(integer_id("n:ada")));
    assert_eq!(node.fields[1], PackValue::List(vec![s("Person")]));
    assert_eq!(map_get(&node.fields[2], "name"), Some(&s("Ada")));
    assert_eq!(map_get(&node.fields[2], "age"), Some(&PackValue::Int(36)));
    assert_eq!(node.fields[3], s(&node_element_id("ada")));
    assert_eq!(rows[0][1], s("Ada"), "a property column is unchanged");

    let mut old = connected(&db, FULL, (4, 4));
    let (rows, _) = query(&mut old, "MATCH (n:Person {id: 'ada'}) RETURN n");
    let node = structure(&rows[0][0]);
    assert_eq!((node.tag, node.fields.len()), (0x4E, 3));
}

#[test]
fn return_r_is_a_relationship_whose_ends_match_their_nodes() {
    let (db, _dir) = store();
    let mut conn = connected(&db, FULL, (5, 0));
    let (rows, _) = query(
        &mut conn,
        "MATCH (a {id: 'ada'})-[r:KNOWS]->(b) RETURN a, r, b",
    );
    let (a, r, b) = (
        structure(&rows[0][0]),
        structure(&rows[0][1]),
        structure(&rows[0][2]),
    );
    assert_eq!(r.tag, 0x52);
    assert_eq!(r.fields.len(), 8);
    assert_eq!(r.fields[1], a.fields[0], "start id is the start node's id");
    assert_eq!(r.fields[2], b.fields[0], "end id is the end node's id");
    assert_eq!(r.fields[3], s("KNOWS"));
    assert!(
        matches!(r.fields[4], PackValue::Map(_)),
        "{:?}",
        r.fields[4]
    );
    assert_eq!(
        r.fields[5],
        s(&relationship_element_id("KNOWS", "ada", "bob"))
    );
    assert_eq!(r.fields[6], a.fields[3]);
    assert_eq!(r.fields[7], b.fields[3]);
    let mut old = connected(&db, FULL, (4, 4));
    let (rows, _) = query(&mut old, "MATCH ()-[r:KNOWS]->() RETURN r");
    assert_eq!(structure(&rows[0][0]).fields.len(), 5);
}

#[test]
fn return_p_is_a_path_structure() {
    let (db, _dir) = store();
    let mut conn = connected(&db, FULL, (5, 0));
    let (rows, _) = query(
        &mut conn,
        "MATCH p = (b {id: 'bob'})<-[:KNOWS]-(a) RETURN p",
    );
    let path = structure(&rows[0][0]);
    assert_eq!(path.tag, 0x50);
    let PackValue::List(nodes) = &path.fields[0] else {
        panic!()
    };
    let PackValue::List(rels) = &path.fields[1] else {
        panic!()
    };
    assert_eq!(structure(&nodes[0]).fields[3], s("n:bob"));
    assert_eq!(structure(&nodes[1]).fields[3], s("n:ada"));
    let rel = structure(&rels[0]);
    assert_eq!((rel.tag, rel.fields.len()), (0x72, 4));
    // Walked from bob against the relationship's direction: index -1, then node 1.
    assert_eq!(
        path.fields[2],
        PackValue::List(vec![PackValue::Int(-1), PackValue::Int(1)])
    );
}

#[test]
fn a_role_sees_its_nodes_whole_and_no_relationship_to_a_hidden_node() {
    let (db, _dir) = store();
    let mut conn = connected(&db, ROLE, (5, 0));
    let (rows, _) = query(&mut conn, "MATCH (a {id: 'ada'})-[r]->(b) RETURN r, b");
    assert_eq!(
        rows.len(),
        1,
        "the OWNS relationship to the Secret is not there"
    );
    assert_eq!(structure(&rows[0][0]).fields[3], s("KNOWS"));
    let (rows, _) = query(&mut conn, "MATCH (n) RETURN n");
    assert_eq!(rows.len(), 2);
    for row in &rows {
        assert_eq!(
            structure(&row[0]).fields[1],
            PackValue::List(vec![s("Person")])
        );
    }
    // The same inside a transaction the role opens.
    let out = conn.handle(ClientMessage::Other(
        MessageKind::Begin,
        vec![PackValue::Map(vec![])],
    ));
    assert!(
        matches!(out.as_slice(), [ServerMessage::Success(_)]),
        "{out:?}"
    );
    let (rows, _) = query(&mut conn, "MATCH p = (a {id: 'ada'})-[]->(b) RETURN p");
    assert_eq!(rows.len(), 1);
    assert_eq!(structure(&rows[0][0]).tag, 0x50);
}

#[test]
fn a_write_success_carries_its_counters() {
    let (db, _dir) = store();
    let mut conn = connected(&db, FULL, (5, 0));
    let int = |n: i64| PackValue::Int(n);
    let (_, meta) = query(&mut conn, "CREATE (:Person {id: 'cam', name: 'Cam'})");
    let st = stats(&meta).expect("a CREATE has stats");
    assert_eq!(map_get(st, "nodes-created"), Some(&int(1)));
    assert_eq!(map_get(st, "labels-added"), Some(&int(1)));
    assert_eq!(map_get(st, "properties-set"), Some(&int(2)));
    assert_eq!(
        map_get(st, "nodes-deleted"),
        None,
        "an unmoved counter is left out"
    );
    let (_, meta) = query(
        &mut conn,
        "MATCH (a {id: 'cam'}), (b {id: 'bob'}) CREATE (a)-[:KNOWS]->(b)",
    );
    assert_eq!(
        map_get(stats(&meta).unwrap(), "relationships-created"),
        Some(&int(1))
    );
    let (rows, meta) = query(&mut conn, "MATCH (n {id: 'cam'}) SET n.age = 7 RETURN n");
    assert_eq!(structure(&rows[0][0]).tag, 0x4E);
    assert_eq!(
        map_get(stats(&meta).unwrap(), "properties-set"),
        Some(&int(1))
    );
    let (_, meta) = query(&mut conn, "MATCH (n {id: 'cam'}) DETACH DELETE n");
    let st = stats(&meta).unwrap();
    assert_eq!(map_get(st, "nodes-deleted"), Some(&int(1)));
    assert_eq!(map_get(st, "relationships-deleted"), Some(&int(1)));
    let (_, meta) = query(
        &mut conn,
        "CREATE INDEX person_name FOR (n:Person) ON (n.name)",
    );
    assert_eq!(
        map_get(stats(&meta).unwrap(), "indexes-added"),
        Some(&int(1))
    );
    let (_, meta) = query(&mut conn, "MATCH (n:Person) RETURN n.name");
    assert_eq!(stats(&meta), None, "a read has no stats");
    // Inside a transaction, per statement.
    let out = conn.handle(ClientMessage::Other(
        MessageKind::Begin,
        vec![PackValue::Map(vec![])],
    ));
    assert!(
        matches!(out.as_slice(), [ServerMessage::Success(_)]),
        "{out:?}"
    );
    let (rows, meta) = query(&mut conn, "CREATE (n:Person {id: 'dan'}) RETURN n");
    assert_eq!(structure(&rows[0][0]).fields[3], s("n:dan"));
    assert_eq!(
        map_get(stats(&meta).unwrap(), "nodes-created"),
        Some(&int(1))
    );
}
