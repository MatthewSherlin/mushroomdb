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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Predicate {
    KeyMatch { field: String },
    FieldEqual { field: String },
    Overlap { field: String, min: f64 },
    All(Vec<Predicate>),
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
        | Predicate::Overlap { field, .. } => {
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
            let mut score = f64::INFINITY;
            for part in parts {
                score = score.min(evaluate(part, src, dst)?);
            }
            Some(score)
        }
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
}
