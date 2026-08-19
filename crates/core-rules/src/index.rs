use crate::def::Predicate;
use core_storage::{list_tokens, Value, ValueKey};
use std::collections::{BTreeMap, BTreeSet};

#[cfg(test)]
thread_local! {
    static VECTOR_DIM_REJECT: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
    static VECTOR_EARLY_EXIT: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
}

fn vector_dim_reject_enabled() -> bool {
    #[cfg(test)]
    {
        VECTOR_DIM_REJECT.with(|c| c.get())
    }
    #[cfg(not(test))]
    {
        true
    }
}

pub(crate) fn vector_early_exit_enabled() -> bool {
    #[cfg(test)]
    {
        VECTOR_EARLY_EXIT.with(|c| c.get())
    }
    #[cfg(not(test))]
    {
        true
    }
}

/// Force the ScanAll dim fast-reject on or off. Identity-proof hook.
#[cfg(test)]
pub fn with_vector_dim_reject<R>(enabled: bool, f: impl FnOnce() -> R) -> R {
    VECTOR_DIM_REJECT.with(|c| {
        let prev = c.replace(enabled);
        let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        c.set(prev);
        match out {
            Ok(v) => v,
            Err(p) => std::panic::resume_unwind(p),
        }
    })
}

/// Force the checkpointed Cauchy-Schwarz early-exit on or off. Identity-proof hook.
#[cfg(test)]
pub fn with_vector_early_exit<R>(enabled: bool, f: impl FnOnce() -> R) -> R {
    VECTOR_EARLY_EXIT.with(|c| {
        let prev = c.replace(enabled);
        let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        c.set(prev);
        match out {
            Ok(v) => v,
            Err(p) => std::panic::resume_unwind(p),
        }
    })
}

#[derive(Debug, Default)]
pub struct SideIndex {
    by_key: BTreeMap<ValueKey, BTreeSet<u32>>,
    /// Per-node `(dim, L2 norm)` for `ScanAll` members. Maintained by the
    /// same insert/remove choke-points as `by_key`. Cosine still reads live
    /// props; `dim` is a fast-reject; `norm` is the freshness gate for the
    /// checkpointed Cauchy-Schwarz early-exit (Plan 11 T3).
    vec_meta: BTreeMap<u32, (u32, f64)>,
    /// Per-node checkpointed suffix norms for the Cauchy-Schwarz early-exit.
    /// `ckpts[i]` = L2 norm of `xs[i * dim / 8 ..]`.
    /// `ckpts[0]` = full L2 norm; `ckpts[7]` = norm of the last eighth.
    /// Built at index-insert, torn out at index-remove — maintained in lockstep
    /// with `vec_meta` by the same choke-points.
    /// Memory: 8 × 8 = 64 bytes per indexed vector (6.4 MB at 100k vectors).
    vec_checkpoints: BTreeMap<u32, [f64; 8]>,
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

pub(crate) fn as_finite_f64(v: &Value) -> Option<f64> {
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

pub(crate) fn as_numeric_list(v: &Value) -> Option<Vec<f64>> {
    let Value::List(items) = v else {
        return None;
    };
    if items.is_empty() {
        return None;
    }
    items.iter().map(as_finite_f64).collect()
}

fn vec_dim_norm(v: &Value) -> Option<(u32, f64)> {
    let xs = as_numeric_list(v)?;
    let mut n2 = 0.0;
    for x in &xs {
        n2 += *x * *x;
    }
    Some((xs.len() as u32, n2.sqrt()))
}

/// Checkpointed suffix norms for Cauchy-Schwarz early exit.
///
/// `ckpts[i]` = L2 norm of `xs[boundary(i)..]` where `boundary(i) = i * dim / 8`.
/// `ckpts[0]` equals the full L2 norm; `ckpts[7]` is the last eighth's norm.
/// Multiple checkpoints may share the same boundary for dim < 8 (correct but no-op).
fn compute_ckpts(xs: &[f64]) -> [f64; 8] {
    let dim = xs.len();
    let mut ckpts = [0.0f64; 8];
    if dim == 0 {
        return ckpts;
    }
    // boundaries[i] = i * dim / 8 (integer division).
    let boundaries: [usize; 8] = std::array::from_fn(|i| i * dim / 8);
    let mut suffix_sq = 0.0f64;
    // Walk right-to-left; ci is the highest checkpoint not yet recorded.
    let mut ci = 7i32;
    for j in (0..dim).rev() {
        suffix_sq += xs[j] * xs[j];
        // Assign all checkpoints whose boundary equals j.
        while ci >= 0 && boundaries[ci as usize] == j {
            ckpts[ci as usize] = suffix_sq.sqrt();
            ci -= 1;
        }
    }
    ckpts
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
        if let CandidateSpec::ScanAll { field } = spec {
            if let Some(xs) = get(field).as_ref().and_then(as_numeric_list) {
                let mut n2 = 0.0f64;
                for x in &xs {
                    n2 += x * x;
                }
                let norm = n2.sqrt();
                self.vec_meta.insert(node, (xs.len() as u32, norm));
                self.vec_checkpoints.insert(node, compute_ckpts(&xs));
            }
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
        if let CandidateSpec::ScanAll { field } = spec {
            if get(field).as_ref().and_then(as_numeric_list).is_some() {
                self.vec_meta.remove(&node);
                self.vec_checkpoints.remove(&node);
            }
        }
    }

    /// Cached vector dimension for a `ScanAll` member, if present.
    pub fn vec_dim(&self, node: u32) -> Option<u32> {
        self.vec_meta.get(&node).map(|(d, _)| *d)
    }

    /// Cached `(dim, L2 norm)` for tests / debug.
    pub fn vec_meta(&self, node: u32) -> Option<(u32, f64)> {
        self.vec_meta.get(&node).copied()
    }

    /// Cached checkpoints for tests / debug.
    pub fn vec_ckpts(&self, node: u32) -> Option<&[f64; 8]> {
        self.vec_checkpoints.get(&node)
    }

    /// Returns `(cached_norm, &checkpoints)` if the cached `(dim, norm)`
    /// exactly matches `live`'s `(dim, norm)`.
    ///
    /// # Stale-cache gate (exactness invariant)
    ///
    /// The checkpoints are computed from the indexed vector at insert time.
    /// If the live prop differs from the indexed one, the checkpoints are
    /// stale and could produce a FALSE REJECT (under-approximation, forbidden).
    /// The exact-equality guard on `(dim, norm)` prevents this: if the live
    /// vector has changed, its norm will differ, and we return `None`,
    /// forcing a fall-through to the brute-force `evaluate()` path.
    ///
    /// In normal operation the index choke-points (insert/remove in
    /// `on_node_changed`) ensure the cache is always coherent with live props
    /// by evaluation time.  The guard is belt-and-suspenders.
    pub(crate) fn fresh_ckpts_for<'a>(
        &'a self,
        node: u32,
        live: &[f64],
    ) -> Option<(f64, &'a [f64; 8])> {
        let &(dim, norm) = self.vec_meta.get(&node)?;
        if dim != live.len() as u32 {
            return None;
        }
        // Compute the live norm with the same sequential accumulation used at
        // insert time so the bits are identical when the vector is unchanged.
        let live_norm = {
            let mut n2 = 0.0f64;
            for x in live {
                n2 += x * x;
            }
            n2.sqrt()
        };
        if norm != live_norm {
            return None; // stale — fall back to brute-force evaluate()
        }
        let ckpts = self.vec_checkpoints.get(&node)?;
        Some((norm, ckpts))
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
        // Exact: VectorSimilar evaluate is None when dims differ.
        if vector_dim_reject_enabled() {
            if let CandidateSpec::ScanAll { field } = spec {
                if let Some((dim, _)) = get(field).as_ref().and_then(vec_dim_norm) {
                    out.retain(|id| self.vec_meta.get(id).is_none_or(|(d, _)| *d == dim));
                }
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
        assert_eq!(
            hits.into_iter().collect::<Vec<_>>(),
            vec![1, 2],
            "dim-2 probe must drop the dim-3 member"
        );
        assert_eq!(
            idx.candidates(&spec, &getter(&emb(&[1.0, 2.0, 3.0])))
                .into_iter()
                .collect::<Vec<_>>(),
            vec![3]
        );
        with_vector_dim_reject(false, || {
            assert_eq!(
                idx.candidates(&spec, &getter(&emb(&[1.0, 0.0])))
                    .into_iter()
                    .collect::<Vec<_>>(),
                vec![1, 2, 3],
                "unfiltered ScanAll still returns every vector node"
            );
        });
        assert_eq!(idx.vec_dim(1), Some(2));
        assert_eq!(idx.vec_dim(3), Some(3));
        assert!(idx.vec_meta(1).is_some());
        assert!(idx.vec_dim(4).is_none());
        assert!(idx.candidates(&spec, &getter(&empty)).is_empty());
        assert!(idx.candidates(&spec, &getter(&text)).is_empty());
        assert!(idx.candidates(&spec, &getter(&missing)).is_empty());
        idx.remove(&spec, 1, &getter(&emb(&[1.0, 0.0])));
        assert!(idx.vec_dim(1).is_none());
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

    /// Checkpoints are populated at insert, torn out at remove,
    /// and ckpts[0] must equal the full L2 norm.
    #[test]
    fn checkpoint_populated_and_consistent_with_norm() {
        let pred = Predicate::VectorSimilar {
            field: "emb".into(),
            min: 0.8,
        };
        let spec = candidate_spec(&pred);
        let xs = [3.0f64, 4.0]; // norm = 5.0
        let mut idx = SideIndex::default();
        idx.insert(&spec, 1, &getter(&emb(&xs)));

        let ckpts = idx.vec_ckpts(1).expect("checkpoints must exist after insert");
        let (_, norm) = idx.vec_meta(1).unwrap();
        assert!(
            (ckpts[0] - norm).abs() < 1e-12,
            "ckpts[0] must equal the full L2 norm; got {} vs {}",
            ckpts[0],
            norm
        );
        assert!(
            (norm - 5.0).abs() < 1e-12,
            "norm of [3,4] must be 5.0, got {norm}"
        );

        // Remove must tear out checkpoints.
        idx.remove(&spec, 1, &getter(&emb(&xs)));
        assert!(
            idx.vec_ckpts(1).is_none(),
            "checkpoints must be removed after remove()"
        );
    }

    /// fresh_ckpts_for returns None when the live vector's norm differs
    /// (freshness gate) and Some when it matches.
    #[test]
    fn fresh_ckpts_for_freshness_gate() {
        let pred = Predicate::VectorSimilar {
            field: "emb".into(),
            min: 0.8,
        };
        let spec = candidate_spec(&pred);
        let xs = [1.0f64, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let mut idx = SideIndex::default();
        idx.insert(&spec, 7, &getter(&emb(&xs)));

        // Correct live vector → gate passes.
        let result = idx.fresh_ckpts_for(7, &xs);
        assert!(result.is_some(), "fresh_ckpts_for must succeed with matching live vector");
        let (norm, ckpts) = result.unwrap();
        assert!((norm - 1.0).abs() < 1e-12);
        assert!((ckpts[0] - 1.0).abs() < 1e-12);

        // Wrong norm → gate rejects.
        let wrong = [2.0f64, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]; // norm = 2.0
        assert!(
            idx.fresh_ckpts_for(7, &wrong).is_none(),
            "freshness gate must reject mismatched norm"
        );

        // Wrong dim → gate rejects.
        let short = [1.0f64, 0.0];
        assert!(
            idx.fresh_ckpts_for(7, &short).is_none(),
            "freshness gate must reject mismatched dim"
        );

        // Missing node → returns None.
        assert!(idx.fresh_ckpts_for(99, &xs).is_none());
    }

    /// Checkpoints for a dim-16 vector: ckpts[i] must be non-increasing
    /// (suffix norms decrease as the suffix shrinks).
    #[test]
    fn checkpoint_suffix_norms_non_increasing() {
        let pred = Predicate::VectorSimilar {
            field: "emb".into(),
            min: 0.5,
        };
        let spec = candidate_spec(&pred);
        let xs: Vec<f64> = (1..=16).map(|i| i as f64).collect();
        let mut idx = SideIndex::default();
        idx.insert(&spec, 42, &getter(&emb(&xs)));

        let ckpts = *idx.vec_ckpts(42).unwrap();
        for c in 0..7 {
            assert!(
                ckpts[c] >= ckpts[c + 1] - 1e-12,
                "suffix norm must be non-increasing: ckpts[{c}]={} < ckpts[{}]={}",
                ckpts[c],
                c + 1,
                ckpts[c + 1]
            );
        }
        // ckpts[7] = suffix norm of the last 2 elements (14..=16).
        let expected_last = ((15.0f64 * 15.0 + 16.0 * 16.0).sqrt());
        assert!(
            (ckpts[7] - expected_last).abs() < 1e-9,
            "ckpts[7] should be norm of last segment; got {} vs {}",
            ckpts[7],
            expected_last
        );
    }
}
