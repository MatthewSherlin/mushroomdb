//! `asof --at <date>`.
//!
//! The parser refuses both forms together and neither: a precedence rule
//! between a commit index and a date is how a caller ends up believing it asked
//! for one and got the other (spec D2).

use cli::{parse_args, Command};

#[test]
fn at_parses_into_the_date_slot() {
    match parse_args(&["asof", "/tmp/db", "--at", "2026-06-19"]).expect("parse") {
        Command::AsOf { commit, at, .. } => {
            assert_eq!(commit, None);
            assert_eq!(at.as_deref(), Some("2026-06-19"));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_equals_form_parses_too() {
    match parse_args(&["asof", "/tmp/db", "--at=2026-06-19T12:00:00Z"]).expect("parse") {
        Command::AsOf { at, .. } => assert_eq!(at.as_deref(), Some("2026-06-19T12:00:00Z")),
        other => panic!("{other:?}"),
    }
}

#[test]
fn commit_still_parses_on_its_own() {
    match parse_args(&["asof", "/tmp/db", "--commit", "7"]).expect("parse") {
        Command::AsOf { commit, at, .. } => {
            assert_eq!(commit, Some(7));
            assert_eq!(at, None);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn both_together_is_refused() {
    let err = parse_args(&["asof", "/tmp/db", "--commit", "7", "--at", "2026-06-19"])
        .expect_err("must refuse both");
    assert!(err.contains("not both"), "unhelpful: {err}");
}

#[test]
fn neither_is_refused_and_names_both_forms() {
    let err = parse_args(&["asof", "/tmp/db"]).expect_err("must refuse neither");
    assert!(err.contains("--commit"), "{err}");
    assert!(err.contains("--at"), "{err}");
}
