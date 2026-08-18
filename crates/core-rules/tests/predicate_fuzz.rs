//! Predicate serde + evaluate robustness.
//!
//! Three generators, 256 cases each (768 total):
//!   (a) arbitrary `Predicate` trees (depth ≤ 3, all 7 variants) → bincode
//!       round-trip
//!   (b) the same tree strategy → `serde_json` round-trip (finite floats only)
//!   (c) arbitrary predicate + two prop maps → `evaluate` returns `Some`/`None`
//!       and never panics (`None` is always acceptable)

use core_rules::{evaluate, NodeView, Predicate};
use core_storage::Value;
use proptest::prelude::*;
use std::collections::HashMap;

fn field_strategy() -> impl Strategy<Value = String> {
    proptest::collection::vec(prop::char::range('a', 'z'), 0..8)
        .prop_map(|cs| cs.into_iter().collect())
}

fn finite_f64() -> BoxedStrategy<f64> {
    any::<f64>()
        .prop_filter("finite", |x| x.is_finite())
        .boxed()
}

/// JSON numbers are decimal; tiny binary floats do not survive `to_string`.
/// Integers and dyadic rationals in a small range round-trip exactly.
fn json_safe_f64() -> BoxedStrategy<f64> {
    prop_oneof![
        (-64i32..=64).prop_map(|i| i as f64),
        (-64i32..=64).prop_map(|i| i as f64 / 4.0),
        Just(0.0_f64),
        Just(1.0_f64),
    ]
    .boxed()
}

fn overlap_min() -> impl Strategy<Value = f64> {
    (1u8..=8).prop_map(|k| k as f64 / 8.0)
}

fn leaf_predicate_with(num: BoxedStrategy<f64>) -> impl Strategy<Value = Predicate> {
    let pos = num.clone().prop_map(|x| x.abs());
    let km = num.prop_map(|x| {
        let a = x.abs();
        if a == 0.0 {
            1.0
        } else {
            a
        }
    });
    prop_oneof![
        field_strategy().prop_map(|field| Predicate::KeyMatch { field }),
        field_strategy().prop_map(|field| Predicate::FieldEqual { field }),
        (field_strategy(), overlap_min())
            .prop_map(|(field, min)| Predicate::Overlap { field, min }),
        (field_strategy(), pos)
            .prop_map(|(field, tolerance)| Predicate::NumericWithin { field, tolerance }),
        (field_strategy(), km).prop_map(|(field, km)| Predicate::GeoRadius { field, km }),
        (field_strategy(), overlap_min())
            .prop_map(|(field, min)| Predicate::VectorSimilar { field, min }),
        Just(Predicate::All(vec![])),
    ]
}

fn predicate_strategy() -> impl Strategy<Value = Predicate> {
    leaf_predicate_with(finite_f64()).prop_recursive(3, 16, 3, |inner| {
        proptest::collection::vec(inner, 1..4).prop_map(Predicate::All)
    })
}

fn json_predicate_strategy() -> impl Strategy<Value = Predicate> {
    leaf_predicate_with(json_safe_f64()).prop_recursive(3, 16, 3, |inner| {
        proptest::collection::vec(inner, 1..4).prop_map(Predicate::All)
    })
}

fn scalar_value() -> impl Strategy<Value = Value> {
    prop_oneof![
        any::<i64>().prop_map(Value::Int),
        any::<f64>().prop_map(Value::Float),
        field_strategy().prop_map(Value::Str),
        any::<bool>().prop_map(Value::Bool),
    ]
}

fn value_strategy() -> impl Strategy<Value = Value> {
    prop_oneof![
        scalar_value(),
        proptest::collection::vec(scalar_value(), 0..6).prop_map(Value::List),
    ]
}

fn props_strategy() -> impl Strategy<Value = HashMap<String, Value>> {
    proptest::collection::hash_map(field_strategy(), value_strategy(), 0..6)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]
    #[test]
    fn predicate_bincode_roundtrip(p in predicate_strategy()) {
        let bytes = bincode::serialize(&p).expect("serialize");
        let back: Predicate = bincode::deserialize(&bytes).expect("deserialize");
        prop_assert_eq!(p, back);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]
    #[test]
    fn predicate_json_roundtrip(p in json_predicate_strategy()) {
        let s = serde_json::to_string(&p).expect("to_json");
        let back: Predicate = serde_json::from_str(&s).expect("from_json");
        prop_assert_eq!(p, back);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]
    #[test]
    fn evaluate_never_panics_on_arbitrary_props(
        p in predicate_strategy(),
        src_props in props_strategy(),
        dst_props in props_strategy(),
    ) {
        let sp = |f: &str| src_props.get(f).cloned();
        let dp = |f: &str| dst_props.get(f).cloned();
        let src = NodeView { key: "src", props: &sp };
        let dst = NodeView { key: "dst", props: &dp };
        let score = evaluate(&p, &src, &dst);
        if let Some(s) = score {
            prop_assert!(s.is_finite() && (0.0..=1.0).contains(&s), "score {s}");
        }
    }
}
