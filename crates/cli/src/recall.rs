//! `mushroomdb recall <db>`: the body of the `UserPromptSubmit` hook.
//!
//! The hook reads the prompt off stdin, asks the store what it already knows
//! about it, and prints a digest or nothing at all. Everything specific to
//! being a hook stays here: reading the payload, opening the store read-only,
//! staying inside one byte budget, and staying silent on any error. A recall
//! hook must never block or slow a user's prompt.
//!
//! Until 0.7 this gated on the prompt naming a *code identifier* — a path, a
//! `mod::name`, a snake_case or backticked word — and said nothing otherwise.
//! On a memory store, whose prompts are sentences about people and projects,
//! that was silence on every prompt. The digest itself is
//! [`core_api::memory::recall::recall_digest`], which has no identifier gate.
//!
//! The dirty-working-tree nudge — what the changed files reach, who owns them,
//! which learned concepts went stale — read edges only `ingest-git` writes. It
//! left with the rest of the code-graph door in 0.7.
use crate::hook::{cut_to, open_for_hook};
use core_api::digest::{MAX_OUTPUT_BYTES, UNTRUSTED_FRAMING};
use core_api::memory::recall::{recall_digest, RecallOutcome};
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

/// The hook's whole output. Never an error, never a panic: an empty string is
/// how this hook says "nothing to add", and every failure path says that.
#[must_use]
pub fn run_recall(db_dir: &Path, hook_stdin: &str) -> String {
    let Some(prompt) = prompt_from_payload(hook_stdin) else {
        return String::new();
    };
    let Some(db) = open_for_hook(db_dir) else {
        return String::new();
    };
    // The digest is stored content on its way into an assistant's context, so
    // it is marked as data before its first line — the same marker the MCP
    // tools put on the same digest at their own output layer. The marker is
    // charged against the one byte budget.
    let budget = MAX_OUTPUT_BYTES.saturating_sub(UNTRUSTED_FRAMING.len());
    match recall_digest(&db, &prompt, &db_dir.display().to_string(), budget) {
        RecallOutcome::Hits(digest) => {
            cut_to(format!("{UNTRUSTED_FRAMING}{digest}"), MAX_OUTPUT_BYTES)
        }
        // The tool answers this because someone asked it a question. This
        // hook was not asked; it fires on every prompt, so the same line here
        // nags every turn until the store is fixed. The brief says it once per
        // session instead.
        RecallOutcome::NoIndex | RecallOutcome::NoMatch => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::prompt_from_payload;

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
}
