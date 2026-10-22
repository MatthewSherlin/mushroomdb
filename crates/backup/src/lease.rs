//! The bucket lease: an advisory, remote single-writer lock on a backup
//! prefix.
//!
//! One object, `{prefix}/LEASE`, names the instance that uploads to the
//! prefix:
//!
//! ```json
//! { "instance_id": "…", "acquired_at": "…", "renewed_at": "…", "ttl_s": 30 }
//! ```
//!
//! The holder renews it every `ttl_s / 3` and deletes it on shutdown. Another
//! instance that finds a lease renewed less than its `ttl_s` ago (by the
//! reader's clock) uploads nothing. A lease older than that — its holder died
//! or lost the network — may be taken over.
//!
//! # Advisory, not a distributed mutex
//!
//! This catches the common operator error: two machines configured with one
//! backup bucket. It does not make two writers safe. What it does and does
//! not promise:
//!
//! - **Acquisition and renewal are compare-and-swap where the service allows
//!   it.** Taking the lease is one conditional PUT — `If-None-Match: *` on an
//!   empty key, `If-Match: <etag>` over a stale lease — and so is every
//!   renewal. Two instances that start together get exactly one lease between
//!   them, and a holder whose lease was taken over learns so at its next
//!   renewal ([`LeaseError::Lost`]). Where the service lacks conditional
//!   PUT, each step is a read followed by an unconditional write
//!   ([`Exclusion::BestEffort`]): two instances that start together may both
//!   pass the check, and the later writer wins at the next renewal.
//! - **A holder fences itself.** It may start an upload only until two thirds
//!   of a TTL after its last successful renewal ([`BucketLease::may_write`],
//!   measured on both the monotonic and the wall clock), and the worker
//!   cancels any upload still in flight at that moment. Nobody else may take
//!   the lease before a full TTL has passed, so a holder that cannot renew —
//!   partitioned from the bucket, or stalled — stops writing a third of a TTL
//!   before its lease can be taken, and writes nothing after a takeover, the
//!   partition healed or not.
//! - **Clock skew** up to that third of a TTL (10 s at the default 30 s) is
//!   absorbed: a contender whose clock runs ahead of the holder's sees the
//!   lease expire that much early, still after the holder stopped. A lease
//!   stamped in the future by a holder whose clock runs ahead counts as fresh;
//!   such a holder that dies delays a takeover by its skew.
//! - **What no lease can stop.** S3 has no fencing token for the other
//!   objects, so an upload the client has already sent completes on the
//!   service even after the client gives up on it, and a process paused
//!   between its fence check and its request (a VM frozen mid-call) writes
//!   when it wakes. Every backup object is immutable or names a restore point
//!   that verifies on its own, so the cost of such a late write is a
//!   superseded `LATEST` or an orphaned object, not a corrupt restore point.
//!
//! # Which services give real exclusion
//!
//! | Service | Acquire | Renew | Exclusion |
//! |---|---|---|---|
//! | AWS S3 | `If-None-Match` / `If-Match` | `If-Match` | conditional |
//! | Cloudflare R2 | `If-None-Match` / `If-Match` | `If-Match` | conditional |
//! | MinIO | `If-None-Match` / `If-Match` | `If-Match` | conditional |
//! | A service that ignores the headers | detected at acquire | read, then write | best-effort |
//! | `MUSHROOMDB_BACKUP_LEASE_CONDITIONAL=off` | read, then write | read, then write | best-effort |
//!
//! A service that accepts a conditional PUT but ignores its precondition is
//! caught by a probe at acquire: the lease is written a second time with
//! `If-None-Match: *`, which must be refused. The lease reports which kind it
//! is through [`BucketLease::exclusion`], and the worker logs it.

use crate::manifest::object_key;
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, PutMode, PutPayload, PutResult, UpdateVersion};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio::time::Instant;

/// How many times [`BucketLease::acquire`] looks again after losing a race
/// for the key before it reports the winner.
const ACQUIRE_RACES: usize = 3;

/// `{prefix}/LEASE`.
pub fn lease_key(prefix: &str) -> String {
    object_key(prefix, "LEASE")
}

/// The `LEASE` object's contents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseRecord {
    /// 32 hex digits, random per process start.
    pub instance_id: String,
    /// RFC 3339, the holder's clock, when it took the lease.
    pub acquired_at: String,
    /// RFC 3339, the holder's clock, when it last renewed.
    pub renewed_at: String,
    /// How long after `renewed_at` the lease stays the holder's. The holder's
    /// value governs, whatever a contender's own TTL is.
    pub ttl_s: u64,
}

impl LeaseRecord {
    /// Whether the lease is still its holder's at `now`. A `renewed_at` in
    /// the future (the holder's clock runs ahead) is fresh; one that does not
    /// parse is not.
    pub fn is_fresh(&self, now: OffsetDateTime) -> bool {
        let Ok(renewed) = OffsetDateTime::parse(&self.renewed_at, &Rfc3339) else {
            return false;
        };
        let ttl = time::Duration::seconds(i64::try_from(self.ttl_s).unwrap_or(i64::MAX));
        now - renewed < ttl
    }
}

/// How strongly the lease excludes a second writer on this service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exclusion {
    /// Every acquire and renewal is one compare-and-swap on the service.
    Conditional,
    /// The service has no usable conditional PUT: each step reads, then
    /// writes, and two writers can overlap until the next renewal.
    BestEffort,
}

/// Why a lease could not be taken, renewed or released.
#[derive(Debug)]
pub enum LeaseError {
    /// Another instance holds a fresh lease on the prefix.
    ForeignActive { instance_id: String },
    /// This lease is no longer ours: the object was replaced or deleted.
    /// `instance_id` names whoever holds it now, when anyone does.
    Lost { instance_id: Option<String> },
    /// The object store refused or failed a request.
    Store(object_store::Error),
}

impl std::fmt::Display for LeaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LeaseError::ForeignActive { instance_id } => write!(
                f,
                "the backup bucket's lease is held by another instance ({instance_id})"
            ),
            LeaseError::Lost {
                instance_id: Some(id),
            } => write!(f, "the bucket lease was taken over by {id}"),
            LeaseError::Lost { instance_id: None } => write!(f, "the bucket lease was lost"),
            LeaseError::Store(e) => {
                write!(f, "object store: {}", crate::redact(&e.to_string()))
            }
        }
    }
}

impl std::error::Error for LeaseError {}

impl From<object_store::Error> for LeaseError {
    fn from(e: object_store::Error) -> Self {
        LeaseError::Store(e)
    }
}

#[derive(Debug)]
struct Held {
    /// The version our last write produced, for the next compare-and-swap.
    version: UpdateVersion,
    /// Taken just before the last successful write was sent: the lease is
    /// ours for `ttl` from here, on each clock.
    confirmed: Instant,
    confirmed_wall: SystemTime,
    exclusion: Exclusion,
    lost: bool,
}

/// A held lease on `{prefix}/LEASE`. See the [module docs](self).
#[derive(Debug)]
pub struct BucketLease {
    store: Arc<dyn ObjectStore>,
    key: ObjectPath,
    instance_id: String,
    ttl_s: u64,
    acquired_at: String,
    held: Mutex<Held>,
}

impl BucketLease {
    /// Take the lease on `prefix` for `ttl_s` seconds (0 counts as 1).
    ///
    /// # Errors
    ///
    /// [`LeaseError::ForeignActive`] when another instance holds a fresh
    /// lease — or won the race for it against this call;
    /// [`LeaseError::Store`] when the service fails a request.
    pub async fn acquire(
        store: Arc<dyn ObjectStore>,
        prefix: &str,
        ttl_s: u64,
    ) -> Result<BucketLease, LeaseError> {
        let ttl_s = ttl_s.max(1);
        let key = ObjectPath::from(lease_key(prefix));
        let instance_id: String = rand::random::<[u8; 16]>()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        for _ in 0..ACQUIRE_RACES {
            let current = read(&store, &key).await?;
            if let Some((Some(rec), _)) = &current {
                if rec.instance_id != instance_id && rec.is_fresh(OffsetDateTime::now_utc()) {
                    return Err(LeaseError::ForeignActive {
                        instance_id: rec.instance_id.clone(),
                    });
                }
            }
            let (confirmed, confirmed_wall) = (Instant::now(), SystemTime::now());
            let stamp = rfc3339(confirmed_wall);
            let record = LeaseRecord {
                instance_id: instance_id.clone(),
                acquired_at: stamp.clone(),
                renewed_at: stamp.clone(),
                ttl_s,
            };
            let mode = match &current {
                None => PutMode::Create,
                Some((_, v)) => PutMode::Update(v.clone()),
            };
            let (result, exclusion) = match put(&store, &key, &record, mode).await {
                Ok(r) => (r, Exclusion::Conditional),
                Err(object_store::Error::AlreadyExists { .. })
                | Err(object_store::Error::Precondition { .. }) => continue,
                Err(object_store::Error::NotImplemented) => (
                    put(&store, &key, &record, PutMode::Overwrite).await?,
                    Exclusion::BestEffort,
                ),
                Err(e) => return Err(e.into()),
            };
            let lease = BucketLease {
                store,
                key,
                instance_id,
                ttl_s,
                acquired_at: stamp,
                held: Mutex::new(Held {
                    version: version_of(result),
                    confirmed,
                    confirmed_wall,
                    exclusion,
                    lost: false,
                }),
            };
            if exclusion == Exclusion::Conditional {
                lease.probe(&record).await;
            }
            return Ok(lease);
        }
        // Lost every race: whoever won holds it now.
        let holder = read(&store, &key)
            .await?
            .and_then(|(r, _)| r)
            .map_or_else(|| "unknown".to_string(), |r| r.instance_id);
        Err(LeaseError::ForeignActive {
            instance_id: holder,
        })
    }

    /// Check that the service refuses a create over an existing object. One
    /// that accepts it ignores preconditions: the lease is best-effort there.
    async fn probe(&self, record: &LeaseRecord) {
        match put(&self.store, &self.key, record, PutMode::Create).await {
            Err(object_store::Error::AlreadyExists { .. })
            | Err(object_store::Error::Precondition { .. }) => {}
            Ok(r) => {
                let mut h = self.lock();
                h.version = version_of(r);
                h.exclusion = Exclusion::BestEffort;
            }
            // Unknown: claim nothing stronger than best effort.
            Err(_) => self.lock().exclusion = Exclusion::BestEffort,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Held> {
        self.held.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// This process's instance id, as the `LEASE` object names it.
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    /// The TTL this lease was taken with, in seconds.
    pub fn ttl_s(&self) -> u64 {
        self.ttl_s
    }

    /// How often to [`renew`](Self::renew): a third of the TTL.
    pub fn renew_interval(&self) -> Duration {
        Duration::from_millis(self.ttl_s * 1000 / 3)
    }

    /// How strongly this lease excludes a second writer on this service.
    pub fn exclusion(&self) -> Exclusion {
        self.lock().exclusion
    }

    /// Whether the lease was found replaced or deleted. A lost lease never
    /// comes back; take a new one.
    pub fn is_lost(&self) -> bool {
        self.lock().lost
    }

    /// The moment, on Tokio's clock, after which this holder must not write:
    /// two thirds of a TTL after the last successful renewal. `None` once the
    /// lease is lost or that moment has passed on either clock.
    pub fn write_deadline(&self) -> Option<Instant> {
        let h = self.lock();
        let fence = Duration::from_millis(self.ttl_s * 2000 / 3);
        let wall_ok = match SystemTime::now().duration_since(h.confirmed_wall) {
            Ok(elapsed) => elapsed < fence,
            // The wall clock stepped back past the renewal: the monotonic
            // clock alone decides.
            Err(_) => true,
        };
        let deadline = h.confirmed + fence;
        (!h.lost && wall_ok && Instant::now() < deadline).then_some(deadline)
    }

    /// Whether this holder may start a write now. See
    /// [`write_deadline`](Self::write_deadline).
    pub fn may_write(&self) -> bool {
        self.write_deadline().is_some()
    }

    /// Extend the lease: rewrite it with a new `renewed_at`, provided it is
    /// still exactly ours.
    ///
    /// # Errors
    ///
    /// [`LeaseError::Lost`] when the object was replaced or deleted since our
    /// last write — the lease is then lost for good; [`LeaseError::Store`]
    /// when the service fails a request (retry; the fence keeps the holder
    /// safe meanwhile).
    pub async fn renew(&self) -> Result<(), LeaseError> {
        let (exclusion, version) = {
            let h = self.lock();
            if h.lost {
                return Err(LeaseError::Lost { instance_id: None });
            }
            (h.exclusion, h.version.clone())
        };
        let (confirmed, confirmed_wall) = (Instant::now(), SystemTime::now());
        let record = LeaseRecord {
            instance_id: self.instance_id.clone(),
            acquired_at: self.acquired_at.clone(),
            renewed_at: rfc3339(confirmed_wall),
            ttl_s: self.ttl_s,
        };
        let written = match exclusion {
            Exclusion::Conditional => {
                match put(&self.store, &self.key, &record, PutMode::Update(version)).await {
                    Err(object_store::Error::NotImplemented) => {
                        self.lock().exclusion = Exclusion::BestEffort;
                        self.renew_best_effort(&record).await
                    }
                    Err(object_store::Error::Precondition { .. })
                    | Err(object_store::Error::NotFound { .. }) => Err(self.lose().await),
                    other => other.map_err(LeaseError::from),
                }
            }
            Exclusion::BestEffort => self.renew_best_effort(&record).await,
        }?;
        let mut h = self.lock();
        h.version = version_of(written);
        h.confirmed = confirmed;
        h.confirmed_wall = confirmed_wall;
        Ok(())
    }

    /// Read the lease and rewrite it only if it still names us.
    async fn renew_best_effort(&self, record: &LeaseRecord) -> Result<PutResult, LeaseError> {
        match read(&self.store, &self.key).await? {
            Some((Some(r), _)) if r.instance_id == self.instance_id => {
                Ok(put(&self.store, &self.key, record, PutMode::Overwrite).await?)
            }
            _ => Err(self.lose().await),
        }
    }

    /// Mark the lease lost, and say who holds it now.
    async fn lose(&self) -> LeaseError {
        self.lock().lost = true;
        let holder = read(&self.store, &self.key)
            .await
            .ok()
            .flatten()
            .and_then(|(r, _)| r)
            .map(|r| r.instance_id)
            .filter(|id| *id != self.instance_id);
        LeaseError::Lost {
            instance_id: holder,
        }
    }

    /// Give the lease up: DELETE `{prefix}/LEASE` if it still names us. A
    /// lost lease, or one another instance now holds, is left alone.
    ///
    /// The check and the delete are two requests (object_store 0.11 has no
    /// conditional DELETE): a takeover landing between them would be deleted,
    /// and its holder would rewrite it at its next renewal.
    ///
    /// # Errors
    ///
    /// [`LeaseError::Store`] when the service fails a request; the lease then
    /// expires by itself after its TTL.
    pub async fn release(self) -> Result<(), LeaseError> {
        if self.is_lost() {
            return Ok(());
        }
        match read(&self.store, &self.key).await? {
            Some((Some(r), _)) if r.instance_id == self.instance_id => {
                match self.store.delete(&self.key).await {
                    Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
                    Err(e) => Err(e.into()),
                }
            }
            _ => Ok(()),
        }
    }
}

/// The lease object, parsed when it parses, with the version to swap it by.
async fn read(
    store: &Arc<dyn ObjectStore>,
    key: &ObjectPath,
) -> Result<Option<(Option<LeaseRecord>, UpdateVersion)>, LeaseError> {
    let got = match store.get(key).await {
        Ok(g) => g,
        Err(object_store::Error::NotFound { .. }) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let version = UpdateVersion {
        e_tag: got.meta.e_tag.clone(),
        version: got.meta.version.clone(),
    };
    let bytes = got.bytes().await?;
    Ok(Some((serde_json::from_slice(&bytes).ok(), version)))
}

async fn put(
    store: &Arc<dyn ObjectStore>,
    key: &ObjectPath,
    record: &LeaseRecord,
    mode: PutMode,
) -> Result<PutResult, object_store::Error> {
    let body = serde_json::to_vec(record).expect("a lease record always serialises");
    store
        .put_opts(key, PutPayload::from(body), mode.into())
        .await
}

fn version_of(r: PutResult) -> UpdateVersion {
    UpdateVersion {
        e_tag: r.e_tag,
        version: r.version,
    }
}

fn rfc3339(t: SystemTime) -> String {
    OffsetDateTime::from(t).format(&Rfc3339).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(renewed: OffsetDateTime, ttl_s: u64) -> LeaseRecord {
        let s = renewed.format(&Rfc3339).unwrap();
        LeaseRecord {
            instance_id: "x".into(),
            acquired_at: s.clone(),
            renewed_at: s,
            ttl_s,
        }
    }

    #[test]
    fn freshness_is_strictly_less_than_the_ttl() {
        let now = OffsetDateTime::now_utc();
        assert!(record(now - time::Duration::seconds(29), 30).is_fresh(now));
        assert!(!record(now - time::Duration::seconds(30), 30).is_fresh(now));
        assert!(record(now + time::Duration::seconds(5), 30).is_fresh(now));
        let mut garbage = record(now, 30);
        garbage.renewed_at = "yesterday".into();
        assert!(!garbage.is_fresh(now));
        assert!(record(now, u64::MAX).is_fresh(now));
    }

    #[test]
    fn the_key_sits_at_the_prefix_root() {
        assert_eq!(lease_key("a/b"), "a/b/LEASE");
        assert_eq!(lease_key(""), "LEASE");
    }
}
