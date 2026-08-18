use crate::def::Predicate;
use core_storage::{list_tokens, Value, ValueKey};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Default)]
pub struct SideIndex {
    by_key: BTreeMap<ValueKey, BTreeSet<u32>>,
}

#[derive(Debug, Default)]
pub struct RuleIndex {
    pub src_side: SideIndex,
    pub dst_side: SideIndex,
}

pub enum CandidateSpec<'a> {
    ByKey,
    Scalar { field: &'a str },
    Tokens { field: &'a str },
    NumericBucket { field: &'a str, tolerance: f64 },
    GeoGrid { field: &'a str, km: f64 },
    ScanAll { field: &'a str },
}

/// Returns the candidate strategy derived from `p`.
///
/// `All(parts)` delegates to `parts[0]`: order predicates most-selective-first;
/// a leading `VectorSimilar` means full-scan candidates.
///
/// # Panics
///
/// Panics on `All([])`. Predicates must pass `RuleDef::validate()` first.
pub fn candidate_spec(p: &Predicate) -> CandidateSpec<'_> {
    match p {
        Predicate::KeyMatch { .. } => CandidateSpec::ByKey,
        Predicate::FieldEqual { field } => CandidateSpec::Scalar { field },
        Predicate::Overlap { field, .. } => CandidateSpec::Tokens { field },
        Predicate::NumericWithin { field, tolerance } => CandidateSpec::NumericBucket {
            field,
            tolerance: *tolerance,
        },
        Predicate::GeoRadius { field, km } => CandidateSpec::GeoGrid { field, km: *km },
        Predicate::VectorSimilar { field, .. } => CandidateSpec::ScanAll { field },
        Predicate::All(parts) => {
            debug_assert!(
                !parts.is_empty(),
                "candidate_spec requires a validated predicate"
            );
            candidate_spec(&parts[0])
        }
    }
}

fn as_finite_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) if f.is_finite() => Some(*f),
        _ => None,
    }
}

fn as_latlon(v: &Value) -> Option<(f64, f64)> {
    let Value::List(items) = v else {
        return None;
    };
    if items.len() != 2 {
        return None;
    }
    let lat = as_finite_f64(&items[0])?;
    let lon = as_finite_f64(&items[1])?;
    if (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon) {
        Some((lat, lon))
    } else {
        None
    }
}

fn as_numeric_list(v: &Value) -> Option<Vec<f64>> {
    let Value::List(items) = v else {
        return None;
    };
    if items.is_empty() {
        return None;
    }
    items.iter().map(as_finite_f64).collect()
}

fn floor_to_i64(x: f64) -> i64 {
    let floored = x.floor();
    if !floored.is_finite() {
        return 0;
    }
    if floored >= i64::MAX as f64 {
        i64::MAX
    } else if floored <= i64::MIN as f64 {
        i64::MIN
    } else {
        floored as i64
    }
}

/// Two values within `tolerance` always land in adjacent buckets
/// (`|floor(a/tol) − floor(b/tol)| ≤ 1`), so probing `{b−1, b, b+1}` is a
/// superset of every evaluate-match.
fn numeric_index_key(v: f64, tolerance: f64) -> Option<ValueKey> {
    if !tolerance.is_finite() || tolerance < 0.0 {
        return None;
    }
    if tolerance == 0.0 {
        let v = if v == 0.0 { 0.0_f64 } else { v };
        return Some(ValueKey::FloatBits(v.to_bits()));
    }
    Some(ValueKey::Int(floor_to_i64(v / tolerance)))
}

fn numeric_probe_keys(v: f64, tolerance: f64) -> BTreeSet<ValueKey> {
    match numeric_index_key(v, tolerance) {
        None => BTreeSet::new(),
        Some(k @ ValueKey::FloatBits(_)) => BTreeSet::from([k]),
        Some(ValueKey::Int(b)) => BTreeSet::from([
            ValueKey::Int(b.saturating_sub(1)),
            ValueKey::Int(b),
            ValueKey::Int(b.saturating_add(1)),
        ]),
        Some(other) => BTreeSet::from([other]),
    }
}

fn geo_cell(lat: f64, lon: f64, km: f64) -> Option<(i64, i64, f64, i64)> {
    if !km.is_finite() || km <= 0.0 {
        return None;
    }
    let cell_deg = (km / 111.0).max(1e-6);
    let gx = floor_to_i64(lat / cell_deg);
    // Longitude wraps; lat does not (validated range, no pole crossing
    // within the supported |lat|≲87 envelope — see cos clamp below).
    let lon_cells = (360.0 / cell_deg).ceil() as i64;
    let lon_cells = lon_cells.max(1);
    let gy = floor_to_i64(lon / cell_deg).rem_euclid(lon_cells);
    Some((gx, gy, cell_deg, lon_cells))
}

fn geo_index_key(lat: f64, lon: f64, km: f64) -> Option<ValueKey> {
    let (gx, gy, _, _) = geo_cell(lat, lon, km)?;
    Some(ValueKey::Str(format!("{gx}|{gy}")))
}

fn geo_probe_keys(lat: f64, lon: f64, km: f64) -> BTreeSet<ValueKey> {
    let Some((gx, gy, cell_deg, lon_cells)) = geo_cell(lat, lon, km) else {
        return BTreeSet::new();
    };
    // Cos clamp keeps the probe a superset up to |lat| ≈ 87.
    let cos_lat = lat.to_radians().cos().max(0.05);
    let n = ((km / (111.0 * cos_lat)) / cell_deg).ceil();
    let n = if n.is_finite() {
        floor_to_i64(n).max(0)
    } else {
        0
    };
    let mut out = BTreeSet::new();
    for dx in -1..=1 {
        for dy in -n..=n {
            let cx = gx.saturating_add(dx);
            let cy = gy.saturating_add(dy).rem_euclid(lon_cells);
            out.insert(ValueKey::Str(format!("{cx}|{cy}")));
        }
    }
    out
}

/// Vector candidates are a deliberate full scan of opposite-side
/// vector-bearing nodes; ANN is Plan 8+.
const SCAN_ALL_SENTINEL: ValueKey = ValueKey::Bool(true);

impl SideIndex {
    fn index_keys(spec: &CandidateSpec, get: &dyn Fn(&str) -> Option<Value>) -> BTreeSet<ValueKey> {
        match spec {
            CandidateSpec::ByKey => BTreeSet::new(),
            CandidateSpec::Scalar { field } => get(field)
                .as_ref()
                .and_then(ValueKey::from_value)
                .into_iter()
                .collect(),
            CandidateSpec::Tokens { field } => get(field)
                .as_ref()
                .and_then(list_tokens)
                .unwrap_or_default(),
            CandidateSpec::NumericBucket { field, tolerance } => get(field)
                .as_ref()
                .and_then(as_finite_f64)
                .and_then(|v| numeric_index_key(v, *tolerance))
                .into_iter()
                .collect(),
            CandidateSpec::GeoGrid { field, km } => get(field)
                .as_ref()
                .and_then(as_latlon)
                .and_then(|(lat, lon)| geo_index_key(lat, lon, *km))
                .into_iter()
                .collect(),
            CandidateSpec::ScanAll { field } => get(field)
                .as_ref()
                .and_then(as_numeric_list)
                .map(|_| SCAN_ALL_SENTINEL)
                .into_iter()
                .collect(),
        }
    }

    fn probe_keys(spec: &CandidateSpec, get: &dyn Fn(&str) -> Option<Value>) -> BTreeSet<ValueKey> {
        match spec {
            CandidateSpec::ByKey | CandidateSpec::Scalar { .. } | CandidateSpec::Tokens { .. } => {
                Self::index_keys(spec, get)
            }
            CandidateSpec::NumericBucket { field, tolerance } => get(field)
                .as_ref()
                .and_then(as_finite_f64)
                .map(|v| numeric_probe_keys(v, *tolerance))
                .unwrap_or_default(),
            CandidateSpec::GeoGrid { field, km } => get(field)
                .as_ref()
                .and_then(as_latlon)
                .map(|(lat, lon)| geo_probe_keys(lat, lon, *km))
                .unwrap_or_default(),
            CandidateSpec::ScanAll { field } => get(field)
                .as_ref()
                .and_then(as_numeric_list)
                .map(|_| SCAN_ALL_SENTINEL)
                .into_iter()
                .collect(),
        }
    }

    pub fn insert(&mut self, spec: &CandidateSpec, node: u32, get: &dyn Fn(&str) -> Option<Value>) {
        for k in Self::index_keys(spec, get) {
            self.by_key.entry(k).or_default().insert(node);
        }
    }

    pub fn remove(&mut self, spec: &CandidateSpec, node: u32, get: &dyn Fn(&str) -> Option<Value>) {
        for k in Self::index_keys(spec, get) {
            if let Some(set) = self.by_key.get_mut(&k) {
                set.remove(&node);
                if set.is_empty() {
                    self.by_key.remove(&k);
                }
            }
        }
    }

    pub fn candidates(
        &self,
        spec: &CandidateSpec,
        get: &dyn Fn(&str) -> Option<Value>,
    ) -> BTreeSet<u32> {
        let mut out = BTreeSet::new();
        for k in Self::probe_keys(spec, get) {
            if let Some(set) = self.by_key.get(&k) {
                out.extend(set.iter().copied());
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::def::Predicate;
    use core_storage::Value;
    use std::collections::HashMap;

    fn getter(map: &HashMap<String, Value>) -> impl Fn(&str) -> Option<Value> + '_ {
        move |f: &str| map.get(f).cloned()
    }

    #[test]
    fn scalar_index_buckets_by_value() {
        let pred = Predicate::FieldEqual {
            field: "ind".into(),
        };
        let spec = candidate_spec(&pred);
        let mut idx = SideIndex::default();
        let a: HashMap<_, _> = [("ind".to_string(), Value::Str("arch".into()))].into();
        let b: HashMap<_, _> = [("ind".to_string(), Value::Str("law".into()))].into();
        idx.insert(&spec, 1, &getter(&a));
        idx.insert(&spec, 2, &getter(&b));
        idx.insert(&spec, 3, &getter(&a));
        let c = idx.candidates(&spec, &getter(&a));
        assert_eq!(c.into_iter().collect::<Vec<_>>(), vec![1, 3]);
        idx.remove(&spec, 3, &getter(&a));
        assert_eq!(idx.candidates(&spec, &getter(&a)).len(), 1);
        // node without the field indexes nothing and matches nothing
        let empty: HashMap<String, Value> = HashMap::new();
        idx.insert(&spec, 9, &getter(&empty));
        assert!(idx.candidates(&spec, &getter(&empty)).is_empty());
    }

    #[test]
    fn token_index_unions_buckets() {
        let mk =
            |items: &[&str]| Value::List(items.iter().map(|s| Value::Str((*s).into())).collect());
        let pred = Predicate::Overlap {
            field: "tags".into(),
            min: 0.5,
        };
        let spec = candidate_spec(&pred);
        let mut idx = SideIndex::default();
        let a: HashMap<_, _> = [("tags".to_string(), mk(&["x", "y"]))].into();
        let b: HashMap<_, _> = [("tags".to_string(), mk(&["y", "z"]))].into();
        let c: HashMap<_, _> = [("tags".to_string(), mk(&["q"]))].into();
        idx.insert(&spec, 1, &getter(&a));
        idx.insert(&spec, 2, &getter(&b));
        idx.insert(&spec, 3, &getter(&c));
        let probe: HashMap<_, _> = [("tags".to_string(), mk(&["y"]))].into();
        assert_eq!(
            idx.candidates(&spec, &getter(&probe))
                .into_iter()
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        idx.remove(&spec, 2, &getter(&b));
        assert_eq!(
            idx.candidates(&spec, &getter(&probe))
                .into_iter()
                .collect::<Vec<_>>(),
            vec![1]
        );
    }

    #[test]
    fn all_uses_first_part_and_bykey_indexes_nothing() {
        let all = Predicate::All(vec![
            Predicate::FieldEqual {
                field: "ind".into(),
            },
            Predicate::Overlap {
                field: "tags".into(),
                min: 0.5,
            },
        ]);
        assert!(matches!(
            candidate_spec(&all),
            CandidateSpec::Scalar { field: "ind" }
        ));
        let km = Predicate::KeyMatch { field: "fk".into() };
        assert!(matches!(candidate_spec(&km), CandidateSpec::ByKey));
        let mut idx = SideIndex::default();
        let a: HashMap<_, _> = [("fk".to_string(), Value::Str("c1".into()))].into();
        idx.insert(&candidate_spec(&km), 1, &getter(&a));
        assert!(idx.candidates(&candidate_spec(&km), &getter(&a)).is_empty());
    }

    fn year(v: Value) -> HashMap<String, Value> {
        [("year".to_string(), v)].into()
    }

    fn loc(lat: f64, lon: f64) -> HashMap<String, Value> {
        [(
            "loc".to_string(),
            Value::List(vec![Value::Float(lat), Value::Float(lon)]),
        )]
        .into()
    }

    fn emb(vals: &[f64]) -> HashMap<String, Value> {
        [(
            "emb".to_string(),
            Value::List(vals.iter().copied().map(Value::Float).collect()),
        )]
        .into()
    }

    fn bucket_int(spec: &CandidateSpec, map: &HashMap<String, Value>) -> Option<i64> {
        match SideIndex::index_keys(spec, &getter(map)).into_iter().next() {
            Some(ValueKey::Int(b)) => Some(b),
            _ => None,
        }
    }

    #[test]
    fn numeric_bucket_adjacency_and_far_value() {
        let pred = Predicate::NumericWithin {
            field: "year".into(),
            tolerance: 2.0,
        };
        let spec = candidate_spec(&pred);
        assert!(matches!(
            spec,
            CandidateSpec::NumericBucket {
                field: "year",
                tolerance
            } if tolerance == 2.0
        ));

        let v10 = year(Value::Float(10.0));
        let v119 = year(Value::Float(11.9));
        let v99 = year(Value::Float(9.9));
        let v141 = year(Value::Float(14.1));

        let b10 = bucket_int(&spec, &v10).unwrap();
        let b119 = bucket_int(&spec, &v119).unwrap();
        let b99 = bucket_int(&spec, &v99).unwrap();
        // 10.0 and 11.9 share a bucket; 9.9 is adjacent (forces ±1 probe).
        assert!((b10 - b119).abs() <= 1);
        assert!((b10 - b99).abs() <= 1);

        let mut idx = SideIndex::default();
        idx.insert(&spec, 1, &getter(&v10));
        idx.insert(&spec, 2, &getter(&v119));
        idx.insert(&spec, 3, &getter(&v141));
        idx.insert(&spec, 4, &getter(&v99));
        let hits = idx.candidates(&spec, &getter(&v10));
        assert_eq!(hits.into_iter().collect::<Vec<_>>(), vec![1, 2, 4]);
    }

    #[test]
    fn numeric_tol_zero_int_float_collide() {
        let pred = Predicate::NumericWithin {
            field: "year".into(),
            tolerance: 0.0,
        };
        let spec = candidate_spec(&pred);
        let mut idx = SideIndex::default();
        idx.insert(&spec, 1, &getter(&year(Value::Int(2))));
        assert_eq!(
            idx.candidates(&spec, &getter(&year(Value::Float(2.0))))
                .into_iter()
                .collect::<Vec<_>>(),
            vec![1]
        );
        assert!(idx
            .candidates(&spec, &getter(&year(Value::Float(2.1))))
            .is_empty());
    }

    #[test]
    fn numeric_tol_zero_signed_zero_collides() {
        let pred = Predicate::NumericWithin {
            field: "year".into(),
            tolerance: 0.0,
        };
        let spec = candidate_spec(&pred);
        let neg = year(Value::Float(-0.0));
        let pos = year(Value::Float(0.0));
        let mut idx = SideIndex::default();
        idx.insert(&spec, 1, &getter(&neg));
        assert_eq!(
            idx.candidates(&spec, &getter(&pos))
                .into_iter()
                .collect::<Vec<_>>(),
            vec![1]
        );
        let mut idx2 = SideIndex::default();
        idx2.insert(&spec, 2, &getter(&pos));
        assert_eq!(
            idx2.candidates(&spec, &getter(&neg))
                .into_iter()
                .collect::<Vec<_>>(),
            vec![2]
        );
    }

    #[test]
    fn geo_grid_same_cell_cross_cell_and_far_city() {
        let pred = Predicate::GeoRadius {
            field: "loc".into(),
            km: 400.0,
        };
        let spec = candidate_spec(&pred);
        assert!(matches!(
            spec,
            CandidateSpec::GeoGrid {
                field: "loc",
                km
            } if km == 400.0
        ));

        let paris = loc(48.8566, 2.3522);
        let london = loc(51.5074, -0.1278);
        let nearby = loc(48.9, 2.4); // same cell as Paris at km=400
        let ny = loc(40.7128, -74.0060);

        let mut idx = SideIndex::default();
        idx.insert(&spec, 1, &getter(&paris));
        idx.insert(&spec, 2, &getter(&london));
        idx.insert(&spec, 3, &getter(&nearby));
        idx.insert(&spec, 4, &getter(&ny));

        let from_paris = idx.candidates(&spec, &getter(&paris));
        assert!(from_paris.contains(&1), "same-cell self");
        assert!(from_paris.contains(&3), "same-cell neighbor");
        assert!(from_paris.contains(&2), "cross-cell Paris↔London ~343.5 km");
        assert!(!from_paris.contains(&4), "New York not in 400 km probe");
    }

    #[test]
    fn geo_grid_high_latitude_probe_is_superset() {
        let pred = Predicate::GeoRadius {
            field: "loc".into(),
            km: 340.0,
        };
        let spec = candidate_spec(&pred);
        let reyk = loc(64.1466, -21.9426);
        let lat = 64.0_f64;
        let dlon = 300.0 / (111.0 * lat.to_radians().cos());
        let east = loc(lat, -21.9426 + dlon);

        let mut idx = SideIndex::default();
        idx.insert(&spec, 1, &getter(&reyk));
        idx.insert(&spec, 2, &getter(&east));
        let hits = idx.candidates(&spec, &getter(&reyk));
        assert!(
            hits.contains(&2),
            "300 km east of Reykjavik must stay in the high-lat probe"
        );
    }

    #[test]
    fn geo_grid_antimeridian_wrap_and_evaluate_agree() {
        let pred = Predicate::GeoRadius {
            field: "loc".into(),
            km: 400.0,
        };
        let spec = candidate_spec(&pred);
        let east = loc(70.0, 179.9);
        let west = loc(70.0, -179.9);

        let mut idx = SideIndex::default();
        idx.insert(&spec, 1, &getter(&east));
        assert!(
            idx.candidates(&spec, &getter(&west)).contains(&1),
            "±180 pair at lat 70 must land in the wrapped probe"
        );

        let sp = |f: &str| east.get(f).cloned();
        let dp = |f: &str| west.get(f).cloned();
        let score = crate::def::evaluate(
            &pred,
            &crate::def::NodeView {
                key: "e",
                props: &sp,
            },
            &crate::def::NodeView {
                key: "w",
                props: &dp,
            },
        );
        assert!(
            score.is_some(),
            "haversine must match across the antimeridian"
        );

        // Wrap must not alias distant longitudes into the Paris probe.
        let paris = loc(48.8566, 2.3522);
        let ny = loc(40.7128, -74.0060);
        let mut idx2 = SideIndex::default();
        idx2.insert(&spec, 4, &getter(&ny));
        assert!(
            !idx2.candidates(&spec, &getter(&paris)).contains(&4),
            "New York still not in the Paris probe after wrap"
        );
    }

    #[test]
    fn scan_all_returns_vector_nodes_skips_malformed() {
        let pred = Predicate::VectorSimilar {
            field: "emb".into(),
            min: 0.5,
        };
        let spec = candidate_spec(&pred);
        assert!(matches!(spec, CandidateSpec::ScanAll { field: "emb" }));

        let mut idx = SideIndex::default();
        idx.insert(&spec, 1, &getter(&emb(&[1.0, 0.0])));
        idx.insert(&spec, 2, &getter(&emb(&[0.0, 1.0])));
        idx.insert(&spec, 3, &getter(&emb(&[1.0, 2.0, 3.0])));
        let empty: HashMap<_, _> = [("emb".to_string(), Value::List(vec![]))].into();
        let text: HashMap<_, _> =
            [("emb".to_string(), Value::List(vec![Value::Str("x".into())]))].into();
        let missing: HashMap<String, Value> = HashMap::new();
        idx.insert(&spec, 4, &getter(&empty));
        idx.insert(&spec, 5, &getter(&text));
        idx.insert(&spec, 6, &getter(&missing));

        let hits = idx.candidates(&spec, &getter(&emb(&[1.0, 0.0])));
        assert_eq!(hits.into_iter().collect::<Vec<_>>(), vec![1, 2, 3]);
        assert!(idx.candidates(&spec, &getter(&empty)).is_empty());
        assert!(idx.candidates(&spec, &getter(&text)).is_empty());
        assert!(idx.candidates(&spec, &getter(&missing)).is_empty());
    }

    #[test]
    fn legacy_specs_probe_keys_equal_index_keys() {
        let a: HashMap<_, _> = [
            ("ind".to_string(), Value::Str("arch".into())),
            (
                "tags".to_string(),
                Value::List(vec![Value::Str("x".into()), Value::Str("y".into())]),
            ),
            ("fk".to_string(), Value::Str("c1".into())),
        ]
        .into();
        let get = getter(&a);
        for pred in [
            Predicate::KeyMatch { field: "fk".into() },
            Predicate::FieldEqual {
                field: "ind".into(),
            },
            Predicate::Overlap {
                field: "tags".into(),
                min: 0.5,
            },
        ] {
            let spec = candidate_spec(&pred);
            assert_eq!(
                SideIndex::index_keys(&spec, &get),
                SideIndex::probe_keys(&spec, &get)
            );
        }
    }

    #[test]
    fn all_delegates_to_first_part_including_vector() {
        let all = Predicate::All(vec![
            Predicate::VectorSimilar {
                field: "emb".into(),
                min: 0.9,
            },
            Predicate::FieldEqual {
                field: "ind".into(),
            },
        ]);
        assert!(matches!(
            candidate_spec(&all),
            CandidateSpec::ScanAll { field: "emb" }
        ));
    }
}
