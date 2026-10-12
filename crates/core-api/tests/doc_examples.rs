//! The Rust page quotes its examples from files that compile and run.
//!
//! Before this, nothing compiled a code block in the docs, and several had
//! stopped matching the API: a compare-and-set batch shown taking a closure,
//! a synchronous call shown with `.await`. Each example on
//! `docs/site/rust.md` is the region of a file under `examples/` between
//! `// docs:start` and `// docs:end`. CI builds and runs those files; this
//! test fails when the page's copy and the file's region differ.

use std::path::PathBuf;

const EXAMPLES: [&str; 3] = ["embed_basics", "embed_batch", "embed_shared"];

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The lines between `// docs:start` and `// docs:end`, with the indent of
/// the first one removed from all of them.
fn quoted_region(source: &str, name: &str) -> String {
    let start = source
        .find("// docs:start")
        .unwrap_or_else(|| panic!("examples/{name}.rs has no `// docs:start` line"));
    let end = source
        .find("// docs:end")
        .unwrap_or_else(|| panic!("examples/{name}.rs has no `// docs:end` line"));
    let body: Vec<&str> = source[start..end].lines().skip(1).collect();
    let body = &body[..body.len().saturating_sub(1)];
    let indent = body
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    body.iter()
        .map(|l| {
            if l.len() >= indent {
                &l[indent..]
            } else {
                l.trim_start()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn a_region_is_dedented_and_excludes_its_markers() {
    let source = "fn main() {\n    // docs:start\n    let a = 1;\n\n    if a > 0 {\n        go();\n    }\n    // docs:end\n}\n";
    assert_eq!(
        quoted_region(source, "x"),
        "let a = 1;\n\nif a > 0 {\n    go();\n}"
    );
}

#[test]
fn the_rust_page_quotes_each_example_as_it_is() {
    let page_path = repo().join("docs/site/rust.md");
    let page = std::fs::read_to_string(&page_path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", page_path.display()));
    for name in EXAMPLES {
        let path = repo().join(format!("crates/core-api/examples/{name}.rs"));
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        let region = quoted_region(&source, name);
        assert!(
            page.contains(&format!("```rust\n{region}\n```")),
            "docs/site/rust.md does not quote examples/{name}.rs as it is. The page's block must be exactly the lines between `// docs:start` and `// docs:end`:\n{region}"
        );
    }
}
