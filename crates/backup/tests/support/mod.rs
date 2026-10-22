//! A fault-injecting object store for the backup tests.
//!
//! [`Faulty`] wraps a real store and, while its switches say so, refuses every
//! request (a partition), hangs uploads other than the lease, or ignores the
//! preconditions of a conditional PUT (a service that does not honor
//! `If-None-Match` / `If-Match`). It counts the requests that would change
//! objects other than `LEASE`, so a test can prove a fenced writer wrote
//! nothing.
#![allow(dead_code)]

use futures_util::stream::{self, BoxStream, StreamExt};
use object_store::path::Path;
use object_store::{
    GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore, PutMode,
    PutMultipartOpts, PutOptions, PutPayload, PutResult, Result,
};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

type Fut<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

#[derive(Debug)]
pub struct Faulty {
    pub inner: Arc<dyn ObjectStore>,
    /// Every request fails at once, as if the network were gone.
    pub down: AtomicBool,
    /// Every request for `LEASE` fails; the rest go through.
    pub lease_down: AtomicBool,
    /// Every upload of an object other than `LEASE` hangs.
    pub hang_data_puts: AtomicBool,
    /// Conditional PUTs are performed as plain overwrites.
    pub ignore_preconditions: AtomicBool,
    /// `If-Match` PUTs are performed as plain overwrites; `If-None-Match`
    /// is honored.
    pub ignore_if_match: AtomicBool,
    /// A PUT of `LEASE` is applied, then reported as failed: the answer was
    /// lost on the way back.
    pub lose_lease_acks: AtomicBool,
    /// PUTs, multipart uploads and DELETEs of objects other than `LEASE`
    /// that reached the store.
    pub data_writes: AtomicU64,
}

impl Faulty {
    pub fn new(inner: Arc<dyn ObjectStore>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            down: AtomicBool::new(false),
            lease_down: AtomicBool::new(false),
            hang_data_puts: AtomicBool::new(false),
            ignore_preconditions: AtomicBool::new(false),
            ignore_if_match: AtomicBool::new(false),
            lose_lease_acks: AtomicBool::new(false),
            data_writes: AtomicU64::new(0),
        })
    }

    pub fn set_down(&self, down: bool) {
        self.down.store(down, Ordering::SeqCst);
    }

    pub fn data_writes(&self) -> u64 {
        self.data_writes.load(Ordering::SeqCst)
    }

    fn refuse_at(&self, location: &Path) -> Result<()> {
        if is_lease(location) && self.lease_down.load(Ordering::SeqCst) {
            return Err(object_store::Error::Generic {
                store: "Faulty",
                source: "the lease object is unreachable".into(),
            });
        }
        self.refuse()
    }

    fn refuse(&self) -> Result<()> {
        if self.down.load(Ordering::SeqCst) {
            return Err(object_store::Error::Generic {
                store: "Faulty",
                source: "the network is partitioned".into(),
            });
        }
        Ok(())
    }

    /// Count a write of a data object; hang it when asked to.
    async fn data_write(&self, location: &Path) {
        if is_lease(location) {
            return;
        }
        self.data_writes.fetch_add(1, Ordering::SeqCst);
        if self.hang_data_puts.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_secs(365 * 24 * 3600)).await;
        }
    }
}

fn is_lease(location: &Path) -> bool {
    location.filename() == Some("LEASE")
}

impl std::fmt::Display for Faulty {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Faulty({})", self.inner)
    }
}

impl ObjectStore for Faulty {
    fn put_opts<'a, 'b, 'c>(
        &'a self,
        location: &'b Path,
        payload: PutPayload,
        opts: PutOptions,
    ) -> Fut<'c, PutResult>
    where
        'a: 'c,
        'b: 'c,
        Self: 'c,
    {
        Box::pin(async move {
            self.refuse_at(location)?;
            self.data_write(location).await;
            let ignore = self.ignore_preconditions.load(Ordering::SeqCst)
                || (matches!(opts.mode, PutMode::Update(_))
                    && self.ignore_if_match.load(Ordering::SeqCst));
            let opts = if ignore {
                PutOptions {
                    mode: PutMode::Overwrite,
                    ..opts
                }
            } else {
                opts
            };
            let r = self.inner.put_opts(location, payload, opts).await;
            if r.is_ok() && is_lease(location) && self.lose_lease_acks.load(Ordering::SeqCst) {
                return Err(object_store::Error::Generic {
                    store: "Faulty",
                    source: "the answer was lost".into(),
                });
            }
            r
        })
    }

    fn put_multipart_opts<'a, 'b, 'c>(
        &'a self,
        location: &'b Path,
        opts: PutMultipartOpts,
    ) -> Fut<'c, Box<dyn MultipartUpload>>
    where
        'a: 'c,
        'b: 'c,
        Self: 'c,
    {
        Box::pin(async move {
            self.refuse()?;
            self.data_write(location).await;
            self.inner.put_multipart_opts(location, opts).await
        })
    }

    fn get_opts<'a, 'b, 'c>(&'a self, location: &'b Path, options: GetOptions) -> Fut<'c, GetResult>
    where
        'a: 'c,
        'b: 'c,
        Self: 'c,
    {
        Box::pin(async move {
            self.refuse_at(location)?;
            self.inner.get_opts(location, options).await
        })
    }

    fn delete<'a, 'b, 'c>(&'a self, location: &'b Path) -> Fut<'c, ()>
    where
        'a: 'c,
        'b: 'c,
        Self: 'c,
    {
        Box::pin(async move {
            self.refuse_at(location)?;
            if !is_lease(location) {
                self.data_writes.fetch_add(1, Ordering::SeqCst);
            }
            self.inner.delete(location).await
        })
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'_, Result<ObjectMeta>> {
        if let Err(e) = self.refuse() {
            return stream::once(async move { Err(e) }).boxed();
        }
        self.inner.list(prefix)
    }

    fn list_with_delimiter<'a, 'b, 'c>(&'a self, prefix: Option<&'b Path>) -> Fut<'c, ListResult>
    where
        'a: 'c,
        'b: 'c,
        Self: 'c,
    {
        Box::pin(async move {
            self.refuse()?;
            self.inner.list_with_delimiter(prefix).await
        })
    }

    fn copy<'a, 'b, 'c, 'd>(&'a self, from: &'b Path, to: &'c Path) -> Fut<'d, ()>
    where
        'a: 'd,
        'b: 'd,
        'c: 'd,
        Self: 'd,
    {
        Box::pin(async move {
            self.refuse()?;
            self.inner.copy(from, to).await
        })
    }

    fn copy_if_not_exists<'a, 'b, 'c, 'd>(&'a self, from: &'b Path, to: &'c Path) -> Fut<'d, ()>
    where
        'a: 'd,
        'b: 'd,
        'c: 'd,
        Self: 'd,
    {
        Box::pin(async move {
            self.refuse()?;
            self.inner.copy_if_not_exists(from, to).await
        })
    }
}
