//! `query` and `asof --query` take the statement from `--file` or `--stdin`
//! (ledger row 70): a statement with quotes needs no shell escaping, and the
//! newline a file or pipe ends in is not part of it.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use mushroomdb::GraphDb;

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mdb-query-stdin-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// A store with two `Person` nodes, one of them named with an apostrophe.
fn store(root: &Path) -> PathBuf {
    let db_dir = root.join("db");
    let mut db = GraphDb::open(&db_dir).expect("open");
    let params = BTreeMap::new();
    db.query_write("CREATE (n:Person {id: 'ada'})", &params)
        .expect("create");
    db.query_write("CREATE (n:Person {id: 'bob'})", &params)
        .expect("create");
    drop(db);
    db_dir
}

fn run(args: &[&str], stdin: Option<&str>) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_mushroomdb"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mushroomdb");
    let mut pipe = child.stdin.take().expect("stdin");
    if let Some(text) = stdin {
        pipe.write_all(text.as_bytes()).expect("write stdin");
    }
    drop(pipe);
    child.wait_with_output().expect("exit")
}

fn stdout(out: &Output) -> String {
    assert!(
        out.status.success(),
        "failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn file_reads_the_statement_from_a_file() {
    let root = temp_dir("file");
    let db = store(&root);
    let file = root.join("q.cyp");
    std::fs::write(&file, "MATCH (n:Person) RETURN n.id AS id ORDER BY id").unwrap();
    let out = stdout(&run(
        &[
            "query",
            db.to_str().unwrap(),
            "--file",
            file.to_str().unwrap(),
        ],
        None,
    ));
    assert!(out.contains("ada") && out.contains("bob"), "{out}");
}

#[test]
fn stdin_reads_the_statement_from_piped_input() {
    let root = temp_dir("stdin");
    let db = store(&root);
    let out = stdout(&run(
        &["query", db.to_str().unwrap(), "--stdin"],
        Some("MATCH (n:Person {id: 'ada'}) RETURN n.id AS id"),
    ));
    assert!(out.contains("ada") && !out.contains("bob"), "{out}");
}

#[test]
fn stdin_trailing_newline_is_stripped() {
    let root = temp_dir("newline");
    let db = store(&root);
    let file = root.join("q.cyp");
    let bare = "MATCH (n) RETURN count(n) AS c";
    std::fs::write(&file, format!("{bare}\n")).unwrap();
    let from_file = stdout(&run(
        &[
            "query",
            db.to_str().unwrap(),
            "--file",
            file.to_str().unwrap(),
        ],
        None,
    ));
    let from_stdin = stdout(&run(
        &["query", db.to_str().unwrap(), "--stdin"],
        Some(&format!("{bare}\n\n")),
    ));
    let inline = stdout(&run(&["query", db.to_str().unwrap(), bare], None));
    assert_eq!(from_file, inline);
    assert_eq!(from_stdin, inline);
}

#[test]
fn asof_reads_the_statement_from_a_file_or_stdin() {
    let root = temp_dir("asof");
    let db = store(&root);
    let q = "MATCH (n:Person) RETURN n.id AS id ORDER BY id";
    let file = root.join("q.cyp");
    std::fs::write(&file, format!("{q}\n")).unwrap();
    let inline = stdout(&run(
        &["asof", db.to_str().unwrap(), "--commit", "1", "--query", q],
        None,
    ));
    let from_file = stdout(&run(
        &[
            "asof",
            db.to_str().unwrap(),
            "--commit",
            "1",
            "--file",
            file.to_str().unwrap(),
        ],
        None,
    ));
    let from_stdin = stdout(&run(
        &["asof", db.to_str().unwrap(), "--commit", "1", "--stdin"],
        Some(&format!("{q}\n")),
    ));
    assert!(inline.contains("ada"), "{inline}");
    assert_eq!(from_file, inline);
    assert_eq!(from_stdin, inline);
}

#[test]
fn two_sources_and_empty_input_are_refused() {
    let root = temp_dir("refuse");
    let db = store(&root);
    let both = run(
        &[
            "query",
            db.to_str().unwrap(),
            "--stdin",
            "MATCH (n) RETURN n",
        ],
        Some("MATCH (n) RETURN n"),
    );
    assert!(!both.status.success());
    let empty = run(&["query", db.to_str().unwrap(), "--stdin"], Some("\n"));
    assert!(!empty.status.success());
    assert!(String::from_utf8_lossy(&empty.stderr).contains("empty"));
}

#[test]
fn bom_and_trailing_semicolon_are_not_part_of_the_statement() {
    let root = temp_dir("bom");
    let db = store(&root);
    let file = root.join("q.cyp");
    std::fs::write(
        &file,
        b"\xEF\xBB\xBFMATCH (n:Person) RETURN count(n) AS c;\n",
    )
    .unwrap();
    let from_file = stdout(&run(
        &[
            "query",
            db.to_str().unwrap(),
            "--file",
            file.to_str().unwrap(),
        ],
        None,
    ));
    let from_stdin = stdout(&run(
        &["query", db.to_str().unwrap(), "--stdin"],
        Some("MATCH (n:Person) RETURN count(n) AS c ;  \n"),
    ));
    let inline = stdout(&run(
        &[
            "query",
            db.to_str().unwrap(),
            "MATCH (n:Person) RETURN count(n) AS c;",
        ],
        None,
    ));
    assert!(from_file.contains('2'), "{from_file}");
    assert_eq!(from_stdin, from_file);
    assert_eq!(inline, from_file);
}

/// `asof` without a statement prints only the as-of header, as it always did.
#[test]
fn asof_without_a_statement_prints_only_the_header() {
    let root = temp_dir("asof-none");
    let db = store(&root);
    let out = stdout(&run(&["asof", db.to_str().unwrap(), "--commit", "1"], None));
    assert!(out.starts_with("as-of commit 1 of "), "{out}");
    assert_eq!(out.lines().count(), 1, "{out}");
}
