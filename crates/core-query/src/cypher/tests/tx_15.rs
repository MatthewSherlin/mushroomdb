//! TX-15: the catalog procedures — their columns, their rows, and the errors
//! they give — over a catalog with known contents.

use crate::cypher::proc::{call_procedure, ProcCatalog, ProcResult, SchemaRow};
use core_storage::Value;
use std::collections::BTreeMap;

fn text(s: &str) -> Value {
    Value::Str(s.to_string())
}

struct Catalog;

impl ProcCatalog for Catalog {
    fn labels(&self) -> Vec<String> {
        vec!["Person".into(), "Secret".into()]
    }
    fn relationship_types(&self) -> Vec<String> {
        vec!["KNOWS".into(), "OWNS".into()]
    }
    fn property_keys(&self) -> Vec<String> {
        vec!["age".into(), "name".into()]
    }
    fn relationship_patterns(&self) -> Vec<(String, String, String)> {
        vec![
            ("KNOWS".into(), "Person".into(), "Person".into()),
            ("OWNS".into(), "Secret".into(), "Person".into()),
        ]
    }
    fn indexes(&self) -> Vec<SchemaRow> {
        vec![vec![
            text("person_name"),
            text("range"),
            text("Person"),
            text("name"),
            text("online"),
        ]]
    }
    fn constraints(&self) -> Vec<SchemaRow> {
        vec![vec![
            text("secret_code"),
            text("unique"),
            text("Secret"),
            text("code"),
        ]]
    }
}

fn run(ns: &str, name: &str) -> ProcResult {
    call_procedure(&Catalog, ns, name, &[]).unwrap_or_else(|e| panic!("{ns}.{name}: {e}"))
}

fn column(result: &ProcResult, name: &str) -> Vec<Value> {
    let at = result
        .columns
        .iter()
        .position(|c| c == name)
        .unwrap_or_else(|| panic!("no column {name} in {:?}", result.columns));
    result.rows.iter().map(|r| r[at].clone()).collect()
}

#[test]
fn the_name_lists_have_one_column_each() {
    for (name, column_name, want) in [
        ("labels", "label", vec!["Person", "Secret"]),
        (
            "relationshipTypes",
            "relationshipType",
            vec!["KNOWS", "OWNS"],
        ),
        ("propertyKeys", "propertyKey", vec!["age", "name"]),
    ] {
        let rs = run("db", name);
        assert_eq!(rs.columns, [column_name], "{name}");
        let want: Vec<Value> = want.into_iter().map(text).collect();
        assert_eq!(column(&rs, column_name), want, "{name}");
    }
}

#[test]
fn indexes_and_constraints_use_driver_tool_columns() {
    let rs = run("db", "indexes");
    assert_eq!(
        rs.columns,
        ["name", "type", "labelsOrTypes", "properties", "state"]
    );
    assert_eq!(
        rs.rows,
        vec![vec![
            text("person_name"),
            text("range"),
            Value::List(vec![text("Person")]),
            Value::List(vec![text("name")]),
            text("online"),
        ]]
    );
    let rs = run("db", "constraints");
    assert_eq!(rs.columns, ["name", "type", "labelsOrTypes", "properties"]);
    assert_eq!(
        rs.rows,
        vec![vec![
            text("secret_code"),
            text("unique"),
            Value::List(vec![text("Secret")]),
            Value::List(vec![text("code")]),
        ]]
    );
}

#[test]
fn components_name_the_engine_and_its_version() {
    let rs = run("dbms", "components");
    assert_eq!(rs.columns, ["name", "versions", "edition"]);
    assert_eq!(rs.rows.len(), 1);
    assert_eq!(rs.rows[0][0], text("mushroomdb"));
    assert_eq!(
        rs.rows[0][1],
        Value::List(vec![text(env!("CARGO_PKG_VERSION"))])
    );
    assert_eq!(rs.rows[0][2], text("open"));
}

#[test]
fn the_schema_picture_is_one_row_with_a_node_per_label() {
    let rs = run("db.schema", "visualization");
    assert_eq!(rs.columns, ["nodes", "relationships"]);
    assert_eq!(rs.rows.len(), 1);
    let node = |label: &str| Value::Map(BTreeMap::from([("label".to_string(), text(label))]));
    assert_eq!(
        rs.rows[0][0],
        Value::List(vec![node("Person"), node("Secret")])
    );
    let rel = |t: &str, from: &str, to: &str| {
        Value::Map(BTreeMap::from([
            ("type".to_string(), text(t)),
            ("from".to_string(), text(from)),
            ("to".to_string(), text(to)),
        ]))
    };
    assert_eq!(
        rs.rows[0][1],
        Value::List(vec![
            rel("KNOWS", "Person", "Person"),
            rel("OWNS", "Secret", "Person")
        ])
    );
}

#[test]
fn the_catalog_procedures_take_no_arguments() {
    for (ns, name) in [
        ("db", "labels"),
        ("db", "relationshipTypes"),
        ("db", "propertyKeys"),
        ("db", "indexes"),
        ("db", "constraints"),
        ("dbms", "components"),
        ("db.schema", "visualization"),
    ] {
        let err = call_procedure(&Catalog, ns, name, &[Value::Int(1)]).unwrap_err();
        assert_eq!(err, format!("{ns}.{name} takes no arguments, got 1"));
    }
}
