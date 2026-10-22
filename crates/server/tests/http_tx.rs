//! Interactive transactions over HTTP (0.10 TX-4): `/tx/begin`, `/tx/{id}/run`,
//! `/tx/{id}/commit`, `/tx/{id}/rollback`, `DELETE /tx/{id}` and `GET /tx/{id}`.
//!
//! The shapes and status codes are the ones the TypeScript client codes against:
//! 409 commit conflict (or a busy transaction), 404 unknown or not yours, 410
//! idle-expired, 503 over the open-transaction limit.

use mushroomdb::{schema::Schema, RoleDef, SharedDb, Value};
use serde_json::{json, Value as Json};
use server::TxLimits;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

const ADM_KEY: &str = "adm-key-0123456789abcdef0123456789abcdef";
const WR_KEY: &str = "wr-key-0123456789abcdef0123456789abcdef";
const RD_KEY: &str = "rd-key-0123456789abcdef0123456789abcdef";
const OTHER_ADM_KEY: &str = "oth-key-0123456789abcdef0123456789abcdef";

fn tmp(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let d = std::env::temp_dir().join(format!(
        "graphdb-srv-tx-{name}-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn limits(max_open: usize, idle_ms: u64) -> TxLimits {
    TxLimits {
        max_open,
        idle_timeout: Duration::from_millis(idle_ms),
        max_lifetime: Duration::from_secs(300),
    }
}

struct Reply {
    status: u16,
    body: Json,
    retry_after: Option<String>,
}

struct Srv {
    base: String,
    http: reqwest::Client,
    db: SharedDb,
}

impl Srv {
    async fn call(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<Json>,
    ) -> Reply {
        let method = reqwest::Method::from_bytes(method.as_bytes()).unwrap();
        let mut req = self.http.request(method, format!("{}{path}", self.base));
        if let Some(t) = token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        // The TypeScript client sends this header on every call, body or not.
        req = req.header("content-type", "application/json");
        if let Some(b) = body {
            req = req.body(b.to_string());
        }
        let resp = req.send().await.unwrap();
        let status = resp.status().as_u16();
        let retry_after = resp
            .headers()
            .get("retry-after")
            .map(|v| v.to_str().unwrap().to_string());
        let text = resp.text().await.unwrap();
        let body = serde_json::from_str(&text).unwrap_or(Json::String(text));
        Reply {
            status,
            body,
            retry_after,
        }
    }

    async fn begin(&self, token: Option<&str>) -> Reply {
        self.call("POST", "/tx/begin", token, None).await
    }

    async fn begin_id(&self, token: Option<&str>) -> String {
        let r = self.begin(token).await;
        assert_eq!(r.status, 200, "{}", r.body);
        r.body["tx_id"].as_str().unwrap().to_string()
    }

    async fn run(&self, id: &str, token: Option<&str>, cypher: &str) -> Reply {
        self.call(
            "POST",
            &format!("/tx/{id}/run"),
            token,
            Some(json!({"cypher": cypher, "params": {}})),
        )
        .await
    }

    async fn post(&self, id: &str, token: Option<&str>, verb: &str) -> Reply {
        self.call("POST", &format!("/tx/{id}/{verb}"), token, None)
            .await
    }

    async fn query(&self, token: Option<&str>, cypher: &str) -> Json {
        let r = self
            .call(
                "POST",
                "/query?format=json",
                token,
                Some(json!({"cypher": cypher})),
            )
            .await;
        assert_eq!(r.status, 200, "{}", r.body);
        r.body
    }
}

async fn serve(
    name: &str,
    token: Option<&str>,
    role_tokens: HashMap<String, String>,
    keys: Option<mushroomdb::AccessKeys>,
    tx_limits: TxLimits,
) -> Srv {
    let db = SharedDb::open(&tmp(name)).unwrap();
    db.write()
        .apply_schema(&Schema {
            roles: vec![RoleDef {
                name: "viewer".into(),
                labels: vec!["Person".into()],
                keys: vec![],
                visible_where: None,
                namespaces: None,
                write: None,
            }],
            ..Default::default()
        })
        .unwrap();
    db.write()
        .insert_node(
            "Person",
            "seed",
            vec![("id".into(), Value::Str("seed".into()))],
        )
        .unwrap();
    db.write()
        .insert_node("Secret", "s1", vec![("id".into(), Value::Str("s1".into()))])
        .unwrap();
    let app = server::router_with_tx_limits(
        db.clone(),
        token.map(String::from),
        role_tokens,
        keys,
        tx_limits,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Srv {
        base: format!("http://{addr}"),
        http: reqwest::Client::new(),
        db,
    }
}

async fn plain(name: &str, tx_limits: TxLimits) -> Srv {
    serve(name, None, HashMap::new(), None, tx_limits).await
}

fn keyed(name: &str) -> mushroomdb::AccessKeys {
    let dir = tmp(&format!("keys-{name}"));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("keys.json");
    let entries = [
        (ADM_KEY, "admin", None),
        (OTHER_ADM_KEY, "admin", None),
        (WR_KEY, "write", None),
        (RD_KEY, "read", Some("viewer")),
    ];
    let keys: Vec<Json> = entries
        .iter()
        .enumerate()
        .map(|(n, (token, tier, role))| {
            let mut entry = json!({
                "name": format!("key{n}"),
                "hash": mushroomdb::hash_token(token).unwrap(),
                "tier": tier,
            });
            if let Some(role) = role {
                entry["role"] = json!(role);
            }
            entry
        })
        .collect();
    std::fs::write(&path, json!({ "keys": keys }).to_string()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    mushroomdb::AccessKeys::load(&path).unwrap()
}

#[tokio::test]
async fn begin_run_commit_roundtrip() {
    let s = plain("roundtrip", limits(100, 30_000)).await;
    let begun = s.begin(None).await;
    assert_eq!(begun.status, 200, "{}", begun.body);
    let id = begun.body["tx_id"].as_str().unwrap();
    assert_eq!(id.len(), 32);
    assert!(id.bytes().all(|b| b.is_ascii_hexdigit()), "{id}");
    let expires = begun.body["expires_at"].as_str().unwrap();
    assert!(expires.ends_with('Z') && expires.contains('T'), "{expires}");

    let created = s.run(id, None, "CREATE (n:Person {id: 'tx1'})").await;
    assert_eq!(created.status, 200, "{}", created.body);

    // The transaction reads its own write; nothing outside does yet.
    let inside = s
        .run(id, None, "MATCH (n:Person {id: 'tx1'}) RETURN n.id AS id")
        .await;
    assert_eq!(inside.body["columns"], json!(["id"]));
    assert_eq!(inside.body["rows"], json!([["tx1"]]));
    let outside = s
        .query(None, "MATCH (n:Person {id: 'tx1'}) RETURN n.id AS id")
        .await;
    assert_eq!(outside["rows"], json!([]));

    let info = s.call("GET", &format!("/tx/{id}"), None, None).await;
    assert_eq!(info.status, 200);
    assert_eq!(info.body["tx_id"], json!(id));
    assert_eq!(info.body["op_count"], json!(2));

    let committed = s.post(id, None, "commit").await;
    assert_eq!(committed.status, 200, "{}", committed.body);
    let bookmark = committed.body["bookmark"]
        .as_u64()
        .expect("numeric bookmark");
    assert_eq!(
        committed.body["bookmark_text"],
        json!(format!("mushroomdb:commit:{bookmark}"))
    );
    assert_eq!(bookmark, s.db.reader().commit_seq());
    let after = s
        .query(None, "MATCH (n:Person {id: 'tx1'}) RETURN n.id AS id")
        .await;
    assert_eq!(after["rows"], json!([["tx1"]]));

    // The transaction is gone once it committed.
    assert_eq!(s.run(id, None, "RETURN 1 AS x").await.status, 404);
}

#[tokio::test]
async fn rollback_discards_the_writes() {
    let s = plain("rollback", limits(100, 30_000)).await;
    let id = s.begin_id(None).await;
    s.run(&id, None, "CREATE (n:Person {id: 'gone'})").await;
    let r = s.post(&id, None, "rollback").await;
    assert_eq!(r.status, 200, "{}", r.body);
    let after = s
        .query(None, "MATCH (n:Person {id: 'gone'}) RETURN n.id AS id")
        .await;
    assert_eq!(after["rows"], json!([]));
    assert_eq!(s.post(&id, None, "commit").await.status, 404);
}

#[tokio::test]
async fn delete_rolls_back() {
    let s = plain("delete", limits(100, 30_000)).await;
    let id = s.begin_id(None).await;
    s.run(&id, None, "CREATE (n:Person {id: 'gone2'})").await;
    let r = s.call("DELETE", &format!("/tx/{id}"), None, None).await;
    assert_eq!(r.status, 200, "{}", r.body);
    let after = s
        .query(None, "MATCH (n:Person {id: 'gone2'}) RETURN n.id AS id")
        .await;
    assert_eq!(after["rows"], json!([]));
    let info = s.call("GET", &format!("/tx/{id}"), None, None).await;
    assert_eq!(info.status, 404);
}

#[tokio::test]
async fn a_failed_statement_leaves_the_transaction_usable() {
    let s = plain("failed-stmt", limits(100, 30_000)).await;
    let id = s.begin_id(None).await;
    let bad = s.run(&id, None, "THIS IS NOT CYPHER").await;
    assert_eq!(bad.status, 400, "{}", bad.body);
    let missing = s
        .call(
            "POST",
            &format!("/tx/{id}/run"),
            None,
            Some(json!({"params": {}})),
        )
        .await;
    assert_eq!(missing.status, 400);
    let ok = s.run(&id, None, "RETURN 1 AS x").await;
    assert_eq!(ok.status, 200, "{}", ok.body);
}

#[tokio::test]
async fn an_idle_transaction_expires_with_410_and_is_reaped() {
    let s = plain("expiry", limits(1, 300)).await;
    let id = s.begin_id(None).await;
    s.run(&id, None, "CREATE (n:Person {id: 'late'})").await;
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let r = s.run(&id, None, "RETURN 1 AS x").await;
    assert_eq!(r.status, 410, "{}", r.body);
    assert_eq!(s.post(&id, None, "commit").await.status, 410);
    // Rolled back, not committed.
    let after = s
        .query(None, "MATCH (n:Person {id: 'late'}) RETURN n.id AS id")
        .await;
    assert_eq!(after["rows"], json!([]));
    // The slot is free again (limit is 1), and an id never issued is 404.
    let again = s.begin(None).await;
    assert_eq!(again.status, 200, "{}", again.body);
    assert_eq!(
        s.run(&"0".repeat(32), None, "RETURN 1 AS x").await.status,
        404
    );
}

#[tokio::test]
async fn the_idle_timer_restarts_on_every_run() {
    let s = plain("restart", limits(100, 1_000)).await;
    let id = s.begin_id(None).await;
    for _ in 0..4 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let r = s.run(&id, None, "RETURN 1 AS x").await;
        assert_eq!(r.status, 200, "{}", r.body);
    }
    assert_eq!(s.post(&id, None, "commit").await.status, 200);
}

#[tokio::test]
async fn a_concurrent_write_makes_commit_a_409() {
    let s = plain("conflict", limits(100, 30_000)).await;
    let id = s.begin_id(None).await;
    let r = s
        .run(&id, None, "MATCH (n:Person {id: 'seed'}) SET n.v = 1")
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    s.query(None, "MATCH (n:Person {id: 'seed'}) SET n.v = 2")
        .await;
    let c = s.post(&id, None, "commit").await;
    assert_eq!(c.status, 409, "{}", c.body);
    assert!(c.body["error"].as_str().unwrap().contains("CAS conflict"));
    assert_eq!(c.body["code"], json!("conflict"));
    // The transaction ended with the conflict.
    assert_eq!(s.post(&id, None, "rollback").await.status, 404);
    let v = s
        .query(None, "MATCH (n:Person {id: 'seed'}) RETURN n.v AS v")
        .await;
    assert_eq!(v["rows"], json!([[2]]));
}

#[tokio::test]
async fn ddl_inside_a_transaction_is_refused() {
    let s = plain("ddl", limits(100, 30_000)).await;
    let id = s.begin_id(None).await;
    let r = s.run(&id, None, "CREATE INDEX i FOR (n:P) ON (n.a)").await;
    assert_eq!(r.status, 400, "{}", r.body);
    assert!(
        r.body["error"].as_str().unwrap().contains("transaction"),
        "{}",
        r.body
    );
    // SHOW is a read.
    assert_eq!(s.run(&id, None, "SHOW INDEXES").await.status, 200);
}

#[tokio::test]
async fn over_the_open_limit_is_503_with_retry_after() {
    let s = plain("limit", limits(2, 30_000)).await;
    let a = s.begin_id(None).await;
    let _b = s.begin_id(None).await;
    let third = s.begin(None).await;
    assert_eq!(third.status, 503, "{}", third.body);
    assert_eq!(third.retry_after.as_deref(), Some("1"));
    assert_eq!(s.post(&a, None, "rollback").await.status, 200);
    assert_eq!(s.begin(None).await.status, 200);
}

#[tokio::test]
async fn auth_is_required_and_a_tx_belongs_to_its_identity() {
    let s = serve(
        "owner",
        Some("full-tok"),
        HashMap::new(),
        Some(keyed("owner")),
        limits(100, 30_000),
    )
    .await;
    assert_eq!(s.begin(None).await.status, 401);
    assert_eq!(s.begin(Some("nope")).await.status, 401);

    let id = s.begin_id(Some(ADM_KEY)).await;
    for stranger in ["full-tok", OTHER_ADM_KEY, WR_KEY, RD_KEY] {
        let t = Some(stranger);
        assert_eq!(
            s.run(&id, t, "RETURN 1 AS x").await.status,
            404,
            "{stranger}"
        );
        assert_eq!(s.post(&id, t, "commit").await.status, 404, "{stranger}");
        assert_eq!(s.post(&id, t, "rollback").await.status, 404, "{stranger}");
        let info = s.call("GET", &format!("/tx/{id}"), t, None).await;
        assert_eq!(info.status, 404, "{stranger}");
        let del = s.call("DELETE", &format!("/tx/{id}"), t, None).await;
        assert_eq!(del.status, 404, "{stranger}");
    }
    // The owner is unaffected.
    assert_eq!(s.run(&id, Some(ADM_KEY), "RETURN 1 AS x").await.status, 200);
    assert_eq!(s.post(&id, Some(ADM_KEY), "commit").await.status, 200);
}

#[tokio::test]
async fn a_write_tier_key_can_commit_a_write() {
    let s = serve(
        "write-tier",
        None,
        HashMap::new(),
        Some(keyed("write-tier")),
        limits(100, 30_000),
    )
    .await;
    let id = s.begin_id(Some(WR_KEY)).await;
    let r = s
        .run(&id, Some(WR_KEY), "CREATE (n:Person {id: 'w1'})")
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(s.post(&id, Some(WR_KEY), "commit").await.status, 200);
    let after = s
        .query(
            Some(WR_KEY),
            "MATCH (n:Person {id: 'w1'}) RETURN n.id AS id",
        )
        .await;
    assert_eq!(after["rows"], json!([["w1"]]));
}

#[tokio::test]
async fn a_read_tier_key_reads_its_role_view_and_never_writes() {
    let s = serve(
        "read-tier",
        None,
        HashMap::new(),
        Some(keyed("read-tier")),
        limits(100, 30_000),
    )
    .await;
    let id = s.begin_id(Some(RD_KEY)).await;

    let seen = s
        .run(&id, Some(RD_KEY), "MATCH (n:Person) RETURN n.id AS id")
        .await;
    assert_eq!(seen.status, 200, "{}", seen.body);
    assert_eq!(seen.body["rows"], json!([["seed"]]));
    // `Secret` is outside the viewer role.
    let hidden = s
        .run(&id, Some(RD_KEY), "MATCH (n:Secret) RETURN n.id AS id")
        .await;
    assert_eq!(hidden.body["rows"], json!([]));

    let write = s
        .run(&id, Some(RD_KEY), "CREATE (n:Person {id: 'r1'})")
        .await;
    assert_eq!(write.status, 403, "{}", write.body);
    assert_eq!(
        write.body["error"],
        json!("read-tier key: writes are not permitted")
    );
    // The refusal did not end the transaction.
    assert_eq!(s.run(&id, Some(RD_KEY), "RETURN 1 AS x").await.status, 200);
    assert_eq!(s.post(&id, Some(RD_KEY), "commit").await.status, 200);
    let after = s
        .query(
            Some(ADM_KEY),
            "MATCH (n:Person {id: 'r1'}) RETURN n.id AS id",
        )
        .await;
    assert_eq!(after["rows"], json!([]));
}

#[tokio::test]
async fn a_role_token_reads_under_its_role_and_cannot_write() {
    let tokens: HashMap<String, String> = [("role-tok".to_string(), "viewer".to_string())].into();
    let s = serve(
        "role-tok",
        Some("full-tok"),
        tokens,
        None,
        limits(100, 30_000),
    )
    .await;
    let id = s.begin_id(Some("role-tok")).await;
    let hidden = s
        .run(&id, Some("role-tok"), "MATCH (n:Secret) RETURN n.id AS id")
        .await;
    assert_eq!(hidden.status, 200, "{}", hidden.body);
    assert_eq!(hidden.body["rows"], json!([]));
    let write = s
        .run(&id, Some("role-tok"), "CREATE (n:Person {id: 'r2'})")
        .await;
    assert_eq!(write.status, 403, "{}", write.body);
    assert_eq!(s.post(&id, Some("role-tok"), "rollback").await.status, 200);
    // The full token cannot see the role's transaction.
    let other = s.begin_id(Some("role-tok")).await;
    assert_eq!(
        s.run(&other, Some("full-tok"), "RETURN 1 AS x")
            .await
            .status,
        404
    );
}

/// A statement that takes a good while: every pair of `Big` nodes.
const SLOW: &str = "MATCH (a:Big), (b:Big) WHERE a.id <> b.id RETURN count(a) AS c";

fn seed_big(db: &SharedDb, n: usize) {
    let ops = (0..n)
        .map(|i| mushroomdb::BatchOp::InsertNode {
            label: "Big".into(),
            key: format!("big{i}"),
            props: vec![("id".into(), Value::Str(format!("big{i}")))],
        })
        .collect();
    db.submit_batch(ops).unwrap();
}

/// Drops the request future after `ms`, as a client that hangs up does.
async fn cancelled_run(s: &Srv, id: &str, ms: u64) {
    let fut = s.run(id, None, SLOW);
    let _ = tokio::time::timeout(Duration::from_millis(ms), fut).await;
}

#[tokio::test]
async fn a_cancelled_run_hands_the_transaction_back() {
    let s = plain("cancel-run", limits(100, 30_000)).await;
    seed_big(&s.db, 1_500);
    let id = s.begin_id(None).await;
    cancelled_run(&s, &id, 50).await;
    // The statement finishes without its request, and the transaction is
    // usable again, with that statement counted.
    let mut counted = false;
    for _ in 0..600 {
        let info = s.call("GET", &format!("/tx/{id}"), None, None).await;
        assert_eq!(info.status, 200, "{}", info.body);
        if info.body["op_count"] == json!(1) {
            counted = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(counted, "the transaction never came back");
    assert_eq!(s.run(&id, None, "RETURN 1 AS x").await.status, 200);
    assert_eq!(s.post(&id, None, "commit").await.status, 200);
}

#[tokio::test]
async fn a_cancelled_commit_still_ends_the_transaction() {
    let s = plain("cancel-commit", limits(1, 30_000)).await;
    seed_big(&s.db, 10);
    let id = s.begin_id(None).await;
    s.run(&id, None, "CREATE (n:Person {id: 'cc1'})").await;
    // Hang up at once: the commit may or may not have started, but the
    // handler's drop must not strand the slot.
    let fut = s.post(&id, None, "commit");
    let _ = tokio::time::timeout(Duration::from_millis(1), fut).await;
    for _ in 0..200 {
        // Either the commit ran (the slot is free) or the request never
        // reached the handler (the transaction is still ours to end).
        let r = s.post(&id, None, "rollback").await;
        if r.status != 409 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(s.begin(None).await.status, 200, "slot was stranded");
}

#[tokio::test]
async fn cancelled_requests_do_not_exhaust_the_open_limit() {
    // Limit 2, 400 ms idle: eight transactions, each hung up on mid-statement
    // and left to expire, only succeed if every abandoned slot is reclaimed.
    let s = plain("cancel-many", limits(2, 400)).await;
    seed_big(&s.db, 1_500);
    for round in 0..8 {
        let mut id = None;
        for _ in 0..200 {
            let r = s.begin(None).await;
            if r.status == 200 {
                id = Some(r.body["tx_id"].as_str().unwrap().to_string());
                break;
            }
            assert_eq!(r.status, 503, "round {round}: {}", r.body);
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let id = id.unwrap_or_else(|| panic!("round {round}: no slot ever came free"));
        cancelled_run(&s, &id, 30).await;
    }
}

#[tokio::test]
async fn a_second_request_while_one_runs_is_busy() {
    let s = plain("busy", limits(100, 30_000)).await;
    seed_big(&s.db, 1_500);
    let id = s.begin_id(None).await;
    let slow = s.run(&id, None, SLOW);
    let probe = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        s.run(&id, None, "RETURN 1 AS x").await
    };
    let (slow, probe) = tokio::join!(slow, probe);
    assert_eq!(slow.status, 200, "{}", slow.body);
    assert_eq!(probe.status, 409, "{}", probe.body);
    assert_eq!(probe.body["code"], json!("busy"));
    assert_eq!(probe.body["error"], json!("transaction busy"));
    // The busy answer changed nothing: the transaction is still ours.
    assert_eq!(s.run(&id, None, "RETURN 1 AS x").await.status, 200);
}
