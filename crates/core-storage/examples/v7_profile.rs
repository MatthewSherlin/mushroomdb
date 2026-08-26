//! One-off stage profile of a V7 snapshot open.
//!
//! ```text
//! cargo run --release -p mushroomdb-storage --example v7_profile -- <snapshot.bin>
//! ```

use std::time::Instant;

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: v7_profile <snapshot.bin>");
    let bytes = std::fs::read(&path).expect("read snapshot");
    println!("file_bytes={}", bytes.len());
    assert_eq!(&bytes[0..4], b"GDB1");
    let version = u16::from_le_bytes([bytes[4], bytes[5]]);
    println!("version={version}");
    assert_eq!(version, 7, "expected V7 snapshot");

    let t = Instant::now();
    let inner = zstd::decode_all(&bytes[6..]).expect("zstd");
    println!(
        "zstd_decode_s={:.3} inner_bytes={}",
        t.elapsed().as_secs_f64(),
        inner.len()
    );

    let t = Instant::now();
    let crc = crc32fast::hash(&inner[4..]);
    println!(
        "crc_s={:.3} crc_ok={}",
        t.elapsed().as_secs_f64(),
        crc == u32::from_le_bytes(inner[0..4].try_into().unwrap())
    );

    let payload = &inner[4..];
    let topo_len = u32::from_le_bytes(payload[0..4].try_into().unwrap()) as usize;
    let props_off = 4 + topo_len;
    let props_len =
        u32::from_le_bytes(payload[props_off..props_off + 4].try_into().unwrap()) as usize;
    let meta_len = payload.len() - props_off - 4 - props_len;
    println!("topo_bytes={topo_len} props_bytes={props_len} meta_bytes={meta_len}");

    let t = Instant::now();
    let state = core_storage::snapshot::decode(&bytes)
        .expect("decode")
        .expect("some");
    println!("decode_total_s={:.3}", t.elapsed().as_secs_f64());
    drop(state);
}
