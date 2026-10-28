//! Offline TY-13 fixture generator. Never run as part of a test or CI.
//!
//! `cargo run -j 3 -p mushroomdb --example gen_v11_fixture -- /new/path/v11_fixture.bin`
//! uses one fixed node, deterministic interner order, an empty history and
//! empty declarations. The single-entry typed column has no map-order choice.
//! Generate twice in separate processes and compare bytes before committing.

use core_storage::snapshot::{stamp_container_version, VERSION_11};
use core_storage::v8::encode::{encode_v8, V8Meta};
use core_storage::{ColumnStore, IdMap, Interner, Topology, Value};
use std::io::Write;

fn main() {
    let path = std::env::args_os()
        .nth(1)
        .expect("usage: gen_v11_fixture <new-output-path>");
    let mut ids = IdMap::new();
    assert_eq!(ids.get_or_insert("e1"), 0);
    let mut symbols = Interner::new();
    let label = symbols.intern("E");
    let mut columns = ColumnStore::new();
    columns.set(0, "on", Value::Date(19_737));
    let meta = V8Meta {
        labels: vec![label],
        v11: true,
        ..V8Meta::default()
    };
    let mut bytes = Vec::new();
    encode_v8(
        None,
        None,
        None,
        None,
        None,
        &Topology::new(),
        &columns,
        &ids,
        &symbols,
        &meta,
        &mut bytes,
    )
    .expect("encode typed V11 sections");
    stamp_container_version(&mut bytes, VERSION_11).expect("stamp V11 header and checksum");
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .expect("create new fixture (refuses overwrite)")
        .write_all(&bytes)
        .expect("write fixture");
}
