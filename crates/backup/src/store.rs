//! Configuration from the environment, and the object store it names.

use crate::BackupError;
use object_store::aws::{AmazonS3Builder, AmazonS3ConfigKey, Checksum};
use object_store::ObjectStore;
use std::sync::Arc;

/// Default WAL-segment cadence (`MUSHROOMDB_BACKUP_WAL_INTERVAL_MS`).
pub const DEFAULT_WAL_INTERVAL_MS: u64 = 1000;
/// Default cadence of segment-only manifests (`MUSHROOMDB_BACKUP_MANIFEST_INTERVAL_S`).
pub const DEFAULT_MANIFEST_INTERVAL_S: u64 = 60;
/// Default number of manifests kept (`MUSHROOMDB_BACKUP_RETAIN_N`; 0 keeps all).
pub const DEFAULT_RETAIN_N: usize = 7;
/// Default bucket-lease TTL in seconds (`MUSHROOMDB_BACKUP_LEASE_TTL`).
pub const DEFAULT_LEASE_TTL_S: u64 = 30;
/// Default deadline on each object-store operation (`MUSHROOMDB_BACKUP_OP_TIMEOUT_MS`).
pub const DEFAULT_OP_TIMEOUT_MS: u64 = 10_000;
/// Default bound on the shutdown flush (`MUSHROOMDB_BACKUP_SHUTDOWN_GRACE_MS`).
pub const DEFAULT_SHUTDOWN_GRACE_MS: u64 = 10_000;

/// Backup settings, read from the environment by [`BackupConfig::from_env`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupConfig {
    /// `MUSHROOMDB_BACKUP_S3_URL`: `s3://bucket/prefix`, `r2://bucket/prefix`
    /// or `minio://bucket/prefix`.
    pub s3_url: String,
    /// `MUSHROOMDB_BACKUP_WAL_INTERVAL_MS`: how often new WAL bytes ship.
    pub wal_interval_ms: u64,
    /// `MUSHROOMDB_BACKUP_MANIFEST_INTERVAL_S`: how often a manifest is
    /// written while segments accumulate between snapshots.
    pub manifest_interval_s: u64,
    /// `MUSHROOMDB_BACKUP_RETAIN_N`: manifests kept; 0 keeps every one.
    pub retain_n: usize,
    /// `MUSHROOMDB_BACKUP_SSE`: server-side encryption (`AES256`, `aws:kms`,
    /// `aws:kms:dsse`), or `None` for the bucket's default.
    pub sse: Option<String>,
    /// `MUSHROOMDB_BACKUP_LEASE_TTL`: bucket-lease TTL in seconds.
    pub lease_ttl_s: u64,
    /// `MUSHROOMDB_BACKUP_OP_TIMEOUT_MS`: the most one object-store request
    /// (one PUT, one part, one GET, one LIST, one DELETE) may take before the
    /// worker gives up on it and counts a failure. The client's own retries
    /// can otherwise hold a black-holed bucket for minutes.
    pub op_timeout_ms: u64,
    /// `MUSHROOMDB_BACKUP_SHUTDOWN_GRACE_MS`: how long the shutdown flush may
    /// take before the in-flight upload is abandoned and the worker stops.
    pub shutdown_grace_ms: u64,
}

/// Where a backup URL points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3Location {
    /// `s3`, `r2` or `minio`.
    pub scheme: String,
    pub bucket: String,
    /// Key prefix inside the bucket, without leading or trailing `/`; may be
    /// empty.
    pub prefix: String,
}

impl BackupConfig {
    /// Read the configuration from the process environment. `Ok(None)` when
    /// `MUSHROOMDB_BACKUP_S3_URL` is unset or empty: backup is off.
    ///
    /// # Errors
    ///
    /// A set but malformed value — a backup the operator asked for is never
    /// silently turned off by a typo.
    pub fn from_env() -> Result<Option<Self>, BackupError> {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    /// [`from_env`](Self::from_env) over any variable lookup.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Option<Self>, BackupError> {
        let Some(s3_url) = get("MUSHROOMDB_BACKUP_S3_URL").filter(|v| !v.trim().is_empty()) else {
            return Ok(None);
        };
        let s3_url = s3_url.trim().to_string();
        parse_location(&s3_url)?;
        let number = |key: &str, default: u64| -> Result<u64, BackupError> {
            match get(key).filter(|v| !v.trim().is_empty()) {
                None => Ok(default),
                Some(v) => v.trim().parse().map_err(|_| {
                    BackupError::Config(format!("{key} must be a whole number, got {v:?}"))
                }),
            }
        };
        let wal_interval_ms = number("MUSHROOMDB_BACKUP_WAL_INTERVAL_MS", DEFAULT_WAL_INTERVAL_MS)?;
        if wal_interval_ms == 0 {
            return Err(BackupError::Config(
                "MUSHROOMDB_BACKUP_WAL_INTERVAL_MS must be at least 1".into(),
            ));
        }
        Ok(Some(Self {
            s3_url,
            wal_interval_ms,
            manifest_interval_s: number(
                "MUSHROOMDB_BACKUP_MANIFEST_INTERVAL_S",
                DEFAULT_MANIFEST_INTERVAL_S,
            )?,
            retain_n: number("MUSHROOMDB_BACKUP_RETAIN_N", DEFAULT_RETAIN_N as u64)? as usize,
            sse: get("MUSHROOMDB_BACKUP_SSE")
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty()),
            lease_ttl_s: number("MUSHROOMDB_BACKUP_LEASE_TTL", DEFAULT_LEASE_TTL_S)?,
            op_timeout_ms: positive(
                number("MUSHROOMDB_BACKUP_OP_TIMEOUT_MS", DEFAULT_OP_TIMEOUT_MS)?,
                "MUSHROOMDB_BACKUP_OP_TIMEOUT_MS",
            )?,
            shutdown_grace_ms: positive(
                number(
                    "MUSHROOMDB_BACKUP_SHUTDOWN_GRACE_MS",
                    DEFAULT_SHUTDOWN_GRACE_MS,
                )?,
                "MUSHROOMDB_BACKUP_SHUTDOWN_GRACE_MS",
            )?,
        }))
    }

    /// A configuration for `s3_url` with every other setting at its default.
    pub fn new(s3_url: impl Into<String>) -> Self {
        Self {
            s3_url: s3_url.into(),
            wal_interval_ms: DEFAULT_WAL_INTERVAL_MS,
            manifest_interval_s: DEFAULT_MANIFEST_INTERVAL_S,
            retain_n: DEFAULT_RETAIN_N,
            sse: None,
            lease_ttl_s: DEFAULT_LEASE_TTL_S,
            op_timeout_ms: DEFAULT_OP_TIMEOUT_MS,
            shutdown_grace_ms: DEFAULT_SHUTDOWN_GRACE_MS,
        }
    }

    /// The bucket and prefix `s3_url` names.
    pub fn location(&self) -> Result<S3Location, BackupError> {
        parse_location(&self.s3_url)
    }
}

fn positive(v: u64, key: &str) -> Result<u64, BackupError> {
    if v == 0 {
        return Err(BackupError::Config(format!("{key} must be at least 1")));
    }
    Ok(v)
}

/// Split `scheme://bucket/prefix`.
pub fn parse_location(url: &str) -> Result<S3Location, BackupError> {
    let bad = |why: &str| {
        BackupError::Config(format!(
            "MUSHROOMDB_BACKUP_S3_URL {url:?} {why}; expected s3://bucket/prefix, \
             r2://bucket/prefix or minio://bucket/prefix"
        ))
    };
    let (scheme, rest) = url.split_once("://").ok_or_else(|| bad("has no scheme"))?;
    if !matches!(scheme, "s3" | "r2" | "minio") {
        return Err(bad("has an unsupported scheme"));
    }
    let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
    if bucket.is_empty() {
        return Err(bad("names no bucket"));
    }
    Ok(S3Location {
        scheme: scheme.to_string(),
        bucket: bucket.to_string(),
        prefix: prefix.trim_matches('/').to_string(),
    })
}

/// Build the S3 client for `config`.
///
/// Credentials and endpoint come from the standard `AWS_*` variables
/// (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_ENDPOINT_URL`,
/// `AWS_REGION`, default `us-east-1`). `r2://` and `minio://` need
/// `AWS_ENDPOINT_URL`; a plain-`http` endpoint is allowed, for a local MinIO.
/// Every upload carries an S3 SHA-256 checksum, so the service rejects a body
/// that was corrupted in transit.
pub fn build_store(config: &BackupConfig) -> Result<Arc<dyn ObjectStore>, BackupError> {
    build_store_with(config, |k| std::env::var(k).ok())
}

/// [`build_store`] over any variable lookup.
pub fn build_store_with(
    config: &BackupConfig,
    get: impl Fn(&str) -> Option<String>,
) -> Result<Arc<dyn ObjectStore>, BackupError> {
    let loc = config.location()?;
    let get = |k: &str| {
        get(k)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let endpoint = get("AWS_ENDPOINT_URL");
    if loc.scheme != "s3" && endpoint.is_none() {
        return Err(BackupError::Config(format!(
            "{}:// needs AWS_ENDPOINT_URL set to the service's endpoint",
            loc.scheme
        )));
    }
    let mut b = AmazonS3Builder::new()
        .with_bucket_name(&loc.bucket)
        .with_region(get("AWS_REGION").unwrap_or_else(|| "us-east-1".into()))
        .with_checksum_algorithm(Checksum::SHA256);
    if let Some(id) = get("AWS_ACCESS_KEY_ID") {
        b = b.with_access_key_id(id);
    }
    if let Some(secret) = get("AWS_SECRET_ACCESS_KEY") {
        b = b.with_secret_access_key(secret);
    }
    if let Some(token) = get("AWS_SESSION_TOKEN") {
        b = b.with_token(token);
    }
    if let Some(ep) = endpoint {
        b = b
            .with_allow_http(ep.starts_with("http://"))
            .with_endpoint(ep);
    }
    if let Some(sse) = &config.sse {
        let key: AmazonS3ConfigKey = "aws_server_side_encryption"
            .parse()
            .map_err(|e| BackupError::Config(format!("{e}")))?;
        b = b.with_config(key, sse);
    }
    let store = b
        .build()
        .map_err(|e| BackupError::Config(format!("cannot build the S3 client: {e}")))?;
    Ok(Arc::new(store))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| m.get(k).cloned()
    }

    #[test]
    fn unset_url_means_backup_is_off() {
        assert_eq!(BackupConfig::from_lookup(env(&[])).unwrap(), None);
        assert_eq!(
            BackupConfig::from_lookup(env(&[("MUSHROOMDB_BACKUP_S3_URL", " ")])).unwrap(),
            None
        );
    }

    #[test]
    fn defaults_apply_when_only_the_url_is_set() {
        let c = BackupConfig::from_lookup(env(&[("MUSHROOMDB_BACKUP_S3_URL", "s3://b/p")]))
            .unwrap()
            .unwrap();
        assert_eq!(c, BackupConfig::new("s3://b/p"));
        assert_eq!(c.wal_interval_ms, 1000);
        assert_eq!(c.manifest_interval_s, 60);
    }

    #[test]
    fn every_setting_is_read() {
        let c = BackupConfig::from_lookup(env(&[
            ("MUSHROOMDB_BACKUP_S3_URL", "minio://bk/a/b/"),
            ("MUSHROOMDB_BACKUP_WAL_INTERVAL_MS", "250"),
            ("MUSHROOMDB_BACKUP_MANIFEST_INTERVAL_S", "5"),
            ("MUSHROOMDB_BACKUP_RETAIN_N", "0"),
            ("MUSHROOMDB_BACKUP_SSE", "AES256"),
            ("MUSHROOMDB_BACKUP_LEASE_TTL", "9"),
            ("MUSHROOMDB_BACKUP_OP_TIMEOUT_MS", "1500"),
            ("MUSHROOMDB_BACKUP_SHUTDOWN_GRACE_MS", "2500"),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(
            (
                c.wal_interval_ms,
                c.manifest_interval_s,
                c.retain_n,
                c.lease_ttl_s
            ),
            (250, 5, 0, 9)
        );
        assert_eq!(c.sse.as_deref(), Some("AES256"));
        assert_eq!((c.op_timeout_ms, c.shutdown_grace_ms), (1500, 2500));
        let loc = c.location().unwrap();
        assert_eq!((loc.bucket.as_str(), loc.prefix.as_str()), ("bk", "a/b"));
    }

    #[test]
    fn malformed_values_are_errors_not_silence() {
        for (k, v) in [
            ("MUSHROOMDB_BACKUP_S3_URL", "gs://b/p"),
            ("MUSHROOMDB_BACKUP_S3_URL", "bucket/p"),
            ("MUSHROOMDB_BACKUP_S3_URL", "s3:///p"),
        ] {
            assert!(BackupConfig::from_lookup(env(&[(k, v)])).is_err(), "{v}");
        }
        let bad_n = env(&[
            ("MUSHROOMDB_BACKUP_S3_URL", "s3://b"),
            ("MUSHROOMDB_BACKUP_WAL_INTERVAL_MS", "fast"),
        ]);
        assert!(BackupConfig::from_lookup(bad_n).is_err());
        let zero = env(&[
            ("MUSHROOMDB_BACKUP_S3_URL", "s3://b"),
            ("MUSHROOMDB_BACKUP_WAL_INTERVAL_MS", "0"),
        ]);
        assert!(BackupConfig::from_lookup(zero).is_err());
    }

    #[test]
    fn a_non_aws_scheme_needs_an_endpoint() {
        let c = BackupConfig::new("minio://b/p");
        assert!(build_store_with(&c, env(&[])).is_err());
        assert!(
            build_store_with(&c, env(&[("AWS_ENDPOINT_URL", "http://127.0.0.1:9000")])).is_ok()
        );
        assert!(build_store_with(&BackupConfig::new("s3://b/p"), env(&[])).is_ok());
    }
}
