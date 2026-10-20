//! Bolt error codes and the mapping from engine errors.
//!
//! Every failure the connection reports is a `FAILURE` message carrying a
//! `code` and a `message`. Drivers retry on the `TransientError` class, so the
//! class is part of the contract, not decoration.
//!
//! The code strings keep the prefix the official drivers match on; it is a
//! protocol constant and appears here and in tests only.

use mushroomdb::GraphError;

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

impl From<GraphError> for BoltError {
    fn from(err: GraphError) -> Self {
        let message = err.to_string();
        match err {
            GraphError::CasConflict { .. } => BoltError::Conflict(message),
            GraphError::Io(_) | GraphError::Busy { .. } => BoltError::Unavailable(message),
            GraphError::QueryError { .. } => BoltError::SyntaxError(message),
            GraphError::DuplicateKey { .. } => BoltError::ConstraintValidation(message),
            GraphError::RoleWriteDenied { .. }
            | GraphError::ReadOnly
            | GraphError::MaskedReadOnly => BoltError::Forbidden(message),
            _ => BoltError::Internal(message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_errors_map_to_the_documented_codes() {
        let conflict = BoltError::from(GraphError::CasConflict {
            key: "k".into(),
            expected: 1,
            actual: 2,
        });
        assert_eq!(
            conflict.code(),
            "Neo.TransientError.Transaction.DeadlockDetected"
        );
        assert_eq!(conflict.classification(), Classification::Transient);

        let io = BoltError::from(GraphError::Io(std::io::Error::other("disk")));
        assert_eq!(io.code(), "Neo.TransientError.General.DatabaseUnavailable");
        let busy = BoltError::from(GraphError::Busy { holder: None });
        assert_eq!(busy.classification(), Classification::Transient);

        let query = BoltError::from(GraphError::QueryError { detail: "x".into() });
        assert_eq!(query.code(), "Neo.ClientError.Statement.SyntaxError");
        let denied = BoltError::from(GraphError::RoleWriteDenied { reason: "r".into() });
        assert_eq!(denied.code(), "Neo.ClientError.Security.Forbidden");
        let dup = BoltError::from(GraphError::DuplicateKey { key: "k".into() });
        assert_eq!(
            dup.code(),
            "Neo.ClientError.Schema.ConstraintValidationFailed"
        );
        let other = BoltError::from(GraphError::Corrupt { detail: "c".into() });
        assert_eq!(other.code(), "Neo.DatabaseError.General.UnknownError");
        assert_eq!(other.classification(), Classification::Database);
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
