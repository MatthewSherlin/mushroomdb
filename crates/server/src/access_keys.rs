//! Reloading `keys.json` on SIGHUP.

use mushroomdb::AccessKeys;

/// Re-reads `keys` from its file whenever the process gets SIGHUP.
///
/// The handler is registered before this returns, so a signal sent right
/// afterwards is never the default action (which would end the process). Needs
/// a Tokio runtime; without one it logs and does nothing. A reload that fails
/// leaves the keys in force and logs why (never a token). On non-Unix targets
/// this does nothing.
pub fn watch_sighup(keys: &AccessKeys) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        if tokio::runtime::Handle::try_current().is_err() {
            tracing::error!("keys.json: no runtime, SIGHUP reload is off");
            return;
        }
        let mut hangups = match signal(SignalKind::hangup()) {
            Ok(hangups) => hangups,
            Err(e) => {
                tracing::error!("keys.json: cannot listen for SIGHUP: {e}");
                return;
            }
        };
        let keys = keys.clone();
        tokio::spawn(async move {
            while hangups.recv().await.is_some() {
                // A file read: keep it off the async workers.
                let keys = keys.clone();
                match tokio::task::spawn_blocking(move || keys.reload()).await {
                    Ok(Ok(count)) => tracing::info!("keys.json reloaded: {count} key(s)"),
                    Ok(Err(e)) => {
                        tracing::error!("keys.json reload failed, keeping the old keys: {e}")
                    }
                    Err(_) => tracing::error!("keys.json reload task panicked"),
                }
            }
        });
    }
    #[cfg(not(unix))]
    {
        let _ = keys;
    }
}
