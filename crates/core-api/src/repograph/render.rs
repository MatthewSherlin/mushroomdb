//! Turning graph facts into lines an assistant reads.
//!
//! Everything here is generic over what is being rendered: the digests in this
//! module's siblings share the line budget, the number formatting, the path
//! shortening, and — above all — [`sanitize`], which every string that came
//! out of the graph must pass through before it reaches a rendered line.

use crate::repograph::map::RepoMap;
use std::fmt::Write as _;

/// Longest digest any `repograph` tool may print, in lines.
pub const MAX_MAP_LINES: usize = 40;

/// Separator between the items of a one-line list.
pub const SEP: &str = " · ";

/// Replace every ASCII control character (`0x00-0x1f` and `0x7f`, tabs and
/// newlines included) with a space, so a value read out of the graph cannot
/// forge a line break, a section header, or a terminal escape sequence.
///
/// One byte in, one byte out, so a caller's size budget is unaffected.
#[must_use]
pub fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_control() { ' ' } else { c })
        .collect()
}

/// `1204` → `1,204`. Groups of three, ASCII digits only.
#[must_use]
pub fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `n` of `word`, pluralised by adding an `s`. `1 file`, `2 files`.
#[must_use]
pub fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("{n} {word}")
    } else {
        format!("{} {word}s", thousands(n))
    }
}

/// A duration in seconds as one coarse unit: `45s`, `12m`, `3h`, `20d`.
/// Negative input — a clock that ran backwards — reads as `0s`.
#[must_use]
pub fn age(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3_600 {
        format!("{}m", s / 60)
    } else if s < 86_400 {
        format!("{}h", s / 3_600)
    } else {
        format!("{}d", s / 86_400)
    }
}

/// The last `/`-separated segment of a key: `src/core/db.rs` → `db.rs`.
#[must_use]
pub fn basename(key: &str) -> &str {
    key.rsplit_once('/').map_or(key, |(_, base)| base)
}

/// The directory segments of a key: `src/core/db.rs` → `["src", "core"]`.
/// A key with no `/` has none.
#[must_use]
pub fn dir_components(key: &str) -> Vec<&str> {
    let mut parts: Vec<&str> = key.split('/').collect();
    parts.pop();
    parts
}

/// The longest directory prefix every key shares, `/`-joined. Empty when the
/// keys share no leading directory at all.
#[must_use]
pub fn common_dir_prefix(keys: &[String]) -> String {
    let mut iter = keys.iter().map(|k| dir_components(k));
    let Some(mut prefix) = iter.next() else {
        return String::new();
    };
    for comps in iter {
        let shared = prefix
            .iter()
            .zip(comps.iter())
            .take_while(|(a, b)| a == b)
            .count();
        prefix.truncate(shared);
        if prefix.is_empty() {
            break;
        }
    }
    prefix.join("/")
}

/// The `n` path segments most keys carry, ignoring `prefix`.
///
/// A segment is counted once per key, so a directory that appears in twenty
/// keys beats a filename that appears in one. Ties go to the segment that
/// sorts first, which is what makes the answer stable. With `dirs_only` the
/// basename is skipped, leaving the segments that say where a file lives.
#[must_use]
pub fn top_tokens(keys: &[String], prefix: &str, n: usize, dirs_only: bool) -> Vec<String> {
    let mut counts: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for key in keys {
        let rest = match prefix.is_empty() {
            true => key.as_str(),
            false => key
                .strip_prefix(prefix)
                .unwrap_or(key)
                .trim_start_matches('/'),
        };
        let mut seen: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
        if dirs_only {
            seen.pop();
        }
        seen.sort_unstable();
        seen.dedup();
        for token in seen {
            *counts.entry(token).or_default() += 1;
        }
    }
    let mut ranked: Vec<(&str, usize)> = counts.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    ranked
        .into_iter()
        .take(n)
        .map(|(t, _)| t.to_string())
        .collect()
}

/// What a set of files with no shared directory is called.
pub const MIXED: &str = "<mixed>";

/// What to call a set of files.
///
/// The directory they all sit under, when there is one — that is the name a
/// person would use — followed by the two subdirectories most of them sit in,
/// which is what tells two clusters under the same root apart. Files that
/// share no directory get [`MIXED`] in the prefix's place.
///
/// Files sitting directly in the shared directory add nothing to it, so a
/// cluster that is exactly one directory deep is named by that directory
/// alone.
#[must_use]
pub fn cluster_name(keys: &[String]) -> String {
    let prefix = common_dir_prefix(keys);
    let head = if prefix.is_empty() {
        MIXED.to_string()
    } else {
        prefix.clone()
    };
    let mut tokens = top_tokens(keys, &prefix, 2, true);
    if tokens.is_empty() && prefix.is_empty() {
        // Everything is at the root: the filenames are all there is to say.
        tokens = top_tokens(keys, &prefix, 2, false);
    }
    if tokens.is_empty() {
        head
    } else {
        format!("{head} {}", tokens.join(", "))
    }
}

/// Shorten keys to their filenames, keeping the full path for any filename
/// that would otherwise appear twice.
///
/// `mod.rs, mod.rs` names nothing; `src/net/mod.rs, src/io/mod.rs` names two
/// files. Sanitized, since the result is printed.
#[must_use]
pub fn short_names(keys: &[String]) -> Vec<String> {
    let mut seen: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for key in keys {
        *seen.entry(basename(key)).or_default() += 1;
    }
    keys.iter()
        .map(|k| match seen.get(basename(k)) {
            Some(1) => sanitize(basename(k)),
            _ => sanitize(k),
        })
        .collect()
}

/// Keep at most `max` lines, dropping the rest.
#[must_use]
pub fn cap_lines(text: &str, max: usize) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines().take(max) {
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// The one line a store with nothing in it gets: what is missing, and the
/// command that fixes it.
pub const EMPTY_MAP: &str =
    "mushroomdb map — empty store; run: mushroomdb ingest-git <db> <repo>\n";

/// Render a [`RepoMap`] as the digest an assistant reads: at most
/// [`MAX_MAP_LINES`] lines, byte-identical for the same map.
///
/// Every value that came out of the graph is sanitized again here, so the
/// output is safe whether or not the map was built by
/// [`repo_map`](crate::repograph::repo_map).
#[must_use]
pub fn render_map(m: &RepoMap) -> String {
    if m.files == 0 {
        return EMPTY_MAP.to_string();
    }
    let mut out = String::new();

    // Header: the size of the graph, and how current it is.
    let sync = match &m.last_sync {
        None => "not synced".to_string(),
        Some(s) => {
            let sha = sanitize(&s.sha);
            let short: String = sha.chars().take(7).collect();
            match s.age_secs {
                Some(secs) => format!("synced {} ago at {short}", age(secs)),
                None => format!("synced at {short}"),
            }
        }
    };
    let _ = writeln!(
        out,
        "mushroomdb map — {}, {}, {}, {} · {sync}{}",
        plural(m.files, "file"),
        plural(m.symbols, "symbol"),
        plural(m.commits, "commit"),
        plural(m.authors, "author"),
        if m.truncated { " (truncated)" } else { "" }
    );

    if !m.communities.is_empty() {
        out.push_str("clusters (co-change + imports)\n");
        for (i, c) in m.communities.iter().enumerate() {
            let samples = short_names(&c.samples);
            let _ = writeln!(
                out,
                "  {}. {}  ({}, cohesion {:.2}){}{}",
                i + 1,
                sanitize(&c.name),
                plural(c.size, "file"),
                c.cohesion,
                if samples.is_empty() { "" } else { "  " },
                samples.join(", ")
            );
        }
    }

    if !m.key_files.is_empty() {
        out.push_str("key files (most depended-on)\n");
        let items: Vec<String> = m
            .key_files
            .iter()
            .map(|(k, s)| format!("{} {s:.3}", sanitize(k)))
            .collect();
        let _ = writeln!(out, "  {}", items.join(SEP));
    }

    if !m.owners.is_empty() {
        out.push_str("owners\n");
        let items: Vec<String> = m
            .owners
            .iter()
            .enumerate()
            .map(|(i, (name, n))| match i {
                // The unit is stated once, on the first entry.
                0 => format!("{} {}", sanitize(name), plural(*n, "file")),
                _ => format!("{} {n}", sanitize(name)),
            })
            .collect();
        let _ = writeln!(out, "  {}", items.join(SEP));
    }

    if !m.hot_files.is_empty() {
        let _ = writeln!(out, "hot (last {} days)", m.hot_days);
        let items: Vec<String> = m
            .hot_files
            .iter()
            .map(|(k, n)| format!("{} {n}", sanitize(k)))
            .collect();
        let _ = writeln!(out, "  {}", items.join(SEP));
    }

    if m.stale_concepts > 0 {
        let (noun, verb) = if m.stale_concepts == 1 {
            ("concept", "needs")
        } else {
            ("concepts", "need")
        };
        let _ = writeln!(
            out,
            "notes: {} {noun} {verb} re-learning (source changed)",
            m.stale_concepts
        );
    }

    if !m.questions.is_empty() {
        let asks: Vec<String> = m.questions.iter().map(|q| sanitize(q)).collect();
        let _ = writeln!(out, "ask me: {}", asks.join(SEP));
    }

    cap_lines(&out, MAX_MAP_LINES)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_replaces_every_control_character_one_for_one() {
        let forged = "Ada\nmushroomdb map\t— 9 files\u{7f}\u{1b}[31m";
        let clean = sanitize(forged);
        assert_eq!(clean.len(), forged.len(), "one byte in, one byte out");
        assert!(!clean.contains('\n') && !clean.contains('\t') && !clean.contains('\u{1b}'));
        assert_eq!(clean, "Ada mushroomdb map — 9 files  [31m");
    }

    #[test]
    fn thousands_groups_from_the_right() {
        for (n, want) in [
            (0, "0"),
            (7, "7"),
            (999, "999"),
            (1_000, "1,000"),
            (1_204, "1,204"),
            (999_999, "999,999"),
            (1_830_412, "1,830,412"),
        ] {
            assert_eq!(thousands(n), want, "{n}");
        }
    }

    #[test]
    fn plural_says_one_file_and_two_files() {
        assert_eq!(plural(1, "file"), "1 file");
        assert_eq!(plural(0, "file"), "0 files");
        assert_eq!(plural(1_204, "commit"), "1,204 commits");
    }

    #[test]
    fn age_picks_one_coarse_unit() {
        for (secs, want) in [
            (-5, "0s"),
            (0, "0s"),
            (59, "59s"),
            (60, "1m"),
            (720, "12m"),
            (3_600, "1h"),
            (86_399, "23h"),
            (86_400, "1d"),
            (20 * 86_400, "20d"),
        ] {
            assert_eq!(age(secs), want, "{secs}");
        }
    }

    #[test]
    fn paths_split_into_a_base_and_its_directories() {
        assert_eq!(basename("src/core/db.rs"), "db.rs");
        assert_eq!(basename("README.md"), "README.md");
        assert_eq!(dir_components("src/core/db.rs"), vec!["src", "core"]);
        assert!(dir_components("README.md").is_empty());
    }

    #[test]
    fn a_cluster_is_named_by_the_directory_its_files_share() {
        // One directory deep: the directory is the whole name.
        let same = vec![
            "crates/core-api/src/db.rs".to_string(),
            "crates/core-api/src/algo.rs".to_string(),
        ];
        assert_eq!(cluster_name(&same), "crates/core-api/src");
        // Split across subdirectories: they are what tells this cluster from
        // another one under the same root.
        let partial = vec![
            "crates/core-api/src/db.rs".to_string(),
            "crates/core-api/tests/algo.rs".to_string(),
        ];
        assert_eq!(cluster_name(&partial), "crates/core-api src, tests");
    }

    #[test]
    fn files_sharing_no_directory_are_named_by_their_commonest_segments() {
        let mixed = vec![
            "docs/site/algorithms.md".to_string(),
            "docs/site/install.md".to_string(),
            "site/index.html".to_string(),
            "README.md".to_string(),
        ];
        // Nothing is shared at the root, so the name falls back to segments:
        // `site` appears in three keys, and `docs` in two.
        assert_eq!(cluster_name(&mixed), "<mixed> site, docs");
        assert_eq!(cluster_name(&["a.rs".to_string()]), "<mixed> a.rs");
        assert_eq!(cluster_name(&[]), "<mixed>");
    }

    #[test]
    fn a_segment_counts_once_per_key_however_often_it_repeats() {
        let keys = vec!["a/a/a/a.rs".to_string(), "b/x.rs".to_string()];
        assert_eq!(top_tokens(&keys, "", 1, true), vec!["a".to_string()]);
        // Without `dirs_only` the filenames join the count and `a` still wins.
        assert_eq!(top_tokens(&keys, "", 1, false), vec!["a".to_string()]);
    }

    #[test]
    fn short_names_keep_the_path_only_where_a_filename_repeats() {
        let keys = vec![
            "src/net/mod.rs".to_string(),
            "src/io/mod.rs".to_string(),
            "src/db.rs".to_string(),
        ];
        assert_eq!(
            short_names(&keys),
            vec!["src/net/mod.rs", "src/io/mod.rs", "db.rs"]
        );
    }

    #[test]
    fn cap_lines_keeps_the_first_lines_and_a_trailing_newline() {
        assert_eq!(cap_lines("a\nb\nc\n", 2), "a\nb\n");
        assert_eq!(cap_lines("a\nb", 9), "a\nb\n");
        assert_eq!(cap_lines("", 9), "");
    }
}
