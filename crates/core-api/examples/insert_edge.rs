//! One-shot user-edge insert for e2e setup. HTTP `/ingest` is nodes-only.
use core_api::GraphDb;
use std::env;
use std::path::PathBuf;

fn main() {
    let mut args = env::args().skip(1);
    let dir = PathBuf::from(args.next().expect("db dir"));
    let etype = args.next().expect("edge type");
    let src = args.next().expect("src key");
    let dst = args.next().expect("dst key");
    let mut db = GraphDb::open(&dir).expect("open");
    db.insert_edge(&etype, &src, &dst).expect("insert_edge");
}
