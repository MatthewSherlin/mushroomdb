//! End-to-end coverage: real-socket HTTP, `/watch`, MCP on the same
//! [`SharedDb`], plus a concurrency hammer.
//!
//! Demo fixture choice: rebuilt here via public core-api (`ingest_json` +
//! `create_rule`). Upstream source is `crates/cli/src/lib.rs` (`org_json` /
//! `project_json` / `person_json` / `skill_fit`) — keep this copy in lockstep
//! or the row pins below will drift. `cli → server` already exists; a
//! `server` dev-dep on `cli` would be a crate cycle. Nothing is moved.
#![allow(deprecated)] // serve() used for test convenience; production code uses serve_with_role_tokens

use arrow_array::{Array, BooleanArray, Float64Array, Int64Array, StringArray};
use arrow_ipc::reader::StreamReader;
use axum::body::{to_bytes, Body};
use axum::http::Request;
use core_api::{IngestOptions, MutationEvent, Predicate, RuleDef, SharedDb};
use futures_util::StreamExt;
use serde_json::{json, Number, Value as Json};
use server::{router, run_mcp_stdio, serve};
use std::io::{BufRead, BufReader, Cursor, Write};
use std::net::SocketAddr;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio_tungstenite::tungstenite::Message;
use tower::ServiceExt;

const N_ORGS: usize = 10;
const N_PROJECTS: usize = 20;
const N_PEOPLE: usize = 30;

/// Same sample query the CLI demo prints.
const SAMPLE_QUERY: &str = "\
MATCH (p:Person {id: 'person-01'})-[r:FIT]->(proj:Project)
RETURN p, proj, r.score AS score
ORDER BY score DESC, proj";

fn tmp(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let d = std::env::temp_dir().join(format!("graphdb-e2e-{name}-{}-{nanos}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn wrap_proj(i: usize) -> usize {
    (i - 1) % N_PROJECTS + 1
}

fn skill_window_json(start: usize, len: usize) -> String {
    let parts: Vec<String> = (0..len)
        .map(|k| format!(r#""s{:02}""#, wrap_proj(start + k)))
        .collect();
    format!("[{}]", parts.join(","))
}

fn json_array(rows: impl IntoIterator<Item = String>) -> String {
    let mut out = String::from("[");
    let mut first = true;
    for row in rows {
        if !first {
            out.push(',');
        }
        first = false;
        out.push_str(&row);
    }
    out.push(']');
    out
}

fn org_json() -> String {
    json_array((1..=N_ORGS).map(|i| {
        format!(
            r#"{{"id":"org-{i:02}","name":"Org {i}","skills":{}}}"#,
            skill_window_json(i, 3)
        )
    }))
}

fn project_json() -> String {
    json_array((1..=N_PROJECTS).map(|i| {
        let org = (i - 1) % N_ORGS + 1;
        format!(
            r#"{{"id":"proj-{i:02}","name":"Project {i}","org_id":"org-{org:02}","skills":{}}}"#,
            skill_window_json(i, 3)
        )
    }))
}

fn person_json() -> String {
    json_array((1..=N_PEOPLE).map(|i| {
        let org = (i - 1) % N_ORGS + 1;
        let proj = (i - 1) % N_PROJECTS + 1;
        format!(
            r#"{{"id":"person-{i:02}","name":"Person {i}","org_id":"org-{org:02}","project_id":"proj-{proj:02}","skills":{}}}"#,
            skill_window_json(proj, 3)
        )
    }))
}

/// Demo-equivalent of `cli::run_demo` (`crates/cli/src/lib.rs` is upstream).
fn load_demo_equivalent(dir: &Path) -> SharedDb {
    let db = SharedDb::open(dir).expect("open demo dir");
    let opts = IngestOptions::default();
    {
        let mut w = db.write();
        for (label, payload) in [
            ("Org", org_json()),
            ("Project", project_json()),
            ("Person", person_json()),
        ] {
            let report = w
                .ingest_json(label, &payload, &opts)
                .unwrap_or_else(|e| panic!("ingest {label}: {e}"));
            assert!(
                report.row_errors.is_empty(),
                "ingest {label} row errors: {:?}",
                report.row_errors
            );
        }
        w.create_rule(RuleDef {
            name: "skill_fit".into(),
            src_label: "Person".into(),
            dst_label: "Project".into(),
            predicate: Predicate::Overlap {
                field: "skills".into(),
                min: 0.5,
            },
            edge_type: "FIT".into(),
            weight_prop: Some("score".into()),
            max_edges: None,
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
        })
        .expect("skill_fit");
    }
    let stats = db.read().stats();
    assert_eq!(stats.nodes_live, 60, "10 orgs + 20 projects + 30 people");
    assert_eq!(stats.nodes_tombstoned, 0);
    assert_eq!(stats.edges, 170, "80 auto-FK + 90 FIT");
    assert_eq!(stats.rules.len(), 4, "3 auto-FK + skill_fit");
    db
}

async fn spawn_server(db: SharedDb) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let handle = tokio::spawn(async move {
        serve(db, "127.0.0.1:0".parse().unwrap(), tx, None)
            .await
            .expect("serve");
    });
    let addr = rx.await.expect("readiness oneshot");
    assert_ne!(addr.port(), 0, "ephemeral port must be resolved");
    (addr, handle)
}

/// Connect after bind-before-accept: yield (no timed sleep) until the
/// listener is accepting or the deadline expires.
async fn connect_retry(addr: SocketAddr) -> tokio::net::TcpStream {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match tokio::net::TcpStream::connect(addr).await {
            Ok(s) => return s,
            Err(e) => {
                if Instant::now() >= deadline {
                    panic!("connect {addr}: {e}");
                }
                tokio::task::yield_now().await;
            }
        }
    }
}

fn decode_chunked(mut data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let nl = data
            .windows(2)
            .position(|w| w == b"\r\n")
            .expect("chunk size CRLF");
        let size_line = std::str::from_utf8(&data[..nl]).expect("chunk size utf-8");
        let size_hex = size_line.split(';').next().unwrap().trim();
        let size = usize::from_str_radix(size_hex, 16).expect("chunk size hex");
        data = &data[nl + 2..];
        if size == 0 {
            break;
        }
        assert!(data.len() >= size + 2, "truncated chunk");
        out.extend_from_slice(&data[..size]);
        assert_eq!(&data[size..size + 2], b"\r\n", "chunk CRLF");
        data = &data[size + 2..];
    }
    out
}

fn split_http_response(raw: &[u8]) -> (u16, String, Vec<u8>) {
    let header_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("HTTP header terminator");
    let header = std::str::from_utf8(&raw[..header_end]).expect("HTTP headers utf-8");
    let rest = &raw[header_end + 4..];
    let mut lines = header.split("\r\n");
    let status_line = lines.next().expect("status line");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .expect("status code")
        .parse()
        .expect("status u16");
    let mut content_type = String::new();
    let mut content_length = None;
    let mut chunked = false;
    for line in lines {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        if k.eq_ignore_ascii_case("content-type") {
            content_type = v.trim().to_string();
        } else if k.eq_ignore_ascii_case("content-length") {
            content_length = Some(v.trim().parse::<usize>().expect("content-length"));
        } else if k.eq_ignore_ascii_case("transfer-encoding")
            && v.split(',')
                .any(|p| p.trim().eq_ignore_ascii_case("chunked"))
        {
            chunked = true;
        }
    }
    let body = if chunked {
        decode_chunked(rest)
    } else if let Some(len) = content_length {
        rest.get(..len)
            .unwrap_or_else(|| panic!("body {} < Content-Length {len}", rest.len()))
            .to_vec()
    } else {
        rest.to_vec()
    };
    (status, content_type, body)
}

/// Hand-rolled HTTP/1.1 over a real tokio TcpStream. No extra HTTP client crate.
async fn http11(
    addr: SocketAddr,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
) -> (u16, String, Vec<u8>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = connect_retry(addr).await;
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    if let Some(b) = body {
        head.push_str("Content-Type: application/json\r\n");
        head.push_str(&format!("Content-Length: {}\r\n\r\n", b.len()));
        stream.write_all(head.as_bytes()).await.expect("write head");
        stream.write_all(b).await.expect("write body");
    } else {
        head.push_str("\r\n");
        stream.write_all(head.as_bytes()).await.expect("write head");
    }
    stream.flush().await.expect("flush");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.expect("read response");
    split_http_response(&raw)
}

fn arrow_cell(col: &dyn Array, row: usize) -> Json {
    if col.is_null(row) {
        return Json::Null;
    }
    let any = col.as_any();
    if let Some(a) = any.downcast_ref::<StringArray>() {
        return json!(a.value(row));
    }
    if let Some(a) = any.downcast_ref::<Float64Array>() {
        return Number::from_f64(a.value(row))
            .map(Json::Number)
            .unwrap_or(Json::Null);
    }
    if let Some(a) = any.downcast_ref::<Int64Array>() {
        return json!(a.value(row));
    }
    if let Some(a) = any.downcast_ref::<BooleanArray>() {
        return json!(a.value(row));
    }
    panic!("unsupported arrow type {}", col.data_type());
}

fn arrow_ipc_to_json(bytes: &[u8]) -> Json {
    let mut reader = StreamReader::try_new(Cursor::new(bytes), None).expect("arrow ipc");
    let batch = reader.next().expect("one batch").expect("batch ok");
    assert!(reader.next().is_none(), "single-batch stream");
    let columns: Vec<String> = batch
        .schema()
        .fields()
        .iter()
        .map(|f| f.name().clone())
        .collect();
    let rows: Vec<Vec<Json>> = (0..batch.num_rows())
        .map(|r| {
            (0..batch.num_columns())
                .map(|c| arrow_cell(batch.column(c).as_ref(), r))
                .collect()
        })
        .collect();
    json!({ "columns": columns, "rows": rows })
}

fn parse_json(bytes: &[u8]) -> Json {
    serde_json::from_slice(bytes)
        .unwrap_or_else(|e| panic!("json: {e}: {}", String::from_utf8_lossy(bytes)))
}

const WS_TIMEOUT: Duration = Duration::from_secs(10);

async fn ws_next_text(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> Json {
    loop {
        let msg = tokio::time::timeout(WS_TIMEOUT, ws.next())
            .await
            .expect("ws.next timed out after 10s")
            .expect("ws closed")
            .expect("ws err");
        match msg {
            Message::Text(t) => return serde_json::from_str(t.as_str()).expect("ws json"),
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
            other => panic!("expected text frame, got {other:?}"),
        }
    }
}

/// Binding: one e2e — demo fixture → port 0 → real HTTP query (Arrow+JSON) →
/// /watch for a clone insert → /stats → MCP pipe query on the same SharedDb
/// while the HTTP server is live.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn end_to_end_demo_http_watch_mcp() {
    let dir = tmp("demo");
    let db = load_demo_equivalent(&dir);
    let (addr, server) = spawn_server(db.clone()).await;

    let qbody = json!({"cypher": SAMPLE_QUERY, "params": {}}).to_string();

    let (st_arrow, ctype_arrow, body_arrow) =
        http11(addr, "POST", "/query", Some(qbody.as_bytes())).await;
    assert_eq!(st_arrow, 200, "arrow query status");
    assert_eq!(
        ctype_arrow.split(';').next().unwrap().trim(),
        "application/vnd.apache.arrow.stream"
    );
    let from_arrow = arrow_ipc_to_json(&body_arrow);

    let (st_json, ctype_json, body_json) =
        http11(addr, "POST", "/query?format=json", Some(qbody.as_bytes())).await;
    assert_eq!(st_json, 200, "json query status");
    assert_eq!(
        ctype_json.split(';').next().unwrap().trim(),
        "application/json"
    );
    let from_json = parse_json(&body_json);

    assert_eq!(
        from_arrow, from_json,
        "Arrow-parsed rows must match JSON format"
    );
    assert_eq!(from_json["columns"], json!(["p", "proj", "score"]));
    let rows = from_json["rows"].as_array().expect("rows array");
    assert_eq!(rows.len(), 3, "person-01 has three FIT edges");
    assert_eq!(rows[0], json!(["person-01", "proj-01", 1.0]));
    assert_eq!(rows[1], json!(["person-01", "proj-02", 0.5]));
    assert_eq!(rows[2], json!(["person-01", "proj-20", 0.5]));

    let url = format!("ws://{addr}/watch");
    let (mut ws, _) = tokio_tungstenite::connect_async(url)
        .await
        .expect("ws connect");
    let ack = ws_next_text(&mut ws).await;
    assert_eq!(ack, json!({"subscribed": true}), "wait for subscribe ack");

    db.write()
        .insert_node("Live", "live-e2e", vec![])
        .expect("live insert");

    let frame = ws_next_text(&mut ws).await;
    let expected = serde_json::to_value(MutationEvent::NodeInserted {
        label: "Live".into(),
        key: "live-e2e".into(),
    })
    .unwrap();
    assert_eq!(frame, expected, "watch must see the live insert");

    let (st_stats, _, body_stats) = http11(addr, "GET", "/stats", None).await;
    assert_eq!(st_stats, 200);
    let stats = parse_json(&body_stats);
    assert_eq!(stats["nodes_live"], json!(61), "60 demo + live-e2e");
    assert_eq!(stats["nodes_tombstoned"], json!(0));
    assert_eq!(stats["edges"], json!(170), "Live insert adds no edges");
    let rules = stats["rules"].as_array().expect("rules");
    assert_eq!(rules.len(), 4);
    let mut names: Vec<&str> = rules
        .iter()
        .map(|r| r["name"].as_str().expect("rule name"))
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec![
            "auto_fk_person_org_id",
            "auto_fk_person_project_id",
            "auto_fk_project_org_id",
            "skill_fit",
        ]
    );

    let (req_read, mut req_write) = std::io::pipe().expect("mcp request pipe");
    let (resp_read, resp_write) = std::io::pipe().expect("mcp response pipe");
    let db_mcp = db.clone();
    let mcp = std::thread::spawn(move || {
        run_mcp_stdio(db_mcp, None, BufReader::new(req_read), resp_write)
    });
    let mcp_req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "query",
            "arguments": { "cypher": SAMPLE_QUERY }
        }
    });
    writeln!(req_write, "{mcp_req}").expect("mcp write");
    req_write.flush().expect("mcp flush");

    // Cross-surface: HTTP still answers while MCP is live on the same SharedDb.
    let (st_live, _, body_live) = http11(addr, "GET", "/stats", None).await;
    assert_eq!(st_live, 200);
    assert_eq!(parse_json(&body_live)["nodes_live"], json!(61));

    let mut resp_line = String::new();
    BufReader::new(resp_read)
        .read_line(&mut resp_line)
        .expect("mcp read");
    drop(req_write);
    mcp.join().expect("mcp thread").expect("mcp loop");

    let reply: Json = serde_json::from_str(resp_line.trim()).expect("mcp json-rpc");
    assert!(
        reply.get("error").is_none() || reply["error"].is_null(),
        "mcp protocol error: {reply}"
    );
    let text = reply["result"]["content"][0]["text"]
        .as_str()
        .expect("mcp content text");
    let mcp_rs: Json = serde_json::from_str(text).expect("mcp result-set json");
    assert_eq!(
        mcp_rs["columns"], from_json["columns"],
        "MCP query columns vs HTTP"
    );
    assert_eq!(mcp_rs["rows"], from_json["rows"], "MCP query rows vs HTTP");

    server.abort();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Binding: 4 HTTP-query readers + 2 SharedDb writers for ~1s wall time;
/// no deadlock/panic; final live-node count equals successful inserts.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrency_hammer_readers_and_writers() {
    let dir = tmp("hammer");
    let db = SharedDb::open(&dir).expect("open hammer dir");
    let app = router(db.clone());
    let writes_ok = Arc::new(AtomicUsize::new(0));
    let reads_ok = Arc::new(AtomicUsize::new(0));
    let start = Arc::new(tokio::sync::Barrier::new(6));
    let duration = Duration::from_secs(1);

    let mut readers = Vec::new();
    for _ in 0..4 {
        let app = app.clone();
        let start = Arc::clone(&start);
        let reads_ok = Arc::clone(&reads_ok);
        readers.push(tokio::spawn(async move {
            start.wait().await;
            let deadline = Instant::now() + duration;
            while Instant::now() < deadline {
                let req = Request::builder()
                    .method("POST")
                    .uri("/query?format=json")
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"cypher":"MATCH (n:Hammer) RETURN n","params":{}}"#,
                    ))
                    .expect("req");
                let res = app.clone().oneshot(req).await.expect("oneshot");
                let status = res.status();
                let body = to_bytes(res.into_body(), usize::MAX)
                    .await
                    .expect("body")
                    .to_vec();
                assert!(
                    status.is_success(),
                    "reader query {}: {}",
                    status,
                    String::from_utf8_lossy(&body)
                );
                let v = parse_json(&body);
                assert_eq!(v["columns"], json!(["n"]));
                reads_ok.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }

    let mut writers = Vec::new();
    for wid in 0..2 {
        let db = db.clone();
        let start = Arc::clone(&start);
        let writes_ok = Arc::clone(&writes_ok);
        writers.push(tokio::task::spawn_blocking(move || {
            // Block this worker until the async barrier trips (readers are waiting).
            let rt = tokio::runtime::Handle::current();
            rt.block_on(start.wait());
            let deadline = Instant::now() + duration;
            let mut i = 0usize;
            while Instant::now() < deadline {
                let key = format!("w{wid}-{i}");
                db.write()
                    .insert_node("Hammer", &key, vec![])
                    .unwrap_or_else(|e| panic!("insert {key}: {e}"));
                writes_ok.fetch_add(1, Ordering::Relaxed);
                i += 1;
            }
        }));
    }

    for h in readers {
        h.await.expect("reader task panicked");
    }
    for h in writers {
        h.await.expect("writer task panicked");
    }

    let wrote = writes_ok.load(Ordering::Relaxed);
    let read = reads_ok.load(Ordering::Relaxed);
    let stats = db.read().stats();
    assert!(wrote > 0, "writers made no progress in 1s");
    assert!(read > 0, "readers made no progress in 1s");
    assert_eq!(
        stats.nodes_live, wrote,
        "live nodes must equal successful writes (wrote={wrote}, stats={stats:?})"
    );
    assert_eq!(stats.nodes_tombstoned, 0);
    assert_eq!(stats.edges, 0, "Hammer inserts are isolated nodes");
    let _ = std::fs::remove_dir_all(&dir);
}
