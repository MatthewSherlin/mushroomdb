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
}

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
        validate_pred(&self.predicate)
    }

    pub fn watched_fields(&self) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        collect_fields(&self.predicate, &mut out);
        out
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
        Predicate::All(parts) => parts.iter().for_each(|q| collect_fields(q, out)),
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
        // 1 − 343.5/400 ≈ 0.141
        assert!((score - 0.141).abs() < 0.005);
        let outside = Predicate::GeoRadius {
            field: "loc".into(),
            km: 300.0,
        };
        assert_eq!(eval!(&outside, ("p", paris) => ("l", london)), None);
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
        }
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
        }
    }

    #[test]
    fn old_predicate_variants_keep_encoding() {
        // Captured before Plan 7 appends. Discriminants 0..=3 must not move.
        assert_eq!(
            bincode::serialize(&pin(Predicate::KeyMatch { field: "fk".into() })).unwrap(),
            vec![
                1, 0, 0, 0, 0, 0, 0, 0, 114, 1, 0, 0, 0, 0, 0, 0, 0, 65, 1, 0, 0, 0, 0, 0, 0, 0,
                66, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 102, 107, 1, 0, 0, 0, 0, 0, 0, 0, 69, 0, 0
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
                0, 0
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
                63, 1, 0, 0, 0, 0, 0, 0, 0, 69, 0, 0
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
                63, 1, 0, 0, 0, 0, 0, 0, 0, 69, 0, 0
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
                1, 0, 0, 0, 0, 0, 0, 0, 69, 0, 0
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
                0, 0, 0, 0, 0, 0, 0, 69, 0, 0
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
                236, 63, 1, 0, 0, 0, 0, 0, 0, 0, 69, 0, 0
            ]
        );
    }
}
