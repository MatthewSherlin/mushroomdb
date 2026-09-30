//! `mushroomdb` — thin dispatcher over [`cli`] lib functions.

use cli::{
    format_backup, format_demo, format_stats, format_suggest, install, maybe_run_demo_if_empty,
    parse_args, read_stats, run_algo, run_asof, run_backup, run_build_index, run_demo, run_export,
    run_migrate, run_query, run_schema_apply, run_schema_apply_memory_defaults, run_snapshot,
    run_suggest, run_verify, usage, Command, ServeUi,
};
use core_api::{GraphError, SharedDb};
use std::collections::HashMap;
use std::io::{self, Read as _, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

/// The default log filter.
///
/// The targets are the **lib names** — `server` and `cli` — not the package
/// names. Filtering on `mushroomdb_server` matches nothing at all, and a filter
/// that matches nothing is indistinguishable from a server with nothing to say;
/// `default_filter_enables_the_crates_that_log` exists because that is exactly
/// the mistake this line shipped with for an afternoon.
const DEFAULT_LOG_FILTER: &str = "mushroomdb=info,server=info,cli=info,warn";

/// Install the log subscriber for a long-running `serve`.
///
/// **Default: `mushroomdb=info,warn`.** A healthy server is quiet — one line
/// when it binds, one when it stops, and then nothing until something is
/// actually wrong. Successful requests log at `debug` and are therefore off;
/// failures, refusals and slow requests are on. This is the difference between
/// a log someone reads and a log that fills a disk.
///
/// `MUSHROOMDB_LOG` overrides it with the usual filter syntax, so
/// `MUSHROOMDB_LOG=mushroomdb_server=debug` gives a line per request when
/// someone is actually debugging, with no rebuild.
///
/// Only `serve` installs this. The one-shot commands print their results to
/// stdout and have nothing to log.
fn init_logging() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_env("MUSHROOMDB_LOG")
        .unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG_FILTER));
    // `try_init` rather than `init`: a second call must not panic the server,
    // and an embedder that already installed a subscriber keeps theirs.
    let _ = fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init();
}

fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    match parse_args(&raw) {
        Ok(Command::Help) => {
            print!("{}", usage());
            ExitCode::SUCCESS
        }
        Ok(Command::Recall { db_dir, auto }) => {
            let mut raw = String::new();
            let _ = io::stdin().read_to_string(&mut raw);
            // `run_recall` returns a digest, never an error: it is silent on
            // every failure by contract. A *panic* is the one way it could
            // still reach the user, and it would do so before every prompt, so
            // it is caught here and the digest is simply empty.
            let digest = silently(|| cli::recall::run_recall(&resolve_db(db_dir, auto), &raw))
                .unwrap_or_default();
            // Not `print!`: that panics on EPIPE (exit 101) if the hook runner
            // closes the pipe. Every write error is swallowed instead.
            let mut stdout = io::stdout();
            let _ = stdout.write_all(digest.as_bytes());
            let _ = stdout.flush();
            ExitCode::SUCCESS // never block the prompt
        }
        Ok(Command::Brief { db_dir, auto }) => {
            // A SessionStart hook is the first thing a session sees. A store
            // that cannot be opened at all — missing, held by a writer, not a
            // store — has no brief to give, and greeting the user with that
            // error is worse than opening in silence: every failure here
            // prints nothing and exits 0. (A store that opens but is *empty*
            // does have something to say, and says it.)
            let brief = silently(|| cli::run_brief(&resolve_db(db_dir, auto)))
                .and_then(Result::ok)
                .unwrap_or_default();
            let mut stdout = io::stdout();
            let _ = stdout.write_all(brief.as_bytes());
            let _ = stdout.flush();
            ExitCode::SUCCESS
        }
        Ok(Command::Why { db_dir, a, b }) => print_or_fail(cli::run_why(&db_dir, &a, &b)),
        Ok(Command::Version) => {
            println!("{}", cli::version_string());
            ExitCode::SUCCESS
        }
        Ok(Command::IngestGit { db_dir, opts }) => {
            match cli::ingest_git::run_ingest_git(&db_dir, &opts) {
                Ok(report) => {
                    print!("{}", cli::ingest_git::format_ingest_git(&report));
                    ExitCode::SUCCESS
                }
                Err(e) => busy_aware(&e),
            }
        }
        Ok(Command::Serve {
            db_dir,
            addr,
            ui,
            demo_if_empty,
            token,
            role_tokens,
            snapshot_every,
            restore_from,
            tls_cert,
            tls_key,
        }) => {
            let token = token.filter(|s| !s.is_empty()).or_else(|| {
                std::env::var("MUSHROOMDB_TOKEN")
                    .ok()
                    .filter(|s| !s.is_empty())
            });
            if !addr.ip().is_loopback() && token.as_ref().map(|s| s.is_empty()).unwrap_or(true) {
                return fail(
                    "non-loopback --addr requires --token or MUSHROOMDB_TOKEN \
                     (see SECURITY.md)",
                );
            }
            // Merge --role-token flags with MUSHROOMDB_ROLE_TOKENS env var.
            // Format: "TOKEN1:ROLE1,TOKEN2:ROLE2". Flag entries win over env on
            // collision (flags are inserted last; HashMap retains last writer).
            let mut all_role_tokens: HashMap<String, String> = HashMap::new();
            if let Ok(env_val) = std::env::var("MUSHROOMDB_ROLE_TOKENS") {
                for pair in env_val.split(',') {
                    let pair = pair.trim();
                    if pair.is_empty() {
                        continue;
                    }
                    if let Some((tok, role)) = pair.split_once(':') {
                        if !tok.is_empty() && !role.is_empty() {
                            all_role_tokens.insert(tok.to_string(), role.to_string());
                        }
                    }
                }
            }
            for (tok, role) in role_tokens {
                all_role_tokens.insert(tok, role);
            }
            // Before the demo, so a restored store is never overwritten by it.
            if let Some(from) = restore_from {
                match cli::restore_if_empty(&db_dir, &from) {
                    Ok(cli::RestoreOutcome::Restored { from, files, bytes }) => println!(
                        "restored from {}: {} files, {} bytes",
                        from.display(),
                        files.len(),
                        bytes
                    ),
                    Ok(cli::RestoreOutcome::AlreadyPresent) => eprintln!(
                        "restore-from: {} already holds a store; not restoring",
                        db_dir.display()
                    ),
                    // A warning, not an error: a first boot with an empty
                    // backup volume must still start.
                    Ok(cli::RestoreOutcome::Empty) => {
                        eprintln!("restore-from: no backup found under {}", from.display())
                    }
                    Err(e) => return fail(&e.to_string()),
                }
            }
            if demo_if_empty {
                match maybe_run_demo_if_empty(&db_dir) {
                    Ok(Some(out)) => print!("{}", format_demo(&db_dir, &out)),
                    Ok(None) => {}
                    Err(e) => return fail(&e.to_string()),
                }
            }
            let ui = match ui {
                ServeUi::Filesystem(dir) => match cli::validate_ui_dir(&dir) {
                    Ok(dir) => ServeUi::Filesystem(dir),
                    Err(e) => return fail(&e),
                },
                other => other,
            };
            exit(run_serve(
                db_dir,
                addr,
                ui,
                token,
                all_role_tokens,
                snapshot_every,
                tls_cert,
                tls_key,
            ))
        }
        Ok(Command::Mcp {
            db_dir,
            auto,
            all_tools,
        }) => exit(run_mcp(resolve_db(db_dir, auto), all_tools)),
        Ok(Command::Stats { db_dir }) => match read_stats(&db_dir) {
            Ok(stats) => {
                print!("{}", format_stats(&stats));
                ExitCode::SUCCESS
            }
            Err(e) => fail(&e.to_string()),
        },
        Ok(Command::Demo { db_dir }) => match run_demo(&db_dir) {
            Ok(out) => {
                print!("{}", format_demo(&db_dir, &out));
                ExitCode::SUCCESS
            }
            Err(e) => fail(&e.to_string()),
        },
        Ok(Command::Suggest { db_dir }) => match run_suggest(&db_dir) {
            Ok(suggestions) => {
                print!("{}", format_suggest(&suggestions));
                ExitCode::SUCCESS
            }
            Err(e) => fail(&e.to_string()),
        },
        Ok(Command::AsOf {
            db_dir,
            commit,
            at,
            query,
            namespace,
        }) => match run_asof(
            &db_dir,
            commit,
            at.as_deref(),
            query.as_deref(),
            namespace.as_deref(),
        ) {
            Ok(out) => {
                print!("{out}");
                ExitCode::SUCCESS
            }
            Err(e) => fail(&e.to_string()),
        },
        Ok(Command::Algo {
            db_dir,
            subcmd,
            top,
            dir,
            edge_types,
            weight_prop,
            min_weight,
        }) => match run_algo(
            &db_dir,
            &subcmd,
            top,
            dir,
            edge_types,
            weight_prop,
            min_weight,
        ) {
            Ok(out) => {
                print!("{out}");
                ExitCode::SUCCESS
            }
            Err(e) => fail(&e.to_string()),
        },
        Ok(Command::Query {
            db_dir,
            cypher,
            role,
            namespace,
        }) => match run_query(&db_dir, &cypher, role.as_deref(), namespace.as_deref()) {
            Ok(out) => {
                print!("{out}");
                ExitCode::SUCCESS
            }
            Err(e) => fail(&e.to_string()),
        },
        Ok(Command::Snapshot {
            db_dir,
            wal,
            retention,
        }) => match run_snapshot(&db_dir, wal, retention) {
            Ok(out) => {
                print!("{out}");
                ExitCode::SUCCESS
            }
            Err(e) => fail(&e.to_string()),
        },
        Ok(Command::BuildIndex { db_dir, rule }) => {
            match run_build_index(&db_dir, rule.as_deref()) {
                Ok(out) => {
                    print!("{out}");
                    ExitCode::SUCCESS
                }
                Err(e) => fail(&e.to_string()),
            }
        }
        Ok(Command::SchemaApply {
            db_dir,
            schema_file,
            memory_defaults,
        }) => {
            let result = if memory_defaults {
                run_schema_apply_memory_defaults(&db_dir)
            } else {
                match schema_file {
                    Some(f) => run_schema_apply(&db_dir, &f),
                    None => Err(cli::CliError(
                        "schema apply requires <schema.json> or --memory-defaults".to_string(),
                    )),
                }
            };
            match result {
                Ok(out) => {
                    print!("{out}");
                    ExitCode::SUCCESS
                }
                Err(e) => fail(&e.to_string()),
            }
        }
        Ok(Command::Migrate { db_dir }) => match run_migrate(&db_dir) {
            Ok(out) => {
                print!("{out}");
                ExitCode::SUCCESS
            }
            Err(e) => fail(&e.to_string()),
        },
        Ok(Command::Verify { db_dir }) => match run_verify(&db_dir) {
            Ok(msg) => {
                println!("{msg}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("error: {}", e);
                ExitCode::from(1)
            }
        },
        Ok(Command::Backup { db_dir, dest }) => match run_backup(&db_dir, &dest) {
            Ok(report) => {
                print!("{}", format_backup(&dest, &report));
                if !report.verified {
                    eprintln!("warning: backup verification failed");
                    return ExitCode::from(1);
                }
                ExitCode::SUCCESS
            }
            Err(e) => fail(&e.to_string()),
        },
        Ok(Command::Export {
            db_dir,
            dest,
            format,
        }) => match run_export(&db_dir, &dest, &format) {
            Ok(out) => {
                print!("{out}");
                ExitCode::SUCCESS
            }
            Err(e) => fail(&e.to_string()),
        },
        Ok(Command::Install(opts)) => {
            let home = home_dir();
            let cwd = std::env::current_dir().unwrap_or_default();
            match install::run_install(&cwd, &home, &opts) {
                Ok(out) => {
                    print!("{out}");
                    ExitCode::SUCCESS
                }
                Err(e) => fail(&e.to_string()),
            }
        }
        Ok(Command::Uninstall(opts)) => {
            let home = home_dir();
            let cwd = std::env::current_dir().unwrap_or_default();
            match install::run_uninstall(&cwd, &home, &opts) {
                Ok(out) => {
                    print!("{out}");
                    ExitCode::SUCCESS
                }
                Err(e) => fail(&e.to_string()),
            }
        }
        Ok(Command::Disable(opts)) => {
            let home = home_dir();
            let cwd = std::env::current_dir().unwrap_or_default();
            match install::run_disable(&cwd, &home, &opts) {
                Ok(out) => {
                    print!("{out}");
                    ExitCode::SUCCESS
                }
                Err(e) => fail(&e.to_string()),
            }
        }
        Ok(Command::Enable(opts)) => {
            let home = home_dir();
            let cwd = std::env::current_dir().unwrap_or_default();
            match install::run_enable(&cwd, &home, &opts) {
                Ok(out) => {
                    print!("{out}");
                    ExitCode::SUCCESS
                }
                Err(e) => fail(&e.to_string()),
            }
        }
        Ok(Command::Doctor(opts)) => {
            let home = home_dir();
            let cwd = std::env::current_dir().unwrap_or_default();
            match cli::doctor::run_doctor(&cwd, &home, &opts) {
                Ok(report) => {
                    print!("{}", report.output);
                    if report.had_fail {
                        ExitCode::from(1)
                    } else {
                        ExitCode::SUCCESS
                    }
                }
                Err(e) => fail(&e.to_string()),
            }
        }
        Err(e) => {
            let _ = writeln!(io::stderr(), "{e}");
            eprint!("{}", usage());
            ExitCode::from(1)
        }
    }
}

fn exit(r: Result<(), String>) -> ExitCode {
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(&e),
    }
}

/// Print a digest, or its error on stderr. The graph tools all answer the same
/// way: a string on success, one line on stderr and exit 1 otherwise.
fn print_or_fail(r: Result<String, cli::CliError>) -> ExitCode {
    match r {
        Ok(out) => {
            print!("{out}");
            ExitCode::SUCCESS
        }
        Err(e) => fail(&e.to_string()),
    }
}

fn fail(msg: &str) -> ExitCode {
    let _ = writeln!(io::stderr(), "{msg}");
    ExitCode::from(1)
}

/// Report a store-writing command's failure, distinguishing "busy" from the
/// rest. Exit 3 is "the store is busy, nothing was written" — a caller (a git
/// hook, a retry loop) can act on that without parsing the message.
fn busy_aware(e: &cli::CliError) -> ExitCode {
    let _ = writeln!(io::stderr(), "error: {e}");
    if e.0 == cli::ingest_git::BUSY_MESSAGE {
        ExitCode::from(3)
    } else {
        ExitCode::FAILURE
    }
}

/// Run `f`, returning `None` if it panicked and printing nothing either way.
///
/// The panic hook is replaced for the duration: an unwind would otherwise print
/// a message and a backtrace to stderr and exit 101, which for a hook body is
/// the noisiest possible outcome and the one a user can do least about. Both
/// hook bodies (`recall` and `brief`) go through this.
fn silently<T>(f: impl FnOnce() -> T) -> Option<T> {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    std::panic::set_hook(previous);
    outcome.ok()
}

/// The database a `<db-dir>`-or-`--auto` command should use.
fn resolve_db(db_dir: Option<PathBuf>, auto: bool) -> PathBuf {
    match db_dir {
        Some(dir) => dir,
        None => {
            debug_assert!(auto, "the parser rejects neither a dir nor --auto");
            cli::resolve_auto_db(
                std::env::var_os("CLAUDE_PROJECT_DIR").as_deref(),
                &std::env::current_dir().unwrap_or_default(),
                &home_dir(),
            )
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_serve(
    db_dir: PathBuf,
    addr: SocketAddr,
    ui: ServeUi,
    token: Option<String>,
    role_tokens: HashMap<String, String>,
    snapshot_every: Option<Duration>,
    tls_cert: Option<PathBuf>,
    tls_key: Option<PathBuf>,
) -> Result<(), String> {
    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(async {
        // Same guard as `run_mcp`: checked before `open` creates the
        // directory's files, so it tells a brand-new store from one this
        // invocation is merely reopening (or one `--demo-if-empty` /
        // `--restore-from` already populated above, in which case this is
        // already `true` and nothing here re-declares anything). Declaring
        // full-text on a populated store rebuilds the index at open (227 ms
        // vs. 3.8 ms with none, ledger row 36), so an existing store is never
        // touched here.
        let is_new_store = !core_api::restore::holds_a_store(&db_dir);
        let db = SharedDb::open(&db_dir).map_err(|e| e.to_string())?;
        if is_new_store {
            // The first store most HTTP callers open. Declare the memory
            // schema before the first write so `recall` can answer on it.
            db.write()
                .apply_schema(&core_api::memory_schema::memory_defaults())
                .map_err(|e| e.to_string())?;
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        if let Some(period) = snapshot_every {
            let db_snap = db.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(period);
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                interval.tick().await; // skip immediate fire
                loop {
                    interval.tick().await;
                    let db_snap = db_snap.clone();
                    // A snapshot replaces wal.bin, so it needs the store's
                    // cross-process write lock. If another process holds it,
                    // skip this tick rather than wait: the next one is only a
                    // period away, and a snapshot is never urgent.
                    let taken =
                        tokio::task::spawn_blocking(move || cli::snapshot_shared(&db_snap)).await;
                    match taken {
                        Ok(Ok(())) => {}
                        Ok(Err(GraphError::Busy { .. })) => {
                            eprintln!(
                                "snapshot-every skipped: another process holds the write lock"
                            );
                        }
                        Ok(Err(e)) => eprintln!("snapshot-every failed: {e}"),
                        Err(e) => eprintln!("snapshot-every task panicked: {e}"),
                    }
                }
            });
        }
        // A quiescent server still finishes a vector-index build it was handed:
        // one slice a second, and the rule's edges appear when it completes.
        // Cheap when nothing is pending — a map lookup under the write lock.
        {
            let db_build = db.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_secs(1));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                interval.tick().await; // skip immediate fire
                let mut last: Vec<(String, u64)> = Vec::new();
                loop {
                    interval.tick().await;
                    // Under the *read* lock first: a store with nothing
                    // building must not pay a write lock once a second, and an
                    // idle server must not force the first-write index scan
                    // just because it has been running for a second. Open
                    // registers a build a snapshot cut short, so a restarted
                    // serve sees it here without a write.
                    if db_build.read().builds_in_progress().is_empty() {
                        continue;
                    }
                    let db_build = db_build.clone();
                    let pumped =
                        tokio::task::spawn_blocking(move || db_build.write().pump_index_build())
                            .await;
                    let now = match pumped {
                        Ok(Ok(v)) => v,
                        Ok(Err(GraphError::Busy { .. })) => continue,
                        Ok(Err(e)) => {
                            eprintln!("build-index failed: {e}");
                            continue;
                        }
                        Err(e) => {
                            eprintln!("build-index task panicked: {e}");
                            continue;
                        }
                    };
                    for (rule, total) in &last {
                        if !now.iter().any(|b| &b.rule == rule) {
                            eprintln!("built {rule}: {total} vectors");
                        }
                    }
                    for b in &now {
                        if last.iter().any(|(r, _)| r == &b.rule) {
                            eprintln!("building {}: {}/{}", b.rule, b.indexed, b.total);
                        }
                    }
                    last = now.into_iter().map(|b| (b.rule, b.total)).collect();
                }
            });
        }
        init_logging();
        let db_serve = db.clone();
        // The graceful-stop channel. Firing it — or dropping the sender — makes
        // the server stop accepting and let in-flight requests and subscribers
        // finish. Before 0.6.12 shutdown was `serve.abort()`, which severed
        // them mid-response.
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let mut serve = tokio::spawn(async move {
            // TLS path: both --tls-cert and --tls-key were supplied.
            if let (Some(cert), Some(key)) = (tls_cert, tls_key) {
                #[cfg(feature = "tls")]
                {
                    return server::serve_tls_with_shutdown(
                        db_serve,
                        addr,
                        tx,
                        cert,
                        key,
                        token,
                        role_tokens,
                        stop_rx,
                    )
                    .await;
                }
                #[cfg(not(feature = "tls"))]
                {
                    let _ = (cert, key, db_serve, addr, tx, token, role_tokens, stop_rx);
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Unsupported,
                        "this binary was built without TLS support; \
                         rebuild with --features tls or terminate TLS at a \
                         reverse proxy (see docs/site/deployment.md)",
                    ));
                }
            }
            match ui {
                ServeUi::Filesystem(dir) => {
                    server::serve_with_ui_and_role_tokens_and_shutdown(
                        db_serve,
                        addr,
                        tx,
                        dir,
                        token,
                        role_tokens,
                        stop_rx,
                    )
                    .await
                }
                ServeUi::None => {
                    server::serve_with_role_tokens_and_shutdown(
                        db_serve,
                        addr,
                        tx,
                        token,
                        role_tokens,
                        stop_rx,
                    )
                    .await
                }
                ServeUi::Embedded => {
                    #[cfg(feature = "embed-ui")]
                    {
                        server::serve_with_embedded_ui_and_shutdown(
                            db_serve,
                            addr,
                            tx,
                            token,
                            role_tokens,
                            stop_rx,
                        )
                        .await
                    }
                    #[cfg(not(feature = "embed-ui"))]
                    {
                        server::serve_with_role_tokens_and_shutdown(
                            db_serve,
                            addr,
                            tx,
                            token,
                            role_tokens,
                            stop_rx,
                        )
                        .await
                    }
                }
            }
        });
        match rx.await {
            Ok(bound) => {
                // stdout stays exactly as it was: the docs, the tests and
                // `install` all read this line.
                println!("listening on http://{bound}");
                tracing::info!(addr = %bound, "listening");
            }
            Err(_) => {
                return match serve.await {
                    Ok(Ok(())) => Err("server exited before readiness".into()),
                    Ok(Err(e)) => Err(e.to_string()),
                    Err(e) => Err(e.to_string()),
                };
            }
        }
        tokio::select! {
            result = &mut serve => match result {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => Err(e.to_string()),
                Err(e) => Err(e.to_string()),
            },
            _ = shutdown_signal() => {
                // Ask the server to stop, then wait for it. In-flight requests
                // and live subscribers finish; `serve.abort()` cut them off.
                tracing::info!("shutdown signal received; draining in-flight requests");
                let _ = stop_tx.send(());
                match (&mut serve).await {
                    Ok(Ok(())) => tracing::info!("drained; stopped"),
                    Ok(Err(e)) => tracing::error!(error = %e, "server exited with an error"),
                    Err(e) => tracing::error!(error = %e, "server task failed"),
                }
                // Same rule as the periodic snapshot: it needs the store's
                // write lock. Shutting down without one is fine — the WAL holds
                // every commit, and the next open replays it.
                match cli::snapshot_shared(&db) {
                    Ok(()) => {}
                    Err(GraphError::Busy { .. }) => {
                        eprintln!(
                            "shutdown snapshot skipped: another process holds the write lock"
                        );
                    }
                    Err(e) => return Err(e.to_string()),
                }
                Ok(())
            }
        }
    })
}

async fn shutdown_signal() {
    let ctrl_c = async {
        match tokio::signal::ctrl_c().await {
            Ok(()) => {}
            // Install failure is not SIGINT; park like SIGTERM handler Err.
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                let _ = sig.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

fn run_mcp(db_dir: PathBuf, all_tools: bool) -> Result<(), String> {
    // `holds_a_store` (the same predicate `restore_if_empty` gates on) must be
    // checked before `open` creates the directory's files — it is what tells
    // a brand-new store from one this invocation is merely reopening. Declaring
    // full-text on a populated store rebuilds the index at open (227 ms vs.
    // 3.8 ms with none, ledger row 36), so an existing store is never touched
    // here; only a store this call is creating gets the memory schema.
    let is_new_store = !core_api::restore::holds_a_store(&db_dir);
    let db = SharedDb::open(&db_dir).map_err(|e| e.to_string())?;
    if is_new_store {
        // The first store most MCP hosts open. Declare the memory schema
        // before the first write so `recall` can answer on it from the start.
        db.write()
            .apply_schema(&core_api::memory_schema::memory_defaults())
            .map_err(|e| e.to_string())?;
    }
    let stdin = io::stdin();
    let stdout = io::stdout();
    // The store's path, not just a handle: the `sync` tool re-runs this binary
    // against the directory, and cannot infer it from an open database.
    server::run_mcp_stdio_with(db, Some(db_dir), all_tools, stdin.lock(), stdout.lock())
        .map_err(|e| e.to_string())
}

fn home_dir() -> PathBuf {
    // Prefer HOME env var; fall back to /tmp for safety (never panic).
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

#[cfg(test)]
mod tests {
    use super::DEFAULT_LOG_FILTER;

    /// Binding: the default filter names the target this crate's server
    /// actually logs under.
    ///
    /// A typo here is silent — the server simply says nothing, which looks
    /// like health. This shipped wrong for an afternoon: the filter named
    /// `mushroomdb_server`, the events are emitted under `server`, and the
    /// result was a 404 that logged at INFO and appeared nowhere.
    #[test]
    fn default_filter_names_the_servers_real_log_target() {
        let root = server::LOG_TARGET_ROOT;
        assert!(
            DEFAULT_LOG_FILTER.contains(&format!("{root}=")),
            "the default filter {DEFAULT_LOG_FILTER:?} does not name the \
             server's log target {root:?}, so nothing it logs would appear"
        );
    }

    /// The filter parses. An unparseable default falls back to silence.
    #[test]
    fn default_filter_parses() {
        use tracing_subscriber::EnvFilter;
        let parsed = EnvFilter::builder().parse(DEFAULT_LOG_FILTER);
        assert!(parsed.is_ok(), "default filter does not parse: {parsed:?}");
    }
}
