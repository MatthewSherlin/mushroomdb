use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Value {
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
    List(Vec<Value>),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ValueKey {
    Int(i64),
    FloatBits(u64), // f64::to_bits — equality of representation, fine for index keys
    Str(String),
    Bool(bool),
}

impl ValueKey {
    pub fn from_value(v: &Value) -> Option<ValueKey> {
        match v {
            Value::Int(i) => Some(ValueKey::Int(*i)),
            Value::Float(f) => Some(ValueKey::FloatBits(f.to_bits())),
            Value::Str(s) => Some(ValueKey::Str(s.clone())),
            Value::Bool(b) => Some(ValueKey::Bool(*b)),
            Value::List(_) => None,
        }
    }
}

pub fn list_tokens(v: &Value) -> Option<std::collections::BTreeSet<ValueKey>> {
    match v {
        Value::List(items) => Some(items.iter().filter_map(ValueKey::from_value).collect()),
        _ => None,
    }
}

#[derive(Debug)]
pub enum GraphError {
    KeyNotFound { key: String },
    DuplicateKey { key: String },
    Io(std::io::Error),
    Corrupt { detail: String },
    RuleInvalid { detail: String },
    RuleOwned { detail: String },
    RuleNotFound { name: String },
    QueryError { detail: String },
    IngestError { detail: String },
}

impl std::fmt::Display for GraphError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GraphError::KeyNotFound { key } => write!(f, "node key not found: {key}"),
            GraphError::DuplicateKey { key } => write!(f, "duplicate node key: {key}"),
            GraphError::Io(e) => write!(f, "io error: {e}"),
            GraphError::Corrupt { detail } => write!(f, "corrupt data: {detail}"),
            GraphError::RuleInvalid { detail } => write!(f, "invalid rule: {detail}"),
            GraphError::RuleOwned { detail } => write!(f, "edge is rule-owned: {detail}"),
            GraphError::RuleNotFound { name } => write!(f, "rule not found: {name}"),
            GraphError::QueryError { detail } => write!(f, "query error: {detail}"),
            GraphError::IngestError { detail } => write!(f, "ingest error: {detail}"),
        }
    }
}

impl std::error::Error for GraphError {}

impl From<std::io::Error> for GraphError {
    fn from(e: std::io::Error) -> Self {
        GraphError::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, GraphError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_roundtrips_through_bincode() {
        let vals = vec![
            Value::Int(42),
            Value::Float(1.5),
            Value::Str("hi".into()),
            Value::Bool(true),
        ];
        let bytes = bincode::serialize(&vals).unwrap();
        let back: Vec<Value> = bincode::deserialize(&bytes).unwrap();
        assert_eq!(vals, back);
    }

    #[test]
    fn errors_display_context() {
        let e = GraphError::KeyNotFound { key: "u1".into() };
        assert_eq!(e.to_string(), "node key not found: u1");
    }

    #[test]
    fn list_roundtrips_and_old_variants_keep_encoding() {
        let l = Value::List(vec![Value::Str("a".into()), Value::Int(2)]);
        let back: Value = bincode::deserialize(&bincode::serialize(&l).unwrap()).unwrap();
        assert_eq!(l, back);
        // Appending List must not change the wire tag of existing variants.
        assert_eq!(
            bincode::serialize(&Value::Int(7)).unwrap(),
            vec![0, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0]
        );
    }

    #[test]
    fn value_keys_normalize_scalars_and_tokenize_lists() {
        assert_eq!(ValueKey::from_value(&Value::Int(3)), Some(ValueKey::Int(3)));
        assert_eq!(
            ValueKey::from_value(&Value::Float(1.5)),
            Some(ValueKey::FloatBits(1.5f64.to_bits()))
        );
        assert_eq!(ValueKey::from_value(&Value::List(vec![])), None);
        let toks = list_tokens(&Value::List(vec![
            Value::Str("x".into()),
            Value::Str("x".into()),           // dup collapses
            Value::List(vec![Value::Int(1)]), // nested list skipped
        ]))
        .unwrap();
        assert_eq!(toks.len(), 1);
        assert!(toks.contains(&ValueKey::Str("x".into())));
        assert_eq!(list_tokens(&Value::Int(1)), None);
    }
}
