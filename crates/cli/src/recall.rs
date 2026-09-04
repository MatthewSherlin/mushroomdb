//! `mushroomdb recall <db>`: the body of the UserPromptSubmit hook.
//!
//! Reads the hook's JSON payload from stdin, extracts the prompt, and turns
//! it into an OR-of-terms full-text query. The digest itself — the hybrid
//! search, the edge selection and the rendering — is
//! [`core_api::repograph::recall_digest`]; this module owns only what is
//! specific to being a hook: reading the payload, opening the store
//! read-only, and staying silent on any error. A recall hook must never
//! block or slow the user's prompt.
use core_api::repograph::recall_digest;
use core_api::{GraphDb, OpenOptions};
use std::path::Path;

/// Extract the prompt text from a hook payload. Accepts `prompt`,
/// `user_prompt`, and `user_input` (the docs disagree on the field name).
fn prompt_from_payload(raw: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    for k in ["prompt", "user_prompt", "user_input"] {
        if let Some(s) = v.get(k).and_then(|x| x.as_str()) {
            let s = s.trim();
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    None
}

/// Rewrite free-form prompt text as a full-text OR query.
///
/// The rewrite itself lives in `core_api::repograph::or_query`, because the
/// `recall` MCP tool applies it to its `topic` argument and the two must not
/// disagree about what a prompt means. The tests below stay here: this is the
/// caller whose behaviour they describe.
fn fulltext_or_query(prompt: &str) -> Option<String> {
    core_api::repograph::or_query(prompt)
}

pub fn run_recall(db_dir: &Path, hook_stdin: &str) -> String {
    let Some(prompt) = prompt_from_payload(hook_stdin)
        .as_deref()
        .and_then(fulltext_or_query)
    else {
        return String::new();
    };
    // Guard the open: `RealFs::new` runs `create_dir_all`, so without this a
    // hook pointed at a typo'd path would keep creating empty directories.
    if !db_dir.exists() {
        return String::new();
    }
    // Read-only, with both write flags off as well. `auto_migrate` rewrites an
    // old-format snapshot and deletes a stale `.bak`; `repair_wal` writes the
    // valid prefix back over a torn tail. A digest that fires on every prompt,
    // under a 5 s kill, must never write to the user's store: a `serve`
    // mid-append would lose a frame it believes durable. `read_only` also keeps
    // the hook off the cross-process write lock entirely, so it can never make
    // a writer wait and never fails because one is running. The valid prefix is
    // still replayed in memory.
    let Ok(db) = GraphDb::open_with_options(
        db_dir,
        OpenOptions {
            auto_migrate: false,
            repair_wal: false,
            read_only: true,
        },
    ) else {
        return String::new();
    };
    recall_digest(
        &db,
        &prompt,
        &db_dir.display().to_string(),
        core_api::repograph::MAX_OUTPUT_BYTES,
    )
}

#[cfg(test)]
mod tests {
    use super::{fulltext_or_query, prompt_from_payload};
    use core_api::repograph::MAX_QUERY_TERMS;

    #[test]
    fn prompt_is_read_from_any_of_the_three_documented_fields() {
        for field in ["prompt", "user_prompt", "user_input"] {
            let payload = format!(r#"{{"{field}":"  hello  "}}"#);
            assert_eq!(prompt_from_payload(&payload).as_deref(), Some("hello"));
        }
        assert_eq!(prompt_from_payload(r#"{"prompt":"   "}"#), None);
        assert_eq!(prompt_from_payload(r#"{"other":"hi"}"#), None);
        assert_eq!(prompt_from_payload("not json"), None);
    }

    #[test]
    fn prompt_becomes_an_or_query_of_lowercased_alphanumeric_terms() {
        assert_eq!(
            fulltext_or_query("What about Person 1 and Project 5?").as_deref(),
            Some("what OR about OR person OR 1 OR project OR 5"),
        );
    }

    #[test]
    fn or_query_drops_query_keywords_repeats_and_punctuation() {
        // `and`/`or` are grammar keywords; `-x` would negate and `x*` prefix-match,
        // so splitting on non-alphanumerics is what keeps them inert.
        assert_eq!(
            fulltext_or_query("AND or foo-bar foo baz*").as_deref(),
            Some("foo OR bar OR baz"),
        );
        assert_eq!(fulltext_or_query("  ?! ,, "), None);
    }

    #[test]
    fn or_query_caps_the_number_of_terms() {
        let prompt: String = (0..MAX_QUERY_TERMS + 10)
            .map(|i| format!("w{i} "))
            .collect();
        let q = fulltext_or_query(&prompt).expect("terms");
        assert_eq!(q.split(" OR ").count(), MAX_QUERY_TERMS);
    }
}
