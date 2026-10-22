//! Continuous backup of a mushroomdb store to S3-compatible object storage.
//!
//! A [`BackupWorker`] runs beside the server. Every `wal_interval_ms` it ships
//! the WAL bytes fsynced since the last tick as one immutable segment,
//! `{prefix}/wal/{generation}/{start:020}-{end:020}`; when a snapshot is
//! written it uploads `snapshot.bin`, any new `wal.N.archive` and the side
//! files; and it writes manifests (`{prefix}/manifest/{commit_seq:020}.json`,
//! pointed to by `{prefix}/LATEST`) naming consistent restore points.
//!
//! The recovery point is the segment cadence, not the snapshot interval: a
//! commit is off-box durable once the segment holding it is uploaded.
//!
//! Before it uploads anything the worker takes the prefix's [`BucketLease`],
//! an advisory single-writer lock, and while another instance holds a fresh
//! one it uploads nothing and reports itself unhealthy (see [`lease`]).
//!
//! [`restore_from_s3`] reads a restore point back — the newest, or the one
//! holding a point in time — verifies every object, and installs it as a
//! store with a new backup lineage (see [`restore`]).
//!
//! The worker never takes an engine lock and never writes to the store; a
//! failed upload is retried with back-off and reported through
//! [`BackupHandle`], never to the write path.

/// The object-store crate this one builds on, for callers that read the bucket.
pub use object_store;

pub mod disk;
pub mod lease;
pub mod manifest;
pub mod restore;
pub mod segment;
pub mod store;
pub mod worker;

pub use lease::{BucketLease, Exclusion, LeaseError};
pub use manifest::{FileEntry, Manifest, SegmentEntry, MANIFEST_VERSION};
pub use restore::{
    restore_from_s3, restore_from_store, RestoreS3Error, RestoreS3Outcome, RestoreTarget,
};
pub use store::{build_store, BackupConfig, S3Location};
pub use worker::{
    BackupCounters, BackupHandle, BackupTrigger, BackupWorker, WalSource, SIDE_FILES,
};

/// Why a backup step or its configuration failed.
#[derive(Debug)]
pub enum BackupError {
    /// The environment asked for a backup that cannot be set up.
    Config(String),
    /// The object store refused or failed a request.
    Store(object_store::Error),
    /// Reading the store's files failed.
    Io(std::io::Error),
    /// A manifest in the bucket is missing or unreadable.
    Manifest(String),
    /// The store's files are between states; the step is retried.
    Inconsistent(String),
    /// An object-store request took longer than `op_timeout_ms`.
    Timeout(String),
    /// The bucket lease does not allow this write: it is lost, or it could
    /// not be renewed in time and the holder has fenced itself.
    Lease(String),
    /// The prefix belongs to another store lineage, or this store was
    /// rewound; the worker refuses it.
    Lineage(String),
    /// The bucket refused a conditional write: another writer is active.
    Conflict(String),
}

impl std::fmt::Display for BackupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackupError::Config(m) => write!(f, "backup configuration: {m}"),
            // A refusal's body can carry the access key id and signing
            // material; none of it reaches a log.
            BackupError::Store(e) => write!(f, "object store: {}", redact(&e.to_string())),
            BackupError::Timeout(what) => write!(f, "{what} did not finish within the deadline"),
            BackupError::Io(e) => write!(f, "reading the store: {e}"),
            BackupError::Manifest(m) => write!(f, "manifest: {m}"),
            BackupError::Inconsistent(m) => write!(f, "{m}"),
            BackupError::Lease(m) => write!(f, "bucket lease: {m}"),
            BackupError::Lineage(m) | BackupError::Conflict(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for BackupError {}

impl From<object_store::Error> for BackupError {
    fn from(e: object_store::Error) -> Self {
        BackupError::Store(e)
    }
}

impl From<std::io::Error> for BackupError {
    fn from(e: std::io::Error) -> Self {
        BackupError::Io(e)
    }
}

/// XML elements of an S3 error body whose content identifies the caller or
/// the signature.
const REDACTED_ELEMENTS: [&str; 7] = [
    "AWSAccessKeyId",
    "AccessKeyId",
    "SignatureProvided",
    "StringToSign",
    "StringToSignBytes",
    "CanonicalRequest",
    "CanonicalRequestBytes",
];

/// Query parameters and header fields whose value is signing material.
const REDACTED_FIELDS: [&str; 6] = [
    "X-Amz-Credential=",
    "X-Amz-Signature=",
    "X-Amz-Security-Token=",
    "Credential=",
    "Signature=",
    "x-amz-security-token:",
];

/// `text` with access-key ids, signatures and other credential material
/// replaced by `[redacted]`: the content of [`REDACTED_ELEMENTS`] XML
/// elements, the value after each of [`REDACTED_FIELDS`], and anything shaped
/// like an AWS access key id (`AKIA…`, `ASIA…`: four letters and sixteen more
/// upper-case letters or digits).
pub fn redact(text: &str) -> String {
    let mut out = text.to_string();
    for el in REDACTED_ELEMENTS {
        let (open, close) = (format!("<{el}>"), format!("</{el}>"));
        let mut from = 0;
        while let Some(i) = out[from..].find(&open).map(|i| i + from + open.len()) {
            let Some(j) = out[i..].find(&close).map(|j| j + i) else {
                break;
            };
            out.replace_range(i..j, "[redacted]");
            from = i + "[redacted]".len() + close.len();
        }
    }
    for field in REDACTED_FIELDS {
        let mut from = 0;
        while let Some(i) = out[from..].find(field).map(|i| i + from + field.len()) {
            let end = out[i..]
                .find(|c: char| c == '&' || c == ',' || c == '"' || c == '<' || c.is_whitespace())
                .map_or(out.len(), |j| j + i);
            out.replace_range(i..end, "[redacted]");
            from = i + "[redacted]".len();
        }
    }
    redact_key_ids(&out)
}

fn redact_key_ids(text: &str) -> String {
    const PREFIXES: [&str; 6] = ["AKIA", "ASIA", "AIDA", "AROA", "AGPA", "ANPA"];
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        let boundary_before = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
        let candidate = bytes.get(i..i + 20).filter(|w| {
            PREFIXES.iter().any(|p| w.starts_with(p.as_bytes()))
                && w.iter()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
        });
        let boundary_after = bytes.get(i + 20).is_none_or(|b| !b.is_ascii_alphanumeric());
        if boundary_before && boundary_after && candidate.is_some() {
            out.push_str("[redacted]");
            i += 20;
        } else {
            let ch = text[i..].chars().next().expect("in bounds");
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const BODY: &str = "<?xml version=\"1.0\"?><Error><Code>SignatureDoesNotMatch</Code>\
        <AWSAccessKeyId>AKIAIOSFODNN7EXAMPLE</AWSAccessKeyId>\
        <StringToSign>AWS4-HMAC-SHA256\n20261020T000000Z</StringToSign>\
        <SignatureProvided>abc123</SignatureProvided><RequestId>R1</RequestId></Error>";

    #[test]
    fn an_s3_refusal_body_loses_its_credentials() {
        let r = redact(BODY);
        for secret in ["AKIAIOSFODNN7EXAMPLE", "abc123", "AWS4-HMAC-SHA256"] {
            assert!(!r.contains(secret), "{secret} survived: {r}");
        }
        assert!(
            r.contains("SignatureDoesNotMatch"),
            "the useful part stays: {r}"
        );
        assert!(r.contains("<RequestId>R1</RequestId>"));
    }

    #[test]
    fn signing_parameters_and_bare_key_ids_are_redacted() {
        let r = redact(
            "GET /b?X-Amz-Credential=AKIAIOSFODNN7EXAMPLE%2F2026&X-Amz-Signature=deadbeef ok \
             Authorization: AWS4-HMAC-SHA256 Credential=ASIAABCDEFGHIJKLMNOP/x, Signature=ff",
        );
        for secret in [
            "AKIAIOSFODNN7EXAMPLE",
            "deadbeef",
            "ASIAABCDEFGHIJKLMNOP",
            "=ff",
        ] {
            assert!(!r.contains(secret), "{secret} survived: {r}");
        }
        assert!(r.contains(" ok "));
        // A bare id in prose, too; a longer word that merely starts the same is not.
        assert_eq!(redact("key AKIAIOSFODNN7EXAMPLE."), "key [redacted].");
        assert_eq!(redact("AKIAIOSFODNN7EXAMPLEXX"), "AKIAIOSFODNN7EXAMPLEXX");
    }

    #[test]
    fn a_store_error_displays_redacted() {
        let e = BackupError::Store(object_store::Error::Generic {
            store: "S3",
            source: Box::new(std::io::Error::other(BODY)),
        });
        let shown = e.to_string();
        assert!(!shown.contains("AKIAIOSFODNN7EXAMPLE"), "{shown}");
        assert!(shown.contains("[redacted]"));
    }
}
