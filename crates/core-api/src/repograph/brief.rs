//! `brief` — the repository in one block, computed once per session.
//!
//! A `SessionStart` hook runs before the assistant has asked anything, so the
//! brief cannot be about a question: it is the orientation every question
//! afterwards starts from. What it names is what the graph already says is
//! central — the files the most rank flows to, and the symbols the most other
//! symbols call — plus how big the graph is and which commit it was synced to.
//!
//! # Why it is byte-stable
//!
//! The host caches the hook's output for the whole session, so the same store
//! must render the same bytes however often it is asked. Nothing here reads a
//! clock: unlike [`repo_map`](super::repo_map) there is no sync *age*, only
//! the sha, and no window measured against a "now". Every collection is
//! sorted with the ties broken on the key, so hash iteration order cannot
//! reach the output either.
//!
//! # Where the two rankings come from
//!
//! | Section | From |
//! |---|---|
//! | key files | the same [`file_pagerank`] `map` ranks with — deeper, not different |
//! | key symbols | incoming `CALLS`, ties on the key |
//!
//! Sharing `map`'s ranking is the point: two tools that disagreed about which
//! files matter would each be wrong half the time.

use crate::db::{EdgeTypeCensus, GraphDb};
use crate::repograph::facts::str_prop;
use crate::repograph::map::{file_pagerank, spent, SYNC_KEY};
use crate::repograph::render::{basename, dir_components, sanitize, top_tokens};
use core_storage::fs::Fs;
use core_storage::Value;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

/// Property names one label's line may name before the rest are counted off.
/// A line is the unit the byte budget drops, so one wide label must not be
/// able to spend the whole brief.
const MAX_LABEL_PROPS: usize = 12;
/// Labels one edge type's line may name on either end. An edge type whose
/// sources are of four labels is telling the reader it is polymorphic, not
/// which four.
const MAX_END_LABELS: usize = 3;
/// Rows the `who may see` recipe asks for — enough to see whether a role can
/// see anything at all, few enough to be free.
const ROLE_PROBE_ROWS: usize = 20;
/// The threshold the `how many` recipe filters on. Three is the smallest
/// count that reads as a pattern rather than a coincidence.
const HOW_MANY_MIN: usize = 3;
/// Property names that name a node rather than describe it, and so are never
/// what a `what_if` is about. `id` is the identity prop Cypher `CREATE`
/// writes; `key` is what the store calls the same thing.
const IDENTITY_PROPS: [&str; 2] = ["id", "key"];

/// Characters of the synced sha the brief prints — the usual abbreviation,
/// and the same width [`render_map`](super::render_map) uses.
const SHORT_SHA: usize = 7;
/// Subdirectory names a file's role may be built from.
const ROLE_TOKENS: usize = 2;
/// What the brief may spend. The `SessionStart` hook has five seconds, and a
/// store too large to describe inside three of them yields what it had reached
/// — a partial ranking is still a valid ordering, partial counts are still
/// lower bounds, and a partial brief is worth more at the start of a session
/// than none.
///
/// Both surfaces are budgeted, and the memory one needs it more: its work is
/// [`GraphDb::wal_total_commits`], which re-reads the WAL — seconds on a store
/// nobody has snapshotted — plus two passes whose length is the store's.
const RANK_BUDGET: Duration = Duration::from_secs(3);
/// The edge types that make a file a candidate for the key-files list: the
/// *structural* two of the three [`file_pagerank`] ranks over.
///
/// `CO_CHANGED` is deliberately not here. It says two files were edited in the
/// same commits, which is true of every asset added in one go — a directory of
/// fonts co-changes with itself ten ways and reads to PageRank as a small
/// tightly-knit cluster. The brief orients an assistant on *code structure*, so
/// a file qualifies only when something imports it or calls into it; a file
/// related to the codebase by co-change alone is still reachable through
/// `context`, `impact` and `why`, which are the tools that ask about it.
const DEPENDENCY_EDGES: [&str; 2] = ["IMPORTS", "CALLS"];

/// How much of each ranking the brief lists, and how long it may take.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BriefOptions {
    /// Files listed, most central first.
    pub max_files: usize,
    /// Symbols listed, most called first.
    pub max_symbols: usize,
    /// Wall-clock the whole brief may spend, [`RANK_BUDGET`] by default.
    /// [`Duration::ZERO`] is a budget already spent — every count comes back
    /// as the lower bound reached, which is what the tests use. A budget too
    /// large to add to the clock is no budget at all.
    pub budget: Duration,
}

impl Default for BriefOptions {
    fn default() -> Self {
        Self {
            max_files: 25,
            max_symbols: 25,
            budget: RANK_BUDGET,
        }
    }
}

/// One label of a memory store's schema: what it is called, how many nodes
/// carry it, and every property name any of them has.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LabelBrief {
    pub label: String,
    pub nodes: usize,
    /// The union of the property names across the label's nodes, sorted, cut
    /// at [`MAX_LABEL_PROPS`] with `hidden` counting the rest.
    pub props: Vec<String>,
    pub hidden_props: usize,
}

/// One edge type of a memory store's schema: what derives it, what it runs
/// between, and how many there are.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EdgeTypeBrief {
    pub edge_type: String,
    /// The first rule that declares it, sorted. `None` for an edge type
    /// written by hand.
    pub rule: Option<String>,
    /// How many further rules also derive it — two rules deriving one type is
    /// normal, and naming one while implying it is the only one would be a
    /// half-truth.
    pub hidden_rules: usize,
    /// The labels seen on each end, sorted, cut at [`MAX_END_LABELS`].
    pub src: Vec<String>,
    pub dst: Vec<String>,
    pub edges: usize,
}

/// One question kind and the single call that answers it, with the store's
/// own keys, labels and edge types already substituted in.
///
/// The point is that the call is *worked*: an assistant that copies the line
/// reaches an answer without first probing the store for its schema, which is
/// the round trip this section exists to remove.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Recipe {
    /// The question kind, as a reader would name it — `why`, `as of`.
    pub question: String,
    /// The call that answers it.
    pub call: String,
}

/// What a memory store *is*: its labels, its edge types, how deep its history
/// runs, who may read it, and the call that answers each kind of question.
///
/// Present on a store no repository was ingested into — the same
/// `GitSync`-marker test the MCP server's tool listing splits on — and absent
/// on a code graph, which is described by its rankings instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SchemaBrief {
    /// Live nodes, all labels together.
    pub nodes: usize,
    /// Labels, most populous first, ties on the name.
    pub labels: Vec<LabelBrief>,
    /// Edge types, most numerous first, ties on the name.
    pub edge_types: Vec<EdgeTypeBrief>,
    /// How many commits of history are still reachable — `total` above the
    /// horizon floor. A *count*, not an index: the newest commit `edges_at`,
    /// `node_history` and `was_linked` accept is one below it, which is what
    /// the `as of` recipe names. `Some(0)` on a store whose WAL was truncated
    /// and whose archives are gone, and then there is no `as of` recipe at all.
    ///
    /// `None` when the budget ran out before it could be counted: the scan
    /// that answers it re-reads the WAL, so it is the first thing a spent
    /// budget drops. Unknown and zero are different answers, which is why this
    /// is an `Option` and not a zero.
    pub commits: Option<u64>,
    /// `(role name, the labels it may see)`, sorted by name.
    pub roles: Vec<(String, Vec<String>)>,
    /// One worked call per question kind, in a fixed order.
    pub recipes: Vec<Recipe>,
    /// The budget ran out while counting: every count above is a *lower
    /// bound*, the listings may be short of entries, and `commits` is
    /// `None`. Rendered, so a reader never mistakes a partial count for a
    /// complete one.
    pub partial: bool,
}

/// The repository, as a session starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BriefReport {
    /// The repository's own name: the last segment of the path the `GitSync`
    /// marker records. Empty on a store no repository was ingested into.
    pub repo: String,
    pub files: usize,
    pub symbols: usize,
    pub edges: usize,
    /// The abbreviated sha the store was synced to. `None` without a marker.
    /// The sha and not an age: an age would change between two prompts of the
    /// same session and the host caches this output.
    pub last_sync: Option<String>,
    /// `(path, role)`, most central first, ties on the path. The role is
    /// empty whenever the path has already said everything there is to say.
    pub key_files: Vec<(String, String)>,
    /// `(key, first line of the signature)`, most called first, ties on the
    /// key.
    pub key_symbols: Vec<(String, String)>,
    /// The schema, on a memory store. `None` on a code graph, whose two
    /// rankings above are its description.
    pub schema: Option<SchemaBrief>,
}

/// Summarise the store for the start of a session.
///
/// Deterministic for the same store state, with no `now` to pin: see the
/// module docs for why this one tool reads no clock at all.
///
/// # Two surfaces
///
/// A store `ingest-git` built carries the `GitSync` marker and is described
/// by the two rankings above. Any other store is a *memory* store, and the
/// same marker is what the MCP server's `tools/list` splits on — so a store
/// whose session is offered `explain_association` and `query` is exactly the
/// store this describes with a [`SchemaBrief`] instead. Ranking a memory
/// store by `IMPORTS` and `CALLS` would rank nothing; naming its labels, its
/// edge types and one worked call per question kind is what spares the
/// session from probing for them.
#[must_use]
pub fn brief<F: Fs>(db: &GraphDb<F>, opts: &BriefOptions) -> BriefReport {
    // One deadline for the whole brief, taken before the first read, so the
    // two surfaces cannot each spend the budget in turn.
    let deadline = Instant::now().checked_add(opts.budget);

    let mut file_keys: Vec<String> = db
        .nodes_with_label("File")
        .iter()
        .map(|n| n.key().to_string())
        .collect();
    file_keys.sort();

    if !db.has_node(SYNC_KEY) {
        return BriefReport {
            repo: String::new(),
            files: file_keys.len(),
            symbols: db.nodes_with_label("Symbol").len(),
            edges: usize::try_from(db.edge_count()).unwrap_or(usize::MAX),
            last_sync: None,
            key_files: Vec::new(),
            key_symbols: Vec::new(),
            schema: Some(memory_schema(db, deadline)),
        };
    }

    // `file_pagerank` returns the ranking already sorted the way every digest
    // prints one — score first, ties on the key — so the brief takes its head.
    // Cut short by the budget it is a partial ranking, which is still an
    // ordering; the brief has no "(truncated)" to report and does not pretend
    // otherwise.
    let (ranked, _truncated) = file_pagerank(db, &file_keys, deadline);

    // The ranking alone fills the list with a repository's assets. A file
    // nothing imports has no rank of its own, so PageRank leaves it on the
    // uniform teleport mass and the tie breaks alphabetically; a directory of
    // fonts added in one commit does better still, since co-change makes it a
    // small tightly-knit cluster passing rank around inside itself. `map` only
    // ever showed five entries, too few for either to surface — twenty-five is
    // not. So the ranking says what *order* the files come in and
    // [`DEPENDENCY_EDGES`] says which are eligible at all, and listing fewer
    // files beats listing files whose structure the graph knows nothing about.
    let connected = connected_files(db);
    let key_files = ranked
        .iter()
        .filter(|(k, _)| connected.contains(k.as_str()))
        .take(opts.max_files)
        .map(|(k, _)| (sanitize(k), role_of(db, k, &file_keys)))
        .collect();

    // Who calls whom, counted once: a symbol many others call is one a reader
    // will meet whichever thread they pull.
    let mut callers: BTreeMap<String, usize> = BTreeMap::new();
    for (_src, dst, _w) in db.weighted_edges("CALLS", None) {
        *callers.entry(dst).or_default() += 1;
    }
    let symbols = db.nodes_with_label("Symbol");
    let mut ranked_symbols: Vec<(String, usize, String)> = symbols
        .iter()
        .map(|n| {
            let key = n.key().to_string();
            let called = callers.get(&key).copied().unwrap_or(0);
            (key, called, first_line(n.prop("signature")))
        })
        .collect();
    ranked_symbols.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let key_symbols = ranked_symbols
        .into_iter()
        .take(opts.max_symbols)
        .map(|(k, _, sig)| (sanitize(&k), sig))
        .collect();

    BriefReport {
        repo: str_prop(db, SYNC_KEY, "repo")
            .map(|p| sanitize(basename(p.trim_end_matches('/'))))
            .unwrap_or_default(),
        files: file_keys.len(),
        symbols: symbols.len(),
        edges: usize::try_from(db.edge_count()).unwrap_or(usize::MAX),
        last_sync: str_prop(db, SYNC_KEY, "sha")
            .map(|s| sanitize(&s).chars().take(SHORT_SHA).collect()),
        key_files,
        key_symbols,
        schema: None,
    }
}

/// What a memory store is, in two passes and no more.
///
/// # Why neither pass is per edge
///
/// The counts here are per *label* and per *edge type*, and there are two ways
/// to get them expensively. One is to ask the store about each edge in turn —
/// `explain` per edge is a rule evaluation per edge. The other is to
/// materialise every edge first: [`GraphDb::all_edges_for_export`] gives the
/// rule names, the ends and the counts in one sweep, but it pays three
/// `String`s and a provenance entry per edge to do it, which on the 1.3 M-edge
/// association store is seven seconds and hundreds of megabytes spent to print
/// nine lines.
///
/// So edges go through [`GraphDb::edge_type_census`], which walks the topology
/// and sums neighbour slice lengths without building a record per edge, and
/// nodes through [`GraphDb::all_nodes_for_export`], which is one record per
/// node — on a memory store there are thousands of those, not millions.
///
/// Both come back sorted, and the census's sample edge is the first of its
/// type in the store's own id order, so the worked calls name the same keys on
/// every run.
///
/// # The budget
///
/// Neither pass is bounded by anything but the store, and the history count
/// after them re-reads the WAL. `deadline` is checked inside both loops and
/// before the WAL scan, so what a spent budget costs is *completeness*, not
/// the brief: the counts reached become lower bounds, the history goes
/// uncounted, and the `as of` recipe — which needs a commit index the scan
/// would have supplied — is not shown at all rather than shown wrong.
fn memory_schema<F: Fs>(db: &GraphDb<F>, deadline: Option<Instant>) -> SchemaBrief {
    let nodes = db.all_nodes_for_export();
    let mut partial = false;

    // Pass one: label → how many nodes, and every property name any of them
    // has. `props` is a `BTreeMap`, so the union arrives sorted.
    let mut by_label: BTreeMap<String, (usize, BTreeSet<String>)> = BTreeMap::new();
    let mut counted = 0usize;
    for n in &nodes {
        if spent(deadline) {
            partial = true;
            break;
        }
        counted += 1;
        let entry = by_label.entry(n.label.clone()).or_default();
        entry.0 += 1;
        entry.1.extend(n.props.keys().cloned());
    }

    // Pass two: the per-type census, keyed for the recipes to read back.
    let mut by_type: BTreeMap<String, EdgeTypeCensus> = BTreeMap::new();
    if spent(deadline) {
        partial = true;
    } else {
        for c in db.edge_type_census() {
            if spent(deadline) {
                partial = true;
                break;
            }
            by_type.insert(c.edge_type.clone(), c);
        }
    }

    let mut labels: Vec<LabelBrief> = by_label
        .iter()
        .map(|(label, (count, props))| {
            let all: Vec<String> = props.iter().map(|p| sanitize(p)).collect();
            let hidden = all.len().saturating_sub(MAX_LABEL_PROPS);
            LabelBrief {
                label: sanitize(label),
                nodes: *count,
                props: all.into_iter().take(MAX_LABEL_PROPS).collect(),
                hidden_props: hidden,
            }
        })
        .collect();
    labels.sort_by(|a, b| b.nodes.cmp(&a.nodes).then(a.label.cmp(&b.label)));

    let mut edge_types: Vec<EdgeTypeBrief> = by_type
        .values()
        .map(|c| EdgeTypeBrief {
            edge_type: sanitize(&c.edge_type),
            rule: c.rules.first().map(|r| sanitize(r)),
            hidden_rules: c.rules.len().saturating_sub(1),
            src: ends(&c.src_labels),
            dst: ends(&c.dst_labels),
            edges: usize::try_from(c.edges).unwrap_or(usize::MAX),
        })
        .collect();
    edge_types.sort_by(|a, b| b.edges.cmp(&a.edges).then(a.edge_type.cmp(&b.edge_type)));

    let mut roles: Vec<(String, Vec<String>)> = db
        .roles()
        .into_iter()
        .map(|r| {
            (
                sanitize(&r.name),
                r.labels.iter().map(|l| sanitize(l)).collect(),
            )
        })
        .collect();
    roles.sort();

    // Which property each rule actually reads, per label it reads it on. The
    // `what_if` recipe is only worth copying when it names a field some rule
    // has an opinion about: changing a node's display name loses and gains
    // nothing, and a recipe that demonstrates nothing teaches nothing.
    let mut rule_fields: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for rule in db.rules() {
        let mut fields = BTreeSet::new();
        predicate_fields(&rule.predicate, &mut fields);
        for label in [&rule.src_label, &rule.dst_label] {
            rule_fields
                .entry(label.clone())
                .or_default()
                .extend(fields.iter().cloned());
        }
    }

    // Two different numbers, and the difference is the whole point. The
    // history line reports how many commits the store has replayed, which is
    // what a reader wants to know about its depth. The `edges_at` recipe needs
    // an *index*, and `wal_total_commits` is the only thing that knows the top
    // of that range — `commit_seq` counts replayed frames and is seeded from
    // the snapshot's sequence numbers, so it is neither the count nor the
    // index on a store that has ever been snapshotted.
    //
    // `wal_total_commits` re-reads the WAL, which is not free; it is bought
    // once here rather than paid for by a session that copies a broken call.
    // `edges_at` itself pays the same scan, so a brief that can afford to name
    // the call can afford to have checked it.
    // A WAL-truncating snapshot that has outlived its archives leaves a store
    // with no reachable history at all: `floor == total`, an empty range, and
    // every commit index out of it. There is no `as of` call to show, so the
    // brief shows none — a recipe that cannot answer is worse than a missing
    // one, since the session that copies it learns the tool is broken.
    //
    // And it is the first thing the budget drops: on a store nobody has
    // snapshotted the scan is seconds on its own, which is the whole of the
    // hook's five. Uncounted history renders as `unknown` and takes the `as
    // of` recipe with it — a recipe whose commit index was guessed is the one
    // failure a recipe must not have.
    let (commits, latest_commit) = if spent(deadline) {
        partial = true;
        (None, None)
    } else {
        let floor = db.wal_horizon_floor();
        let total = db.wal_total_commits().unwrap_or(floor);
        (
            Some(total.saturating_sub(floor)),
            (total > floor).then(|| total - 1),
        )
    };
    let recipes = recipes(
        &nodes,
        &by_type,
        &labels,
        &edge_types,
        &roles,
        &rule_fields,
        latest_commit,
    );

    SchemaBrief {
        // What was actually counted, so the number the brief prints is a
        // lower bound the pass reached rather than a total it never read.
        nodes: counted,
        labels,
        edge_types,
        commits,
        roles,
        recipes,
        partial,
    }
}

/// Every property name a predicate reads, its branches included.
fn predicate_fields(p: &core_rules::Predicate, out: &mut BTreeSet<String>) {
    use core_rules::Predicate as P;
    match p {
        P::KeyMatch { field }
        | P::FieldEqual { field }
        | P::Overlap { field, .. }
        | P::NumericWithin { field, .. }
        | P::GeoRadius { field, .. }
        | P::VectorSimilar { field, .. } => {
            out.insert(field.clone());
        }
        P::All(parts) | P::Any(parts) => {
            for part in parts {
                predicate_fields(part, out);
            }
        }
    }
}

/// The labels on one end of an edge type, cut at [`MAX_END_LABELS`].
fn ends(labels: &[String]) -> Vec<String> {
    labels
        .iter()
        .take(MAX_END_LABELS)
        .map(|l| sanitize(l))
        .collect()
}

/// One worked call per question kind, in the order a session meets them.
///
/// Every placeholder the store can fill is filled: a pair a rule actually
/// derived for `explain_association`, a key that has edges for `node_edges`,
/// a commit `edges_at` accepts, a property that key really carries for
/// `what_if`, a role out of `roles.json`, and the store's own labels and edge
/// type in the counting template. What the store cannot supply — the *new*
/// value in a `what_if` — stays an angle-bracketed placeholder rather than an
/// invention.
///
/// # The keys come back through `key(n)`
///
/// A node's key is not a property, so `RETURN n.key` parses, runs, and answers
/// a column of nulls. `key(n)` is the function that returns it. Same failure
/// mode as the commit below: a line that reads like a call and answers
/// nothing.
///
/// In the counting recipe the key is taken in the `WITH` rather than the
/// `RETURN`, because after a grouping `WITH` the variable is a projected
/// scalar and no longer a node: `key(b)` in the `RETURN` errors there.
///
/// # The commit is a commit, not a count
///
/// `history: N commits` counts; `edges_at`'s `at` is a zero-based WAL index
/// whose range is `wal_horizon_floor..total_commits`. Substituting the count
/// names one past the end, and the worked example answers `CommitOutOfRange`
/// on every store there has ever been — the worst thing a recipe can do, since
/// a session that copies it learns the tool is broken. So the recipe gets
/// `latest_commit`, the newest index the store will accept.
fn recipes(
    nodes: &[crate::db::NodeInfo],
    by_type: &BTreeMap<String, EdgeTypeCensus>,
    labels: &[LabelBrief],
    edge_types: &[EdgeTypeBrief],
    roles: &[(String, Vec<String>)],
    rule_fields: &BTreeMap<String, BTreeSet<String>>,
    latest_commit: Option<u64>,
) -> Vec<Recipe> {
    // The pair to explain: prefer a type some rule derives, since that is the
    // pair `explain_association` can name a predicate for. `edge_types` is
    // already sorted most-numerous-first, so this is the busiest such type.
    let sample_of = |t: &EdgeTypeBrief| by_type.get(&t.edge_type).and_then(|c| c.sample.clone());
    let pair = edge_types
        .iter()
        .filter(|t| t.rule.is_some())
        .find_map(sample_of)
        .or_else(|| edge_types.iter().find_map(sample_of));
    let (a, b) = match &pair {
        Some((a, b)) => (sanitize(a), sanitize(b)),
        None => ("<a>".to_string(), "<b>".to_string()),
    };
    // The key to ask about: the source of that pair, which is known to have
    // edges. Failing any edge at all, the store's first key.
    let key = match &pair {
        Some((a, _)) => sanitize(a),
        None => nodes
            .first()
            .map_or_else(|| "<key>".to_string(), |n| sanitize(&n.key)),
    };
    // A property that key really carries, and — where the store has a rule
    // reading one — a property some rule reads, so the `what_if` shown is one
    // that would actually lose and gain edges. A `what_if` on a display name
    // is a demonstration of nothing.
    //
    // `id` and `key` are skipped either way: both name the node rather than
    // describe it, and changing a node's name is `rename_node`, not a question
    // about what its relationships would become.
    let field = nodes
        .iter()
        .find(|n| n.key == key)
        .and_then(|n| {
            let watched = rule_fields.get(&n.label);
            let mut usable = n
                .props
                .keys()
                .filter(|f| !IDENTITY_PROPS.contains(&f.as_str()));
            usable
                .clone()
                .find(|f| watched.is_some_and(|w| w.contains(*f)))
                .or_else(|| usable.next())
        })
        .map_or_else(|| "<field>".to_string(), |f| sanitize(f));
    // The role and the label it probes have to be chosen *together*. Picked
    // independently — the first role, the most populous label — the
    // association store rendered `MATCH (n:Talent) … role: client`, and
    // `client` reads only `Company` and `Job`: zero rows, and the session that
    // copies the line learns the tool is broken. So walk the roles in the
    // order they are printed and take the first that can see any label at all,
    // probing the busiest label it can see (`labels` is already sorted most
    // populous first). A store whose roles name no label the brief lists —
    // roles scoped to individual keys, or no roles at all — falls back to the
    // first role and the busiest label, which is as much as can be said.
    let (role, probe_label) = roles
        .iter()
        .find_map(|(name, visible)| {
            labels
                .iter()
                .find(|l| visible.contains(&l.label))
                .map(|l| (name.clone(), l.label.clone()))
        })
        .unwrap_or_else(|| {
            (
                roles
                    .first()
                    .map_or_else(|| "<name>".to_string(), |(n, _)| n.clone()),
                labels
                    .first()
                    .map_or_else(|| "<label>".to_string(), |l| l.label.clone()),
            )
        });
    // The counting template. The busiest edge type, with the labels it was
    // actually seen between.
    let (l1, etype, l2) = edge_types.first().map_or_else(
        || ("<L1>".to_string(), "<TYPE>".to_string(), "<L2>".to_string()),
        |t| {
            (
                t.src.first().cloned().unwrap_or_else(|| "<L1>".to_string()),
                t.edge_type.clone(),
                t.dst.first().cloned().unwrap_or_else(|| "<L2>".to_string()),
            )
        },
    );

    let mut out = vec![
        Recipe {
            question: "why".to_string(),
            call: format!("explain_association {a} {b}"),
        },
        Recipe {
            question: "relationships".to_string(),
            call: format!("node_edges {key}"),
        },
    ];
    if let Some(at) = latest_commit {
        out.push(Recipe {
            question: "as of".to_string(),
            call: format!("edges_at {key} {at}"),
        });
    }
    out.extend([
        Recipe {
            question: "what if".to_string(),
            call: format!("what_if {key} {field} <value>"),
        },
        Recipe {
            question: "who may see".to_string(),
            call: format!(
                "query 'MATCH (n:{probe_label}) RETURN key(n) LIMIT {ROLE_PROBE_ROWS}' role: {role}"
            ),
        },
        Recipe {
            question: "how many".to_string(),
            call: format!(
                "MATCH (a:{l1})-[:{etype}]->(b:{l2}) WITH key(b) AS b_key, count(a) AS n \
                 WHERE n >= {HOW_MANY_MIN} RETURN b_key, n"
            ),
        },
    ]);
    out
}

/// Every file the graph records a [`DEPENDENCY_EDGES`] edge for, in either
/// direction — the files something imports or calls into.
///
/// `IMPORTS` names files directly; `CALLS` runs between symbols, so both of its
/// endpoints are read back to the file that defines them, the same projection
/// [`file_pagerank`] does. A call inside one file counts: it is still the graph
/// knowing that file's structure, which is what this set is asked about, even
/// though the ranking drops it as a self-loop.
///
/// Keys that are not files come back too (an `IMPORTS` edge to a path the
/// store has no node for, say); the caller only ever asks about file keys, so
/// they cost a lookup and change nothing.
fn connected_files<F: Fs>(db: &GraphDb<F>) -> BTreeSet<String> {
    let mut sym_file: BTreeMap<String, String> = BTreeMap::new();
    for node in db.nodes_with_label("Symbol") {
        if let Some(Value::Str(file)) = node.prop("file_id") {
            sym_file.insert(node.key().to_string(), file);
        }
    }
    let mut out = BTreeSet::new();
    for edge_type in DEPENDENCY_EDGES {
        for (src, dst, _w) in db.weighted_edges(edge_type, None) {
            for end in [src, dst] {
                match sym_file.get(&end) {
                    Some(file) => out.insert(file.clone()),
                    None => out.insert(end),
                };
            }
        }
    }
    out
}

/// The first line of a signature prop, or nothing for a node without one.
///
/// One line, because a multi-line signature would forge a section heading in
/// a digest that is read as lines — [`sanitize`] would flatten the break, but
/// the rest of the signature would still be spliced into someone else's line.
fn first_line(v: Option<Value>) -> String {
    match v {
        Some(Value::Str(s)) => sanitize(s.lines().next().unwrap_or_default().trim()),
        _ => String::new(),
    }
}

/// What a file is, in a few words.
///
/// Its `role` prop when the graph carries one. Otherwise what its directory is
/// made of: the subdirectory names most of its neighbours sit in, which is the
/// part of the directory's cluster name the path printed beside it does not
/// already show. A file in a leaf directory therefore has no role, and the
/// brief spends no bytes repeating its own path back at the reader.
fn role_of<F: Fs>(db: &GraphDb<F>, key: &str, file_keys: &[String]) -> String {
    if let Some(role) = str_prop(db, key, "role") {
        let role = sanitize(role.trim());
        if !role.is_empty() {
            return role;
        }
    }
    let dir = dir_components(key).join("/");
    if dir.is_empty() {
        return String::new(); // a file at the root is under nothing
    }
    let prefix = format!("{dir}/");
    let neighbours: Vec<String> = file_keys
        .iter()
        .filter(|k| k.starts_with(&prefix))
        .cloned()
        .collect();
    sanitize(&top_tokens(&neighbours, &dir, ROLE_TOKENS, true).join(", "))
}
