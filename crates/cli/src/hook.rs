//! Rules every hook body in this binary obeys, in one place.
//!
//! `recall`, `brief`, `touch`, `intercept`, `impact-hook` and `enrich` all run
//! as a host's hook: inside a short timeout, in front of or beside a tool call
//! the user is waiting on, with a store that may be missing, busy, or older
//! than this binary. Two of those rules are worth stating once rather than
//! restating in each module — how such a body opens a store
//! ([`open_for_hook`]), and how it keeps inside a byte budget ([`cut_to`]).

use std::path::Path;

/// Open `db_dir` the only way a hook body may.
///
/// - **Never create.** `RealFs::new` runs `create_dir_all`, so an unguarded
///   open from a hook left behind by an uninstall — or pointed at a typo'd
///   path — would leave an empty `mushroom-memory/` in the user's repository
///   and then answer out of it. The existence check is the guard.
/// - **Never migrate, never repair the WAL, read-only.** A hook running in
///   front of a tool call has no business writing to the store, and must never
///   wait on a lock: the user is waiting on the tool call, not on us.
///
/// `None` for every failure, which is also every hook body's answer to one:
/// the tool call proceeds exactly as it would with no hook installed.
#[must_use]
pub(crate) fn open_for_hook(db_dir: &Path) -> Option<crate::structure::Db> {
    if !db_dir.exists() {
        return None;
    }
    core_api::GraphDb::open_with_options(
        db_dir,
        core_api::OpenOptions {
            auto_migrate: false,
            repair_wal: false,
            read_only: true,
        },
    )
    .ok()
}

/// `s` cut to `max` bytes on a character boundary, with an ellipsis marking
/// the cut. Unchanged when it already fits.
///
/// The same idiom `recall` and `render` use for their own budgets: walk back to
/// a boundary rather than slicing blind, because a hook that panics is the
/// loudest possible failure and a `&str` index inside a multi-byte character
/// is the easiest way to get one.
#[must_use]
pub(crate) fn cut_to(s: String, max: usize) -> String {
    if s.len() <= max {
        return s;
    }
    // The ellipsis is part of the budget, so the cut point leaves room for it.
    let mut end = max.saturating_sub('…'.len_utf8());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", s[..end].trim_end())
}

#[cfg(test)]
mod tests {
    use super::cut_to;

    #[test]
    fn the_budget_holds_on_a_multibyte_cut() {
        let out = cut_to("é".repeat(100), 41);
        assert!(out.len() <= 41, "{} bytes", out.len());
        assert!(out.ends_with('…'), "{out}");
    }

    #[test]
    fn a_string_inside_the_budget_is_untouched() {
        assert_eq!(cut_to("short".to_string(), 600), "short");
    }
}
