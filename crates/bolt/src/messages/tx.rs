//! `BEGIN`, `COMMIT` and `ROLLBACK`: explicit transactions.
//!
//! `BEGIN` carries one map, `extra`:
//!
//! | key | meaning here |
//! |---|---|
//! | `bookmarks` | list of commit bookmarks; `BEGIN` waits until the store has reached the highest, then freezes its snapshot |
//! | `db` | the database; checked against the routing config, as `ROUTE` and `RUN` are |
//! | `mode` | `"w"` (default) or `"r"`; a read transaction refuses write statements |
//! | `tx_timeout` | validated (a non-negative integer, milliseconds) but not enforced; the connection's idle limit applies |
//! | `tx_metadata` | validated (a map) and otherwise ignored |
//! | `imp_user` | impersonation is not supported; naming a user is refused |
//!
//! `COMMIT` replies `SUCCESS {bookmark}` where the bookmark is the commit the
//! transaction made (`mushroomdb:commit:<n>`), or, for a transaction that wrote
//! nothing, the commit its snapshot read. A commit that loses an optimistic
//! conflict replies with a transient `FAILURE` the driver retries from `BEGIN`.
//! `ROLLBACK` replies `SUCCESS {}`.

use crate::connection::{OpenTransaction, ResultCursor};
use crate::error::BoltError;
use crate::messages::run::{
    map_get, parse_bookmark, read_key_refusal, role_mask_error, tx_role_write_refusal, RowCursor,
};
use crate::packstream::PackValue;
use mushroomdb::{NodeMask, ReaderSnapshot, Value};
use std::collections::BTreeMap;

/// A decoded `BEGIN`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BeginRequest {
    /// The `db` field; `None` when absent or null.
    pub database: Option<String>,
    pub bookmarks: Vec<u64>,
    pub read_only: bool,
}

impl BeginRequest {
    /// Reads the fields of a `BEGIN` structure: one map. A wrong shape is a
    /// protocol violation (the caller decides); a user the server cannot act
    /// as is a request fault, [`BoltError::Forbidden`].
    pub fn from_fields(fields: Vec<PackValue>) -> Result<Result<Self, BoltError>, BoltError> {
        let [PackValue::Map(extra)] = fields.as_slice() else {
            return Err(BoltError::Invalid(
                "BEGIN takes exactly one map field".into(),
            ));
        };
        let database = match map_get(extra, "db") {
            None | Some(PackValue::Null) => None,
            Some(PackValue::String(name)) => Some(name.clone()),
            Some(_) => return Err(BoltError::Invalid("BEGIN db must be a string".into())),
        };
        let bookmarks = match map_get(extra, "bookmarks") {
            None | Some(PackValue::Null) => Vec::new(),
            Some(PackValue::List(items)) => items
                .iter()
                .map(|item| match item {
                    PackValue::String(text) => parse_bookmark(text),
                    _ => Err(BoltError::Invalid("a bookmark must be a string".into())),
                })
                .collect::<Result<_, _>>()?,
            Some(_) => {
                return Err(BoltError::Invalid(
                    "BEGIN bookmarks must be a list of strings".into(),
                ))
            }
        };
        let read_only = match map_get(extra, "mode") {
            None | Some(PackValue::Null) => false,
            Some(PackValue::String(mode)) if mode == "w" => false,
            Some(PackValue::String(mode)) if mode == "r" => true,
            Some(_) => {
                return Err(BoltError::Invalid(
                    "BEGIN mode must be \"r\" or \"w\"".into(),
                ))
            }
        };
        match map_get(extra, "tx_timeout") {
            None | Some(PackValue::Null) => {}
            Some(PackValue::Int(ms)) if *ms >= 0 => {}
            Some(_) => {
                return Err(BoltError::Invalid(
                    "BEGIN tx_timeout must be a non-negative integer".into(),
                ))
            }
        }
        match map_get(extra, "tx_metadata") {
            None | Some(PackValue::Null | PackValue::Map(_)) => {}
            Some(_) => return Err(BoltError::Invalid("BEGIN tx_metadata must be a map".into())),
        }
        match map_get(extra, "imp_user") {
            None | Some(PackValue::Null) => {}
            Some(PackValue::String(_)) => {
                return Ok(Err(BoltError::Forbidden(
                    "this server does not support impersonation".into(),
                )))
            }
            Some(_) => return Err(BoltError::Invalid("BEGIN imp_user must be a string".into())),
        }
        Ok(Ok(Self {
            database,
            bookmarks,
            read_only,
        }))
    }
}

/// A transaction that refuses write statements: `BEGIN` with `mode: "r"`.
pub struct ReadOnlyTransaction {
    inner: Box<dyn OpenTransaction>,
}

impl ReadOnlyTransaction {
    pub fn new(inner: Box<dyn OpenTransaction>) -> Self {
        Self { inner }
    }
}

impl OpenTransaction for ReadOnlyTransaction {
    fn rollback(self: Box<Self>) {
        self.inner.rollback();
    }

    fn commit(self: Box<Self>) -> Result<u64, BoltError> {
        self.inner.commit()
    }

    fn execute(
        &mut self,
        cypher: &str,
        params: &BTreeMap<String, Value>,
    ) -> Result<Box<dyn ResultCursor>, BoltError> {
        if mushroomdb::is_write_query(cypher).map_err(BoltError::SyntaxError)? {
            return Err(BoltError::Forbidden(
                "a read-only transaction cannot run a write statement".into(),
            ));
        }
        self.inner.execute(cypher, params)
    }
}

/// The transaction a role token or a `read` key gets, the twin of the HTTP
/// `/tx` scoped body: every statement reads through the role's mask on the
/// snapshot frozen at `BEGIN`, and every write is refused with the text
/// HTTP gives. The transaction engine has no role-scoped write path, so such
/// a transaction never writes, whatever write scope the role carries (ledger
/// row 140). Its commit writes nothing and returns the snapshot's commit.
pub struct ScopedTransaction {
    snapshot: ReaderSnapshot,
    mask: NodeMask,
    read_tier_key: bool,
}

impl ScopedTransaction {
    /// `mask` is the role's, resolved on `snapshot`. `read_tier_key` picks the
    /// refusal text a write gets, as it does over HTTP.
    pub(crate) fn new(snapshot: ReaderSnapshot, mask: NodeMask, read_tier_key: bool) -> Self {
        Self {
            snapshot,
            mask,
            read_tier_key,
        }
    }
}

impl OpenTransaction for ScopedTransaction {
    fn rollback(self: Box<Self>) {}

    fn commit(self: Box<Self>) -> Result<u64, BoltError> {
        Ok(self.snapshot.commit_seq())
    }

    fn execute(
        &mut self,
        cypher: &str,
        params: &BTreeMap<String, Value>,
    ) -> Result<Box<dyn ResultCursor>, BoltError> {
        if mushroomdb::is_write_query(cypher).map_err(BoltError::SyntaxError)? {
            return Err(if self.read_tier_key {
                read_key_refusal()
            } else {
                tx_role_write_refusal()
            });
        }
        let result = self
            .snapshot
            .query_masked(cypher, params, &self.mask)
            .map_err(role_mask_error)?;
        Ok(Box::new(RowCursor::new(result, false, None)))
    }

    fn is_scoped(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn begin(extra: Vec<(&str, PackValue)>) -> Vec<PackValue> {
        vec![PackValue::Map(
            extra.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        )]
    }

    #[test]
    fn an_empty_extra_map_is_a_write_transaction_on_the_default_database() {
        let request = BeginRequest::from_fields(begin(vec![])).unwrap().unwrap();
        assert_eq!(
            request,
            BeginRequest {
                database: None,
                bookmarks: vec![],
                read_only: false
            }
        );
    }

    #[test]
    fn bookmarks_mode_and_db_are_read() {
        let request = BeginRequest::from_fields(begin(vec![
            (
                "bookmarks",
                PackValue::List(vec![
                    PackValue::String("mushroomdb:commit:4".into()),
                    PackValue::String("mushroomdb:commit:9".into()),
                ]),
            ),
            ("mode", PackValue::String("r".into())),
            ("db", PackValue::String("x".into())),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(request.bookmarks, vec![4, 9]);
        assert!(request.read_only);
        assert_eq!(request.database.as_deref(), Some("x"));
    }

    #[test]
    fn a_user_to_impersonate_is_a_request_fault_not_a_violation() {
        let outcome =
            BeginRequest::from_fields(begin(vec![("imp_user", PackValue::String("u".into()))]))
                .unwrap();
        assert!(matches!(outcome, Err(BoltError::Forbidden(_))));
    }
}
