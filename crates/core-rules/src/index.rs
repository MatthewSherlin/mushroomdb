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
}

pub fn candidate_spec(p: &Predicate) -> CandidateSpec<'_> {
    match p {
        Predicate::KeyMatch { .. } => CandidateSpec::ByKey,
        Predicate::FieldEqual { field } => CandidateSpec::Scalar { field },
        Predicate::Overlap { field, .. } => CandidateSpec::Tokens { field },
        Predicate::All(parts) => candidate_spec(&parts[0]),
    }
}

impl SideIndex {
    fn keys_for(spec: &CandidateSpec, get: &dyn Fn(&str) -> Option<Value>) -> BTreeSet<ValueKey> {
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
        }
    }

    pub fn insert(&mut self, spec: &CandidateSpec, node: u32, get: &dyn Fn(&str) -> Option<Value>) {
        for k in Self::keys_for(spec, get) {
            self.by_key.entry(k).or_default().insert(node);
        }
    }

    pub fn remove(&mut self, spec: &CandidateSpec, node: u32, get: &dyn Fn(&str) -> Option<Value>) {
        for k in Self::keys_for(spec, get) {
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
        for k in Self::keys_for(spec, get) {
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
}
