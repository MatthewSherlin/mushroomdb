//! `graphdb` — thin dispatcher over [`cli`] lib functions.

use cli::{format_demo, format_stats, parse_args, read_stats, run_demo, usage, Command};
use core_api::SharedDb;
use std::io::{self, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    match parse_args(&raw) {
        Ok(Command::Help) => {
            print!("{}", usage());
            ExitCode::SUCCESS
        }
        Ok(Command::Serve { db_dir, addr }) => exit(run_serve(db_dir, addr)),
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

fn run_serve(db_dir: PathBuf, addr: SocketAddr) -> Result<(), String> {
    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(async {
        let db = SharedDb::open(&db_dir).map_err(|e| e.to_string())?;
        let (tx, rx) = tokio::sync::oneshot::channel();
        let serve = tokio::spawn(async move { server::serve(db, addr, tx).await });
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
        match serve.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(e.to_string()),
            Err(e) => Err(e.to_string()),
        }
    })
}

fn run_mcp(db_dir: PathBuf) -> Result<(), String> {
    let db = SharedDb::open(&db_dir).map_err(|e| e.to_string())?;
    let stdin = io::stdin();
    let stdout = io::stdout();
    server::run_mcp_stdio(db, stdin.lock(), stdout.lock()).map_err(|e| e.to_string())
}
