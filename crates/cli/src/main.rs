//! `mushroomdb` — thin dispatcher over [`cli`] lib functions.

use cli::{
    format_demo, format_stats, format_suggest, maybe_run_demo_if_empty, parse_args, read_stats,
    run_algo, run_asof, run_demo, run_migrate, run_query, run_schema_apply, run_snapshot,
    run_suggest, usage, Command, ServeUi,
};
use core_api::SharedDb;
use std::io::{self, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    match parse_args(&raw) {
        Ok(Command::Help) => {
            print!("{}", usage());
            ExitCode::SUCCESS
        }
        Ok(Command::Serve {
            db_dir,
            addr,
            ui,
            demo_if_empty,
            token,
            snapshot_every,
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
            exit(run_serve(db_dir, addr, ui, token, snapshot_every))
        }
        Ok(Command::Mcp { db_dir }) => exit(run_mcp(db_dir)),
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
        }) => match run_algo(&db_dir, &subcmd, top) {
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
        Ok(Command::Snapshot { db_dir, keep_wal }) => match run_snapshot(&db_dir, keep_wal) {
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

fn run_serve(
    db_dir: PathBuf,
    addr: SocketAddr,
    ui: ServeUi,
    token: Option<String>,
    snapshot_every: Option<Duration>,
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
                    match tokio::task::spawn_blocking(move || db_snap.write().snapshot()).await {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => eprintln!("snapshot-every failed: {e}"),
                        Err(e) => eprintln!("snapshot-every task panicked: {e}"),
                    }
                }
            });
        }
        let db_serve = db.clone();
        let mut serve = tokio::spawn(async move {
            match ui {
                ServeUi::Filesystem(dir) => {
                    server::serve_with_ui(db_serve, addr, tx, dir, token).await
                }
                ServeUi::None => server::serve(db_serve, addr, tx, token).await,
                ServeUi::Embedded => {
                    #[cfg(feature = "embed-ui")]
                    {
                        server::serve_with_embedded_ui(db_serve, addr, tx, token).await
                    }
                    #[cfg(not(feature = "embed-ui"))]
                    {
                        server::serve(db_serve, addr, tx, token).await
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
                db.write().snapshot().map_err(|e| e.to_string())?;
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
