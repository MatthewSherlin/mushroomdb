//! Turning graph facts into lines an assistant reads.
//!
//! Everything here is generic over what is being rendered: the digests in this
//! module's siblings share the line budget, the number formatting, the path
//! shortening, and — above all — [`sanitize`], which every string that came
//! out of the graph must pass through before it reaches a rendered line.

use crate::repograph::context::{ContextReport, Target};
use crate::repograph::impact::{FileImpact, ImpactReport, Partner};
use crate::repograph::map::RepoMap;
use crate::repograph::owners::OwnersReport;
use crate::repograph::why::{WhyLink, WhyReport};
use std::fmt::Write as _;

/// Longest digest any `repograph` tool may print, in lines.
pub const MAX_MAP_LINES: usize = 40;
/// Longest [`render_context`] digest, in lines. Wider than the others because
/// it quotes source.
pub const MAX_CONTEXT_LINES: usize = 60;
/// Longest digest every other tool here prints, in lines.
pub const MAX_TOOL_LINES: usize = 25;

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

/// Seconds in a day.
const DAY: i64 = 86_400;

/// The civil `(year, month, day)` a count of days since 1970-01-01 falls on,
/// proleptic Gregorian. Days before the epoch are negative and convert the
/// same way.
///
/// This is the days-to-civil algorithm every calendar library implements; it
/// is here rather than behind a dependency because two dozen lines of integer
/// arithmetic is the whole of what these digests need a calendar for.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    // Shift the epoch to 0000-03-01, so a leap day is always the last day of
    // the (shifted) year and the month arithmetic below needs no special case.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // day of era, 0..=146_096
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // 0..=399
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day of shifted year
    let mp = (5 * doy + 2) / 153; // shifted month, 0..=11 with March = 0
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// A Unix timestamp as a calendar date in UTC: `2026-09-04`.
#[must_use]
pub fn ymd(ts: i64) -> String {
    let (y, m, d) = civil_from_days(ts.div_euclid(DAY));
    format!("{y:04}-{m:02}-{d:02}")
}

/// The quarter a timestamp falls in, counted from year 0 so that subtracting
/// one index from another gives a number of quarters.
#[must_use]
pub fn quarter_index(ts: i64) -> i64 {
    let (y, m, _) = civil_from_days(ts.div_euclid(DAY));
    y * 4 + i64::from((m - 1) / 3)
}

/// A quarter index as its label: `2026Q3`.
#[must_use]
pub fn quarter_label(index: i64) -> String {
    format!("{}Q{}", index.div_euclid(4), index.rem_euclid(4) + 1)
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
        // Two decimals, like every other float here. A PageRank score is a
        // ranking, and the order it is printed in already carries that; the
        // number is there for the gap between one file and the next.
        let items: Vec<String> = m
            .key_files
            .iter()
            .map(|(k, s)| format!("{} {s:.2}", sanitize(k)))
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

// ── the four per-node digests ───────────────────────────────────────────────

/// Source lines [`render_context`] prints before it says how many are left.
/// The report keeps up to
/// [`MAX_SOURCE_LINES`](crate::repograph::MAX_SOURCE_LINES); a digest that
/// quoted all of them would have room for nothing else.
const MAX_SOURCE_PRINTED: usize = 40;
/// Candidates [`render_context`] lists for an ambiguous name. Past this many
/// the list is not a choice anyone can make from a digest, and the caller wants
/// a longer key rather than a longer list.
const MAX_CANDIDATES: usize = 20;
/// Files [`render_impact`] prints in full.
const MAX_IMPACT_FILES: usize = 5;
/// Links [`render_why`] prints in full.
const MAX_WHY_LINKS: usize = 5;

/// Write a `name  a · b · c` section, or nothing when there is nothing to say.
fn section(out: &mut String, name: &str, items: &[String]) {
    if !items.is_empty() {
        let _ = writeln!(out, "{name}  {}", items.join(SEP));
    }
}

/// `(sha, ts, subject)` as one line of a digest.
fn commit_line(sha: &str, ts: i64, subject: &str) -> String {
    let short: String = sanitize(sha).chars().take(7).collect();
    format!("{short} {} {}", ymd(ts), sanitize(subject))
}

/// Render a [`ContextReport`] as the digest an assistant reads: at most
/// [`MAX_CONTEXT_LINES`] lines, byte-identical for the same report.
#[must_use]
pub fn render_context(c: &ContextReport) -> String {
    let mut out = String::new();
    match &c.target {
        Target::Unknown { target } if c.candidates.is_empty() => {
            let _ = writeln!(out, "mushroomdb context — unknown: {}", sanitize(target));
            return out;
        }
        Target::Unknown { target } => {
            let _ = writeln!(
                out,
                "mushroomdb context — {} is ambiguous: {}",
                sanitize(target),
                plural(c.candidates.len(), "symbol")
            );
            for key in c.candidates.iter().take(MAX_CANDIDATES) {
                let _ = writeln!(out, "  {}", sanitize(key));
            }
            if c.candidates.len() > MAX_CANDIDATES {
                let _ = writeln!(
                    out,
                    "  … {} not shown",
                    plural(c.candidates.len() - MAX_CANDIDATES, "symbol")
                );
            }
            return cap_lines(&out, MAX_CONTEXT_LINES);
        }
        Target::File { path } => {
            let _ = writeln!(out, "mushroomdb context — file {}", sanitize(path));
        }
        Target::Symbol { key } => {
            let _ = writeln!(
                out,
                "mushroomdb context — symbol {} in {}",
                sanitize(key),
                sanitize(&c.file)
            );
        }
    }

    if let Some(sig) = &c.signature {
        let _ = writeln!(out, "signature  {}", sanitize(sig));
    }
    if let Some(doc) = &c.doc {
        let _ = writeln!(out, "doc  {}", sanitize(doc));
    }
    let mut about: Vec<String> = Vec::new();
    if let Some((first, last)) = c.lines {
        about.push(format!("lines {first}-{last}"));
    }
    if let Some(owner) = &c.owner {
        about.push(format!("owner {}", sanitize(owner)));
    }
    section(&mut out, "where", &about);

    if let Some(source) = &c.source {
        let first = c.lines.map_or(1, |(first, _)| first);
        let total = source.lines().count();
        let _ = writeln!(out, "source");
        for (i, line) in source.lines().take(MAX_SOURCE_PRINTED).enumerate() {
            let n = first as usize + i;
            let _ = writeln!(out, "  {n:>5} | {}", sanitize(line));
        }
        if total > MAX_SOURCE_PRINTED {
            let _ = writeln!(
                out,
                "  … {} more",
                plural(total - MAX_SOURCE_PRINTED, "line")
            );
        }
    }

    let calls = |items: &[(String, u32)]| -> Vec<String> {
        items
            .iter()
            .map(|(key, line)| match line {
                0 => sanitize(key),
                n => format!("{} line {n}", sanitize(key)),
            })
            .collect()
    };
    section(&mut out, "callers", &calls(&c.callers));
    section(&mut out, "callees", &calls(&c.callees));
    section(
        &mut out,
        "imports",
        &c.imports.iter().map(|k| sanitize(k)).collect::<Vec<_>>(),
    );
    section(
        &mut out,
        "importers",
        &c.importers.iter().map(|k| sanitize(k)).collect::<Vec<_>>(),
    );
    section(
        &mut out,
        "co-change",
        &c.partners
            .iter()
            .map(|(k, s)| format!("{} {s:.2}", sanitize(k)))
            .collect::<Vec<_>>(),
    );
    section(
        &mut out,
        "commits",
        &c.recent_commits
            .iter()
            .map(|(sha, ts, subject)| commit_line(sha, *ts, subject))
            .collect::<Vec<_>>(),
    );
    for (key, text) in &c.notes {
        let _ = writeln!(out, "note  {} {}", sanitize(key), sanitize(text));
    }
    for (key, name) in &c.concepts {
        let _ = writeln!(out, "concept  {} {}", sanitize(key), sanitize(name));
    }
    cap_lines(&out, MAX_CONTEXT_LINES)
}

/// One partner or importer as `path score modified`, with the parts that say
/// nothing left off.
fn partner_item(p: &Partner, with_score: bool) -> String {
    let mut item = sanitize(&p.path);
    if with_score {
        let _ = write!(item, " {:.2}", p.score);
    }
    if p.modified {
        item.push_str(" modified");
    }
    item
}

/// Render an [`ImpactReport`]: at most [`MAX_TOOL_LINES`] lines.
#[must_use]
pub fn render_impact(r: &ImpactReport) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "mushroomdb impact — {}",
        plural(r.files.len(), "changed file")
    );
    for f in r.files.iter().take(MAX_IMPACT_FILES) {
        render_file_impact(&mut out, f);
    }
    if r.files.len() > MAX_IMPACT_FILES {
        let _ = writeln!(
            out,
            "… {} not shown",
            plural(r.files.len() - MAX_IMPACT_FILES, "file")
        );
    }
    for path in &r.unknown {
        let _ = writeln!(out, "unknown: {}", sanitize(path));
    }
    cap_lines(&out, MAX_TOOL_LINES)
}

fn render_file_impact(out: &mut String, f: &FileImpact) {
    match &f.owner {
        Some(owner) => {
            let _ = writeln!(out, "{} ({})", sanitize(&f.path), sanitize(owner));
        }
        None => {
            let _ = writeln!(out, "{}", sanitize(&f.path));
        }
    }
    section(
        out,
        "  partners ",
        &f.partners
            .iter()
            .map(|p| partner_item(p, true))
            .collect::<Vec<_>>(),
    );
    section(
        out,
        "  importers",
        &f.importers
            .iter()
            .map(|p| partner_item(p, false))
            .collect::<Vec<_>>(),
    );
    section(
        out,
        "  used by  ",
        &f.symbols_used_elsewhere
            .iter()
            .map(|(key, n)| format!("{} {}", sanitize(key), plural(*n, "caller")))
            .collect::<Vec<_>>(),
    );
}

/// Render an [`OwnersReport`]: at most [`MAX_TOOL_LINES`] lines.
///
/// The author key is printed once, on the `top` line and in parentheses, so a
/// reader can address the person the graph means without every other line
/// carrying a mail address.
#[must_use]
pub fn render_owners(o: &OwnersReport) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "mushroomdb owners — {}", sanitize(&o.path));
    if let Some((name, key, share)) = &o.top {
        let _ = writeln!(
            out,
            "top  {} ({}) {share:.2} of the file's commits",
            sanitize(name),
            sanitize(key)
        );
    }
    section(
        &mut out,
        "knows",
        &o.knows
            .iter()
            .map(|(name, score)| format!("{} {score:.2}", sanitize(name)))
            .collect::<Vec<_>>(),
    );
    if let Some((sha, ts, subject)) = &o.last_touch {
        let _ = writeln!(out, "last touch  {}", commit_line(sha, *ts, subject));
    }
    section(
        &mut out,
        "by quarter",
        &o.by_quarter
            .iter()
            .map(|(q, name, n)| format!("{} {} {n}", sanitize(q), sanitize(name)))
            .collect::<Vec<_>>(),
    );
    cap_lines(&out, MAX_TOOL_LINES)
}

/// Render a [`WhyReport`]: at most [`MAX_TOOL_LINES`] lines.
#[must_use]
pub fn render_why(w: &WhyReport) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "mushroomdb why — {} ↔ {}",
        sanitize(&w.a),
        sanitize(&w.b)
    );
    for key in &w.unknown {
        let _ = writeln!(out, "unknown: {}", sanitize(key));
    }
    if !w.unknown.is_empty() {
        return cap_lines(&out, MAX_TOOL_LINES);
    }
    let links = pair_up(&w.links);
    for (link, both_ways) in links.iter().take(MAX_WHY_LINKS) {
        render_link(&mut out, link, *both_ways);
    }
    if links.len() > MAX_WHY_LINKS {
        let _ = writeln!(
            out,
            "… {} not shown",
            plural(links.len() - MAX_WHY_LINKS, "link")
        );
    }
    if !w.path.is_empty() {
        let mut walk = sanitize(&w.a);
        for (edge_type, node) in &w.path {
            let _ = write!(walk, " -[{}]-> {}", sanitize(edge_type), sanitize(node));
        }
        let _ = writeln!(out, "path  {walk}");
    }
    if w.links.is_empty() && w.path.is_empty() {
        let _ = writeln!(out, "no link");
    }
    cap_lines(&out, MAX_TOOL_LINES)
}

/// Pair off the two edges a symmetric rule writes between the same nodes.
///
/// A rule such as `co_changed` matches both ways round and the engine reports
/// an edge each way. They are one relationship, and printing the same three
/// commits twice under it says nothing the first printing did not — so the
/// second is folded into the first, which then reads `a↔b`. The report itself
/// keeps both edges: they are what the graph holds.
fn pair_up(links: &[WhyLink]) -> Vec<(&WhyLink, bool)> {
    let mut out: Vec<(&WhyLink, bool)> = Vec::new();
    let mut folded: Vec<bool> = vec![false; links.len()];
    for (i, link) in links.iter().enumerate() {
        if folded[i] {
            continue;
        }
        let mut both_ways = false;
        for (j, other) in links.iter().enumerate().skip(i + 1) {
            if !folded[j]
                && other.rule == link.rule
                && other.edge_type == link.edge_type
                && other.direction != link.direction
            {
                folded[j] = true;
                both_ways = true;
                break;
            }
        }
        out.push((link, both_ways));
    }
    out
}

fn render_link(out: &mut String, link: &WhyLink, both_ways: bool) {
    let mut head = format!(
        "{} {}  {}",
        sanitize(&link.edge_type),
        if both_ways {
            "a↔b".to_string()
        } else {
            sanitize(&link.direction)
        },
        sanitize(&link.rule)
    );
    if let Some(score) = link.score {
        let _ = write!(head, " {score:.2}");
    }
    if let Some(via) = &link.via {
        let _ = write!(head, " via {}", sanitize(via));
    }
    let _ = writeln!(out, "{head}");
    for line in &link.evidence {
        let _ = writeln!(out, "  {}", sanitize(line));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_timestamp_reads_as_a_utc_date_and_a_quarter() {
        // Epoch, a leap day, the end of a century that is not a leap year, and
        // a date before the epoch.
        for (ts, date, quarter) in [
            (0_i64, "1970-01-01", "1970Q1"),
            (1_582_934_400, "2020-02-29", "2020Q1"),
            (951_782_400, "2000-02-29", "2000Q1"),
            (1_600_000_000, "2020-09-13", "2020Q3"),
            (1_609_459_199, "2020-12-31", "2020Q4"),
            (1_609_459_200, "2021-01-01", "2021Q1"),
            (-1, "1969-12-31", "1969Q4"),
        ] {
            assert_eq!(ymd(ts), date, "{ts}");
            assert_eq!(quarter_label(quarter_index(ts)), quarter, "{ts}");
        }
    }

    #[test]
    fn quarter_indices_are_a_count_a_window_can_be_measured_in() {
        let q3 = quarter_index(1_600_000_000); // 2020Q3
        assert_eq!(quarter_label(q3 - 3), "2019Q4");
        assert_eq!(quarter_label(q3 + 1), "2020Q4");
        assert_eq!(quarter_label(q3 + 2), "2021Q1");
    }

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
