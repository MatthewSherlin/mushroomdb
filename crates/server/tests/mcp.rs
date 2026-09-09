//! Binding tests for the MCP JSON-RPC 2.0 stdio loop.
//!
//! Framing is newline-delimited JSON only (`Cursor<Vec<u8>>` transcripts).
//! LSP `Content-Length` framing is not accepted.
//!
//! # Error split (protocol vs tool)
//!
//! Protocol errors are JSON-RPC `error` objects:
//! - `-32700` — unparseable line (invalid JSON / invalid UTF-8)
//! - `-32600` — parsed JSON that is not a request object, or a request
//!   with missing / non-string `method`
//! - `-32601` — unknown `method` on a request
//! - `-32602` — `tools/call` *envelope* invalid: `params` not an object,
//!   missing / non-string `name`, `arguments` present but not an object,
//!   or unknown tool name
//!
//! Tool-level failures are JSON-RPC *results* with `isError: true` and a
//! text message: missing or wrong-typed fields inside a known tool's
//! `arguments`, and every `GraphError` from core-api (bad Cypher, ingest
//! shape, unknown key, …).

use core_api::repograph::UNTRUSTED_FRAMING;
use core_api::{SharedDb, Value};
use serde_json::{json, Value as Js};
use server::run_mcp_stdio;
use std::collections::BTreeSet;
use std::io::Cursor;
use std::path::PathBuf;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("graphdb-mcp-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn open(name: &str) -> SharedDb {
    SharedDb::open(&tmp(name)).unwrap()
}

fn exchange(db: SharedDb, stdin: &str) -> (std::io::Result<()>, Vec<u8>) {
    exchange_at(db, None, stdin)
}

/// Like [`exchange`], but tells the loop where the store is on disk — what
/// `mushroomdb mcp <db>` passes and what the `sync` tool needs.
fn exchange_at(
    db: SharedDb,
    db_dir: Option<PathBuf>,
    stdin: &str,
) -> (std::io::Result<()>, Vec<u8>) {
    let mut reader = Cursor::new(stdin.as_bytes().to_vec());
    let mut writer = Cursor::new(Vec::new());
    let res = run_mcp_stdio(db, db_dir, &mut reader, &mut writer);
    (res, writer.into_inner())
}

/// Like [`exchange`], but with the full tool list — what `mushroomdb mcp
/// --all-tools` runs.
fn exchange_all_tools(db: SharedDb, stdin: &str) -> (std::io::Result<()>, Vec<u8>) {
    let mut reader = Cursor::new(stdin.as_bytes().to_vec());
    let mut writer = Cursor::new(Vec::new());
    let res = server::run_mcp_stdio_with(db, None, true, &mut reader, &mut writer);
    (res, writer.into_inner())
}

fn parse_lines(out: &[u8]) -> Vec<Js> {
    let text = String::from_utf8(out.to_vec()).expect("stdout utf-8");
    text.lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("stdout line json: {e}: {l}")))
        .collect()
}

fn line(obj: Js) -> String {
    format!("{obj}\n")
}

fn req(id: Js, method: &str, params: Option<Js>) -> String {
    let mut obj = json!({"jsonrpc": "2.0", "id": id, "method": method});
    if let Some(p) = params {
        obj["params"] = p;
    }
    line(obj)
}

fn notify(method: &str, params: Option<Js>) -> String {
    let mut obj = json!({"jsonrpc": "2.0", "method": method});
    if let Some(p) = params {
        obj["params"] = p;
    }
    line(obj)
}

fn call(id: i64, name: &str, arguments: Js) -> String {
    req(
        json!(id),
        "tools/call",
        Some(json!({"name": name, "arguments": arguments})),
    )
}

fn content_json(reply: &Js) -> Js {
    assert!(
        reply.get("error").is_none() || reply["error"].is_null(),
        "expected tool result, got protocol error: {reply}"
    );
    let text = reply["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("content[0].text string: {reply}"));
    assert_eq!(reply["result"]["content"][0]["type"], "text");
    serde_json::from_str(text).unwrap_or_else(|e| panic!("tool text json: {e}: {text}"))
}

fn seed_person(db: &SharedDb, key: &str) {
    db.write()
        .insert_node("Person", key, vec![("id".into(), Value::Str(key.into()))])
        .unwrap();
}

/// Binding: initialize result fields are exact; `notifications/initialized` is silent.
#[test]
fn handshake_initialize_then_initialized_is_silent() {
    let stdin = format!(
        "{}{}",
        req(
            json!(1),
            "initialize",
            Some(json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0"}
            })),
        ),
        notify("notifications/initialized", None),
    );
    let (res, out) = exchange(open("handshake"), &stdin);
    assert!(res.is_ok(), "{res:?}");
    let replies = parse_lines(&out);
    assert_eq!(
        replies.len(),
        1,
        "initialized must not emit a response: {replies:?}"
    );
    let r = &replies[0];
    assert_eq!(r["jsonrpc"], "2.0");
    assert_eq!(r["id"], 1);
    assert!(r.get("error").is_none() || r["error"].is_null());
    assert_eq!(
        r["result"],
        json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "mushroomdb", "version": env!("CARGO_PKG_VERSION")}
        })
    );
}

/// Binding: `--all-tools` returns all expected tools with the specified
/// schemas.
#[test]
fn tools_list_returns_all_tools_with_schemas() {
    let stdin = req(json!(1), "tools/list", None);
    let (res, out) = exchange_all_tools(open("list"), &stdin);
    assert!(res.is_ok(), "{res:?}");
    let replies = parse_lines(&out);
    assert_eq!(replies.len(), 1);
    let tools = replies[0]["result"]["tools"]
        .as_array()
        .expect("result.tools array");
    let names: BTreeSet<&str> = tools
        .iter()
        .map(|t| t["name"].as_str().expect("tool name"))
        .collect();
    // Original eight tools plus agent-memory tools plus history tools.
    for expected in &[
        "query",
        "ingest_json",
        "explain",
        "stats",
        "neighborhood",
        "node_info",
        "node_edges",
        "create_rule",
        "upsert_entity",
        "find_similar",
        "explain_association",
        "hybrid_search",
        "node_history",
        "edge_history",
        "was_linked",
        "rename_node",
    ] {
        assert!(names.contains(*expected), "missing tool: {expected}");
    }
    assert_eq!(tools.len(), 25);

    let by_name = |n: &str| {
        tools
            .iter()
            .find(|t| t["name"] == n)
            .unwrap_or_else(|| panic!("missing tool {n}"))
    };

    let query = by_name("query");
    assert_eq!(query["inputSchema"]["type"], "object");
    assert!(query["inputSchema"]["properties"].get("cypher").is_some());
    assert!(query["inputSchema"]["properties"].get("params").is_some());
    assert_eq!(query["inputSchema"]["required"], json!(["cypher"]));

    let ingest = by_name("ingest_json");
    assert!(ingest["inputSchema"]["properties"].get("label").is_some());
    assert!(ingest["inputSchema"]["properties"]
        .get("rows_json")
        .is_some());
    assert!(ingest["inputSchema"]["properties"]
        .get("key_field")
        .is_some());
    assert!(ingest["inputSchema"]["properties"]
        .get("auto_fk_suffix")
        .is_some());
    assert_eq!(
        ingest["inputSchema"]["required"],
        json!(["label", "rows_json"])
    );

    let explain = by_name("explain");
    assert_eq!(
        explain["inputSchema"]["properties"]["a"],
        json!({"type": "string", "minLength": 1})
    );
    assert_eq!(
        explain["inputSchema"]["properties"]["b"],
        json!({"type": "string", "minLength": 1})
    );
    assert_eq!(explain["inputSchema"]["required"], json!(["a", "b"]));

    let stats = by_name("stats");
    assert_eq!(stats["inputSchema"]["type"], "object");

    let nb = by_name("neighborhood");
    assert!(nb["inputSchema"]["properties"].get("key").is_some());
    assert!(nb["inputSchema"]["properties"].get("depth").is_some());
    assert!(nb["inputSchema"]["properties"].get("edge_types").is_some());
    assert!(nb["inputSchema"]["properties"].get("direction").is_some());
    assert_eq!(nb["inputSchema"]["required"], json!(["key"]));

    let info = by_name("node_info");
    assert_eq!(info["inputSchema"]["type"], "object");
    assert!(info["inputSchema"]["properties"].get("key").is_some());
    assert_eq!(info["inputSchema"]["required"], json!(["key"]));

    let edges = by_name("node_edges");
    assert_eq!(edges["inputSchema"]["type"], "object");
    assert!(edges["inputSchema"]["properties"].get("key").is_some());
    assert_eq!(edges["inputSchema"]["required"], json!(["key"]));

    let cr = by_name("create_rule");
    assert_eq!(cr["inputSchema"]["type"], "object");
    assert!(cr["inputSchema"]["properties"].get("predicate").is_some());
    assert_eq!(
        cr["inputSchema"]["required"],
        json!(["name", "src_label", "dst_label", "predicate", "edge_type"])
    );
    assert!(ingest["inputSchema"]["properties"].get("edges").is_some());
}

/// Binding: tools/call happy path for each of the five tools against a seeded db.
#[test]
fn tools_call_happy_path_for_each_tool() {
    let db = open("each-tool");
    {
        let mut w = db.write();
        w.insert_node("Org", "acme", vec![]).unwrap();
        w.insert_node(
            "Person",
            "p1",
            vec![
                ("id".into(), Value::Str("p1".into())),
                ("org_id".into(), Value::Str("acme".into())),
            ],
        )
        .unwrap();
        w.insert_node("Person", "p2", vec![("id".into(), Value::Str("p2".into()))])
            .unwrap();
        w.insert_edge("KNOWS", "p1", "p2").unwrap();
        w.create_rule(core_api::RuleDef {
            name: "works_at".into(),
            src_label: "Person".into(),
            dst_label: "Org".into(),
            predicate: core_api::Predicate::KeyMatch {
                field: "org_id".into(),
            },
            edge_type: "WORKS_AT".into(),
            weight_prop: None,
            max_edges: None,
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
        })
        .unwrap();
    }

    let stdin = format!(
        "{}{}{}{}{}",
        call(
            1,
            "query",
            json!({"cypher": "MATCH (t:Person {id: $tid}) RETURN t", "params": {"tid": "p1"}}),
        ),
        call(
            2,
            "ingest_json",
            json!({
                "label": "Person",
                "rows_json": "[{\"id\":\"p3\",\"name\":\"ada\"}]"
            }),
        ),
        call(3, "explain", json!({"a": "p1", "b": "acme"})),
        call(4, "stats", json!({})),
        call(
            5,
            "neighborhood",
            json!({
                "key": "p1",
                "depth": 1,
                "edge_types": ["KNOWS"],
                "direction": "out"
            }),
        ),
    );
    let (res, out) = exchange(db.clone(), &stdin);
    assert!(res.is_ok(), "{res:?}");
    let replies = parse_lines(&out);
    assert_eq!(replies.len(), 5);

    let q = content_json(&replies[0]);
    assert_eq!(q["columns"], json!(["t"]));
    assert_eq!(q["rows"], json!([["p1"]]));

    let ingest = content_json(&replies[1]);
    assert_eq!(ingest["inserted"], json!(1));
    assert_eq!(ingest["row_errors"], json!([]));
    assert!(db.read().has_node("p3"));

    let expl = content_json(&replies[2]);
    assert!(expl.is_array());
    assert_eq!(expl[0]["rule"], json!("works_at"));
    assert_eq!(expl[0]["edge_type"], json!("WORKS_AT"));
    assert_eq!(expl[0]["src_key"], json!("p1"));
    assert_eq!(expl[0]["dst_key"], json!("acme"));
    assert_eq!(expl[0]["predicate"]["kind"], json!("key_match"));
    assert_eq!(expl[0]["predicate"]["fields"], json!(["org_id"]));

    let stats = content_json(&replies[3]);
    assert!(stats["nodes_live"].as_u64().unwrap() >= 3);

    let nb = content_json(&replies[4]);
    assert_eq!(nb["columns"], json!(["key", "label", "depth"]));
    assert_eq!(nb["rows"], json!([["p2", "Person", 1]]));
}

/// Binding: MCP `query` dispatches CREATE through the write lock.
#[test]
fn query_create_is_a_write() {
    let db = open("mcp-create");
    let stdin = call(
        1,
        "query",
        json!({"cypher": "CREATE (n:L {id: 'k'}) RETURN n"}),
    );
    let (res, out) = exchange(db.clone(), &stdin);
    assert!(res.is_ok(), "{res:?}");
    let replies = parse_lines(&out);
    assert_eq!(replies.len(), 1);
    assert!(
        replies[0].get("error").is_none() || replies[0]["error"].is_null(),
        "CREATE via query must not be a protocol error: {}",
        replies[0]
    );
    assert_ne!(
        replies[0]["result"]["isError"],
        json!(true),
        "CREATE via query must succeed: {}",
        replies[0]
    );
    let q = content_json(&replies[0]);
    assert_eq!(q["columns"], json!(["n"]));
    assert_eq!(q["rows"], json!([["k"]]));
    assert_eq!(db.read().stats().nodes_live, 1);
}

/// Binding: node_info / node_edges MCP payloads match the HTTP wire shapes.
#[test]
fn node_info_and_edges_tool_parity() {
    let db = open("node-tools");
    {
        let mut w = db.write();
        w.insert_node("Org", "acme", vec![]).unwrap();
        w.create_rule(core_api::RuleDef {
            name: "works_at".into(),
            src_label: "Person".into(),
            dst_label: "Org".into(),
            predicate: core_api::Predicate::KeyMatch {
                field: "org_id".into(),
            },
            edge_type: "WORKS_AT".into(),
            weight_prop: None,
            max_edges: None,
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
        })
        .unwrap();
        w.insert_node(
            "Person",
            "p1",
            vec![
                ("id".into(), Value::Str("p1".into())),
                ("org_id".into(), Value::Str("acme".into())),
            ],
        )
        .unwrap();
        w.insert_node("Person", "p2", vec![]).unwrap();
        w.insert_edge("KNOWS", "p1", "p2").unwrap();
    }

    let stdin = format!(
        "{}{}{}",
        call(1, "node_info", json!({"key": "p1"})),
        call(2, "node_edges", json!({"key": "p1"})),
        call(3, "node_info", json!({"key": "ghost"})),
    );
    let (res, out) = exchange(db, &stdin);
    assert!(res.is_ok(), "{res:?}");
    let replies = parse_lines(&out);
    assert_eq!(replies.len(), 3);

    let info = content_json(&replies[0]);
    assert_eq!(info["key"], json!("p1"));
    assert_eq!(info["label"], json!("Person"));
    assert_eq!(info["props"]["id"], json!("p1"));
    assert_eq!(info["props"]["org_id"], json!("acme"));

    let edges = content_json(&replies[1]);
    assert_eq!(
        edges,
        json!({
            "edges": [
                {
                    "edge_type": "KNOWS",
                    "src_key": "p1",
                    "dst_key": "p2",
                    "derived": false
                },
                {
                    "edge_type": "WORKS_AT",
                    "src_key": "p1",
                    "dst_key": "acme",
                    "derived": true
                }
            ]
        })
    );

    assert_eq!(replies[2]["result"]["isError"], json!(true));
    let msg = replies[2]["result"]["content"][0]["text"]
        .as_str()
        .expect("error text");
    assert_eq!(msg, "node key not found: ghost");
}

/// Marquee: ingest through MCP → query through MCP → explain shows auto-FK provenance.
#[test]
fn agent_memory_loop() {
    let db = open("agent-loop");
    let stdin = format!(
        "{}{}{}{}{}{}",
        req(
            json!(1),
            "initialize",
            Some(json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "agent", "version": "0"}
            })),
        ),
        notify("notifications/initialized", None),
        call(
            2,
            "ingest_json",
            json!({
                "label": "Org",
                "rows_json": "[{\"id\":\"acme\",\"name\":\"Acme\"}]"
            }),
        ),
        call(
            3,
            "ingest_json",
            json!({
                "label": "Person",
                "rows_json": "[{\"id\":\"p1\",\"org_id\":\"acme\",\"name\":\"ada\"}]"
            }),
        ),
        call(
            4,
            "query",
            json!({
                "cypher": "MATCH (t:Person {id: $tid}) RETURN t",
                "params": {"tid": "p1"}
            }),
        ),
        call(5, "explain", json!({"a": "p1", "b": "acme"})),
    );
    let (res, out) = exchange(db.clone(), &stdin);
    assert!(res.is_ok(), "{res:?}");
    let replies = parse_lines(&out);
    assert_eq!(
        replies.len(),
        5,
        "initialize + 2 ingest + query + explain; initialized silent"
    );

    assert_eq!(replies[0]["result"]["serverInfo"]["name"], "mushroomdb");
    assert_eq!(
        replies[0]["result"]["serverInfo"]["version"],
        env!("CARGO_PKG_VERSION")
    );

    let org = content_json(&replies[1]);
    assert_eq!(org["inserted"], json!(1));

    let person = content_json(&replies[2]);
    assert_eq!(person["inserted"], json!(1));
    assert_eq!(person["rules_created"], json!(["auto_fk_person_org_id"]));
    assert!(db.read().has_node("p1"));

    let q = content_json(&replies[3]);
    assert_eq!(q["columns"], json!(["t"]));
    assert_eq!(q["rows"], json!([["p1"]]));

    let expl = content_json(&replies[4]);
    assert_eq!(expl[0]["rule"], json!("auto_fk_person_org_id"));
    assert_eq!(expl[0]["edge_type"], json!("ORG"));
    assert_eq!(expl[0]["src_key"], json!("p1"));
    assert_eq!(expl[0]["dst_key"], json!("acme"));
    // auto-FK stores no weight prop; explain recomputes the KeyMatch score.
    assert_eq!(expl[0]["weight"], json!(1.0));
    assert_eq!(expl[0]["predicate"]["kind"], json!("key_match"));
    assert_eq!(expl[0]["predicate"]["fields"], json!(["org_id"]));
}

/// Binding: a malformed line is -32700; the next valid request still works.
#[test]
fn malformed_line_is_parse_error_then_next_request_works() {
    let stdin = format!("this is not json\n{}", req(json!(1), "tools/list", None));
    let (res, out) = exchange(open("malformed"), &stdin);
    assert!(res.is_ok(), "{res:?}");
    let replies = parse_lines(&out);
    assert_eq!(replies.len(), 2);
    assert_eq!(replies[0]["jsonrpc"], "2.0");
    assert_eq!(replies[0]["id"], Js::Null);
    assert_eq!(replies[0]["error"]["code"], json!(-32700));
    assert!(replies[1]["result"]["tools"].is_array());
}

/// Binding: unknown method on a request is -32601.
#[test]
fn unknown_method_is_minus_32601() {
    let stdin = req(json!("abc"), "no/such", None);
    let (res, out) = exchange(open("unknown"), &stdin);
    assert!(res.is_ok(), "{res:?}");
    let replies = parse_lines(&out);
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0]["id"], "abc");
    assert_eq!(replies[0]["error"]["code"], json!(-32601));
}

/// Binding: envelope failures are -32602; tool-argument / execution failures are isError.
#[test]
fn bad_tool_args_split_protocol_vs_tool() {
    let db = open("bad-args");
    seed_person(&db, "p1");
    let stdin = format!(
        "{}{}{}{}{}{}",
        req(
            json!(1),
            "tools/call",
            Some(json!({"arguments": {"cypher": "RETURN 1"}})),
        ),
        call(2, "nope", json!({})),
        req(json!(5), "tools/call", Some(json!(42))),
        req(
            json!(6),
            "tools/call",
            Some(json!({"name": "query", "arguments": "string"})),
        ),
        call(3, "query", json!({})),
        call(4, "query", json!({"cypher": "MATCH (n)"})),
    );
    let (res, out) = exchange(db, &stdin);
    assert!(res.is_ok(), "{res:?}");
    let replies = parse_lines(&out);
    assert_eq!(replies.len(), 6);

    // Protocol: tools/call missing name.
    assert_eq!(replies[0]["error"]["code"], json!(-32602));
    // Protocol: unknown tool name.
    assert_eq!(replies[1]["error"]["code"], json!(-32602));
    // Protocol: params is not an object.
    assert_eq!(replies[2]["id"], 5);
    assert_eq!(replies[2]["error"]["code"], json!(-32602));
    // Protocol: arguments is not an object.
    assert_eq!(replies[3]["id"], 6);
    assert_eq!(replies[3]["error"]["code"], json!(-32602));
    // Tool-level: known tool, missing required argument.
    assert_eq!(replies[4]["result"]["isError"], json!(true));
    assert_eq!(replies[4]["result"]["content"][0]["type"], "text");
    assert!(replies[4]["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("cypher"));
    // Tool-level: GraphError (bad Cypher) is not a protocol error.
    assert!(replies[5].get("error").is_none() || replies[5]["error"].is_null());
    assert_eq!(replies[5]["result"]["isError"], json!(true));
    let msg = replies[5]["result"]["content"][0]["text"]
        .as_str()
        .expect("error text");
    assert!(msg.contains("parse:"), "expected parse: detail, got {msg}");
}

/// Binding: a parsed non-object line is -32600 Invalid Request, not parse error.
#[test]
fn non_object_json_is_invalid_request() {
    let stdin = format!(
        "[1]\n42\n\"hi\"\ntrue\n{}",
        req(json!(1), "tools/list", None)
    );
    let (res, out) = exchange(open("non-object"), &stdin);
    assert!(res.is_ok(), "{res:?}");
    let replies = parse_lines(&out);
    assert_eq!(replies.len(), 5);
    for r in &replies[..4] {
        assert_eq!(r["id"], Js::Null);
        assert_eq!(r["error"]["code"], json!(-32600));
        assert_eq!(r["error"]["message"], "Invalid Request");
    }
    assert!(replies[4]["result"]["tools"].is_array());
}

/// Binding: missing or non-string method on a request is -32600, not -32601.
#[test]
fn missing_or_non_string_method_is_invalid_request() {
    let stdin = format!(
        "{}\n{}\n",
        json!({"jsonrpc": "2.0", "id": 1}),
        json!({"jsonrpc": "2.0", "id": 2, "method": 42}),
    );
    let (res, out) = exchange(open("no-method"), &stdin);
    assert!(res.is_ok(), "{res:?}");
    let replies = parse_lines(&out);
    assert_eq!(replies.len(), 2);
    assert_eq!(replies[0]["id"], 1);
    assert_eq!(replies[0]["error"]["code"], json!(-32600));
    assert_eq!(replies[0]["error"]["message"], "Invalid Request");
    assert_eq!(replies[1]["id"], 2);
    assert_eq!(replies[1]["error"]["code"], json!(-32600));
    assert_eq!(replies[1]["error"]["message"], "Invalid Request");
}

/// Binding: `id: null` is a request; the response echoes `id: null`.
#[test]
fn id_null_request_echoes_null() {
    let stdin = req(Js::Null, "tools/list", None);
    let (res, out) = exchange(open("id-null"), &stdin);
    assert!(res.is_ok(), "{res:?}");
    let replies = parse_lines(&out);
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0]["id"], Js::Null);
    assert!(replies[0]["result"]["tools"].is_array());
}

/// Binding: a notification (no `id`) never writes a response.
#[test]
fn notification_without_id_emits_no_bytes() {
    let stdin = notify("tools/list", None);
    let (res, out) = exchange(open("notify"), &stdin);
    assert!(res.is_ok(), "{res:?}");
    assert!(out.is_empty(), "notification must not write: {out:?}");
}

/// Binding: EOF on the reader is a clean `Ok(())`.
#[test]
fn eof_exits_ok() {
    let mut reader = Cursor::new(Vec::<u8>::new());
    let mut writer = Cursor::new(Vec::new());
    let res = run_mcp_stdio(open("eof"), None, &mut reader, &mut writer);
    assert!(res.is_ok(), "{res:?}");
    assert!(writer.into_inner().is_empty());
}

/// Binding: ingest_json edges + create_rule match the HTTP write surface.
#[test]
fn ingest_edges_and_create_rule_tools() {
    let db = open("write-tools");
    {
        let mut w = db.write();
        w.insert_node("Person", "a", vec![]).unwrap();
        w.insert_node("Person", "b", vec![]).unwrap();
        w.insert_node("Org", "o1", vec![("founded_year".into(), Value::Int(2010))])
            .unwrap();
        w.insert_node("Org", "o2", vec![("founded_year".into(), Value::Int(2011))])
            .unwrap();
    }
    let stdin = format!(
        "{}{}",
        call(
            1,
            "ingest_json",
            json!({
                "label": "Person",
                "rows_json": "[]",
                "edges": [{"edge_type": "KNOWS", "src": "a", "dst": "b"}]
            }),
        ),
        call(
            2,
            "create_rule",
            json!({
                "name": "founded_within",
                "src_label": "Org",
                "dst_label": "Org",
                "predicate": {"NumericWithin": {"field": "founded_year", "tolerance": 2.0}},
                "edge_type": "FOUNDED_WITHIN",
                "weight_prop": "score",
                "max_edges": null
            }),
        ),
    );
    let (res, out) = exchange(db.clone(), &stdin);
    assert!(res.is_ok(), "{res:?}");
    let replies = parse_lines(&out);
    assert_eq!(replies.len(), 2);
    let ingest = content_json(&replies[0]);
    assert_eq!(ingest["edges_inserted"], json!(1));
    let created = content_json(&replies[1]);
    assert_eq!(created, json!({"ok": true, "name": "founded_within"}));
    let edges = db.read().node_edges("a").unwrap();
    assert!(edges.iter().any(|e| e.edge_type == "KNOWS" && !e.derived));
    assert!(db.read().rules().iter().any(|r| r.name == "founded_within"));
}

/// Binding: create_rule without `weight_prop` stores the score under `weight`,
/// so a derived edge's score is queryable instead of null.
#[test]
fn mcp_create_rule_defaults_weight_prop_to_weight() {
    let db = open("default-weight-prop");
    {
        let mut w = db.write();
        for key in ["p1", "p2"] {
            w.insert_node(
                "Person",
                key,
                vec![("industry".into(), Value::Str("design".into()))],
            )
            .unwrap();
        }
        for key in ["o1", "o2"] {
            w.insert_node(
                "Org",
                key,
                vec![("industry".into(), Value::Str("design".into()))],
            )
            .unwrap();
        }
    }
    let stdin = format!(
        "{}{}",
        call(
            1,
            "create_rule",
            json!({
                "name": "same",
                "src_label": "Person",
                "dst_label": "Org",
                "predicate": {"FieldEqual": {"field": "industry"}},
                "edge_type": "SAME"
            }),
        ),
        call(
            2,
            "query",
            json!({"cypher": "MATCH (p:Person)-[r:SAME]->(o:Org) RETURN r.weight LIMIT 1"}),
        ),
    );
    let (res, out) = exchange(db.clone(), &stdin);
    assert!(res.is_ok(), "{res:?}");
    let replies = parse_lines(&out);
    assert_eq!(replies.len(), 2);
    assert_eq!(
        content_json(&replies[0]),
        json!({"ok": true, "name": "same"})
    );
    let q = content_json(&replies[1]);
    assert_eq!(q["columns"], json!(["r.weight"]));
    assert_eq!(
        q["rows"][0][0],
        json!(1.0),
        "score must be stored under the default weight prop, got {q}"
    );
}

/// Binding: ingest_json mixed batch is atomic — a bad edge persists no nodes.
#[test]
fn ingest_json_bad_edge_is_atomic() {
    let db = open("mcp-atomic");
    let stdin = call(
        1,
        "ingest_json",
        json!({
            "label": "Person",
            "rows_json": "[{\"id\":\"newbie\"}]",
            "edges": [{"edge_type": "KNOWS", "src": "newbie", "dst": "ghost"}]
        }),
    );
    let (res, out) = exchange(db.clone(), &stdin);
    assert!(res.is_ok(), "{res:?}");
    let replies = parse_lines(&out);
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0]["result"]["isError"], json!(true));
    let msg = replies[0]["result"]["content"][0]["text"]
        .as_str()
        .expect("error text");
    assert!(
        msg.contains("node key not found"),
        "preview error, got {msg}"
    );
    assert!(
        !db.read().has_node("newbie"),
        "newbie must not persist after a rejected mixed batch"
    );
}

// ---------------------------------------------------------------------------
// Task 3: hybrid_search MCP tool
// ---------------------------------------------------------------------------

/// hybrid_search returns fused results with both a text and vector match.
/// Fixture: three "Doc" nodes:
///   "both"   — body="hello", emb=[1,0] close to query [1,0].
///   "t_only" — body="hello", emb=[-1,0] (antipodal → cosine -1 < 0 → filtered).
///   "v_only" — body="other", emb=[1,0] exactly aligned with query.
///
/// Text ranking: both(rank 1), t_only(rank 2).
/// Vector ranking: both(rank 1, cosine 1.0), v_only filtered? No wait —
/// actually both=[1,0] and v_only=[1,0] tie at cosine 1.0.
/// Use distinct vectors: both=[0.9, 0.436] (≈ 0.9 cosine) and v_only=[1,0].
///
/// Text: both rank 1, t_only rank 2.
/// Vector: v_only rank 1, both rank 2.
/// RRF: both = 1/61+1/62, v_only = 1/61, t_only = 1/62.
/// Order: both > v_only > t_only.
#[test]
fn hybrid_search_round_trip() {
    let db = open("mcp-hybrid");

    // Enable fulltext.
    {
        let mut w = db.write();
        w.enable_fulltext("Doc", "body").unwrap();
        w.insert_node(
            "Doc",
            "both",
            vec![
                ("body".into(), Value::Str("hello".into())),
                (
                    "emb".into(),
                    Value::List(vec![Value::Float(1.0), Value::Float(0.5)]),
                ),
            ],
        )
        .unwrap();
        w.insert_node(
            "Doc",
            "t_only",
            vec![
                ("body".into(), Value::Str("hello".into())),
                (
                    "emb".into(),
                    Value::List(vec![Value::Float(-1.0), Value::Float(0.0)]),
                ),
            ],
        )
        .unwrap();
        w.insert_node(
            "Doc",
            "v_only",
            vec![
                ("body".into(), Value::Str("other".into())),
                (
                    "emb".into(),
                    Value::List(vec![Value::Float(1.0), Value::Float(0.0)]),
                ),
            ],
        )
        .unwrap();
    }

    let stdin = call(
        1,
        "hybrid_search",
        json!({
            "query_text": "hello",
            "text_field": "body",
            "vector": [1.0, 0.0],
            "vector_field": "emb",
            "label": "Doc",
            "k": 3
        }),
    );
    let (res, out) = exchange(db, &stdin);
    assert!(res.is_ok(), "{res:?}");
    let replies = parse_lines(&out);
    assert_eq!(replies.len(), 1);

    let payload = content_json(&replies[0]);
    assert!(!replies[0]["result"]["isError"].as_bool().unwrap_or(false));

    let results = payload["results"].as_array().expect("results array");
    assert_eq!(results.len(), 3, "all three nodes returned");

    // "both" must be first with the highest fused score.
    assert_eq!(results[0]["key"], json!("both"), "both must rank first");
    assert_eq!(
        results[1]["key"],
        json!("v_only"),
        "v_only must rank second"
    );
    assert_eq!(results[2]["key"], json!("t_only"), "t_only must rank third");

    let s_both = results[0]["score"].as_f64().expect("score float");
    let s_v_only = results[1]["score"].as_f64().expect("score float");
    let s_t_only = results[2]["score"].as_f64().expect("score float");
    let expected_both = 1.0_f64 / 61.0 + 1.0_f64 / 62.0;
    let expected_v_only = 1.0_f64 / 61.0;
    let expected_t_only = 1.0_f64 / 62.0;
    assert!((s_both - expected_both).abs() < 1e-12, "both score");
    assert!((s_v_only - expected_v_only).abs() < 1e-12, "v_only score");
    assert!((s_t_only - expected_t_only).abs() < 1e-12, "t_only score");
}

/// hybrid_search with no vector → text-only ranking; missing required fields → error.
#[test]
fn hybrid_search_text_only_and_missing_field_errors() {
    let db = open("mcp-hybrid-errs");

    {
        let mut w = db.write();
        w.enable_fulltext("Doc", "body").unwrap();
        w.insert_node(
            "Doc",
            "alpha",
            vec![("body".into(), Value::Str("foo".into()))],
        )
        .unwrap();
    }

    // Text-only (no vector field).
    let stdin = call(
        1,
        "hybrid_search",
        json!({ "query_text": "foo", "text_field": "body", "k": 5 }),
    );
    let (res, out) = exchange(db.clone(), &stdin);
    assert!(res.is_ok(), "{res:?}");
    let replies = parse_lines(&out);
    let payload = content_json(&replies[0]);
    let results = payload["results"].as_array().expect("results");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["key"], json!("alpha"));

    // Missing query_text → tool error.
    let stdin2 = call(2, "hybrid_search", json!({ "text_field": "body" }));
    let (_, out2) = exchange(db, &stdin2);
    let replies2 = parse_lines(&out2);
    assert_eq!(
        replies2[0]["result"]["isError"],
        json!(true),
        "missing query_text must be an error"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Task tools: explore, map, context, impact, owners, why, recall, remember,
// sync
//
// These nine answer a question about a graphed repository rather than about
// the graph API, so they come first in `tools/list` and the graph tools listed
// beside them are prefixed `Advanced:`. Each returns the rendered digest as
// its text content and nothing else; a caller that wants the report passes
// `json: true` and gets it *as* the text.
// ─────────────────────────────────────────────────────────────────────────────

/// The nine task tools, in the order `tools/list` must list them.
const TASK_TOOLS: [&str; 9] = [
    "explore", "map", "context", "impact", "owners", "why", "recall", "remember", "sync",
];

/// The sixteen graph tools, in their established order, after the task tools.
const ADVANCED_TOOLS: [&str; 16] = [
    "query",
    "ingest_json",
    "create_rule",
    "explain",
    "stats",
    "neighborhood",
    "node_info",
    "node_edges",
    "upsert_entity",
    "find_similar",
    "explain_association",
    "hybrid_search",
    "node_history",
    "edge_history",
    "was_linked",
    "rename_node",
];

/// The three files of the synthetic code store, and who tops each.
const CODE_FILES: [(&str, &str); 3] = [
    ("src/core.rs", "a@example.test"),
    ("src/util.rs", "a@example.test"),
    ("src/web.rs", "b@example.test"),
];
/// Commits in the synthetic code store.
const CODE_COMMITS: usize = 4;
/// Unix seconds of its oldest commit. Fixed, so nothing here reads a clock.
const CODE_T0: i64 = 1_700_000_000;

fn s(v: &str) -> Value {
    Value::Str(v.to_string())
}

fn list(items: &[String]) -> Value {
    Value::List(items.iter().map(|i| s(i)).collect())
}

fn code_sha(i: usize) -> String {
    format!("{:07x}{:033x}", 0x00ab_cd00usize + i * 4093, i)
}

/// Files commit `i` touched: every commit touches `core` and `web`, and the
/// first also touches `util` — so `core`/`web` overlap fully and `core`/`util`
/// overlap at exactly the `co_changed` threshold.
fn code_touched(i: usize) -> Vec<&'static str> {
    if i == 0 {
        vec!["src/core.rs", "src/util.rs", "src/web.rs"]
    } else {
        vec!["src/core.rs", "src/web.rs"]
    }
}

/// The rules `ingest-git` declares, reduced to the ones these tools read.
///
/// Recreated here rather than imported: core-api's fixture is a private module
/// of its own test crates, and the server crate cannot reach into it.
fn code_rules() -> Vec<core_api::RuleDef> {
    fn key_rule(name: &str, src: &str, dst: &str, field: &str, edge: &str) -> core_api::RuleDef {
        let predicate = core_api::Predicate::KeyMatch {
            field: field.into(),
        };
        let max_edges = Some(core_api::default_max_edges(&predicate));
        core_api::RuleDef {
            name: name.into(),
            src_label: src.into(),
            dst_label: dst.into(),
            predicate,
            edge_type: edge.into(),
            weight_prop: None,
            max_edges,
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
        }
    }
    let mut out = vec![
        key_rule(
            "auto_fk_symbol_file_id",
            "Symbol",
            "File",
            "file_id",
            "DEFINES",
        ),
        key_rule("imports", "File", "File", "imports", "IMPORTS"),
        key_rule("calls", "Symbol", "Symbol", "calls_to", "CALLS"),
        key_rule(
            "auto_fk_commit_author_id",
            "Commit",
            "Author",
            "author_id",
            "AUTHOR",
        ),
        key_rule(
            "auto_fk_file_top_author_id",
            "File",
            "Author",
            "top_author_id",
            "TOP_AUTHOR",
        ),
    ];
    for label in core_api::repograph::rules::ABOUT_LABELS {
        out.push(core_api::repograph::rules::about_rule(label));
    }
    let co = core_api::Predicate::Overlap {
        field: "commits".into(),
        min: 0.25,
    };
    out.push(core_api::RuleDef {
        name: "co_changed".into(),
        src_label: "File".into(),
        dst_label: "File".into(),
        predicate: co.clone(),
        edge_type: "CO_CHANGED".into(),
        weight_prop: Some("score".into()),
        max_edges: Some(10),
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
    });
    out.push(core_api::RuleDef {
        name: "knows".into(),
        src_label: "Author".into(),
        dst_label: "File".into(),
        predicate: co,
        edge_type: "KNOWS".into(),
        weight_prop: Some("score".into()),
        max_edges: Some(20),
        approximate: false,
        via_label: Some("File".into()),
        via_edge: Some("TOP_AUTHOR".into()),
        via_dir: Some(core_api::Direction::In),
    });
    out
}

/// Write the synthetic code graph into an already-open store.
fn seed_code_graph(db: &SharedDb) {
    let mut w = db.write();
    for (key, name) in [
        ("a@example.test", "Ada Example"),
        ("b@example.test", "Bea Example"),
    ] {
        w.insert_node("Author", key, vec![("name".into(), s(name))])
            .expect("author");
    }

    let mut commits_of: std::collections::BTreeMap<&str, Vec<String>> = Default::default();
    let mut authors_of: std::collections::BTreeMap<&str, std::collections::BTreeMap<&str, usize>> =
        Default::default();
    for i in 0..CODE_COMMITS {
        let author = if i % 2 == 0 {
            "a@example.test"
        } else {
            "b@example.test"
        };
        for f in code_touched(i) {
            commits_of.entry(f).or_default().push(code_sha(i));
            *authors_of.entry(f).or_default().entry(author).or_default() += 1;
        }
    }

    for (key, top) in CODE_FILES {
        let commits = commits_of.get(key).cloned().unwrap_or_default();
        let counts: Vec<String> = authors_of
            .get(key)
            .into_iter()
            .flatten()
            .map(|(email, n)| format!("{email}\t{n}"))
            .collect();
        let mut props = vec![
            ("id".into(), s(key)),
            ("path".into(), s(key)),
            ("dir".into(), s("src")),
            ("ext".into(), s("rs")),
            ("lang".into(), s("rust")),
            ("lines".into(), Value::Int(42)),
            ("top_author_id".into(), s(top)),
            ("n_commits".into(), Value::Int(commits.len() as i64)),
            ("commits".into(), list(&commits)),
            ("author_counts".into(), list(&counts)),
        ];
        // Both other files import the core one, and quote the line they did so on.
        if key != "src/core.rs" {
            props.push(("imports".into(), list(&["src/core.rs".to_string()])));
            props.push(("import_lines".into(), list(&["src/core.rs\t3".to_string()])));
        }
        w.insert_node("File", key, props).expect("file");
    }

    for (file, name, callee) in [
        ("src/core.rs", "core::init", None),
        ("src/web.rs", "web::serve", Some("src/core.rs#core::init")),
    ] {
        let key = format!("{file}#{name}");
        let mut props = vec![
            ("id".into(), s(&key)),
            ("name".into(), s(name)),
            ("kind".into(), s("function")),
            ("path".into(), s(file)),
            ("file_id".into(), s(file)),
            ("line_start".into(), Value::Int(10)),
            ("line_end".into(), Value::Int(20)),
            ("signature".into(), s(&format!("fn {name}()"))),
            ("doc".into(), s(&format!("what {name} does"))),
        ];
        if let Some(target) = callee {
            props.push(("calls_to".into(), list(&[target.to_string()])));
            props.push(("call_lines".into(), list(&[format!("{target}\t14")])));
        }
        w.insert_node("Symbol", &key, props).expect("symbol");
    }

    for i in 0..CODE_COMMITS {
        let sha = code_sha(i);
        let author = if i % 2 == 0 {
            "a@example.test"
        } else {
            "b@example.test"
        };
        w.insert_node(
            "Commit",
            &sha,
            vec![
                ("id".into(), s(&sha)),
                ("message".into(), s(&format!("change {i:02}"))),
                ("ts".into(), Value::Int(CODE_T0 + i as i64 * 86_400)),
                ("author_id".into(), s(author)),
            ],
        )
        .expect("commit");
        for f in code_touched(i) {
            w.insert_edge("TOUCHED", &sha, f).expect("touched");
        }
    }

    w.insert_node(
        "Note",
        "note:seed",
        vec![
            ("id".into(), s("note:seed")),
            ("text".into(), s("the core module is the entry point")),
            ("kind".into(), s("note")),
            ("ts".into(), Value::Int(CODE_T0)),
            ("source".into(), s("agent")),
            ("about".into(), list(&["src/core.rs".to_string()])),
        ],
    )
    .expect("note");

    w.insert_node(
        "GitSync",
        "__mushroomdb_git_sync__",
        vec![
            ("id".into(), s("__mushroomdb_git_sync__")),
            ("sha".into(), s(&code_sha(CODE_COMMITS - 1))),
            ("synced_at".into(), Value::Int(CODE_T0 + 4 * 86_400)),
            // Deliberately not a real path: `context` must still answer from
            // the graph when the working tree it names is not there.
            ("repo".into(), s("/nonexistent/mushroomdb-test-repo")),
            ("recurse".into(), Value::Bool(false)),
            ("prs".into(), Value::Bool(false)),
            ("structure".into(), Value::Bool(true)),
            ("docs".into(), Value::Bool(true)),
        ],
    )
    .expect("gitsync");

    for (label, field) in [("File", "path"), ("Symbol", "name"), ("Note", "text")] {
        w.enable_fulltext(label, field).expect("fulltext");
    }
    for def in code_rules() {
        w.create_rule(def).expect("rule");
    }
}

/// A store shaped the way `ingest-git` leaves one, small enough to assert on.
fn code_store(name: &str) -> SharedDb {
    let db = open(name);
    seed_code_graph(&db);
    db
}

/// The text of a successful task reply, and nothing else — a task tool's reply
/// carries one text block and no `structuredContent`.
fn task_text(reply: &Js) -> String {
    assert!(
        reply.get("error").is_none() || reply["error"].is_null(),
        "expected tool result, got protocol error: {reply}"
    );
    assert!(
        !reply["result"]["isError"].as_bool().unwrap_or(false),
        "expected success, got tool error: {reply}"
    );
    assert_eq!(reply["result"]["content"][0]["type"], "text");
    assert_eq!(
        reply["result"]["content"].as_array().map(Vec::len),
        Some(1),
        "one content block, not two: {reply}"
    );
    assert!(
        reply["result"].get("structuredContent").is_none(),
        "a task tool must not ship the report beside the text: {reply}"
    );
    reply["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("content[0].text string: {reply}"))
        .to_string()
}

/// Unwrap a task tool's rendered digest.
///
/// Asserts the untrusted-data framing line on every task tool that goes through
/// it — repository content reaches an assistant here, and it has to be marked
/// as data before the assistant reads a word of it. The digest is returned
/// **without** that line, so a caller's assertions are about the render.
fn task_reply(reply: &Js) -> String {
    let text = task_text(reply);
    let body = text
        .strip_prefix(UNTRUSTED_FRAMING)
        .unwrap_or_else(|| {
            panic!("task tool text must open with the untrusted-data framing line: {text:?}")
        })
        .to_string();
    assert!(
        !body.contains(UNTRUSTED_FRAMING),
        "the framing line must be stamped once, not twice: {text:?}"
    );
    body
}

/// The report behind a task tool, asked for with `json: true`: the text content
/// *is* the serialised report, with no digest and no framing line.
fn task_report(db: SharedDb, name: &str, mut args: Js) -> Js {
    args["json"] = json!(true);
    let reply = one_task_call(db, name, args);
    let text = task_text(&reply);
    assert!(
        !text.starts_with(UNTRUSTED_FRAMING),
        "a json reply is data for a program, not prose to frame: {text}"
    );
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("json reply must parse: {e}: {text}"))
}

/// A read-only task tool's digest and report together, from two calls on the
/// same store.
fn task_both(db: SharedDb, name: &str, args: Js) -> (String, Js) {
    let text = task_reply(&one_task_call(db.clone(), name, args.clone()));
    (text, task_report(db, name, args))
}

/// The text of a tool error reply.
fn error_text(reply: &Js) -> String {
    assert_eq!(
        reply["result"]["isError"],
        json!(true),
        "expected a tool error: {reply}"
    );
    reply["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

fn one_task_call(db: SharedDb, name: &str, args: Js) -> Js {
    let (res, out) = exchange(db, &call(1, name, args));
    assert!(res.is_ok(), "{res:?}");
    parse_lines(&out).remove(0)
}

/// The graph tools a default `tools/list` keeps: an arbitrary Cypher read, a
/// bulk load, and the store's own counts. The other thirteen are `--all-tools`.
const DEFAULT_GRAPH_TOOLS: [&str; 3] = ["query", "ingest_json", "stats"];

/// The eight a memory store lists: [`TASK_TOOLS`] less `explore`, which has no
/// code graph to explore there. Derived from the one list this file keeps, so
/// adding a task tool above cannot leave two lists disagreeing here.
fn memory_task_tools() -> Vec<&'static str> {
    TASK_TOOLS.into_iter().filter(|n| *n != "explore").collect()
}

/// Binding: a store no repository was ingested into keeps today's eleven — the
/// eight memory task tools plus those three graph tools, in that order.
#[test]
fn a_memory_store_keeps_the_eleven_tool_surface() {
    let (res, out) = exchange(open("list-default"), &req(json!(1), "tools/list", None));
    assert!(res.is_ok(), "{res:?}");
    let replies = parse_lines(&out);
    let tools = replies[0]["result"]["tools"].as_array().expect("tools");
    let names: Vec<&str> = tools
        .iter()
        .map(|t| t["name"].as_str().expect("name"))
        .collect();
    let expected: Vec<&str> = memory_task_tools()
        .into_iter()
        .chain(DEFAULT_GRAPH_TOOLS.iter().copied())
        .collect();
    assert_eq!(names, expected, "default tools/list on a memory store");
    assert_eq!(tools.len(), 11);
}

/// Binding: a store carrying the `GitSync` marker — a repository was ingested
/// into it — lists three tools and nothing else. A host defers MCP schemas and
/// makes the model search for them, so what is listed is what gets found.
#[test]
fn a_code_graph_store_lists_three_tools_by_default() {
    let (res, out) = exchange(
        code_store("surface-code"),
        &req(json!(1), "tools/list", None),
    );
    assert!(res.is_ok(), "{res:?}");
    let replies = parse_lines(&out);
    let names: Vec<&str> = replies[0]["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|t| t["name"].as_str().expect("name"))
        .collect();
    assert_eq!(names, vec!["explore", "query", "stats"]);
}

/// Binding: the surface decides what is *listed*, never what is served. Every
/// tool the memory surface advertises is still callable on a code-graph store,
/// and `explore` is still callable on a memory store.
#[test]
fn a_hidden_tool_is_still_callable_on_either_surface() {
    let map = task_reply(&one_task_call(
        code_store("surface-hidden"),
        "map",
        json!({}),
    ));
    assert!(
        map.starts_with("mushroomdb map —"),
        "map is unlisted on a code graph but must still answer: {map}"
    );
    let explore = task_reply(&one_task_call(
        open("surface-memory-explore"),
        "explore",
        json!({"target": "x"}),
    ));
    assert!(
        explore.contains("unknown: x"),
        "explore is unlisted on a memory store but must still answer: {explore}"
    );
}

/// Binding: an unlisted tool is still served. The flag decides what is
/// advertised, not what a caller that knows the name can reach.
#[test]
fn an_unlisted_graph_tool_is_still_callable() {
    let (res, out) = exchange(
        code_store("list-unlisted"),
        &call(1, "node_info", json!({"key": "src/core.rs"})),
    );
    assert!(res.is_ok(), "{res:?}");
    let reply = parse_lines(&out).remove(0);
    assert!(
        !reply["result"]["isError"].as_bool().unwrap_or(false),
        "node_info is unlisted by default but must still answer: {reply}"
    );
}

/// Binding: `--all-tools` lists 25, task tools first in their fixed order, and
/// every one of the sixteen graph tools carries the `Advanced:` prefix.
#[test]
fn tools_list_has_25_tools_task_tools_first_and_advanced_prefix() {
    let (res, out) = exchange_all_tools(open("list-order"), &req(json!(1), "tools/list", None));
    assert!(res.is_ok(), "{res:?}");
    let replies = parse_lines(&out);
    let tools = replies[0]["result"]["tools"].as_array().expect("tools");
    let names: Vec<&str> = tools
        .iter()
        .map(|t| t["name"].as_str().expect("name"))
        .collect();

    let expected: Vec<&str> = TASK_TOOLS
        .iter()
        .chain(ADVANCED_TOOLS.iter())
        .copied()
        .collect();
    assert_eq!(names, expected, "tools/list order");
    assert_eq!(tools.len(), 25);

    for t in tools.iter().take(TASK_TOOLS.len()) {
        let d = t["description"].as_str().expect("description");
        assert!(
            !d.starts_with("Advanced:"),
            "task tool {} must not be prefixed: {d}",
            t["name"]
        );
        assert_eq!(t["inputSchema"]["type"], "object", "{}", t["name"]);
    }
    for t in tools.iter().skip(TASK_TOOLS.len()) {
        let d = t["description"].as_str().expect("description");
        assert!(
            d.starts_with("Advanced: "),
            "{} must be prefixed Advanced: got {d}",
            t["name"]
        );
    }

    // The schemas the plan fixes. `json` is the one argument all eight share:
    // it is what a program asks for the report with, now that no reply carries
    // one by default.
    let by_name = |n: &str| tools.iter().find(|t| t["name"] == n).expect("tool");
    for t in TASK_TOOLS {
        assert_eq!(
            by_name(t)["inputSchema"]["properties"]["json"]["type"],
            json!("boolean"),
            "{t} must offer the json argument"
        );
    }
    for t in ADVANCED_TOOLS {
        assert!(
            by_name(t)["inputSchema"]["properties"]
                .get("json")
                .is_none(),
            "{t} is not a task tool and must not offer json"
        );
    }
    assert_eq!(
        by_name("map")["inputSchema"]["properties"]
            .as_object()
            .map(serde_json::Map::len),
        Some(1),
        "map takes nothing but json"
    );
    assert_eq!(
        by_name("sync")["inputSchema"]["properties"]
            .as_object()
            .map(serde_json::Map::len),
        Some(1),
        "sync takes nothing but json"
    );
    assert_eq!(
        by_name("context")["inputSchema"]["required"],
        json!(["target"])
    );
    assert_eq!(
        by_name("explore")["inputSchema"]["required"],
        json!(["target"])
    );
    assert_eq!(
        by_name("explore")["inputSchema"]["properties"]["depth"]["enum"],
        json!(["context", "impact", "history", "all"])
    );
    assert_eq!(
        by_name("owners")["inputSchema"]["required"],
        json!(["path"])
    );
    assert_eq!(by_name("why")["inputSchema"]["required"], json!(["a", "b"]));
    assert_eq!(
        by_name("recall")["inputSchema"]["required"],
        json!(["topic"])
    );
    assert_eq!(
        by_name("remember")["inputSchema"]["required"],
        json!(["text"])
    );
    assert!(by_name("impact")["inputSchema"].get("required").is_none());
    assert_eq!(
        by_name("remember")["inputSchema"]["properties"]["kind"]["enum"],
        json!(["note", "decision", "todo"])
    );
}

/// Binding: the published server card lists exactly the tools the server
/// serves, in the same order.
///
/// The card is the whole surface, not the default listing: it says what
/// `mushroomdb mcp --all-tools` advertises and what every name in it can be
/// called as, so it is compared against that list rather than the eleven a
/// default session sees.
#[test]
fn server_card_lists_the_same_tools_in_the_same_order() {
    let card_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.well-known/mcp/server-card.json");
    let card: Js = serde_json::from_str(&std::fs::read_to_string(&card_path).expect("server card"))
        .expect("server card json");
    let listed: Vec<&str> = card["tools"]
        .as_array()
        .expect("card tools array")
        .iter()
        .map(|t| t.as_str().expect("card tool name"))
        .collect();

    let (res, out) = exchange_all_tools(open("card-drift"), &req(json!(1), "tools/list", None));
    assert!(res.is_ok(), "{res:?}");
    let replies = parse_lines(&out);
    let served: Vec<&str> = replies[0]["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|t| t["name"].as_str().expect("name"))
        .collect();

    assert_eq!(listed, served, "{} is out of date", card_path.display());
}

/// Binding: `map` on a store with nothing in it names the command that fills it.
#[test]
fn map_on_empty_store_is_helpful() {
    let (text, structured) = task_both(open("map-empty"), "map", json!({}));
    assert!(
        text.contains("empty store") && text.contains("ingest-git"),
        "empty map must say what to run: {text}"
    );
    assert_eq!(structured["files"], json!(0));
    assert_eq!(structured["symbols"], json!(0));
}

/// Binding: `map` counts what the graph holds and renders the same numbers.
#[test]
fn map_reports_the_graphed_repository() {
    let (text, structured) = task_both(code_store("map-full"), "map", json!({}));
    assert_eq!(structured["files"], json!(3));
    assert_eq!(structured["symbols"], json!(2));
    assert_eq!(structured["commits"], json!(4));
    assert_eq!(structured["authors"], json!(2));
    assert!(text.starts_with("mushroomdb map — 3 files"), "{text}");
    assert!(
        text.lines().count() <= 40,
        "map must stay within its line budget: {text}"
    );
}

/// Binding: a `map` served from a handle another handle wrote through is
/// current without the server being restarted.
#[test]
fn map_reflects_writes_made_by_another_handle() {
    let dir = tmp("map-follows");
    let db = SharedDb::open(&dir).expect("open");
    seed_code_graph(&db);

    let before = task_report(db.clone(), "map", json!({}));
    assert_eq!(before["files"], json!(3));

    // A second handle on the same directory — what a git hook is — inserts a
    // fourth file and exits, releasing the store's write lock.
    {
        let mut other = core_api::GraphDb::open(&dir).expect("second handle");
        other
            .insert_node(
                "File",
                "src/extra.rs",
                vec![
                    ("id".into(), s("src/extra.rs")),
                    ("path".into(), s("src/extra.rs")),
                    ("dir".into(), s("src")),
                    ("lang".into(), s("rust")),
                    ("lines".into(), Value::Int(9)),
                ],
            )
            .expect("insert through second handle");
    }
    // No wait: the read path checks the store on every read, so the very next
    // call sees the other handle's commit.
    let (text, after) = task_both(db.clone(), "map", json!({}));
    assert_eq!(
        after["files"],
        json!(4),
        "the server must follow the other handle's write: {text}"
    );
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Binding: `context` on a bare symbol name resolves it and reports its file,
/// its signature and what calls it.
#[test]
fn context_on_symbol() {
    let (text, structured) = task_both(
        code_store("context-symbol"),
        "context",
        json!({"target": "core::init"}),
    );
    assert_eq!(
        structured["target"]["symbol"]["key"],
        json!("src/core.rs#core::init")
    );
    assert_eq!(structured["file"], json!("src/core.rs"));
    assert_eq!(structured["signature"], json!("fn core::init()"));
    // Callers come back as call sites grouped by the file they sit in.
    let callers: Vec<(&str, Vec<&str>)> = structured["callers"]
        .as_array()
        .expect("callers")
        .iter()
        .map(|c| {
            (
                c["file"].as_str().expect("caller file"),
                c["symbols"]
                    .as_array()
                    .expect("caller symbols")
                    .iter()
                    .map(|s| s.as_str().expect("caller key"))
                    .collect(),
            )
        })
        .collect();
    assert_eq!(callers, vec![("src/web.rs", vec!["src/web.rs#web::serve"])]);
    assert!(text.contains("core::init"), "{text}");
    assert!(
        text.lines().count() <= 60,
        "context must stay within its line budget: {text}"
    );
}

/// Binding: a default `context` answers with a pointer and the graph's facts,
/// inside the reply budget; `full: true` quotes the body from the working tree.
#[test]
fn context_answers_with_pointers_by_default_and_bodies_on_full() {
    let db = code_store("context-pointers");
    // The seed's marker names a working tree that is not there, so `full`
    // would have nothing to quote. Point it at a real one.
    let repo = tmp("context-pointers-tree");
    std::fs::create_dir_all(repo.join("src")).expect("mkdir");
    let body: String = (1..=30).map(|n| format!("// line {n}\n")).collect();
    std::fs::write(repo.join("src/core.rs"), body).expect("write source");
    db.write()
        .set_prop(
            "__mushroomdb_git_sync__",
            "repo",
            s(&repo.to_string_lossy()),
        )
        .expect("repo");

    let (text, structured) = task_both(db.clone(), "context", json!({"target": "core::init"}));
    assert!(
        structured["source"].is_null(),
        "no body by default: {structured}"
    );
    assert!(
        text.contains("src/core.rs:10-20"),
        "pointer line present: {text}"
    );
    assert!(!text.contains("// line 10"), "no body by default: {text}");
    assert!(
        text.len() <= 4_800,
        "default context reply within budget: {}",
        text.len()
    );

    let (full, structured_full) =
        task_both(db, "context", json!({"target": "core::init", "full": true}));
    assert!(
        structured_full["source"].is_string(),
        "full quotes the working tree: {structured_full}"
    );
    assert!(full.contains("// line 10"), "the body is quoted: {full}");
    assert!(
        full.len() > text.len(),
        "the default is the shorter answer: {} vs {}",
        text.len(),
        full.len()
    );
}

/// Binding: `context` on a target the graph does not know says so rather than
/// failing.
#[test]
fn context_on_unknown_target_is_not_an_error() {
    let (text, structured) = task_both(
        code_store("context-unknown"),
        "context",
        json!({"target": "nope"}),
    );
    assert_eq!(structured["target"]["unknown"]["target"], json!("nope"));
    assert!(!text.is_empty());
}

/// Binding: `context` without a target is a tool error.
#[test]
fn context_without_target_is_a_tool_error() {
    let reply = one_task_call(code_store("context-no-target"), "context", json!({}));
    assert!(error_text(&reply).contains("target"));
}

/// Binding: `explore` at `all` is one call for the three answers — the
/// context, the blast radius, and who owns it — inside the default budget.
#[test]
fn explore_all_composes_context_impact_and_history_within_budget() {
    let (text, structured) = task_both(
        code_store("explore-all"),
        "explore",
        json!({"target": "core::init", "depth": "all"}),
    );
    assert_eq!(
        structured["context"]["target"]["symbol"]["key"],
        json!("src/core.rs#core::init")
    );
    assert_eq!(structured["depth"], json!("all"));
    assert!(
        structured["impact"].is_object() && structured["owners"].is_object(),
        "all carries the blast radius and the ownership: {structured}"
    );
    assert!(text.len() <= 4_800, "{} bytes:\n{text}", text.len());
    for want in ["callers", "impact:", "owner:"] {
        assert!(text.contains(want), "the digest is missing {want}:\n{text}");
    }
    // The partners reach a caller through the report; the digest names them
    // once, on the context section's own `co-change` line.
    assert!(
        structured["partners"].is_array(),
        "the report carries the co-change partners: {structured}"
    );
    assert!(
        !text.contains("changes with"),
        "and the digest does not repeat them:\n{text}"
    );
}

/// Binding: the default depth is `context`, and it costs neither the blast
/// radius nor the history.
#[test]
fn explore_defaults_to_context_depth() {
    let (text, structured) = task_both(
        code_store("explore-default"),
        "explore",
        json!({"target": "core::init"}),
    );
    assert_eq!(structured["depth"], json!("context"));
    assert!(structured["impact"].is_null() && structured["owners"].is_null());
    assert!(!text.contains("impact:"), "{text}");
}

/// Binding: `budget` is in tokens and caps the whole reply, framing included.
#[test]
fn explore_budget_caps_the_reply() {
    let reply = one_task_call(
        code_store("explore-budget"),
        "explore",
        json!({"target": "core::init", "depth": "all", "budget": 200}),
    );
    let text = task_reply(&reply);
    assert!(
        text.len() <= 200 * 4,
        "a 200-token budget is 800 bytes, got {}:\n{text}",
        text.len()
    );
    assert!(!text.is_empty(), "the header survives any budget");
}

/// Binding: a depth that is not one of the four is a tool error naming them.
#[test]
fn explore_with_an_unknown_depth_is_a_tool_error() {
    let reply = one_task_call(
        code_store("explore-depth"),
        "explore",
        json!({"target": "core::init", "depth": "everything"}),
    );
    let msg = error_text(&reply);
    assert!(msg.contains("depth") && msg.contains("history"), "{msg}");
}

/// Binding: `impact` on an explicit file list marks the partners that are
/// themselves in that list.
#[test]
fn impact_explicit_files_marks_modified() {
    let (text, structured) = task_both(
        code_store("impact-explicit"),
        "impact",
        json!({"files": ["src/core.rs", "src/web.rs"]}),
    );
    let files = structured["files"].as_array().expect("files");
    assert_eq!(files.len(), 2);
    assert_eq!(files[0]["path"], json!("src/core.rs"));

    let partners = files[0]["partners"].as_array().expect("partners");
    let web = partners
        .iter()
        .find(|p| p["path"] == "src/web.rs")
        .expect("src/web.rs is a co-change partner of src/core.rs");
    assert_eq!(web["modified"], json!(true), "it is in the changed set");
    let util = partners.iter().find(|p| p["path"] == "src/util.rs");
    if let Some(util) = util {
        assert_eq!(
            util["modified"],
            json!(false),
            "it is not in the changed set"
        );
    }
    assert!(text.contains("src/web.rs 1.00 modified"), "{text}");
    assert!(
        text.lines().count() <= 25,
        "impact must stay within its line budget: {text}"
    );
}

/// Binding: `impact` reports a path the graph has never seen as unknown.
#[test]
fn impact_reports_unknown_paths() {
    let (text, structured) = task_both(
        code_store("impact-unknown"),
        "impact",
        json!({"files": ["src/core.rs", "no/such.rs"]}),
    );
    assert_eq!(structured["unknown"], json!(["no/such.rs"]));
    assert!(text.contains("unknown: no/such.rs"), "{text}");
}

// The default `impact` file list — where the diff comes from, how it is
// filtered, and what happens with no checkout — is decided before the graph is
// touched, and is covered by the unit tests in `crates/server/src/mcp_tasks.rs`.
// They take `$CLAUDE_PROJECT_DIR` as an argument; asserting it here would mean
// setting a process-global variable in a binary whose other tests read the
// environment concurrently.

/// Binding: `owners` names the top author once, with the key in parentheses.
#[test]
fn owners_reports_the_top_author() {
    let (text, structured) = task_both(
        code_store("owners-ok"),
        "owners",
        json!({"path": "src/core.rs"}),
    );
    assert_eq!(structured["path"], json!("src/core.rs"));
    assert!(text.contains("Ada Example (a@example.test)"), "{text}");
    assert!(
        text.lines().count() <= 25,
        "owners must stay within its line budget: {text}"
    );
}

/// Binding: `owners` on a path the store holds no file for is a tool error.
#[test]
fn owners_unknown_path_error() {
    let reply = one_task_call(
        code_store("owners-unknown"),
        "owners",
        json!({"path": "no/such.rs"}),
    );
    let msg = error_text(&reply);
    assert!(msg.contains("no/such.rs"), "{msg}");
}

/// Binding: `why` names both unknown keys rather than only the first.
#[test]
fn why_unknown_keys_say_unknown() {
    let (text, structured) = task_both(
        code_store("why-unknown"),
        "why",
        json!({"a": "nope", "b": "zzz"}),
    );
    assert!(text.contains("unknown:"), "{text}");
    assert_eq!(structured["unknown"], json!(["nope", "zzz"]));
}

/// Binding: `why` between two co-changed files reports the link and its
/// evidence.
#[test]
fn why_reports_the_link_between_two_files() {
    let (text, structured) = task_both(
        code_store("why-link"),
        "why",
        json!({"a": "src/core.rs", "b": "src/web.rs"}),
    );
    let links = structured["links"].as_array().expect("links");
    assert!(
        links.iter().any(|l| l["edge_type"] == "CO_CHANGED"),
        "expected a CO_CHANGED link: {structured}"
    );
    assert!(text.contains("CO_CHANGED"), "{text}");
    assert!(
        text.lines().count() <= 25,
        "why must stay within its line budget: {text}"
    );
}

/// Binding: `recall` turns a topic that names something into a digest of
/// pointers to the nodes nearest it.
#[test]
fn recall_returns_digest() {
    let (text, structured) = task_both(
        code_store("recall-topic"),
        "recall",
        json!({"topic": "src/core.rs"}),
    );
    assert_eq!(structured["topic"], json!("src/core.rs"));
    assert!(
        text.contains("src/core.rs"),
        "the digest must name the matching node: {text}"
    );
    // `text` here is the digest with its framing line stripped by `task_reply`,
    // so putting it back must give exactly what core-api produced.
    assert_eq!(
        structured["digest"],
        json!(format!("{UNTRUSTED_FRAMING}{text}")),
        "the reply shows the digest unaltered"
    );
}

/// Binding: a topic with nothing searchable in it is answered, not an error.
#[test]
fn recall_on_an_unsearchable_topic_says_nothing_matched() {
    let text = task_reply(&one_task_call(
        code_store("recall-empty"),
        "recall",
        json!({"topic": "!!! ???"}),
    ));
    assert!(text.contains("nothing"), "{text}");
}

/// Binding: `remember` writes a note and returns its key; unknown `about`
/// keys are all named, and nothing is written.
#[test]
fn remember_writes_note_and_rejects_unknown_about() {
    let db = code_store("remember");

    let reply = one_task_call(
        db.clone(),
        "remember",
        json!({"text": "core::init is the entry point", "about": ["src/core.rs"], "kind": "decision"}),
    );
    let text = task_reply(&reply);
    // The digest is the only place the key is now reported, so it has to name
    // it in full: one call, one note, and the caller can cite what it reads.
    let key = text
        .split_whitespace()
        .find(|w| w.starts_with("note:"))
        .unwrap_or_else(|| panic!("the render must name the note key: {text}"))
        .to_string();
    assert_eq!(key.len(), "note:".len() + 16, "{key}");
    assert!(db.read().has_node(&key), "the note must be in the store");

    // Two unknown keys: both are named, sorted, and nothing is written.
    let before = db.read().node_count();
    let reply = one_task_call(
        db.clone(),
        "remember",
        json!({"text": "about nothing that exists", "about": ["zzz.rs", "no/such.rs"]}),
    );
    let msg = error_text(&reply);
    assert!(
        msg.contains("no/such.rs") && msg.contains("zzz.rs"),
        "{msg}"
    );
    assert_eq!(db.read().node_count(), before, "nothing may be written");
}

/// Binding: `remember` needs text.
#[test]
fn remember_without_text_is_a_tool_error() {
    let reply = one_task_call(code_store("remember-no-text"), "remember", json!({}));
    assert!(error_text(&reply).contains("text"));
}

/// Binding: `remember` rejects a `kind` outside the enum.
#[test]
fn remember_rejects_an_unknown_kind() {
    let reply = one_task_call(
        code_store("remember-kind"),
        "remember",
        json!({"text": "hello", "kind": "shopping list"}),
    );
    assert!(error_text(&reply).contains("kind"));
}

/// Binding: `sync` cannot run without knowing where the store is, and says so.
#[test]
fn sync_without_db_dir_is_a_tool_error() {
    let reply = one_task_call(code_store("sync-no-dir"), "sync", json!({}));
    let msg = error_text(&reply);
    assert!(msg.contains("store path unknown"), "{msg}");
}

/// Binding: with a store path, `sync` runs this binary and reports what it
/// could not do rather than panicking.
#[test]
fn sync_with_db_dir_reports_the_child_failure() {
    let dir = tmp("sync-with-dir");
    let db = SharedDb::open(&dir).expect("open");
    let (res, out) = exchange_at(db.clone(), Some(dir.clone()), &call(1, "sync", json!({})));
    assert!(res.is_ok(), "{res:?}");
    let reply = parse_lines(&out).remove(0);
    // `current_exe()` under `cargo test` is this test binary, not the CLI, so
    // the run cannot produce a sync report. What matters is that the tool
    // reports that as an error instead of hanging or panicking.
    let msg = error_text(&reply);
    assert!(msg.contains("sync"), "{msg}");
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Binding: every task tool stamps its text with the untrusted-data framing
/// line, exactly once, before any repository content.
///
/// `task_reply` asserts this on each tool's own test too; this one sweeps all
/// nine in one place so a tenth tool cannot be added without a framed answer.
#[test]
fn every_task_tool_frames_its_text_as_untrusted() {
    let db = code_store("framing");
    let args = |tool: &str| match tool {
        "explore" | "context" => json!({"target": "core::init"}),
        "impact" => json!({"files": ["src/core.rs"]}),
        "owners" => json!({"path": "src/core.rs"}),
        "why" => json!({"a": "src/core.rs", "b": "src/web.rs"}),
        "recall" => json!({"topic": "src/core.rs"}),
        "remember" => json!({"text": "framing check", "about": ["src/core.rs"]}),
        _ => json!({}),
    };
    for tool in TASK_TOOLS {
        // `sync` has no store path here, so it is the one tool that answers with
        // an error; a tool error is a message to the caller, not graph content,
        // and carries no framing by design.
        if tool == "sync" {
            let reply = one_task_call(db.clone(), tool, args(tool));
            assert!(
                !error_text(&reply).starts_with(UNTRUSTED_FRAMING),
                "a tool error is not graph content"
            );
            continue;
        }
        let reply = one_task_call(db.clone(), tool, args(tool));
        let full = reply["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("{tool}: content[0].text"));
        assert!(
            full.starts_with(UNTRUSTED_FRAMING),
            "{tool} must open with the framing line, got {full:?}"
        );
        assert_eq!(
            full.matches(UNTRUSTED_FRAMING).count(),
            1,
            "{tool} must carry the framing line exactly once"
        );
        // And the report is not shipped beside it.
        assert!(
            reply["result"].get("structuredContent").is_none(),
            "{tool} must answer with text alone"
        );
    }
}

/// Binding: `json: true` swaps the digest for the report — the text content is
/// the serialised report, there is still no `structuredContent`, and the two
/// modes describe the same call.
#[test]
fn json_true_answers_with_the_report_as_the_text() {
    let db = code_store("json-arg");
    let (digest, report) = task_both(db, "impact", json!({"files": ["src/core.rs"]}));
    assert!(digest.starts_with("mushroomdb impact"), "{digest}");
    assert_eq!(report["files"][0]["path"], json!("src/core.rs"));
    assert!(
        report.get("text").is_none(),
        "the report must not carry a copy of the digest: {report}"
    );
}

/// Binding: a `json` reply carries no framing line — it is a document to parse,
/// not prose to read — but every string in it is still sanitized, because the
/// graph content in it came from the repository just as the digest's did.
///
/// `serde_json` escaping keeps a control character from breaking the document.
/// It does nothing about what the reader sees after parsing, which is where an
/// escape sequence, a carriage return or a `DEL` would land.
#[test]
fn a_json_reply_is_unframed_and_sanitized() {
    // The reports sanitize the graph content they are built from, so the field
    // that proves this pass runs is one they do not touch: `recall` echoes the
    // caller's own topic back verbatim.
    let hostile = "core \u{1b}[31m\rforged\u{7f} and\ttabbed";
    let db = code_store("json-sanitize");
    let reply = one_task_call(
        db.clone(),
        "recall",
        json!({"topic": hostile, "json": true}),
    );
    let raw = task_text(&reply);
    assert!(
        !raw.starts_with(UNTRUSTED_FRAMING),
        "a json reply is a document to parse: {raw}"
    );

    let report: Js = serde_json::from_str(&raw).expect("json reply must parse");
    let mut strings: Vec<String> = Vec::new();
    collect_strings(&report, &mut strings);
    for value in &strings {
        assert!(
            !value
                .chars()
                .any(|c| c.is_ascii_control() && c != '\n' && c != '\t'),
            "a control character reached the reply: {value:?}"
        );
    }
    assert_eq!(
        report["topic"],
        json!("core  [31m forged  and\ttabbed"),
        "escapes, carriage return and DEL become spaces; the tab is left alone"
    );

    // And the same content on the digest path is sanitized as it always was.
    let text = task_reply(&one_task_call(db, "recall", json!({"topic": hostile})));
    assert!(
        !text.chars().any(|c| c.is_ascii_control() && c != '\n'),
        "the rendered digest keeps its own line structure and nothing else: {text:?}"
    );
}

/// Binding: the one control character a JSON value keeps is the newline, and
/// keeping it is what makes the report faithful.
///
/// `recall`'s report carries the whole rendered digest under `digest`, and
/// `context` carries quoted source. Those newlines are the document's own
/// structure, not something a contributor injected — a JSON value is delimited
/// by the grammar, so nothing inside one can forge a line the way it could in
/// a line-structured digest.
#[test]
fn a_json_reply_keeps_the_newlines_of_a_multi_line_value() {
    let db = code_store("json-multiline");
    let report = task_report(db, "recall", json!({"topic": "src/core.rs"}));
    let digest = report["digest"].as_str().expect("digest");
    assert!(
        digest.lines().count() > 2,
        "the rendered digest must survive as a document, not one flat line: {digest:?}"
    );
    assert!(digest.starts_with(UNTRUSTED_FRAMING), "{digest:?}");
}

/// Every string value in a JSON reply, at any depth.
fn collect_strings(value: &Js, out: &mut Vec<String>) {
    match value {
        Js::String(s) => out.push(s.clone()),
        Js::Array(items) => items.iter().for_each(|v| collect_strings(v, out)),
        Js::Object(map) => map.values().for_each(|v| collect_strings(v, out)),
        _ => {}
    }
}

/// Binding: `json` is a boolean, and a caller that sends something else is told
/// so rather than served a digest it did not ask for.
#[test]
fn a_wrong_typed_json_argument_is_a_tool_error() {
    let reply = one_task_call(code_store("json-bad"), "map", json!({"json": "yes"}));
    assert!(error_text(&reply).contains("json must be a boolean"));
}

/// Binding: `recall` carries the framing line its own digest already emits, and
/// does not gain a second one.
#[test]
fn recall_is_framed_once_not_twice() {
    let db = code_store("recall-framing");
    let reply = one_task_call(db.clone(), "recall", json!({"topic": "src/core.rs"}));
    let full = task_text(&reply);
    assert_eq!(full.matches(UNTRUSTED_FRAMING).count(), 1, "{full}");
    // The digest core-api produced is what was shown, unaltered.
    assert_eq!(
        task_report(db, "recall", json!({"topic": "src/core.rs"}))["digest"],
        json!(full),
        "recall's own digest already opens with the framing line"
    );
}
