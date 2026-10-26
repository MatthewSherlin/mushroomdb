//! The five hook bodies 0.6 installed and 0.7 retired, run by a machine that
//! upgraded the binary but never re-ran `install`.
//!
//! 0.6 wrote `touch`, `intercept`, `impact-hook` and `enrich` into the
//! assistant's settings and `sync` into three git hooks. Until `install` or
//! `doctor` prunes them, those hooks keep calling the new binary. An unknown
//! subcommand exits 1 with the whole usage text on stderr, which the host
//! shows as a hook error on every Grep, Edit or commit. 0.7 accepted the five
//! inertly for one release; 0.10 (ledger row 44) removed them, so a stale hook
//! now fails as an unknown command and `mushroomdb doctor` lists it.
use std::io::Write;
use std::process::{Command, Stdio};

const RETIRED: [&str; 5] = ["touch", "intercept", "impact-hook", "enrich", "sync"];

fn tmp(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("mdb-retired-{name}-{}-{nanos}", std::process::id()))
}

/// Run `mushroomdb <args>` with `stdin` piped in, as a hook runner does.
fn run(args: &[&str], stdin: &str) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_mushroomdb"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mushroomdb");
    // A write error is the child having exited before reading: that is the
    // child's behaviour under test, not the harness's, so it is not fatal.
    let _ = child
        .stdin
        .take()
        .expect("stdin")
        .write_all(stdin.as_bytes());
    child.wait_with_output().expect("wait")
}

#[test]
fn each_retired_hook_body_is_an_unknown_command_that_fails() {
    let db = tmp("db");
    let payload = r#"{"session_id":"s","tool_name":"Grep","tool_input":{"pattern":"fn main"}}"#;
    for sub in RETIRED {
        let db_arg = db.to_str().unwrap();
        for (args, stdin) in [
            (vec![sub], ""),
            (vec![sub, "--auto"], payload),
            (vec![sub, db_arg], payload),
        ] {
            let out = run(&args, stdin);
            assert_eq!(out.status.code(), Some(1), "{args:?} must exit 1: {out:?}");
            assert!(
                out.stdout.is_empty(),
                "{args:?} must print nothing on stdout, got {:?}",
                String::from_utf8_lossy(&out.stdout)
            );
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(
                stderr.contains(&format!("unknown command: {sub}")),
                "{args:?} stderr: {stderr}"
            );
        }
    }
    assert!(!db.exists(), "a retired hook body must never open a store");
}
