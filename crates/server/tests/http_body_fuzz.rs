//! HTTP body robustness: `/query` and `/ingest` handlers never panic on
//! arbitrary JSON bodies or raw bytes.
//!
//! Two generators, 256 cases each (512 total):
//!   (a) mixed-body strategy (valid `serde_json::Value` 80% / raw bytes 20%)
//!       sent as POST `/query` → any status code is acceptable; a panic
//!       (which propagates through `block_on` and is caught by `catch_unwind`)
//!       is a failure
//!   (b) same strategy sent as POST `/ingest` → same guarantee
//!
//! Harness: mirrors `crates/server/tests/http.rs`; uses `tower::ServiceExt::oneshot`
//! with a fresh clone of a shared `Router` per case.  A `tokio::runtime::Runtime`
//! is held in a `OnceLock` for the lifetime of the test process to avoid the
//! overhead of creating a new runtime per case.
//!
//! Liveness: the `liveness_probe_catch_unwind_catches_panics` test confirms
//! `catch_unwind` captures deliberate panics.  Any panic from a handler would
//! propagate through `block_on` to the outer `catch_unwind` and surface as a
//! `TestCaseError::fail` rather than a silently passing case.

use axum::body::Body;
use axum::http::Request;
use axum::Router;
use core_api::SharedDb;
use proptest::prelude::*;
use server::router;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::OnceLock;
use tower::ServiceExt;

// ── infrastructure ────────────────────────────────────────────────────────────

fn fuzz_dir(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("graphdb-http-fuzz-{}-{}", tag, std::process::id()))
}

static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

fn rt() -> &'static tokio::runtime::Runtime {
    RUNTIME.get_or_init(|| tokio::runtime::Runtime::new().unwrap())
}

static QUERY_ROUTER: OnceLock<Router> = OnceLock::new();
static INGEST_ROUTER: OnceLock<Router> = OnceLock::new();

fn query_app() -> Router {
    QUERY_ROUTER
        .get_or_init(|| {
            let db = SharedDb::open(&fuzz_dir("query")).unwrap();
            router(db)
        })
        .clone()
}

fn ingest_app() -> Router {
    INGEST_ROUTER
        .get_or_init(|| {
            let db = SharedDb::open(&fuzz_dir("ingest")).unwrap();
            router(db)
        })
        .clone()
}

// ── JSON + raw-bytes body strategy ───────────────────────────────────────────

fn json_leaf() -> impl Strategy<Value = serde_json::Value> {
    prop_oneof![
        Just(serde_json::Value::Null),
        any::<bool>().prop_map(serde_json::Value::Bool),
        any::<i64>().prop_map(|n| serde_json::Value::Number(serde_json::Number::from(n))),
        proptest::collection::vec(any::<u8>(), 0..32)
            .prop_map(|b| serde_json::Value::String(String::from_utf8_lossy(&b).into_owned())),
    ]
}

fn json_value() -> BoxedStrategy<serde_json::Value> {
    json_leaf()
        .prop_recursive(3, 16, 4, |inner| {
            prop_oneof![
                proptest::collection::vec(inner.clone(), 0..4).prop_map(serde_json::Value::Array),
                proptest::collection::vec(
                    (
                        proptest::collection::vec(any::<u8>(), 0..16)
                            .prop_map(|b| String::from_utf8_lossy(&b).into_owned()),
                        inner,
                    ),
                    0..4
                )
                .prop_map(|pairs| serde_json::Value::Object(pairs.into_iter().collect())),
            ]
        })
        .boxed()
}

/// Body bytes: valid JSON 80% of the time, raw arbitrary bytes 20% (near-JSON).
fn body_strategy() -> BoxedStrategy<Vec<u8>> {
    prop_oneof![
        4 => json_value().prop_map(|v| v.to_string().into_bytes()),
        1 => proptest::collection::vec(any::<u8>(), 0..512),
    ]
    .boxed()
}

// ── liveness probe ────────────────────────────────────────────────────────────

/// Liveness: `catch_unwind` catches a deliberate panic, so any panic from a
/// handler would surface as a `TestCaseError::fail` rather than passing.
#[test]
fn liveness_probe_catch_unwind_catches_panics() {
    let caught = catch_unwind(AssertUnwindSafe(|| panic!("deliberate probe")));
    assert!(caught.is_err(), "catch_unwind must catch deliberate panics");
}

// ── Block (a): POST /query with arbitrary body → handler never panics ─────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]
    #[test]
    fn query_handler_never_panics_on_arbitrary_body(body_bytes in body_strategy()) {
        let app = query_app();
        let req = Request::builder()
            .method("POST")
            .uri("/query")
            .header("content-type", "application/json")
            .body(Body::from(body_bytes.clone()))
            .unwrap();
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            rt().block_on(async move {
                let _ = app.oneshot(req).await;
            })
        }));
        prop_assert!(
            outcome.is_ok(),
            "POST /query panicked on body ({} bytes): {}",
            body_bytes.len(),
            String::from_utf8_lossy(&body_bytes)
        );
    }
}

// ── Block (b): POST /ingest with arbitrary body → handler never panics ────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]
    #[test]
    fn ingest_handler_never_panics_on_arbitrary_body(body_bytes in body_strategy()) {
        let app = ingest_app();
        let req = Request::builder()
            .method("POST")
            .uri("/ingest")
            .header("content-type", "application/json")
            .body(Body::from(body_bytes.clone()))
            .unwrap();
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            rt().block_on(async move {
                let _ = app.oneshot(req).await;
            })
        }));
        prop_assert!(
            outcome.is_ok(),
            "POST /ingest panicked on body ({} bytes): {}",
            body_bytes.len(),
            String::from_utf8_lossy(&body_bytes)
        );
    }
}
