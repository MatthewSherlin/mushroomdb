//! `mushroomdb` — thin dispatcher over [`cli`] lib functions.

use cli::{
    format_backup, format_demo, format_stats, format_suggest, install, maybe_run_demo_if_empty,
    parse_args, read_stats, run_algo, run_asof, run_backup, run_demo, run_export, run_migrate,
    run_query, run_schema_apply, run_snapshot, run_suggest, run_verify, usage, Command, ServeUi,
};
use core_api::{GraphError, SharedDb};
use std::collections::HashMap;
use std::io::{self, Read as _, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

/// How long a server-initiated snapshot waits for the store's cross-process
/// write lock before giving up.
///
/// Short on purpose. A snapshot is an optimisation — it shortens the next
/// open's replay — so skipping one costs nothing but a longer replay, whereas
/// blocking the shutdown path or piling up timer ticks behind a busy peer
/// costs the operator.
const SNAPSHOT_LOCK_WAIT: Duration = Duration::from_millis(500);

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
            // Not `print!`: that panics on EPIPE (exit 101) if the hook runner
            // closes the pipe. Every write error is swallowed instead.
            let digest = cli::recall::run_recall(&resolve_db(db_dir, auto), &raw);
            let mut stdout = io::stdout();
            let _ = stdout.write_all(digest.as_bytes());
            let _ = stdout.flush();
            ExitCode::SUCCESS // never block the prompt
        }
        Ok(Command::Sync { db_dir }) => match cli::ingest_git::run_sync(&db_dir) {
            Ok(report) => {
                print!("{}", cli::ingest_git::format_sync(&report));
                ExitCode::SUCCESS
            }
            Err(e) => busy_aware(&e),
        },
        Ok(Command::Touch {
            db_dir,
            auto,
            files,
        }) => {
            // Only read stdin when there is nothing on the command line: a hook
            // pipes a payload, a person does not, and blocking a person's
            // terminal on a read that will never end is the worse failure.
            let payload = if files.is_empty() {
                let mut raw = String::new();
                let _ = io::stdin().read_to_string(&mut raw);
                Some(raw)
            } else {
                None
            };
            match cli::ingest_git::run_touch(&resolve_db(db_dir, auto), &files, payload.as_deref())
            {
                Ok(report) => {
                    print!("{}", cli::ingest_git::format_touch(&report));
                    ExitCode::SUCCESS
                }
                Err(e) => busy_aware(&e),
            }
        }
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
        Ok(Command::Mcp { db_dir, auto }) => exit(run_mcp(resolve_db(db_dir, auto))),
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
            query,
        }) => match run_asof(&db_dir, commit, query.as_deref()) {
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
        Ok(Command::Query { db_dir, cypher }) => match run_query(&db_dir, &cypher) {
            Ok(out) => {
                print!("{out}");
                ExitCode::SUCCESS
            }
            Err(e) => fail(&e.to_string()),
        },
        Ok(Command::Snapshot {
            db_dir,
            keep_wal,
            archive_wal,
            retention,
        }) => match run_snapshot(&db_dir, keep_wal, archive_wal, retention) {
            Ok(out) => {
                print!("{out}");
                ExitCode::SUCCESS
            }
            Err(e) => fail(&e.to_string()),
        },
        Ok(Command::SchemaApply {
            db_dir,
            schema_file,
        }) => match run_schema_apply(&db_dir, &schema_file) {
            Ok(out) => {
                print!("{out}");
                ExitCode::SUCCESS
            }
            Err(e) => fail(&e.to_string()),
        },
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
        let db = SharedDb::open(&db_dir).map_err(|e| e.to_string())?;
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
                    let taken = tokio::task::spawn_blocking(move || {
                        db_snap.write_with_wait(SNAPSHOT_LOCK_WAIT)?.snapshot()
                    })
                    .await;
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
        let db_serve = db.clone();
        let mut serve = tokio::spawn(async move {
            // TLS path: both --tls-cert and --tls-key were supplied.
            if let (Some(cert), Some(key)) = (tls_cert, tls_key) {
                #[cfg(feature = "tls")]
                {
                    return server::serve_tls(db_serve, addr, tx, cert, key, token, role_tokens)
                        .await;
                }
                #[cfg(not(feature = "tls"))]
                {
                    let _ = (cert, key, db_serve, addr, tx, token, role_tokens);
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
                    server::serve_with_ui_and_role_tokens(
                        db_serve,
                        addr,
                        tx,
                        dir,
                        token,
                        role_tokens,
                    )
                    .await
                }
                ServeUi::None => {
                    server::serve_with_role_tokens(db_serve, addr, tx, token, role_tokens).await
                }
                ServeUi::Embedded => {
                    #[cfg(feature = "embed-ui")]
                    {
                        server::serve_with_embedded_ui(db_serve, addr, tx, token, role_tokens).await
                    }
                    #[cfg(not(feature = "embed-ui"))]
                    {
                        server::serve_with_role_tokens(db_serve, addr, tx, token, role_tokens).await
                    }
                }
            }
        });
        match rx.await {
            Ok(bound) => println!("listening on http://{bound}"),
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
                serve.abort();
                let _ = serve.await;
                // Same rule as the periodic snapshot: it needs the store's
                // write lock. Shutting down without one is fine — the WAL holds
                // every commit, and the next open replays it.
                match db.write_with_wait(SNAPSHOT_LOCK_WAIT).and_then(|mut g| g.snapshot()) {
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

fn run_mcp(db_dir: PathBuf) -> Result<(), String> {
    let db = SharedDb::open(&db_dir).map_err(|e| e.to_string())?;
    let stdin = io::stdin();
    let stdout = io::stdout();
    server::run_mcp_stdio(db, stdin.lock(), stdout.lock()).map_err(|e| e.to_string())
}

fn home_dir() -> PathBuf {
    // Prefer HOME env var; fall back to /tmp for safety (never panic).
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}
