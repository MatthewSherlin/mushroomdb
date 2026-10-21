//! Bolt error codes and the mapping from engine errors.
//!
//! Every failure the connection reports is a `FAILURE` message carrying a
//! `code` and a `message`. Drivers retry on the `TransientError` class, so the
//! class is part of the contract, not decoration.
//!
//! The code strings keep the prefix the official drivers match on; it is a
//! protocol constant and appears here and in tests only.

use mushroomdb::GraphError;

use crate::packstream::PackValue;

const CLIENT: &str = "Neo.ClientError";
const TRANSIENT: &str = "Neo.TransientError";
const DATABASE: &str = "Neo.DatabaseError";

/// The Bolt classification of an error; drivers retry only `Transient`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Classification {
    Client,
    Transient,
    Database,
}

/// A failure the connection reports to the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoltError {
    /// Bad or missing credentials. Carries no detail on purpose: a failed
    /// login must not say whether a token exists.
    Unauthorized,
    /// The role may not do this.
    Forbidden(String),
    ConstraintValidation(String),
    SyntaxError(String),
    TransactionTimedOut,
    DatabaseNotFound(String),
    /// An optimistic write conflict; drivers retry it.
    Conflict(String),
    /// IO failure or a busy store; drivers retry it.
    Unavailable(String),
    /// A message the connection state does not allow, or a malformed one.
    Invalid(String),
    /// A value the wire format cannot carry (over 4 GiB, or a structure with
    /// more than 65535 fields).
    TooLarge(String),
    /// A legal message whose handler is not built yet.
    NotSupported(String),
    Internal(String),
}

impl BoltError {
    /// The Bolt `code` string.
    pub fn code(&self) -> String {
        let (class, rest) = match self {
            BoltError::Unauthorized => (CLIENT, "Security.Unauthorized"),
            BoltError::Forbidden(_) => (CLIENT, "Security.Forbidden"),
            BoltError::ConstraintValidation(_) => (CLIENT, "Schema.ConstraintValidationFailed"),
            BoltError::SyntaxError(_) => (CLIENT, "Statement.SyntaxError"),
            BoltError::TransactionTimedOut => (CLIENT, "Transaction.TransactionTimedOut"),
            BoltError::DatabaseNotFound(_) => (CLIENT, "Database.DatabaseNotFound"),
            BoltError::Conflict(_) => (TRANSIENT, "Transaction.DeadlockDetected"),
            BoltError::Unavailable(_) => (TRANSIENT, "General.DatabaseUnavailable"),
            BoltError::Invalid(_) | BoltError::TooLarge(_) => (CLIENT, "Request.Invalid"),
            BoltError::NotSupported(_) | BoltError::Internal(_) => {
                (DATABASE, "General.UnknownError")
            }
        };
        format!("{class}.{rest}")
    }

    pub fn classification(&self) -> Classification {
        match self {
            BoltError::Conflict(_) | BoltError::Unavailable(_) => Classification::Transient,
            BoltError::NotSupported(_) | BoltError::Internal(_) => Classification::Database,
            _ => Classification::Client,
        }
    }

    /// The human-readable `message` string.
    pub fn message(&self) -> String {
        match self {
            BoltError::Unauthorized => {
                "The client is unauthorized due to authentication failure.".into()
            }
            BoltError::TransactionTimedOut => {
                "The transaction was idle past its timeout and has been rolled back.".into()
            }
            BoltError::Forbidden(m)
            | BoltError::ConstraintValidation(m)
            | BoltError::SyntaxError(m)
            | BoltError::DatabaseNotFound(m)
            | BoltError::Conflict(m)
            | BoltError::Unavailable(m)
            | BoltError::Invalid(m)
            | BoltError::TooLarge(m)
            | BoltError::NotSupported(m)
            | BoltError::Internal(m) => m.clone(),
        }
    }
}

impl std::fmt::Display for BoltError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code(), self.message())
    }
}

impl std::error::Error for BoltError {}

/// Fixed text for failures whose engine message can carry a path or other
/// internal detail. The detail stays in the server's own logs.
const UNAVAILABLE_MESSAGE: &str = "the database is temporarily unavailable; retry the transaction";
const INTERNAL_MESSAGE: &str = "an internal database error occurred";
const CONFLICT_MESSAGE: &str =
    "the transaction conflicted with a concurrent write; retry the transaction";

/// The mapping is exhaustive on purpose: a new `GraphError` variant must fail
/// to compile here until someone decides its class.
///
impl From<GraphError> for BoltError {
    fn from(err: GraphError) -> Self {
        match err {
            // Transient: the driver retries.
            GraphError::CasConflict { .. } => BoltError::Conflict(CONFLICT_MESSAGE.into()),
            GraphError::Io(_) | GraphError::Busy { .. } => {
                BoltError::Unavailable(UNAVAILABLE_MESSAGE.into())
            }
            // Database errors: a fault in the store, not in the request. The
            // message is fixed so a path or file detail cannot reach a client.
            // An unwritable data directory is a deployment fault on the server,
            // not something the client can fix or retry away; the message names
            // paths and uids, so it stays in the server's own logs.
            GraphError::Corrupt { .. } => BoltError::Internal(INTERNAL_MESSAGE.into()),
            GraphError::Unwritable { .. } => {
                BoltError::Internal(GraphError::UNWRITABLE_CLIENT_MESSAGE.into())
            }
            // Client errors whose text is about the request, not the host.
            GraphError::QueryError { .. } => BoltError::SyntaxError(err.to_string()),
            GraphError::DuplicateKey { .. }
            | GraphError::NamespaceImmutable { .. }
            | GraphError::CrossNamespace { .. }
            | GraphError::ConstraintViolation { .. } => {
                BoltError::ConstraintValidation(err.to_string())
            }
            GraphError::RoleWriteDenied { .. }
            | GraphError::ReadOnly
            | GraphError::MaskedReadOnly => BoltError::Forbidden(err.to_string()),
            GraphError::KeyNotFound { .. }
            | GraphError::RuleInvalid { .. }
            | GraphError::RuleOwned { .. }
            | GraphError::RuleNotFound { .. }
            | GraphError::IngestError { .. }
            | GraphError::CommitOutOfRange { .. }
            | GraphError::ViewPropReadOnly { .. }
            | GraphError::CommitTimeNotMonotonic { .. }
            | GraphError::NoRecordedTime
            | GraphError::TimeBeforeFloor { .. }
            | GraphError::ConstraintCreationConflict { .. }
            | GraphError::ValueTooLarge { .. } => BoltError::Invalid(err.to_string()),
        }
    }
}

impl BoltError {
    /// The `{code, message}` map a `FAILURE` carries.
    pub fn to_pack(&self) -> PackValue {
        PackValue::Map(vec![
            ("code".into(), PackValue::String(self.code())),
            ("message".into(), PackValue::String(self.message())),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mushroomdb::Value;

    const CLIENT_ERROR: &str = "Neo.ClientError";
    const TRANSIENT_ERROR: &str = "Neo.TransientError";

    /// One of every `GraphError` variant, with the code it must map to. The
    /// `match` has no wildcard, so adding a variant fails to compile here.
    fn expected(err: &GraphError) -> &'static str {
        match err {
            GraphError::CasConflict { .. } => "Neo.TransientError.Transaction.DeadlockDetected",
            GraphError::Io(_) | GraphError::Busy { .. } => {
                "Neo.TransientError.General.DatabaseUnavailable"
            }
            GraphError::Corrupt { .. } | GraphError::Unwritable { .. } => {
                "Neo.DatabaseError.General.UnknownError"
            }
            GraphError::QueryError { .. } => "Neo.ClientError.Statement.SyntaxError",
            GraphError::DuplicateKey { .. }
            | GraphError::NamespaceImmutable { .. }
            | GraphError::CrossNamespace { .. }
            | GraphError::ConstraintViolation { .. } => {
                "Neo.ClientError.Schema.ConstraintValidationFailed"
            }
            GraphError::RoleWriteDenied { .. }
            | GraphError::ReadOnly
            | GraphError::MaskedReadOnly => "Neo.ClientError.Security.Forbidden",
            GraphError::KeyNotFound { .. }
            | GraphError::RuleInvalid { .. }
            | GraphError::RuleOwned { .. }
            | GraphError::RuleNotFound { .. }
            | GraphError::IngestError { .. }
            | GraphError::CommitOutOfRange { .. }
            | GraphError::ViewPropReadOnly { .. }
            | GraphError::CommitTimeNotMonotonic { .. }
            | GraphError::NoRecordedTime
            | GraphError::TimeBeforeFloor { .. }
            | GraphError::ConstraintCreationConflict { .. }
            | GraphError::ValueTooLarge { .. } => "Neo.ClientError.Request.Invalid",
        }
    }

    fn every_variant(secret: &str) -> Vec<GraphError> {
        let s = || secret.to_string();
        vec![
            GraphError::KeyNotFound { key: "k".into() },
            GraphError::DuplicateKey { key: "k".into() },
            GraphError::Io(std::io::Error::other(s())),
            GraphError::Corrupt { detail: s() },
            GraphError::RuleInvalid { detail: "d".into() },
            GraphError::RuleOwned { detail: "d".into() },
            GraphError::RuleNotFound { name: "n".into() },
            GraphError::QueryError { detail: "d".into() },
            GraphError::ConstraintViolation {
                constraint: "c".into(),
                field: "f".into(),
            },
            GraphError::ConstraintCreationConflict {
                violations: vec![("k".into(), Value::Int(1))],
            },
            GraphError::ValueTooLarge {
                field: "f".into(),
                size: 2,
                limit: 1,
            },
            GraphError::IngestError { detail: "d".into() },
            GraphError::ReadOnly,
            GraphError::CommitOutOfRange {
                commit: 9,
                total: 3,
                floor: 0,
            },
            GraphError::ViewPropReadOnly {
                view_name: "v".into(),
            },
            GraphError::CasConflict {
                key: "k".into(),
                expected: 1,
                actual: 2,
            },
            GraphError::MaskedReadOnly,
            GraphError::CommitTimeNotMonotonic {
                supplied_ms: 1,
                newest_ms: 2,
            },
            GraphError::NoRecordedTime,
            GraphError::TimeBeforeFloor {
                floor_ms: 1,
                floor_commit: 2,
            },
            GraphError::RoleWriteDenied { reason: "r".into() },
            GraphError::Busy { holder: Some(1) },
            GraphError::Unwritable {
                path: s(),
                detail: s(),
                read_only_filesystem: false,
            },
            GraphError::NamespaceImmutable {
                key: "k".into(),
                from: "a".into(),
                to: "b".into(),
            },
            GraphError::CrossNamespace {
                src: "a".into(),
                src_ns: "x".into(),
                dst: "b".into(),
                dst_ns: "y".into(),
            },
        ]
    }

    #[test]
    fn every_engine_error_maps_to_its_documented_code_and_class() {
        for err in every_variant("x") {
            let want = expected(&err);
            let label = format!("{err:?}");
            let mapped = BoltError::from(err);
            assert_eq!(mapped.code(), want, "{label}");
            let class = if want.starts_with(TRANSIENT_ERROR) {
                Classification::Transient
            } else if want.starts_with(CLIENT_ERROR) {
                Classification::Client
            } else {
                Classification::Database
            };
            assert_eq!(mapped.classification(), class, "{label}");
        }
    }

    #[test]
    fn only_conflict_and_unavailable_are_retried_by_drivers() {
        let retried: Vec<_> = every_variant("x")
            .into_iter()
            .map(|e| (format!("{e:?}"), BoltError::from(e)))
            .filter(|(_, mapped)| mapped.classification() == Classification::Transient)
            .map(|(label, _)| label)
            .collect();
        assert_eq!(retried.len(), 3, "{retried:?}");
        assert!(retried
            .iter()
            .all(|e| e.starts_with("CasConflict") || e.starts_with("Io") || e.starts_with("Busy")));
    }

    #[test]
    fn host_detail_never_reaches_the_message() {
        let secret = "/var/lib/mushroomdb/secret-token-abc/wal.log";
        for err in every_variant(secret) {
            let mapped = BoltError::from(err);
            assert!(
                !mapped.message().contains(secret) && !mapped.message().contains("secret-token"),
                "{mapped}"
            );
        }
        let conflict = BoltError::from(GraphError::CasConflict {
            key: "hidden-node".into(),
            expected: 1,
            actual: 2,
        });
        assert!(!conflict.message().contains("hidden-node"));
    }

    #[test]
    fn to_pack_is_a_code_and_message_map() {
        let err = BoltError::from(GraphError::Busy { holder: None });
        assert_eq!(
            err.to_pack(),
            PackValue::Map(vec![
                (
                    "code".into(),
                    PackValue::String("Neo.TransientError.General.DatabaseUnavailable".into())
                ),
                ("message".into(), PackValue::String(err.message())),
            ])
        );
    }

    #[test]
    fn fixed_codes() {
        assert_eq!(
            BoltError::Unauthorized.code(),
            "Neo.ClientError.Security.Unauthorized"
        );
        assert_eq!(
            BoltError::TransactionTimedOut.code(),
            "Neo.ClientError.Transaction.TransactionTimedOut"
        );
        assert_eq!(
            BoltError::DatabaseNotFound("x".into()).code(),
            "Neo.ClientError.Database.DatabaseNotFound"
        );
        assert_eq!(
            BoltError::Unauthorized.classification(),
            Classification::Client
        );
    }
}
