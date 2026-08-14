use core_api::{Direction, GraphDb, Value};
use proptest::prelude::*;
use sim_harness::{Oracle, SimFs};

#[derive(Debug, Clone)]
enum Op {
    InsertNode(u8),         // key = "k{n}"
    InsertEdge(u8, u8, u8), // etype "e{a%3}", src k, dst k
    SetProp(u8, u8),        // key, int value
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        any::<u8>().prop_map(Op::InsertNode),
        (any::<u8>(), any::<u8>(), any::<u8>()).prop_map(|(t, s, d)| Op::InsertEdge(t, s, d)),
        (any::<u8>(), any::<u8>()).prop_map(|(k, v)| Op::SetProp(k, v)),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]
    #[test]
    fn engine_matches_oracle(ops in proptest::collection::vec(op_strategy(), 1..200)) {
        let mut db = GraphDb::open_with(SimFs::new()).unwrap();
        let mut oracle = Oracle::new();

        for op in &ops {
            match op {
                Op::InsertNode(n) => {
                    let key = format!("k{n}");
                    let props = vec![("seed".to_string(), Value::Int(*n as i64))];
                    let db_ok = db.insert_node("N", &key, props.clone()).is_ok();
                    let or_ok = oracle.insert_node(&key, &props);
                    prop_assert_eq!(db_ok, or_ok);
                }
                Op::InsertEdge(t, s, d) => {
                    let (etype, src, dst) =
                        (format!("e{}", t % 3), format!("k{s}"), format!("k{d}"));
                    let db_res = db.insert_edge(&etype, &src, &dst).ok();
                    let or_res = oracle.insert_edge(&etype, &src, &dst);
                    prop_assert_eq!(db_res, or_res);
                }
                Op::SetProp(k, v) => {
                    let key = format!("k{k}");
                    let db_ok = db.set_prop(&key, "p", Value::Int(*v as i64)).is_ok();
                    let or_ok = oracle.set_prop(&key, "p", Value::Int(*v as i64));
                    prop_assert_eq!(db_ok, or_ok);
                }
            }
        }

        // Full-state comparison.
        prop_assert_eq!(db.node_count(), oracle.node_count());
        prop_assert_eq!(db.edge_count(), oracle.edge_count());
        for n in 0..=255u8 {
            let key = format!("k{n}");
            // "seed" is written by InsertNode; "p" is written by SetProp — both write paths are swept.
            prop_assert_eq!(db.get_prop(&key, "seed"), oracle.get_prop(&key, "seed"));
            prop_assert_eq!(db.get_prop(&key, "p"), oracle.get_prop(&key, "p"));
            for t in 0..3u8 {
                let etype = format!("e{t}");
                for dir in [Direction::Out, Direction::In] {
                    let db_n = db.neighbors(&key, &etype, dir).unwrap_or_default();
                    let or_n = oracle.neighbors(&key, &etype, dir);
                    prop_assert_eq!(&db_n, &or_n, "key={} etype={} dir={:?}", key, etype, dir);
                }
            }
        }
    }
}
