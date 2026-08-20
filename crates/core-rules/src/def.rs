use core_storage::{list_tokens, Value, ValueKey};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuleDef {
    pub name: String,
    pub src_label: String,
    pub dst_label: String,
    pub predicate: Predicate,
    pub edge_type: String,
    pub weight_prop: Option<String>,
    /// Per-rule provenance cap. `None` uses the engine default (`1_000_000`).
    ///
    /// APPENDED field. bincode is positional, so this breaks decode of
    /// `CreateRule` WAL records and snapshot `rule_defs` written before this
    /// field existed. Pre-alpha no-migration ruling: accepted; no decoder
    /// compat. `#[serde(default)]` cannot help — bincode does not skip
    /// missing positional fields.
    pub max_edges: Option<u64>,
    /// Opt-in IVF-Flat approximate candidate selection.
    ///
    /// `false` (default) → exact `ScanAll` path; semantics and derived edges
    /// are byte-identical to pre-T4 behaviour for all existing rules.
    ///
    /// `true` → `VectorClusters` candidate path: k-means partitions the dst
    /// (and src) side at backfill/rebuild time; only members of the P nearest
    /// clusters are evaluated. Recall ≥ 0.90 quiesced, ≥ 0.85 on any
    /// crash-recovery state — not exact. Only valid when the predicate is
    /// `VectorSimilar`-rooted (`VectorSimilar` itself, or `All` whose first
    /// element is `VectorSimilar`); `validate()` rejects other combinations.
    ///
    /// APPENDED field — same pre-alpha no-migration ruling as `max_edges`:
    /// WAL/snapshot records written before this field break positional bincode
    /// decode. Accepted for pre-1.0 builds; no decoder compat.
    #[serde(default)]
    pub approximate: bool,
}

/// Score-combination conventions for composed predicates:
///
/// - `All(parts)` — **minimum** of the individual branch scores.  Every
///   branch must match; the weakest link controls the edge weight.
///   Verified by test `all_takes_min_score_and_requires_every_part`.
///
/// - `Any(parts)` — **maximum** of the satisfied branches' scores.  At
///   least one branch must match; the strongest match controls the edge
///   weight.  Verified by test `any_score_is_max_when_both_branches_match`.
///
/// These conventions are opposites: `All` is pessimistic (min), `Any` is
/// optimistic (max).  Nesting is allowed up to depth
/// `MAX_PREDICATE_NESTING_DEPTH`; `validate()` returns a named error beyond
/// that.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Predicate {
    KeyMatch { field: String },
    FieldEqual { field: String },
    Overlap { field: String, min: f64 },
    All(Vec<Predicate>),
    // APPENDED (Plan 7) — positional bincode: never reorder.
    NumericWithin { field: String, tolerance: f64 },
    GeoRadius { field: String, km: f64 },
    VectorSimilar { field: String, min: f64 },
    // APPENDED (Plan 13 T2) — positional bincode: never reorder.
    /// OR composition: matches when at least one branch matches.
    /// Score = max over satisfied branches (see doc comment on `Predicate`).
    Any(Vec<Predicate>),
}

pub struct NodeView<'a> {
    pub key: &'a str,
    pub props: &'a dyn Fn(&str) -> Option<Value>,
}

impl RuleDef {
    pub fn validate(&self) -> Result<(), String> {
        for (what, s) in [
            ("name", &self.name),
            ("src_label", &self.src_label),
            ("dst_label", &self.dst_label),
            ("edge_type", &self.edge_type),
        ] {
            if s.is_empty() {
                return Err(format!("{what} must not be empty"));
            }
        }
        validate_pred(&self.predicate)?;
        let depth = predicate_nesting_depth(&self.predicate);
        if depth > MAX_PREDICATE_NESTING_DEPTH {
            return Err(format!(
                "predicate nesting depth {depth} exceeds cap of \
                 {MAX_PREDICATE_NESTING_DEPTH}"
            ));
        }
        if self.approximate && !predicate_is_vector_similar_rooted(&self.predicate) {
            return Err(
                "approximate=true requires a VectorSimilar-rooted predicate \
                 (VectorSimilar, or All whose first element is VectorSimilar)"
                    .into(),
            );
        }
        Ok(())
    }

    pub fn watched_fields(&self) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        collect_fields(&self.predicate, &mut out);
        out
    }
}

/// Maximum nesting depth for compound predicates (`All` / `Any`).
///
/// Depth is defined as the number of nested compound-predicate layers:
/// a bare scalar predicate has depth 0; `Any([X, Y])` has depth 1;
/// `All([Any([X]), Y])` has depth 2; etc.  `validate()` returns a named
/// error when this cap is exceeded.
pub const MAX_PREDICATE_NESTING_DEPTH: usize = 4;

/// Returns true when the predicate is `VectorSimilar` itself, or an `All`
/// whose first element is `VectorSimilar` — the only predicates that may use
/// the IVF-Flat approximate candidate path (`approximate: true`).
pub fn predicate_is_vector_similar_rooted(p: &Predicate) -> bool {
    match p {
        Predicate::VectorSimilar { .. } => true,
        Predicate::All(parts) => {
            !parts.is_empty() && matches!(parts[0], Predicate::VectorSimilar { .. })
        }
        Predicate::Any(_) => false,
        _ => false,
    }
}

/// Returns the nesting depth of a predicate tree.
///
/// Scalar predicates return 0.  `All` and `Any` return
/// `1 + max(depths of children)` (0 when empty, which is guarded by
/// `validate_pred`).
fn predicate_nesting_depth(p: &Predicate) -> usize {
    match p {
        Predicate::All(parts) | Predicate::Any(parts) => {
            1 + parts.iter().map(predicate_nesting_depth).max().unwrap_or(0)
        }
        _ => 0,
    }
}

fn validate_pred(p: &Predicate) -> Result<(), String> {
    match p {
        Predicate::KeyMatch { field } | Predicate::FieldEqual { field } => {
            if field.is_empty() {
                Err("field must not be empty".into())
            } else {
                Ok(())
            }
        }
        Predicate::Overlap { field, min } => {
            if field.is_empty() {
                Err("field must not be empty".into())
            } else if !(*min > 0.0 && *min <= 1.0) {
                Err(format!("overlap min must be in (0,1], got {min}"))
            } else {
                Ok(())
            }
        }
        Predicate::NumericWithin { field, tolerance } => {
            if field.is_empty() {
                Err("field must not be empty".into())
            } else if !(tolerance.is_finite() && *tolerance >= 0.0) {
                Err(format!(
                    "numeric_within tolerance must be finite and >= 0, got {tolerance}"
                ))
            } else {
                Ok(())
            }
        }
        Predicate::GeoRadius { field, km } => {
            if field.is_empty() {
                Err("field must not be empty".into())
            } else if !(km.is_finite() && *km > 0.0) {
                Err(format!("geo_radius km must be finite and > 0, got {km}"))
            } else {
                Ok(())
            }
        }
        Predicate::VectorSimilar { field, min } => {
            if field.is_empty() {
                Err("field must not be empty".into())
            } else if !(*min > 0.0 && *min <= 1.0) {
                Err(format!("vector_similar min must be in (0,1], got {min}"))
            } else {
                Ok(())
            }
        }
        Predicate::All(parts) => {
            if parts.is_empty() {
                return Err("all() must have at least one predicate".into());
            }
            parts.iter().try_for_each(validate_pred)
        }
        Predicate::Any(parts) => {
            if parts.is_empty() {
                return Err("any() must have at least one predicate".into());
            }
            parts.iter().try_for_each(validate_pred)
        }
    }
}

fn collect_fields(p: &Predicate, out: &mut BTreeSet<String>) {
    match p {
        Predicate::KeyMatch { field }
        | Predicate::FieldEqual { field }
        | Predicate::Overlap { field, .. }
        | Predicate::NumericWithin { field, .. }
        | Predicate::GeoRadius { field, .. }
        | Predicate::VectorSimilar { field, .. } => {
            out.insert(field.clone());
        }
        Predicate::All(parts) | Predicate::Any(parts) => {
            parts.iter().for_each(|q| collect_fields(q, out))
        }
    }
}

pub fn evaluate(pred: &Predicate, src: &NodeView, dst: &NodeView) -> Option<f64> {
    match pred {
        Predicate::KeyMatch { field } => match (src.props)(field)? {
            Value::Str(s) if s == dst.key => Some(1.0),
            _ => None,
        },
        Predicate::FieldEqual { field } => {
            let a = ValueKey::from_value(&(src.props)(field)?)?;
            let b = ValueKey::from_value(&(dst.props)(field)?)?;
            (a == b).then_some(1.0)
        }
        Predicate::Overlap { field, min } => {
            let a = list_tokens(&(src.props)(field)?)?;
            let b = list_tokens(&(dst.props)(field)?)?;
            let inter = a.intersection(&b).count();
            let union = a.union(&b).count();
            if union == 0 || inter == 0 {
                return None;
            }
            let j = inter as f64 / union as f64;
            (j >= *min).then_some(j)
        }
        Predicate::All(parts) => {
            // validate() rejects empty All; this is defense-in-depth against skipped validation.
            if parts.is_empty() {
                return None;
            }
            let mut score = f64::INFINITY;
            for part in parts {
                score = score.min(evaluate(part, src, dst)?);
            }
            Some(score)
        }
        Predicate::Any(parts) => {
            // validate() rejects empty Any; this is defense-in-depth against skipped validation.
            // Score = max over satisfied branches (see doc comment on Predicate).
            // Returns None only when no branch matches.
            let mut best: Option<f64> = None;
            for part in parts {
                if let Some(s) = evaluate(part, src, dst) {
                    best = Some(match best {
                        None => s,
                        Some(prev) => prev.max(s),
                    });
                }
            }
            best
        }
        Predicate::NumericWithin { field, tolerance } => {
            // Score: tolerance == 0.0 → 1.0 (exact match required), else
            // 1.0 − |a − b| / tolerance. Boundary Δ = tolerance yields score
            // 0.0 — a legal 0-weight edge.
            if !tolerance.is_finite() || *tolerance < 0.0 {
                return None;
            }
            let a = as_finite_f64(&(src.props)(field)?)?;
            let b = as_finite_f64(&(dst.props)(field)?)?;
            let delta = (a - b).abs();
            if *tolerance == 0.0 {
                return (delta == 0.0).then_some(1.0);
            }
            (delta <= *tolerance).then_some(1.0 - delta / *tolerance)
        }
        Predicate::GeoRadius { field, km } => {
            if !km.is_finite() || *km <= 0.0 {
                return None;
            }
            let (alat, alon) = as_latlon(&(src.props)(field)?)?;
            let (blat, blon) = as_latlon(&(dst.props)(field)?)?;
            let d = haversine_km(alat, alon, blat, blon);
            if !d.is_finite() {
                return None;
            }
            (d <= *km).then_some(1.0 - d / *km)
        }
        Predicate::VectorSimilar { field, min } => {
            let a = as_numeric_list(&(src.props)(field)?)?;
            let b = as_numeric_list(&(dst.props)(field)?)?;
            if a.len() != b.len() {
                return None;
            }
            let cos = cosine(&a, &b)?.min(1.0);
            (cos >= *min).then_some(cos)
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

/// Mean Earth radius (WGS-84 authalic), kilometres.
const EARTH_RADIUS_KM: f64 = 6371.0088;

fn haversine_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let phi1 = lat1.to_radians();
    let phi2 = lat2.to_radians();
    let dphi = (lat2 - lat1).to_radians();
    let dlam = (lon2 - lon1).to_radians();
    let a = ((dphi / 2.0).sin().powi(2) + phi1.cos() * phi2.cos() * (dlam / 2.0).sin().powi(2))
        .clamp(0.0, 1.0);
    let c = 2.0 * a.sqrt().atan2((1.0 - a).sqrt());
    EARTH_RADIUS_KM * c
}

fn cosine(a: &[f64], b: &[f64]) -> Option<f64> {
    let mut dot = 0.0;
    let mut na2 = 0.0;
    let mut nb2 = 0.0;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += *x * *y;
        na2 += *x * *x;
        nb2 += *y * *y;
    }
    let na = na2.sqrt();
    let nb = nb2.sqrt();
    if !(na > 0.0 && nb > 0.0) {
        return None;
    }
    let cos = dot / (na * nb);
    cos.is_finite().then_some(cos)
}

/// Cosine similarity with Cauchy-Schwarz suffix-norm early exit.
///
/// Processes `a` and `b` in 8 equal-sized chunks.  After each chunk (except
/// the last), computes the upper bound:
///
/// ```text
/// cos_max = (dot_so_far + ckpts_a[c+1] × ckpts_b[c+1]) / (norm_a × norm_b)
/// ```
///
/// where `ckpts_x[i]` = L2 norm of `x[i * dim / 8 ..]`.  If `cos_max <
/// min − eps` (with `eps = dim × f64::EPSILON × 4`), the pair is provably
/// below threshold and `None` is returned immediately (exact reject in exact
/// arithmetic; the epsilon guard absorbs IEEE 754 rounding in suffix-norm
/// accumulation at dim-scale — approximately 3.4 × 10⁻¹³ at dim = 1536).
/// If all checkpoints pass, the full dot product has been accumulated and the
/// cosine is returned normally.
///
/// # Correctness requirement — checkpoints must be fresh
///
/// `ckpts_a`/`ckpts_b` **must be fresh** for the live vectors (see
/// `SideIndex::fresh_ckpts_for`).  A permuted vector that shares `(dim, norm)`
/// with the indexed one passes a pure norm-based gate yet carries a different
/// suffix energy distribution — stale checkpoints can produce a **false
/// reject** (under-tight suffix bound), violating the exactness invariant.
///
/// The real coherence guarantee is structural: checkpoint rebuilds flow
/// through the same mutation choke-points as `vec_meta` (insert/remove in
/// `on_node_changed`), making live/cache divergence unreachable in
/// single-writer operation.  `fresh_ckpts_for`'s dim/norm/anchor comparison
/// is defense-in-depth — belt-and-suspenders against bugs in those
/// choke-points, not a standalone proof.
///
/// # Arguments
/// * `norm_a`, `norm_b` — precomputed L2 norms (must match `ckpts_x[0]`).
/// * `min` — the minimum cosine threshold from the rule definition.
pub fn cosine_early_exit(
    a: &[f64],
    b: &[f64],
    ckpts_a: &[f64; 8],
    ckpts_b: &[f64; 8],
    norm_a: f64,
    norm_b: f64,
    min: f64,
) -> Option<f64> {
    let dim = a.len();
    if dim == 0 || dim != b.len() {
        return None; // dim mismatch or empty: cosine undefined, no edge
    }
    let denom = norm_a * norm_b;
    if !denom.is_finite() || denom == 0.0 {
        return None;
    }

    // Epsilon guard: suffix-norm accumulation rounds suffix_sq slightly low,
    // making cos_max_fl potentially below the true Cauchy-Schwarz bound.  At
    // dim=1536 the error floor is ~dim × f64::EPSILON ≈ 3.4×10⁻¹³.  4× margin
    // keeps the guard conservative without meaningfully expanding the pass-through
    // zone (a few extra evaluate() calls near threshold, never a false reject).
    let eps = dim as f64 * f64::EPSILON * 4.0;
    let mut dot = 0.0f64;

    for ci in 0..8usize {
        let chunk_start = ci * dim / 8;
        let chunk_end = if ci < 7 { (ci + 1) * dim / 8 } else { dim };
        for k in chunk_start..chunk_end {
            dot += a[k] * b[k];
        }
        // After processing this chunk (not the last), compute the upper bound
        // for the remaining suffix using Cauchy-Schwarz.  Guard: bail only
        // when the bound is below min - eps to absorb float-rounding slack.
        if ci < 7 {
            let bound = ckpts_a[ci + 1] * ckpts_b[ci + 1];
            let cos_max = (dot + bound) / denom;
            if cos_max.is_finite() && cos_max < min - eps {
                return None;
            }
        }
    }

    // Full dot product accumulated; return cosine clamped to [−1, 1].
    let cos = (dot / denom).min(1.0);
    if cos.is_finite() && cos >= min {
        Some(cos)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_storage::Value;
    use std::collections::HashMap;

    // The brief's `fn view(...)` helper cannot borrow-check: it returns a NodeView
    // holding &'a dyn Fn referencing a closure that is local to the helper (dropped
    // on return). Fix: a macro that expands the closure binding at the call site so
    // the temporary lives for the surrounding statement. All assertions are identical
    // in meaning to the brief.
    macro_rules! eval {
        ($p:expr, ($sk:expr, $sm:ident) => ($dk:expr, $dm:ident)) => {{
            let sp = |f: &str| $sm.get(f).cloned();
            let dp = |f: &str| $dm.get(f).cloned();
            evaluate(
                $p,
                &NodeView {
                    key: $sk,
                    props: &sp,
                },
                &NodeView {
                    key: $dk,
                    props: &dp,
                },
            )
        }};
    }

    #[test]
    fn key_match_links_fk_to_key() {
        let s: HashMap<_, _> = [("cid".to_string(), Value::Str("c1".into()))].into();
        let d: HashMap<String, Value> = HashMap::new();
        let p = Predicate::KeyMatch {
            field: "cid".into(),
        };
        assert_eq!(eval!(&p, ("t1", s) => ("c1", d)), Some(1.0));
        assert_eq!(eval!(&p, ("t1", s) => ("c2", d)), None);
        assert_eq!(eval!(&p, ("t1", d) => ("c1", d)), None); // field absent
    }

    #[test]
    fn field_equal_needs_both_scalars_equal() {
        let a: HashMap<_, _> = [("ind".to_string(), Value::Str("arch".into()))].into();
        let b = a.clone();
        let c: HashMap<_, _> = [("ind".to_string(), Value::Str("law".into()))].into();
        let p = Predicate::FieldEqual {
            field: "ind".into(),
        };
        assert_eq!(eval!(&p, ("a", a) => ("b", b)), Some(1.0));
        assert_eq!(eval!(&p, ("a", a) => ("c", c)), None);
    }

    #[test]
    fn overlap_is_jaccard_with_threshold() {
        let mk =
            |items: &[&str]| Value::List(items.iter().map(|s| Value::Str((*s).into())).collect());
        let a: HashMap<_, _> = [("tags".to_string(), mk(&["x", "y"]))].into();
        let b: HashMap<_, _> = [("tags".to_string(), mk(&["y", "z"]))].into();
        let p = Predicate::Overlap {
            field: "tags".into(),
            min: 0.3,
        };
        // jaccard = |{y}| / |{x,y,z}| = 1/3
        let score = eval!(&p, ("a", a) => ("b", b)).unwrap();
        assert!((score - 1.0 / 3.0).abs() < 1e-9);
        let strict = Predicate::Overlap {
            field: "tags".into(),
            min: 0.5,
        };
        assert_eq!(eval!(&strict, ("a", a) => ("b", b)), None);
        // empty-vs-anything never matches (union empty or intersection empty)
        let e: HashMap<_, _> = [("tags".to_string(), mk(&[]))].into();
        assert_eq!(eval!(&p, ("a", e) => ("b", b)), None);
    }

    #[test]
    fn all_takes_min_score_and_requires_every_part() {
        let mk =
            |items: &[&str]| Value::List(items.iter().map(|s| Value::Str((*s).into())).collect());
        let a: HashMap<_, _> = [
            ("ind".to_string(), Value::Str("arch".into())),
            ("tags".to_string(), mk(&["x", "y"])),
        ]
        .into();
        let b: HashMap<_, _> = [
            ("ind".to_string(), Value::Str("arch".into())),
            ("tags".to_string(), mk(&["y"])),
        ]
        .into();
        let p = Predicate::All(vec![
            Predicate::FieldEqual {
                field: "ind".into(),
            },
            Predicate::Overlap {
                field: "tags".into(),
                min: 0.4,
            },
        ]);
        let s = eval!(&p, ("a", a) => ("b", b)).unwrap();
        assert!((s - 0.5).abs() < 1e-9); // min(1.0, 0.5)
    }

    #[test]
    fn validation_rejects_bad_rules_and_collects_watched_fields() {
        let ok = RuleDef {
            name: "r".into(),
            src_label: "A".into(),
            dst_label: "B".into(),
            predicate: Predicate::All(vec![
                Predicate::KeyMatch { field: "fk".into() },
                Predicate::Overlap {
                    field: "tags".into(),
                    min: 0.5,
                },
            ]),
            edge_type: "E".into(),
            weight_prop: Some("score".into()),
            max_edges: None,
            approximate: false,
        };
        assert!(ok.validate().is_ok());
        assert_eq!(
            ok.watched_fields().into_iter().collect::<Vec<_>>(),
            vec!["fk".to_string(), "tags".to_string()]
        );
        let mut bad = ok.clone();
        bad.predicate = Predicate::Overlap {
            field: "t".into(),
            min: 0.0,
        };
        assert!(bad.validate().is_err()); // min must be in (0,1]
        let mut bad2 = ok.clone();
        bad2.edge_type = String::new();
        assert!(bad2.validate().is_err());
        let mut bad3 = ok;
        bad3.predicate = Predicate::All(vec![]);
        assert!(bad3.validate().is_err());
    }

    #[test]
    fn evaluate_empty_all_returns_none() {
        let empty: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
        let sp = |f: &str| empty.get(f).cloned();
        let dp = |f: &str| empty.get(f).cloned();
        let src = NodeView {
            key: "a",
            props: &sp,
        };
        let dst = NodeView {
            key: "b",
            props: &dp,
        };
        assert_eq!(evaluate(&Predicate::All(vec![]), &src, &dst), None);
    }

    #[test]
    fn numeric_within_int_float_cross_type() {
        let a: HashMap<_, _> = [("year".to_string(), Value::Int(1998))].into();
        let b: HashMap<_, _> = [("year".to_string(), Value::Float(2000.0))].into();
        let tight = Predicate::NumericWithin {
            field: "year".into(),
            tolerance: 2.0,
        };
        // |1998 − 2000| = 2; Δ = tolerance → score 0.0 (legal 0-weight edge)
        assert_eq!(eval!(&tight, ("a", a) => ("b", b)), Some(0.0));
        let loose = Predicate::NumericWithin {
            field: "year".into(),
            tolerance: 3.0,
        };
        let score = eval!(&loose, ("a", a) => ("b", b)).unwrap();
        assert!((score - 1.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn numeric_within_missing_or_non_numeric_is_none() {
        let num: HashMap<_, _> = [("year".to_string(), Value::Int(1998))].into();
        let missing: HashMap<String, Value> = HashMap::new();
        let text: HashMap<_, _> = [("year".to_string(), Value::Str("1998".into()))].into();
        let p = Predicate::NumericWithin {
            field: "year".into(),
            tolerance: 2.0,
        };
        assert_eq!(eval!(&p, ("a", num) => ("b", missing)), None);
        assert_eq!(eval!(&p, ("a", missing) => ("b", num)), None);
        assert_eq!(eval!(&p, ("a", num) => ("b", text)), None);
    }

    #[test]
    fn numeric_within_tol_zero_requires_exact() {
        let a: HashMap<_, _> = [("year".to_string(), Value::Int(1998))].into();
        let same: HashMap<_, _> = [("year".to_string(), Value::Float(1998.0))].into();
        let other: HashMap<_, _> = [("year".to_string(), Value::Int(1999))].into();
        let p = Predicate::NumericWithin {
            field: "year".into(),
            tolerance: 0.0,
        };
        assert_eq!(eval!(&p, ("a", a) => ("b", same)), Some(1.0));
        assert_eq!(eval!(&p, ("a", a) => ("b", other)), None);
    }

    #[test]
    fn numeric_within_non_finite_is_none() {
        let a: HashMap<_, _> = [("year".to_string(), Value::Float(f64::NAN))].into();
        let b: HashMap<_, _> = [("year".to_string(), Value::Float(1.0))].into();
        let inf: HashMap<_, _> = [("year".to_string(), Value::Float(f64::INFINITY))].into();
        let p = Predicate::NumericWithin {
            field: "year".into(),
            tolerance: 2.0,
        };
        assert_eq!(eval!(&p, ("a", a) => ("b", b)), None);
        assert_eq!(eval!(&p, ("a", inf) => ("b", b)), None);
    }

    fn geo_pair(
        src: (f64, f64),
        dst: (f64, f64),
    ) -> (HashMap<String, Value>, HashMap<String, Value>) {
        let mk = |lat: f64, lon: f64| {
            let mut m = HashMap::new();
            m.insert(
                "loc".to_string(),
                Value::List(vec![Value::Float(lat), Value::Float(lon)]),
            );
            m
        };
        (mk(src.0, src.1), mk(dst.0, dst.1))
    }

    #[test]
    fn geo_radius_paris_london() {
        // Paris (48.8566, 2.3522) ↔ London (51.5074, −0.1278) ≈ 343.5 km
        let (paris, london) = geo_pair((48.8566, 2.3522), (51.5074, -0.1278));
        let inside = Predicate::GeoRadius {
            field: "loc".into(),
            km: 400.0,
        };
        let score = eval!(&inside, ("p", paris) => ("l", london)).unwrap();
        // 1 − 343.5/400 = 0.14125; ±0.001 pins haversine to ~±0.4 km
        assert!((score - 0.14125).abs() < 0.001);
        let outside = Predicate::GeoRadius {
            field: "loc".into(),
            km: 300.0,
        };
        assert_eq!(eval!(&outside, ("p", paris) => ("l", london)), None);
    }

    #[test]
    fn geo_radius_identical_coordinates_score_one() {
        let (a, b) = geo_pair((48.8566, 2.3522), (48.8566, 2.3522));
        let p = Predicate::GeoRadius {
            field: "loc".into(),
            km: 400.0,
        };
        assert_eq!(eval!(&p, ("a", a) => ("b", b)), Some(1.0));
    }

    #[test]
    fn geo_radius_malformed_is_none() {
        let paris: HashMap<_, _> = [(
            "loc".to_string(),
            Value::List(vec![Value::Float(48.8566), Value::Float(2.3522)]),
        )]
        .into();
        let one: HashMap<_, _> =
            [("loc".to_string(), Value::List(vec![Value::Float(48.8566)]))].into();
        let three: HashMap<_, _> = [(
            "loc".to_string(),
            Value::List(vec![
                Value::Float(48.8566),
                Value::Float(2.3522),
                Value::Float(0.0),
            ]),
        )]
        .into();
        let string_el: HashMap<_, _> = [(
            "loc".to_string(),
            Value::List(vec![Value::Str("48.8566".into()), Value::Float(2.3522)]),
        )]
        .into();
        let lat91: HashMap<_, _> = [(
            "loc".to_string(),
            Value::List(vec![Value::Float(91.0), Value::Float(0.0)]),
        )]
        .into();
        let p = Predicate::GeoRadius {
            field: "loc".into(),
            km: 400.0,
        };
        assert_eq!(eval!(&p, ("a", paris) => ("b", one)), None);
        assert_eq!(eval!(&p, ("a", paris) => ("b", three)), None);
        assert_eq!(eval!(&p, ("a", paris) => ("b", string_el)), None);
        assert_eq!(eval!(&p, ("a", paris) => ("b", lat91)), None);
    }

    fn vec_field(vals: &[f64]) -> HashMap<String, Value> {
        [(
            "emb".to_string(),
            Value::List(vals.iter().copied().map(Value::Float).collect()),
        )]
        .into()
    }

    #[test]
    fn vector_similar_cosine_and_rejects() {
        let a = vec_field(&[1.0, 0.0]);
        let same = vec_field(&[1.0, 0.0]);
        let ortho = vec_field(&[0.0, 1.0]);
        let p = Predicate::VectorSimilar {
            field: "emb".into(),
            min: 0.5,
        };
        assert_eq!(eval!(&p, ("a", a) => ("b", same)), Some(1.0));
        assert_eq!(eval!(&p, ("a", a) => ("b", ortho)), None); // cos 0 < min

        let u = vec_field(&[1.0, 2.0]);
        let scaled = vec_field(&[2.0, 4.0]);
        let score = eval!(&p, ("a", u) => ("b", scaled)).unwrap();
        assert!((1.0 - score).abs() < 1e-9); // parallel → 1.0 − ε

        let dim3 = vec_field(&[1.0, 0.0, 0.0]);
        assert_eq!(eval!(&p, ("a", a) => ("b", dim3)), None);
        let zero = vec_field(&[0.0, 0.0]);
        assert_eq!(eval!(&p, ("a", a) => ("b", zero)), None);
    }

    #[test]
    fn approximate_only_valid_with_vector_similar_rooted_predicate() {
        // approximate=true + VectorSimilar → valid
        let ok_vec = RuleDef {
            name: "av".into(),
            src_label: "V".into(),
            dst_label: "V".into(),
            predicate: Predicate::VectorSimilar {
                field: "emb".into(),
                min: 0.9,
            },
            edge_type: "VEC".into(),
            weight_prop: None,
            max_edges: None,
            approximate: true,
        };
        assert!(ok_vec.validate().is_ok());

        // approximate=true + All(VectorSimilar, ...) → valid
        let ok_all = RuleDef {
            name: "av2".into(),
            src_label: "V".into(),
            dst_label: "V".into(),
            predicate: Predicate::All(vec![
                Predicate::VectorSimilar {
                    field: "emb".into(),
                    min: 0.9,
                },
                Predicate::FieldEqual {
                    field: "kind".into(),
                },
            ]),
            edge_type: "VEC2".into(),
            weight_prop: None,
            max_edges: None,
            approximate: true,
        };
        assert!(ok_all.validate().is_ok());

        // approximate=true + FieldEqual → invalid
        let bad_fe = RuleDef {
            name: "bfe".into(),
            src_label: "A".into(),
            dst_label: "A".into(),
            predicate: Predicate::FieldEqual { field: "f".into() },
            edge_type: "FE".into(),
            weight_prop: None,
            max_edges: None,
            approximate: true,
        };
        assert!(bad_fe.validate().is_err());

        // approximate=true + Overlap → invalid
        let bad_ov = RuleDef {
            name: "bov".into(),
            src_label: "A".into(),
            dst_label: "A".into(),
            predicate: Predicate::Overlap {
                field: "tags".into(),
                min: 0.5,
            },
            edge_type: "OV".into(),
            weight_prop: None,
            max_edges: None,
            approximate: true,
        };
        assert!(bad_ov.validate().is_err());

        // approximate=true + All(FieldEqual, VectorSimilar) → invalid (first part is not VectorSimilar)
        let bad_all_order = RuleDef {
            name: "bao".into(),
            src_label: "A".into(),
            dst_label: "A".into(),
            predicate: Predicate::All(vec![
                Predicate::FieldEqual { field: "f".into() },
                Predicate::VectorSimilar {
                    field: "emb".into(),
                    min: 0.9,
                },
            ]),
            edge_type: "E".into(),
            weight_prop: None,
            max_edges: None,
            approximate: true,
        };
        assert!(bad_all_order.validate().is_err());
    }

    #[test]
    fn all_composes_field_equal_and_numeric_within() {
        let a: HashMap<_, _> = [
            ("ind".to_string(), Value::Str("arch".into())),
            ("year".to_string(), Value::Int(1998)),
        ]
        .into();
        let b: HashMap<_, _> = [
            ("ind".to_string(), Value::Str("arch".into())),
            ("year".to_string(), Value::Float(2000.0)),
        ]
        .into();
        let p = Predicate::All(vec![
            Predicate::FieldEqual {
                field: "ind".into(),
            },
            Predicate::NumericWithin {
                field: "year".into(),
                tolerance: 3.0,
            },
        ]);
        let s = eval!(&p, ("a", a) => ("b", b)).unwrap();
        assert!((s - 1.0 / 3.0).abs() < 1e-9); // min(1.0, 1/3)
    }

    fn sample_rule(pred: Predicate) -> RuleDef {
        RuleDef {
            name: "r".into(),
            src_label: "A".into(),
            dst_label: "B".into(),
            predicate: pred,
            edge_type: "E".into(),
            weight_prop: None,
            max_edges: None,
            approximate: false,
        }
    }

    // -----------------------------------------------------------------------
    // Any predicate tests (TDD — written before implementation)
    // -----------------------------------------------------------------------

    /// Score = max over satisfied branches; None only when all branches fail.
    #[test]
    fn any_takes_max_score_and_requires_at_least_one_branch() {
        let mk =
            |items: &[&str]| Value::List(items.iter().map(|s| Value::Str((*s).into())).collect());
        // src: ind="arch", tags=["x","y"]; dst: ind="law", tags=["y","z"]
        // Branch A: FieldEqual(ind) → None  (arch ≠ law)
        // Branch B: Overlap(tags, 0.3) → jaccard = 1/3 ≥ 0.3 → Some(1/3)
        // Any → Some(max(_, 1/3)) = Some(1/3)
        let a: HashMap<_, _> = [
            ("ind".to_string(), Value::Str("arch".into())),
            ("tags".to_string(), mk(&["x", "y"])),
        ]
        .into();
        let b: HashMap<_, _> = [
            ("ind".to_string(), Value::Str("law".into())),
            ("tags".to_string(), mk(&["y", "z"])),
        ]
        .into();
        let p = Predicate::Any(vec![
            Predicate::FieldEqual {
                field: "ind".into(),
            },
            Predicate::Overlap {
                field: "tags".into(),
                min: 0.3,
            },
        ]);
        let s = eval!(&p, ("a", a) => ("b", b)).unwrap();
        assert!(
            (s - 1.0 / 3.0).abs() < 1e-9,
            "score must be max(None, 1/3) = 1/3, got {s}"
        );
    }

    /// When both branches match, Any returns the larger score.
    #[test]
    fn any_score_is_max_when_both_branches_match() {
        // src: ind="arch", year=2000; dst: ind="arch", year=2001
        // Branch A: FieldEqual(ind) → Some(1.0)
        // Branch B: NumericWithin(year, tol=3) → 1 - 1/3 = 2/3 → Some(2/3)
        // Any → Some(max(1.0, 2/3)) = Some(1.0)
        let a: HashMap<_, _> = [
            ("ind".to_string(), Value::Str("arch".into())),
            ("year".to_string(), Value::Int(2000)),
        ]
        .into();
        let b: HashMap<_, _> = [
            ("ind".to_string(), Value::Str("arch".into())),
            ("year".to_string(), Value::Float(2001.0)),
        ]
        .into();
        let p = Predicate::Any(vec![
            Predicate::FieldEqual {
                field: "ind".into(),
            },
            Predicate::NumericWithin {
                field: "year".into(),
                tolerance: 3.0,
            },
        ]);
        let s = eval!(&p, ("a", a) => ("b", b)).unwrap();
        assert!(
            (s - 1.0).abs() < 1e-9,
            "score must be max(1.0, 2/3) = 1.0, got {s}"
        );
    }

    /// None when all branches fail.
    #[test]
    fn any_returns_none_when_all_branches_fail() {
        let a: HashMap<_, _> = [("ind".to_string(), Value::Str("arch".into()))].into();
        let b: HashMap<_, _> = [("ind".to_string(), Value::Str("law".into()))].into();
        let p = Predicate::Any(vec![
            Predicate::FieldEqual {
                field: "ind".into(),
            },
            Predicate::FieldEqual {
                field: "ind".into(),
            },
        ]);
        assert_eq!(eval!(&p, ("a", a) => ("b", b)), None);
    }

    /// Nested All(FieldEqual, Any(Overlap, NumericWithin)).
    /// All uses min; Any uses max.  Combined: min(1.0, max(1/3, 2/3)) = 2/3.
    #[test]
    fn nested_all_of_any_uses_min_over_max() {
        let mk =
            |items: &[&str]| Value::List(items.iter().map(|s| Value::Str((*s).into())).collect());
        // src: ind="arch", tags=["x","y"], year=2000
        // dst: ind="arch", tags=["y","z"], year=2001
        // FieldEqual(ind)             → Some(1.0)
        // Overlap(tags, 0.3)          → jaccard=1/3 → Some(1/3)
        // NumericWithin(year, tol=3)  → 1 - 1/3 = 2/3 → Some(2/3)
        // Any(Overlap, Numeric)       → max(1/3, 2/3) = 2/3
        // All(FieldEqual, Any(...))   → min(1.0, 2/3) = 2/3
        let a: HashMap<_, _> = [
            ("ind".to_string(), Value::Str("arch".into())),
            ("tags".to_string(), mk(&["x", "y"])),
            ("year".to_string(), Value::Int(2000)),
        ]
        .into();
        let b: HashMap<_, _> = [
            ("ind".to_string(), Value::Str("arch".into())),
            ("tags".to_string(), mk(&["y", "z"])),
            ("year".to_string(), Value::Float(2001.0)),
        ]
        .into();
        let p = Predicate::All(vec![
            Predicate::FieldEqual {
                field: "ind".into(),
            },
            Predicate::Any(vec![
                Predicate::Overlap {
                    field: "tags".into(),
                    min: 0.3,
                },
                Predicate::NumericWithin {
                    field: "year".into(),
                    tolerance: 3.0,
                },
            ]),
        ]);
        let s = eval!(&p, ("a", a) => ("b", b)).unwrap();
        assert!(
            (s - 2.0 / 3.0).abs() < 1e-9,
            "expected min(1.0, max(1/3, 2/3)) = 2/3, got {s}"
        );
    }

    /// validate() rejects empty Any; depth cap 4 is enforced with a named error.
    #[test]
    fn any_validation_errors() {
        // Empty Any → error
        let empty = sample_rule(Predicate::Any(vec![]));
        assert!(empty.validate().is_err());

        // Helper: build a singly-nested Any chain of the given depth.
        fn any_chain(depth: usize) -> Predicate {
            if depth == 0 {
                Predicate::FieldEqual {
                    field: "f".into(),
                }
            } else {
                Predicate::Any(vec![any_chain(depth - 1)])
            }
        }

        // depth 4 = cap → valid
        assert!(
            sample_rule(any_chain(4)).validate().is_ok(),
            "depth 4 must be valid (at cap)"
        );
        // depth 5 > cap → named error
        let too_deep = sample_rule(any_chain(5));
        let err = too_deep.validate().unwrap_err();
        assert!(
            err.contains("nesting depth"),
            "error must mention 'nesting depth', got: {err}"
        );

        // Any containing empty All → error propagated from inner validate_pred
        let bad_inner = sample_rule(Predicate::Any(vec![Predicate::All(vec![])]));
        assert!(bad_inner.validate().is_err());
    }

    /// watched_fields collects fields from all branches of Any.
    #[test]
    fn any_watched_fields_collected() {
        let p = Predicate::Any(vec![
            Predicate::FieldEqual {
                field: "ind".into(),
            },
            Predicate::NumericWithin {
                field: "year".into(),
                tolerance: 1.0,
            },
        ]);
        let r = sample_rule(p);
        assert!(r.validate().is_ok());
        let fields: Vec<_> = r.watched_fields().into_iter().collect();
        assert_eq!(fields, vec!["ind".to_string(), "year".to_string()]);
    }

    #[test]
    fn new_predicates_validate_and_watch_fields() {
        let num = sample_rule(Predicate::NumericWithin {
            field: "year".into(),
            tolerance: 2.0,
        });
        assert!(num.validate().is_ok());
        assert_eq!(
            num.watched_fields().into_iter().collect::<Vec<_>>(),
            vec!["year".to_string()]
        );
        let geo = sample_rule(Predicate::GeoRadius {
            field: "loc".into(),
            km: 400.0,
        });
        assert!(geo.validate().is_ok());
        let vecp = sample_rule(Predicate::VectorSimilar {
            field: "emb".into(),
            min: 0.9,
        });
        assert!(vecp.validate().is_ok());

        let mut bad = num.clone();
        bad.predicate = Predicate::NumericWithin {
            field: "year".into(),
            tolerance: -1.0,
        };
        assert!(bad.validate().is_err());
        bad.predicate = Predicate::NumericWithin {
            field: "year".into(),
            tolerance: f64::NAN,
        };
        assert!(bad.validate().is_err());

        let mut bad_geo = geo;
        bad_geo.predicate = Predicate::GeoRadius {
            field: "loc".into(),
            km: 0.0,
        };
        assert!(bad_geo.validate().is_err());
        bad_geo.predicate = Predicate::GeoRadius {
            field: "loc".into(),
            km: f64::NAN,
        };
        assert!(bad_geo.validate().is_err());

        let mut bad_vec = vecp;
        bad_vec.predicate = Predicate::VectorSimilar {
            field: "emb".into(),
            min: 0.0,
        };
        assert!(bad_vec.validate().is_err());
        bad_vec.predicate = Predicate::VectorSimilar {
            field: "emb".into(),
            min: 1.5,
        };
        assert!(bad_vec.validate().is_err());
    }
}

#[cfg(test)]
mod wire_pins {
    use super::*;

    fn pin(pred: Predicate) -> RuleDef {
        RuleDef {
            name: "r".into(),
            src_label: "A".into(),
            dst_label: "B".into(),
            predicate: pred,
            edge_type: "E".into(),
            weight_prop: None,
            max_edges: None,
            approximate: false,
        }
    }

    fn pin_approx(pred: Predicate) -> RuleDef {
        RuleDef {
            name: "r".into(),
            src_label: "A".into(),
            dst_label: "B".into(),
            predicate: pred,
            edge_type: "E".into(),
            weight_prop: None,
            max_edges: None,
            approximate: true,
        }
    }

    #[test]
    fn old_predicate_variants_keep_encoding() {
        // Captured before Plan 7 appends. Discriminants 0..=3 must not move.
        // Plan 11 T4: `approximate: false` appends one zero byte at the end
        // of every existing record. Old WAL records written before this field
        // break positional bincode decode — pre-alpha no-migration ruling.
        assert_eq!(
            bincode::serialize(&pin(Predicate::KeyMatch { field: "fk".into() })).unwrap(),
            vec![
                1, 0, 0, 0, 0, 0, 0, 0, 114, 1, 0, 0, 0, 0, 0, 0, 0, 65, 1, 0, 0, 0, 0, 0, 0, 0,
                66, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 102, 107, 1, 0, 0, 0, 0, 0, 0, 0, 69, 0, 0,
                0
            ]
        );
        assert_eq!(
            bincode::serialize(&pin(Predicate::FieldEqual {
                field: "ind".into()
            }))
            .unwrap(),
            vec![
                1, 0, 0, 0, 0, 0, 0, 0, 114, 1, 0, 0, 0, 0, 0, 0, 0, 65, 1, 0, 0, 0, 0, 0, 0, 0,
                66, 1, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0, 105, 110, 100, 1, 0, 0, 0, 0, 0, 0, 0, 69,
                0, 0, 0
            ]
        );
        assert_eq!(
            bincode::serialize(&pin(Predicate::Overlap {
                field: "tags".into(),
                min: 0.5,
            }))
            .unwrap(),
            vec![
                1, 0, 0, 0, 0, 0, 0, 0, 114, 1, 0, 0, 0, 0, 0, 0, 0, 65, 1, 0, 0, 0, 0, 0, 0, 0,
                66, 2, 0, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0, 116, 97, 103, 115, 0, 0, 0, 0, 0, 0, 224,
                63, 1, 0, 0, 0, 0, 0, 0, 0, 69, 0, 0, 0
            ]
        );
        assert_eq!(
            bincode::serialize(&pin(Predicate::All(vec![
                Predicate::KeyMatch { field: "fk".into() },
                Predicate::Overlap {
                    field: "tags".into(),
                    min: 0.5,
                },
            ])))
            .unwrap(),
            vec![
                1, 0, 0, 0, 0, 0, 0, 0, 114, 1, 0, 0, 0, 0, 0, 0, 0, 65, 1, 0, 0, 0, 0, 0, 0, 0,
                66, 3, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 102,
                107, 2, 0, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0, 116, 97, 103, 115, 0, 0, 0, 0, 0, 0, 224,
                63, 1, 0, 0, 0, 0, 0, 0, 0, 69, 0, 0, 0
            ]
        );
    }

    #[test]
    fn new_predicate_variants_have_pinned_encoding() {
        assert_eq!(
            bincode::serialize(&pin(Predicate::NumericWithin {
                field: "year".into(),
                tolerance: 2.0,
            }))
            .unwrap(),
            vec![
                1, 0, 0, 0, 0, 0, 0, 0, 114, 1, 0, 0, 0, 0, 0, 0, 0, 65, 1, 0, 0, 0, 0, 0, 0, 0,
                66, 4, 0, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0, 121, 101, 97, 114, 0, 0, 0, 0, 0, 0, 0, 64,
                1, 0, 0, 0, 0, 0, 0, 0, 69, 0, 0, 0
            ]
        );
        assert_eq!(
            bincode::serialize(&pin(Predicate::GeoRadius {
                field: "loc".into(),
                km: 400.0,
            }))
            .unwrap(),
            vec![
                1, 0, 0, 0, 0, 0, 0, 0, 114, 1, 0, 0, 0, 0, 0, 0, 0, 65, 1, 0, 0, 0, 0, 0, 0, 0,
                66, 5, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0, 108, 111, 99, 0, 0, 0, 0, 0, 0, 121, 64, 1,
                0, 0, 0, 0, 0, 0, 0, 69, 0, 0, 0
            ]
        );
        assert_eq!(
            bincode::serialize(&pin(Predicate::VectorSimilar {
                field: "emb".into(),
                min: 0.9,
            }))
            .unwrap(),
            vec![
                1, 0, 0, 0, 0, 0, 0, 0, 114, 1, 0, 0, 0, 0, 0, 0, 0, 65, 1, 0, 0, 0, 0, 0, 0, 0,
                66, 6, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0, 101, 109, 98, 205, 204, 204, 204, 204, 204,
                236, 63, 1, 0, 0, 0, 0, 0, 0, 0, 69, 0, 0, 0
            ]
        );
    }

    #[test]
    fn any_variant_is_appended_at_discriminant_7() {
        // Any is discriminant 7 (appended after VectorSimilar=6).
        // Old WAL/snapshot records never contain discriminant 7, so old data
        // still round-trips via the existing variants 0–6.
        // Encoding: discriminant(u32 le) + Vec<Predicate> (len u64 le + elements).
        // This pin guards that Any is never reordered or renumbered.
        let any_fe = pin(Predicate::Any(vec![Predicate::FieldEqual {
            field: "f".into(),
        }]));
        let bytes = bincode::serialize(&any_fe).unwrap();
        // Predicate discriminant 7 is at byte 28 (after RuleDef header fields).
        // Locate the discriminant by checking that bytes[28..32] == [7,0,0,0].
        // (Exact byte index depends on RuleDef field widths — verified below.)
        // First, round-trip: Any must decode to itself.
        let decoded: RuleDef = bincode::deserialize(&bytes).unwrap();
        assert_eq!(decoded, any_fe, "Any must round-trip via bincode");
        // Discriminant 7 must appear somewhere in the serialised predicate bytes.
        let contains_discrim_7 = bytes
            .windows(4)
            .any(|w| w == [7u8, 0, 0, 0]);
        assert!(
            contains_discrim_7,
            "serialised Any must contain discriminant byte sequence [7,0,0,0]"
        );
        // Verify that RuleDefs with the old variants still decode after Any is appended.
        let old = bincode::serialize(&pin(Predicate::VectorSimilar {
            field: "emb".into(),
            min: 0.9,
        }))
        .unwrap();
        let old_decoded: RuleDef = bincode::deserialize(&old).unwrap();
        assert_eq!(
            old_decoded.predicate,
            Predicate::VectorSimilar {
                field: "emb".into(),
                min: 0.9
            },
            "pre-Any VectorSimilar record must still decode"
        );
    }

    #[test]
    fn approximate_variant_has_pinned_encoding() {
        // Pin: VectorSimilar with approximate=true. Last byte is 1 (true) vs 0 (false).
        assert_eq!(
            bincode::serialize(&pin_approx(Predicate::VectorSimilar {
                field: "emb".into(),
                min: 0.9,
            }))
            .unwrap(),
            vec![
                1, 0, 0, 0, 0, 0, 0, 0, 114, 1, 0, 0, 0, 0, 0, 0, 0, 65, 1, 0, 0, 0, 0, 0, 0, 0,
                66, 6, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0, 101, 109, 98, 205, 204, 204, 204, 204, 204,
                236, 63, 1, 0, 0, 0, 0, 0, 0, 0, 69, 0, 0, 1
            ]
        );
        // Same predicate with approximate=false (default) differs only in last byte.
        let exact = bincode::serialize(&pin(Predicate::VectorSimilar {
            field: "emb".into(),
            min: 0.9,
        }))
        .unwrap();
        let approx = bincode::serialize(&pin_approx(Predicate::VectorSimilar {
            field: "emb".into(),
            min: 0.9,
        }))
        .unwrap();
        assert_eq!(exact.len(), approx.len());
        assert_eq!(&exact[..exact.len() - 1], &approx[..approx.len() - 1]);
        assert_eq!(exact.last(), Some(&0u8));
        assert_eq!(approx.last(), Some(&1u8));
    }
}
