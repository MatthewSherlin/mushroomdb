//! The HTTP page and the router name the same routes.
//!
//! `docs/site/http.md` has one `### METHOD /path` section a route. Before this
//! test its endpoint table listed 15 routes of 29, and nothing said so. A route
//! added to the router with no section fails here by name; so does a section
//! left behind for a route that is gone.
//!
//! The routes are read from the router's source. axum's `Router` cannot be
//! listed at run time, and exporting a table from the server only so that a
//! test could read it would be a change to the product for the docs' sake.

use std::collections::BTreeSet;
use std::path::PathBuf;

const HTTP_RS: &str = include_str!("../src/http.rs");

/// The part of `http.rs` that builds the router: from the first
/// `Router::new()` to the end of the WebSocket router that follows it.
fn router_source() -> &'static str {
    let start = HTTP_RS
        .find("let app = Router::new()")
        .expect("http.rs no longer has `let app = Router::new()`");
    let streaming = HTTP_RS[start..]
        .find("let streaming = Router::new()")
        .expect("http.rs no longer has `let streaming = Router::new()`");
    let end = HTTP_RS[start + streaming..]
        .find(".with_state(")
        .expect("the streaming router no longer ends in `.with_state(`");
    &HTTP_RS[start..start + streaming + end]
}

/// `(METHOD, path)` for every `.route("path", method(handler))` in `src`.
fn routes_in(src: &str) -> BTreeSet<(String, String)> {
    const METHODS: [&str; 4] = ["get(", "post(", "put(", "delete("];
    let mut routes = BTreeSet::new();
    let mut rest = src;
    while let Some(at) = rest.find(".route(") {
        rest = &rest[at + ".route(".len()..];
        let open = rest.find('"').expect("a .route( with no path string");
        let len = rest[open + 1..]
            .find('"')
            .expect("an unterminated path string");
        let path = &rest[open + 1..open + 1 + len];
        let after = &rest[open + 1 + len..];
        let method = METHODS
            .iter()
            .filter_map(|m| after.find(m).map(|i| (i, *m)))
            .min()
            .map(|(_, m)| m.trim_end_matches('(').to_uppercase())
            .unwrap_or_else(|| panic!("no routing method after the path {path:?}"));
        routes.insert((method, path.to_string()));
    }
    routes
}

/// `(METHOD, path)` for every `### METHOD /path` heading in `page`.
fn routes_documented(page: &str) -> BTreeSet<(String, String)> {
    let mut routes = BTreeSet::new();
    for line in page.lines() {
        let Some(heading) = line.strip_prefix("### ") else {
            continue;
        };
        let mut words = heading.split_whitespace();
        let (Some(method), Some(path)) = (words.next(), words.next()) else {
            continue;
        };
        if !["GET", "POST", "PUT", "DELETE"].contains(&method) || !path.starts_with('/') {
            continue;
        }
        assert!(
            words.next().is_none(),
            "a route heading carries more than `### METHOD /path`: {line:?}"
        );
        routes.insert((method.to_string(), path.to_string()));
    }
    routes
}

fn http_page() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/site/http.md");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

#[test]
fn the_reader_finds_a_route_split_across_lines() {
    let src = r#"
        .route("/health", get(health))
        .route(
            "/node/{key}/prop/{field}",
            axum::routing::put(set_node_prop),
        )
        .route("/node/{key}", axum::routing::delete(delete_node))
    "#;
    let found: Vec<_> = routes_in(src).into_iter().collect();
    assert_eq!(
        found,
        vec![
            ("DELETE".to_string(), "/node/{key}".to_string()),
            ("GET".to_string(), "/health".to_string()),
            ("PUT".to_string(), "/node/{key}/prop/{field}".to_string()),
        ]
    );
}

#[test]
fn a_heading_is_a_route_only_when_it_is_a_method_and_a_path() {
    let page =
        "### GET /health\n### Authentication\n#### GET /deeper\n### POST /nodes/{key}/rename\n";
    let found: Vec<_> = routes_documented(page).into_iter().collect();
    assert_eq!(
        found,
        vec![
            ("GET".to_string(), "/health".to_string()),
            ("POST".to_string(), "/nodes/{key}/rename".to_string()),
        ]
    );
}

#[test]
#[should_panic(expected = "more than `### METHOD /path`")]
fn a_route_heading_with_a_query_string_after_it_is_refused() {
    routes_documented("### GET /watch (WebSocket)\n");
}

#[test]
fn the_http_page_and_the_router_name_the_same_routes() {
    let routed = routes_in(router_source());
    assert!(
        routed.len() >= 29,
        "the reader found {} routes in http.rs; it found 29 when this test was written",
        routed.len()
    );
    let documented = routes_documented(&http_page());
    let show = |set: &BTreeSet<(String, String)>, other: &BTreeSet<(String, String)>| {
        set.difference(other)
            .map(|(method, path)| format!("{method} {path}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let missing = show(&routed, &documented);
    let stale = show(&documented, &routed);
    assert!(
        missing.is_empty() && stale.is_empty(),
        "docs/site/http.md and the router disagree.\n  routes with no `### METHOD /path` section: [{missing}]\n  sections for routes the server does not have: [{stale}]"
    );
}
