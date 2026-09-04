use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Value {
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
    List(Vec<Value>),
    /// Nested key-value map. Spills to the Mixed column path like `List`.
    /// Never promotes a homogeneous typed column. Not fulltext-indexed.
    Map(BTreeMap<String, Value>),
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
            // Maps are composite and cannot be reduced to a single index key.
            Value::Map(_) => None,
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
    KeyNotFound {
        key: String,
    },
    DuplicateKey {
        key: String,
    },
    Io(std::io::Error),
    Corrupt {
        detail: String,
    },
    RuleInvalid {
        detail: String,
    },
    RuleOwned {
        detail: String,
    },
    RuleNotFound {
        name: String,
    },
    QueryError {
        detail: String,
    },
    IngestError {
        detail: String,
    },
    /// Attempted mutation on a read-only as-of instance.
    ReadOnly,
    /// Requested commit index is beyond the valid range.
    CommitOutOfRange {
        commit: u64,
        total: u64,
    },
    /// Attempted to write to a view-managed property.
    ViewPropReadOnly {
        view_name: String,
    },
    /// A compare-and-set precondition was not satisfied.
    ///
    /// `expected` is the commit seq the caller expected to see; `actual` is the
    /// commit seq recorded in the last-change map (or `u64::MAX` for
    /// `NodeAbsent` conflicts where the node unexpectedly exists).
    CasConflict {
        key: String,
        expected: u64,
        actual: u64,
    },
    /// Write statement submitted to a read-only masked query path.
    ///
    /// Returned by [`GraphDb::query_masked`] when the Cypher input is a write
    /// statement (CREATE / MERGE / MATCH…SET / DELETE). The HTTP layer maps
    /// this to 400 Bad Request with body `{"error":"masked queries are read-only"}`.
    MaskedReadOnly,
    /// A role-scoped write was denied by the authz decision table (§3, §4.3).
    ///
    /// `reason` carries the verbatim §4.3 error body text (without the JSON
    /// envelope).  The HTTP layer maps this to 403 with body
    /// `{"error": "<reason>"}`.  Five reason patterns are defined by the spec:
    /// - `"role-bound token: label '<L>' not in write scope (<scope_field>)"`
    /// - `"role-bound token: target node not visible"` (hidden ≡ absent)
    /// - `"role-bound token: edge endpoint not visible"`
    /// - `"role-bound token: edge type '<T>' not in write scope (<scope_field>)"`
    /// - `"role-bound token: this endpoint is not permitted"`
    RoleWriteDenied {
        reason: String,
    },
    /// The store's advisory cross-process write lock could not be taken within
    /// the caller's wait budget: another process is writing.
    ///
    /// Nothing was written and in-memory state is unchanged, so retrying later
    /// is always safe. Readers never see this error — reads do not take the
    /// lock.
    ///
    /// `holder` is the process id of the lock holder when the platform makes it
    /// cheaply knowable, and `None` otherwise. It is a diagnostic hint only;
    /// never branch on it.
    Busy {
        holder: Option<u32>,
    },
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
            GraphError::ReadOnly => write!(f, "as-of instances are read-only"),
            GraphError::CommitOutOfRange { commit, total } => write!(
                f,
                "commit {commit} is out of range; valid range is 0..{total}"
            ),
            GraphError::ViewPropReadOnly { view_name } => write!(
                f,
                "property is managed by view {:?} and cannot be written directly",
                view_name
            ),
            GraphError::CasConflict {
                key,
                expected,
                actual,
            } => write!(
                f,
                "CAS conflict on key {key:?}: expected commit {expected}, actual {actual}"
            ),
            GraphError::MaskedReadOnly => write!(f, "masked queries are read-only"),
            GraphError::RoleWriteDenied { reason } => write!(f, "{reason}"),
            GraphError::Busy { holder: Some(pid) } => {
                write!(f, "store is busy: write lock held by process {pid}")
            }
            GraphError::Busy { holder: None } => {
                write!(f, "store is busy: write lock held by another process")
            }
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
