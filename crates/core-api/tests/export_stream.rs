//! `for_each_node_for_export` / `for_each_edge_for_export` hand over exactly
//! what `all_nodes_for_export` / `all_edges_for_export` return, in the same
//! order: live, after deletes and renames, after a snapshot, and after a reopen.
use mushroomdb::{default_max_edges, GraphDb, Predicate, RealFs, RuleDef, Value};
use std::convert::Infallible;

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "mushroomdb-exportstream-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn rule() -> RuleDef {
    let predicate = Predicate::FieldEqual {
        field: "city".into(),
    };
    RuleDef {
        name: "same_city".into(),
        src_label: "P".into(),
        dst_label: "P".into(),
        max_edges: Some(default_max_edges(&predicate)),
        predicate,
        edge_type: "SAME_CITY".into(),
        weight_prop: None,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
        namespace: None,
    }
}

fn check(db: &GraphDb<RealFs>, what: &str) {
    let mut nodes = Vec::new();
    db.for_each_node_for_export::<Infallible>(|n| {
        nodes.push(n);
        Ok(())
    })
    .unwrap();
    assert_eq!(nodes, db.all_nodes_for_export(), "{what}: nodes");
    let mut edges = Vec::new();
    db.for_each_edge_for_export::<Infallible>(|e| {
        edges.push(e);
        Ok(())
    })
    .unwrap();
    assert_eq!(edges, db.all_edges_for_export(), "{what}: edges");
}

#[test]
fn streamed_export_equals_the_materialised_export() {
    let dir = tmp("eq");
    let mut db = GraphDb::<RealFs>::open(&dir).unwrap();
    check(&db, "empty");
    db.create_rule(rule()).unwrap();
    db.write_batch(|b| {
        for i in 0..40 {
            b.insert_node(
                if i % 5 == 0 { "Q" } else { "P" },
                &format!("k{}", (i * 7) % 40),
                vec![
                    ("city".into(), Value::Str(format!("c{}", i % 4))),
                    ("n".into(), Value::Int(i)),
                ],
            );
        }
        for i in 0..39 {
            b.insert_edge("NEXT", &format!("k{i}"), &format!("k{}", i + 1));
            b.insert_edge("ALSO", &format!("k{}", i + 1), &format!("k{i}"));
        }
    })
    .unwrap();
    check(&db, "live");
    db.delete_node("k3").unwrap();
    db.rename_node("k4", "zz").unwrap();
    check(&db, "after delete and rename");
    db.snapshot().unwrap();
    check(&db, "after snapshot");
    drop(db);
    let db = GraphDb::<RealFs>::open(&dir).unwrap();
    check(&db, "after reopen");
}

#[test]
fn streamed_export_stops_at_the_first_error() {
    let dir = tmp("err");
    let mut db = GraphDb::<RealFs>::open(&dir).unwrap();
    db.write_batch(|b| {
        for i in 0..5 {
            b.insert_node("P", &format!("k{i}"), vec![]);
        }
    })
    .unwrap();
    let mut seen = 0;
    let r = db.for_each_node_for_export(|_| {
        seen += 1;
        if seen == 2 {
            Err("stop")
        } else {
            Ok(())
        }
    });
    assert_eq!((r, seen), (Err("stop"), 2));
}
