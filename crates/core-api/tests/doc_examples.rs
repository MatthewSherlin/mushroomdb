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

/// The lines between the `// docs:start` line and the `// docs:end` line,
/// with the smallest indent among them removed from all of them. Exactly one
/// of each marker, or the example is refused.
fn quoted_region(source: &str, name: &str) -> String {
    let lines: Vec<&str> = source.lines().collect();
    let marked = |marker: &str| -> usize {
        let at: Vec<usize> = (0..lines.len())
            .filter(|&i| lines[i].trim() == marker)
            .collect();
        assert!(
            at.len() == 1,
            "examples/{name}.rs must have exactly one `{marker}` line; it has {}",
            at.len()
        );
        at[0]
    };
    let (start, end) = (marked("// docs:start"), marked("// docs:end"));
    assert!(
        start < end,
        "examples/{name}.rs ends its region before it starts"
    );
    let body = &lines[start + 1..end];
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
fn a_region_ends_at_its_marker_wherever_the_marker_is_indented() {
    let source = "fn main() {\n    // docs:start\n    let a = 1;\n    let b = 2;\n// docs:end\n}\n";
    assert_eq!(quoted_region(source, "x"), "let a = 1;\nlet b = 2;");
}

#[test]
#[should_panic(expected = "exactly one `// docs:start` line")]
fn an_example_with_two_regions_is_refused() {
    quoted_region(
        "// docs:start\na\n// docs:end\n// docs:start\nb\n// docs:end\n",
        "x",
    );
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
