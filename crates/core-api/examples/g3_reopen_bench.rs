//! G3 reopen bench: time a cold snapshot open, optionally rewriting the
//! snapshot in the current format first.
//!
//! ```text
//! cargo run --release -p mushroomdb --example g3_reopen_bench -- <db_dir> convert
//! cargo run --release -p mushroomdb --example g3_reopen_bench -- <db_dir> open
//! ```
//!
//! `convert` opens the store (timed), then writes a fresh snapshot in the
//! current VERSION (timed).  `open` just opens (timed).  Wrap either mode in
//! `/usr/bin/time -l` to capture peak RSS.

use core_api::GraphDb;
use std::time::Instant;

fn main() {
    let usage = "usage: g3_reopen_bench <db_dir> <convert|open>";
    let mut args = std::env::args().skip(1);
    let dir = std::path::PathBuf::from(args.next().expect(usage));
    let mode = args.next().expect(usage);

    let t = Instant::now();
    let mut db = GraphDb::open(&dir).expect("open");
    let open_s = t.elapsed().as_secs_f64();
    println!(
        "open_s={open_s:.3} nodes={} edges={}",
        db.node_count(),
        db.edge_count()
    );

    match mode.as_str() {
        "convert" => {
            let t = Instant::now();
            db.snapshot().expect("snapshot");
            println!("snapshot_write_s={:.3}", t.elapsed().as_secs_f64());
            let bytes = std::fs::metadata(dir.join("snapshot.bin"))
                .map(|m| m.len())
                .unwrap_or(0);
            println!("snapshot_bytes={bytes}");
        }
        "open" => {}
        other => panic!("unknown mode {other:?}; {usage}"),
    }
}
