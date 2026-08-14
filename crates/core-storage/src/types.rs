use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Value {
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
}

#[derive(Debug)]
pub enum GraphError {
    KeyNotFound { key: String },
    DuplicateKey { key: String },
    Io(std::io::Error),
    Corrupt { detail: String },
}

impl std::fmt::Display for GraphError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GraphError::KeyNotFound { key } => write!(f, "node key not found: {key}"),
            GraphError::DuplicateKey { key } => write!(f, "duplicate node key: {key}"),
            GraphError::Io(e) => write!(f, "io error: {e}"),
            GraphError::Corrupt { detail } => write!(f, "corrupt data: {detail}"),
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
}
