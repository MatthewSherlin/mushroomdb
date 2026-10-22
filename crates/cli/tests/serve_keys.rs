//! `mushroomdb serve` reads `keys.json` from the store directory (HO-04): the
//! tiers apply over real HTTP, SIGHUP re-reads the file, and a file that does
//! not parse stops the start.
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn tmp(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("mdb-keys-{name}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct Server {
    child: Child,
    addr: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start(dir: &Path) -> Server {
    let child = Command::new(env!("CARGO_BIN_EXE_mushroomdb"))
        .arg("serve")
        .arg(dir)
        .arg("--addr")
        .arg("127.0.0.1:0")
        .arg("--no-ui")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mushroomdb serve");
    // Owned by `Server` from here, so every exit below kills and reaps it.
    let mut server = Server {
        child,
        addr: String::new(),
    };
    let stdout = server.child.stdout.take().expect("piped stdout");
    let mut lines = BufReader::new(stdout).lines();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        assert!(Instant::now() < deadline, "serve never started listening");
        match lines.next() {
            Some(Ok(line)) => {
                if let Some(rest) = line.strip_prefix("listening on http://") {
                    server.addr = rest.split_whitespace().next().unwrap().to_string();
                    return server;
                }
            }
            _ => panic!("serve exited before it started listening"),
        }
    }
}

/// Status code of `GET path` with `Authorization: Bearer token`.
fn status(addr: &str, path: &str, token: &str) -> u16 {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut reply = String::new();
    stream.read_to_string(&mut reply).unwrap();
    reply
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("no status line in {reply:?}"))
}

fn wait_for(addr: &str, path: &str, token: &str, want: u16) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if status(addr, path, token) == want {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// Runs `mushroomdb keys add` and returns the token it printed.
fn keys_add(dir: &Path, name: &str, tier: &str, role: Option<&str>) -> String {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mushroomdb"));
    cmd.arg("keys").arg("add").arg(dir).arg(name);
    cmd.arg("--tier").arg(tier);
    if let Some(role) = role {
        cmd.arg("--role").arg(role);
    }
    let out = cmd.output().expect("run keys add");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let token = String::from_utf8(out.stdout).unwrap().trim().to_string();
    assert_eq!(token.len(), 64, "a 32-byte token in hex");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains(&token),
        "the token is printed once, on stdout"
    );
    token
}

#[cfg(unix)]
fn running_as_root() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
        .unwrap_or(false)
}

#[test]
fn keys_add_prints_a_token_once_and_stores_only_its_hash_in_a_private_file() {
    let dir = tmp("add");
    let first = keys_add(&dir, "ops", "admin", None);
    let second = keys_add(&dir, "app", "write", None);
    let third = keys_add(&dir, "tenant", "read", Some("tenant_a"));
    assert_ne!(first, second);
    let path = dir.join("keys.json");
    let text = std::fs::read_to_string(&path).unwrap();
    for token in [&first, &second, &third] {
        assert!(!text.contains(token.as_str()), "keys.json holds no token");
    }
    assert!(text.contains(&mushroomdb::hash_token(&first).unwrap()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
    // A repeated name is refused and leaves the file as it was.
    let out = Command::new(env!("CARGO_BIN_EXE_mushroomdb"))
        .args(["keys", "add"])
        .arg(&dir)
        .args(["ops", "--tier", "write"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
}

#[test]
fn keys_hash_reads_a_token_from_stdin_and_refuses_a_short_one() {
    let run = |token: &str| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_mushroomdb"))
            .args(["keys", "hash"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(token.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    };
    let token = "t".repeat(40);
    let out = run(&format!("{token}\n"));
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8(out.stdout).unwrap().trim(),
        mushroomdb::hash_token(&token).unwrap()
    );
    // A Windows pipe ends the line with CR LF.
    let out = run(&format!("{token}\r\n"));
    assert_eq!(
        String::from_utf8(out.stdout).unwrap().trim(),
        mushroomdb::hash_token(&token).unwrap()
    );
    let out = run("too-short");
    assert_eq!(out.status.code(), Some(1));
    assert!(!String::from_utf8_lossy(&out.stderr).contains("too-short"));
}

#[test]
fn serve_applies_the_key_tiers_and_reloads_them_on_sighup() {
    let dir = tmp("tiers");
    let admin = keys_add(&dir, "ops", "admin", None);
    let write = keys_add(&dir, "app", "write", None);
    let server = start(&dir);

    assert_eq!(status(&server.addr, "/stats", &admin), 200);
    assert_eq!(status(&server.addr, "/stats", &write), 403);
    assert_eq!(status(&server.addr, "/stats", "unknown"), 401);

    // Issued while the server runs: unknown until SIGHUP.
    let later = keys_add(&dir, "later", "admin", None);
    assert_eq!(status(&server.addr, "/stats", &later), 401);
    let signalled = Command::new("kill")
        .arg("-HUP")
        .arg(server.child.id().to_string())
        .status()
        .unwrap();
    assert!(signalled.success());
    assert!(
        wait_for(&server.addr, "/stats", &later, 200),
        "the new key never started working"
    );
    assert_eq!(status(&server.addr, "/stats", &admin), 200);
    assert_eq!(status(&server.addr, "/stats", &write), 403);
}

#[cfg(unix)]
#[test]
fn serve_refuses_a_keys_file_that_group_or_others_can_read() {
    use std::os::unix::fs::PermissionsExt;
    if running_as_root() {
        return;
    }
    let dir = tmp("loose");
    keys_add(&dir, "ops", "admin", None);
    let path = dir.join("keys.json");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_mushroomdb"))
        .arg("serve")
        .arg(&dir)
        .args(["--addr", "127.0.0.1:0", "--no-ui"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains(&path.display().to_string()), "{stderr}");
    assert!(stderr.contains("0644"), "{stderr}");
    assert!(!dir.join("wal.bin").exists(), "the store was not opened");
}

#[test]
fn a_plaintext_token_in_keys_json_stops_serve_with_the_conversion() {
    let dir = tmp("plaintext");
    let path = dir.join("keys.json");
    std::fs::write(
        &path,
        r#"{"keys":[{"token":"SECRET-TOKEN-0123456789-0123456789-0123","tier":"admin"}]}"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let out = Command::new(env!("CARGO_BIN_EXE_mushroomdb"))
        .arg("serve")
        .arg(&dir)
        .args(["--addr", "127.0.0.1:0", "--no-ui"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("plaintext") && stderr.contains("keys hash"),
        "{stderr}"
    );
    assert!(!stderr.contains("SECRET-TOKEN"), "a token was echoed");
    assert!(!dir.join("wal.bin").exists());
}

#[test]
fn a_token_that_is_both_the_env_token_and_a_key_stops_serve() {
    let dir = tmp("dup");
    let token = keys_add(&dir, "ops", "admin", None);
    let out = Command::new(env!("CARGO_BIN_EXE_mushroomdb"))
        .arg("serve")
        .arg(&dir)
        .args(["--addr", "127.0.0.1:0", "--no-ui", "--token", &token])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("one place"), "{stderr}");
    assert!(!stderr.contains(&token), "a token was echoed");
}

/// Sends SIGHUP to the server.
#[cfg(unix)]
fn hangup(server: &Server) {
    let signalled = Command::new("kill")
        .arg("-HUP")
        .arg(server.child.id().to_string())
        .status()
        .unwrap();
    assert!(signalled.success());
}

/// SIGHUP to a server that started with no keys.json is a no-op: it keeps
/// serving, and answers requests afterwards.
#[cfg(unix)]
#[test]
fn sighup_to_a_server_without_keys_json_leaves_it_serving() {
    let dir = tmp("nokeys");
    let mut server = start(&dir);
    hangup(&server);
    hangup(&server);
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        server.child.try_wait().unwrap().is_none(),
        "SIGHUP ended a server that has no keys.json"
    );
    assert_eq!(status(&server.addr, "/health", "anything"), 200);
    // No key was added, so the server is as open as it was.
    assert_eq!(status(&server.addr, "/stats", "anything"), 200);
}

/// The first key is issued while the server runs; SIGHUP loads it, and from
/// then on the server requires a key.
#[cfg(unix)]
#[test]
fn sighup_loads_a_first_key_added_after_the_server_started() {
    let dir = tmp("firstkey");
    let server = start(&dir);
    assert_eq!(status(&server.addr, "/stats", "anything"), 200);
    let admin = keys_add(&dir, "ops", "admin", None);
    hangup(&server);
    assert!(
        wait_for(&server.addr, "/stats", &admin, 200),
        "the first key never started working"
    );
    assert_eq!(status(&server.addr, "/stats", "anything"), 401);
}

#[test]
fn an_invalid_keys_file_stops_serve_before_the_store_is_opened() {
    let dir = tmp("invalid");
    let path = dir.join("keys.json");
    std::fs::write(
        &path,
        r#"{"keys":[{"hash":"SECRET-TOKEN-1","tier":"superuser"}]}"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let out = Command::new(env!("CARGO_BIN_EXE_mushroomdb"))
        .arg("serve")
        .arg(&dir)
        .arg("--addr")
        .arg("127.0.0.1:0")
        .arg("--no-ui")
        .output()
        .expect("run mushroomdb serve");
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("keys.json"), "{stderr}");
    assert!(!stderr.contains("SECRET-TOKEN-1"), "a token was echoed");
    assert!(
        !dir.join("wal.bin").exists(),
        "the store must not have been opened"
    );
}

/// Starts `serve` on a non-loopback address and says whether it got as far as
/// listening. The wildcard address binds on any host; the server is killed.
fn serves_on_wildcard(dir: &Path) -> (bool, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_mushroomdb"))
        .arg("serve")
        .arg(dir)
        .args(["--addr", "0.0.0.0:0", "--no-ui"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mushroomdb serve");
    let stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let listening = BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
            .any(|line| line.starts_with("listening on"));
        let _ = tx.send(listening);
    });
    let listening = rx
        .recv_timeout(Duration::from_secs(15))
        .unwrap_or_else(|_| panic!("serve neither listened nor exited"));
    let _ = child.kill();
    let _ = child.wait();
    let mut message = String::new();
    let _ = stderr.read_to_string(&mut message);
    (listening, message)
}

const NON_LOOPBACK_MESSAGE: &str =
    "non-loopback --addr requires --token, MUSHROOMDB_TOKEN or an admin key in keys.json";

#[test]
fn an_admin_key_alone_lets_serve_bind_a_non_loopback_address() {
    let dir = tmp("wild-admin");
    keys_add(&dir, "ops", "admin", None);
    let (listening, stderr) = serves_on_wildcard(&dir);
    assert!(listening, "stderr: {stderr}");
}

#[test]
fn read_and_write_keys_alone_do_not_satisfy_the_non_loopback_rule() {
    let dir = tmp("wild-readwrite");
    keys_add(&dir, "app", "write", None);
    keys_add(&dir, "tenant", "read", Some("tenant_a"));
    let (listening, stderr) = serves_on_wildcard(&dir);
    assert!(!listening);
    assert!(stderr.contains(NON_LOOPBACK_MESSAGE), "{stderr}");
}

#[test]
fn nothing_configured_still_refuses_a_non_loopback_address() {
    let dir = tmp("wild-none");
    let (listening, stderr) = serves_on_wildcard(&dir);
    assert!(!listening);
    assert!(stderr.contains(NON_LOOPBACK_MESSAGE), "{stderr}");
}

/// `--help` states the non-loopback rule the refusal applies, admin key
/// included (the generated CLI reference page is built from this text).
#[test]
fn help_names_an_admin_key_as_satisfying_the_non_loopback_rule() {
    let out = Command::new(env!("CARGO_BIN_EXE_mushroomdb"))
        .arg("--help")
        .output()
        .expect("run --help");
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(
        help.contains(
            "Non-loopback --addr requires --token, MUSHROOMDB_TOKEN, or an `admin` key in \
             `<db-dir>/keys.json` (read and write keys alone do not count)."
        ),
        "{help}"
    );
}
