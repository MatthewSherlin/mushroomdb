//! `serve` refuses the documentation placeholder token before it touches the
//! store, so a deploy that never replaced `<set-me>` fails closed.
use std::process::Command;

fn run_serve(use_flag: bool) -> (Option<i32>, String, bool) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("mdb-placeholder-{}-{nanos}", std::process::id()));
    let mut command = Command::new(env!("CARGO_BIN_EXE_mushroomdb"));
    command
        .arg("serve")
        .arg(&dir)
        .arg("--addr")
        .arg("0.0.0.0:0")
        .arg("--no-ui");
    if use_flag {
        command.arg("--token").arg("<set-me>");
    } else {
        command.env("MUSHROOMDB_TOKEN", "<set-me>");
    }
    let out = command.output().expect("run mushroomdb serve");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        dir.exists(),
    )
}

#[test]
fn the_placeholder_token_from_the_environment_stops_serve_before_the_store_is_created() {
    let (code, stderr, created) = run_serve(false);
    assert_eq!(code, Some(1), "stderr: {stderr}");
    assert!(stderr.contains("<set-me>"), "stderr: {stderr}");
    assert!(!created, "store directory must not be created");
}

#[test]
fn the_placeholder_token_from_the_flag_stops_serve_before_the_store_is_created() {
    let (code, stderr, created) = run_serve(true);
    assert_eq!(code, Some(1), "stderr: {stderr}");
    assert!(stderr.contains("<set-me>"), "stderr: {stderr}");
    assert!(!created, "store directory must not be created");
}
