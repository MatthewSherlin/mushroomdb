//! `brief` — a memory store in one block, computed once per session.
//!
//! A `SessionStart` hook runs before the assistant has asked anything, so the
//! brief cannot be about a question: it is the orientation every question
//! afterwards starts from. On a memory store that is the schema — its labels,
//! its edge types, how deep its history runs, who may read it — and one worked
//! call per question kind.
//!
//! # Why it is byte-stable
//!
//! The host caches the hook's output for the whole session, so the same store
//! must render the same bytes however often it is asked. Nothing here reads a
//! clock for content: the budget decides how much is counted, never what is
//! printed for what was. Every collection is sorted with the ties broken on
//! the key, so hash iteration order cannot reach the output either.

use crate::db::{EdgeTypeCensus, GraphDb};
use crate::digest::{sanitize, SEP, UNTRUSTED_FRAMING};
use core_storage::fs::Fs;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
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
/// Edge types the `linked by all of` recipe intersects in one `MATCH`. Three
/// is enough to demonstrate the shape — a fourth pattern would not teach a
/// reader anything a third has not already shown, and every one past the
/// first costs a `, (a)-[:TYPE]->(b)` the byte budget pays for.
const LINKED_BY_ALL_MAX_TYPES: usize = 3;
/// Property names that name a node rather than describe it, and so are never
/// what a `what_if` is about. `id` is the identity prop Cypher `CREATE`
/// writes; `key` is what the store calls the same thing.
const IDENTITY_PROPS: [&str; 2] = ["id", "key"];
/// Property names the schema listing does not print. `embedding` is the vector
/// payload `hybrid_search` and `find_similar` read: hundreds of floats, never
/// a question target, and naming it in a schema a session is meant to write
/// queries from invites a query that returns a wall of numbers. The tools that
/// use it do not need to be told it is there.
const HIDDEN_PROPS: [&str; 1] = ["embedding"];

/// What the brief may spend. The `SessionStart` hook has five seconds, and a
/// store too large to describe inside three of them yields what it had reached
/// — a partial ranking is still a valid ordering, partial counts are still
/// lower bounds, and a partial brief is worth more at the start of a session
/// than none.
///
/// It needs the budget: its work is
/// [`GraphDb::wal_total_commits`], which re-reads the WAL — seconds on a store
/// nobody has snapshotted — plus two passes whose length is the store's.
pub(crate) const RANK_BUDGET: Duration = Duration::from_secs(3);

/// How long the brief may take.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BriefOptions {
    /// Wall-clock the whole brief may spend, [`RANK_BUDGET`] by default.
    /// [`Duration::ZERO`] is a budget already spent — every count comes back
    /// as the lower bound reached, which is what the tests use. A budget too
    /// large to add to the clock is no budget at all.
    pub budget: Duration,
}

impl Default for BriefOptions {
    fn default() -> Self {
        Self {
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
    /// Every edge in the store, whatever its type — the store's own count,
    /// not a sum over [`edge_types`](Self::edge_types), so it is exact even
    /// when the budget cut the census short.
    pub edges: usize,
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

/// Whether the deadline has passed. `None` is a run with no budget.
fn spent(deadline: Option<Instant>) -> bool {
    deadline.is_some_and(|dl| Instant::now() >= dl)
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
#[must_use]
pub fn brief<F: Fs>(db: &GraphDb<F>, opts: &BriefOptions) -> SchemaBrief {
    // One deadline for the whole brief, taken before the first read.
    let deadline = Instant::now().checked_add(opts.budget);

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
        entry.1.extend(
            n.props
                .keys()
                .filter(|p| !HIDDEN_PROPS.contains(&p.as_str()))
                .cloned(),
        );
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
        edges: usize::try_from(db.edge_count()).unwrap_or(usize::MAX),
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
    // about what its relationships would become. [`HIDDEN_PROPS`] goes with
    // them: `what_if key embedding <value>` asks the session to type out a
    // vector.
    //
    // The node itself is kept as `key_node` rather than looked up twice: the
    // intersection below reads its label, the same node the field is read
    // from.
    let key_node = nodes.iter().find(|n| n.key == key);
    let field = key_node
        .and_then(|n| {
            let watched = rule_fields.get(&n.label);
            let mut usable = n.props.keys().filter(|f| {
                !IDENTITY_PROPS.contains(&f.as_str()) && !HIDDEN_PROPS.contains(&f.as_str())
            });
            usable
                .clone()
                .find(|f| watched.is_some_and(|w| w.contains(*f)))
                .or_else(|| usable.next())
        })
        .map_or_else(|| "<field>".to_string(), |f| sanitize(f));
    // The same intersection `linked_by_all_recipe` runs for the store's
    // busiest source label, run here for the label of the key already
    // picked above — up to [`LINKED_BY_ALL_MAX_TYPES`] edge types shared
    // between that label and the one target label they most often reach
    // together. `edges_at`, `node_edges` and `what_if` all accept `all_of` /
    // `edge_type`, so the same selection teaches the one-call intersection
    // form on every recipe that names this key, not only on the one recipe
    // that happens to demonstrate a `MATCH`.
    let intersection = key_node.and_then(|n| types_from(&n.label, edge_types));
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
            call: format!(
                "explain_association {a} {b} — returns each relationship's rule and \
                 the values the two share, so there is no need to fetch raw lists to \
                 compare by hand"
            ),
        },
        Recipe {
            question: "relationships".to_string(),
            call: relationships_call(&key, &intersection),
        },
    ];
    if let Some(at) = latest_commit {
        out.push(Recipe {
            question: "as of".to_string(),
            call: as_of_call(&key, at, &intersection),
        });
    }
    out.extend([
        Recipe {
            question: "what if".to_string(),
            call: what_if_call(&key, &field, &intersection),
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
    // The seventh recipe: "linked by all of" — the multi-hop intersection a
    // benchmark showed agents fail, chaining one `MATCH` per relation with
    // fresh variables and so counting each relation independently instead of
    // requiring all of them at once. One `MATCH` with comma-separated
    // patterns sharing `a` and `b` is the shape that actually intersects.
    //
    // Omitted on a store with only one edge type in it altogether: there is
    // nothing to intersect, and a recipe of one pattern would demonstrate the
    // wrong thing.
    if edge_types.len() > 1 {
        if let Some(recipe) = linked_by_all_recipe(labels, edge_types) {
            out.push(recipe);
        }
    }
    out
}

/// The "linked by all of" recipe: up to [`LINKED_BY_ALL_MAX_TYPES`] edge
/// types run between the store's own busiest source label and the
/// destination label it most commonly reaches, intersected in one `MATCH`
/// rather than chained across several.
///
/// The pair is picked from the store's own schema, not asked for: the most
/// populous label that is ever a source (`labels` is already sorted most
/// populous first), then the destination label its edges most often land on,
/// weighted by how many edges each type carries. With fewer than
/// [`LINKED_BY_ALL_MAX_TYPES`] edge types actually running between that
/// pair, the recipe still renders — one pattern is still a worked call, even
/// though there is nothing yet to intersect it against.
///
/// `None` only when no label is ever a source, which does not happen once
/// `edge_types` is non-empty — every edge type's `src` came from a real
/// source label — but the search stays an `Option` rather than assume it.
fn linked_by_all_recipe(labels: &[LabelBrief], edge_types: &[EdgeTypeBrief]) -> Option<Recipe> {
    let src = &labels
        .iter()
        .find(|l| edge_types.iter().any(|t| t.src.contains(&l.label)))?
        .label;
    let (types, dst) = types_from(src, edge_types)?;

    let mut pattern = format!("(a:{src})-[:{}]->(b:{dst})", types[0]);
    for t in &types[1..] {
        pattern.push_str(&format!(", (a)-[:{t}]->(b)"));
    }

    Some(Recipe {
        question: "linked by all of".to_string(),
        call: format!(
            "MATCH {pattern} WITH b, count(DISTINCT a) AS n WHERE n >= 1 RETURN key(b), n \
             ORDER BY n DESC LIMIT 20 — add `WHERE a.<field> = …` before WITH to filter \
             the source side; one MATCH with comma-separated patterns intersects, \
             separate MATCHes do not"
        ),
    })
}

/// Up to [`LINKED_BY_ALL_MAX_TYPES`] edge types running from `src`, and the
/// one destination label they most often reach together — the selection
/// [`linked_by_all_recipe`] runs for the store's own busiest source label,
/// factored out so [`recipes`] can run the identical census-based pick for
/// the source label of whatever key it has already chosen.
///
/// Weighted by how many edges each type carries, ties on the destination
/// label's name; `edge_types` is already sorted most-numerous-first, ties on
/// the name, so filtering it keeps that order — "most populous first" among
/// the types that actually connect `src` to the label picked.
///
/// `None` when `src` is never a source at all — the only way for there to be
/// no destination label to weigh.
fn types_from(src: &str, edge_types: &[EdgeTypeBrief]) -> Option<(Vec<String>, String)> {
    let mut by_dst: BTreeMap<&str, usize> = BTreeMap::new();
    for t in edge_types.iter().filter(|t| t.src.iter().any(|s| s == src)) {
        for dst in &t.dst {
            *by_dst.entry(dst.as_str()).or_default() += t.edges;
        }
    }
    let (dst, _) = by_dst
        .into_iter()
        .max_by_key(|(name, n)| (*n, std::cmp::Reverse(*name)))?;

    let types: Vec<String> = edge_types
        .iter()
        .filter(|t| t.src.iter().any(|s| s == src) && t.dst.iter().any(|d| d.as_str() == dst))
        .take(LINKED_BY_ALL_MAX_TYPES)
        .map(|t| t.edge_type.clone())
        .collect();
    (!types.is_empty()).then(|| (types, dst.to_string()))
}

/// The `relationships` recipe: `node_edges` with the intersection the key's
/// own source label supports, when there is one.
///
/// `all_of` even for a single type — the reply is the same partner-keys
/// shape either way, and the note names the `edge_type` shortcut rather than
/// the call switching form for it. Falls back to the plain call when the key
/// has no [`types_from`] selection at all (a key that is never a source, or
/// a store with no edge types).
fn relationships_call(key: &str, intersection: &Option<(Vec<String>, String)>) -> String {
    match intersection {
        Some((types, dst)) => format!(
            "node_edges {key} all_of: [{}] label: {dst} — or edge_type: {} for one \
             type's partner keys",
            types.join(", "),
            types[0]
        ),
        None => format!("node_edges {key}"),
    }
}

/// The `as of` recipe: `edges_at` with the same intersection, at commit `at`.
///
/// Unlike [`relationships_call`], a single type switches the call itself to
/// `edge_type` rather than an `all_of` of one.
///
/// # The note is about where `at` comes from
///
/// The first association run lost two time-travel cells the same way: the
/// agent had the right data and still answered from an arbitrary late commit,
/// because the question named a *date* and `at` is a commit index. Nothing in
/// a commit carries a date, so guessing one from the end of the WAL is the
/// failure this note exists to stop — an agent asked a question in calendar time
/// going off to *derive* a commit number, by probing `node_history`, or `stats`,
/// or a date map sitting next to the store. As of v0.6.11 `at` takes the date
/// itself, so the note points at that instead. It is worth more here than the
/// `all_of` explanation the note used to carry, which `relationships_call`
/// already gives on the line above.
fn as_of_call(key: &str, at: u64, intersection: &Option<(Vec<String>, String)>) -> String {
    let note = "— or pass a date in place of the number: `2026-06-19`, \
                `2026-06-19T12:00:00Z`. It resolves to the last commit at or \
                before that instant, so there is no commit to go and find";
    match intersection {
        Some((types, dst)) if types.len() >= 2 => format!(
            "edges_at {key} {at} all_of: [{}] label: {dst} {note}",
            types.join(", ")
        ),
        Some((types, _)) => format!("edges_at {key} {at} edge_type: {} {note}", types[0]),
        None => format!("edges_at {key} {at}"),
    }
}

/// The `what if` recipe: `what_if` with the busiest type from the same
/// intersection, so the shown call also demonstrates narrowing to one type's
/// partner keys.
fn what_if_call(key: &str, field: &str, intersection: &Option<(Vec<String>, String)>) -> String {
    match intersection {
        Some((types, _)) => format!(
            "what_if {key} {field} <value> edge_type: {} — the partners that would be \
             lost or gained under that type",
            types[0]
        ),
        None => format!("what_if {key} {field} <value>"),
    }
}

// ── rendering ───────────────────────────────────────────────────────────────

/// Longest session brief, in bytes.
///
/// A `SessionStart` hook's output is prepended to a session and cached for the
/// whole of it, so it is paid for once but carried by every turn. Four
/// thousand bytes is roughly a thousand tokens: enough for two rankings deep
/// enough to be worth having, short enough that a session that never asks the
/// graph anything has lost almost nothing.
pub const MAX_BRIEF_BYTES: usize = 4_000;

/// The one line a store with nothing in it at all gets as a session opens:
/// what is missing, and the command that fixes it. There is nothing to be
/// central *in*, and no point naming a way to reach an empty graph.
///
/// Not marked with [`UNTRUSTED_FRAMING`], unlike every brief with a graph
/// behind it: not one byte of this line came out of a store, so there is
/// nothing here to mark as data.
pub const EMPTY_BRIEF: &str =
    "mushroomdb brief — empty store; run: mushroomdb ingest-git <db> <repo>\n";

/// Headings a memory store's schema sits under.
const BRIEF_LABELS_HEADING: &str = "labels:\n";
const BRIEF_EDGE_TYPES_HEADING: &str = "edge types:\n";
/// The heading over the worked calls. Named for what a reader wants out of
/// it — one call, not a search — because the failure it exists to stop is a
/// session probing the store for its schema before asking anything.
const BRIEF_RECIPES_HEADING: &str = "ask in one call:\n";

/// `1204` → `1,204`. Groups of three, ASCII digits only.
fn thousands(n: usize) -> String {
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
fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("{n} {word}")
    } else {
        format!("{} {word}s", thousands(n))
    }
}

/// Keep whole lines while they fit in `max` bytes, dropping the rest.
///
/// A budget in bytes, unlike one in lines, can fall in the middle of a line —
/// and half a line is worse than no line: a path cut short still reads as a
/// path, and a caller acts on it. So the cut is always at a line ending, and
/// a first line too long to fit yields nothing rather than a fragment.
fn cap_bytes(text: &str, max: usize) -> String {
    let mut out = String::with_capacity(text.len().min(max));
    for line in text.lines() {
        if out.len() + line.len() + 1 > max {
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// A memory store's brief: the schema, then one worked call per question
/// kind, then `reach`.
///
/// `reach` is one line naming how to reach the graph from this session, which
/// only the caller knows — a tool name on the MCP arm, a command on the CLI
/// arm. It is fitted first and appended last, so everything above it gives way
/// to it rather than the other way round. It is therefore the one part exempt
/// from the budget. A store with no nodes and no edges gets [`EMPTY_BRIEF`]
/// and no `reach` line at all.
///
/// The order is the argument. A session that has just been handed the
/// association surface and an unfamiliar store asks two questions before its
/// own — *what is in here* and *how do I ask* — and the first association run
/// showed it answering both by probing Cypher, one guess at a time. So the
/// labels and the edge types come first, complete enough to write a query
/// against, and the worked calls come last, where a reader who skimmed the
/// schema still lands on them.
///
/// **The calls come off last, and only when nothing else is left.** When the
/// budget is short, entries drop from the listings above — edge types first,
/// then labels, since a label with no edge type is still a thing to query and
/// an edge type with no labels is not — and the cut is counted in the same
/// `… and N more` every other digest uses. Dropping a recipe first would save
/// a line and cost the session the round trip the whole section exists to
/// remove.
///
/// # The cap is hard
///
/// Dropping lines alone is not a ceiling: a store whose names are themselves
/// hundreds of bytes long spends the budget inside the lines that remain — a
/// schema of 250-character edge types rendered 5,986 bytes against a 4,000
/// byte cap, because the loop stopped when it ran out of *lines* rather than
/// when it fit. So three measures run in order, each only when the one before
/// it was not enough:
///
/// 1. the listings render whole, which is what every ordinary store gets;
/// 2. every name is cut to [`BRIEF_NAME_CAP`] characters, and entries then
///    drop from the listings against the shorter lines, counted;
/// 3. the worked calls come off from the end, and the brief says so on a
///    final `(brief truncated at 4,000 bytes)` line.
///
/// A brief whose header, history and roles alone overrun the budget — nothing
/// left to drop — is cut on whole lines by [`cap_bytes`], so the returned
/// string is never longer than the budget whatever the store holds.
#[must_use]
pub fn render(s: &SchemaBrief, reach: &str) -> String {
    if s.nodes == 0 && s.edges == 0 {
        return EMPTY_BRIEF.to_string();
    }
    let tail = format!("reach the graph: {}\n", sanitize(reach));
    let budget = MAX_BRIEF_BYTES.saturating_sub(tail.len());

    // A partial schema counts what it reached, so every count it produced is
    // a lower bound. Marked once in the header rather than on each line — the
    // budget the marker is charged against is the same one the counts came
    // short of.
    let at_least = |n: usize| {
        if s.partial {
            format!("≥ {}", thousands(n))
        } else {
            thousands(n)
        }
    };
    let header = format!(
        "{UNTRUSTED_FRAMING}mushroomdb brief — {}{}\n",
        [
            if s.partial {
                format!("≥ {}", plural(s.nodes, "node"))
            } else {
                plural(s.nodes, "node")
            },
            plural(s.edges, "edge"),
            plural(s.labels.len(), "label"),
        ]
        .join(SEP),
        if s.partial { " (partial)" } else { "" }
    );

    let label_lines = |cap: usize| -> Vec<String> {
        s.labels
            .iter()
            .map(|l| {
                let mut line = format!(
                    "  {} ({})",
                    cap_name(&sanitize(&l.label), cap),
                    at_least(l.nodes)
                );
                if !l.props.is_empty() {
                    let props: Vec<String> = l.props.iter().map(|p| cap_name(p, cap)).collect();
                    let _ = write!(line, " — {}", props.join(", "));
                }
                if l.hidden_props > 0 {
                    let _ = write!(line, ", … +{}", l.hidden_props);
                }
                line.push('\n');
                line
            })
            .collect()
    };
    let edge_type_lines = |cap: usize| -> Vec<String> {
        s.edge_types
            .iter()
            .map(|t| {
                let mut line = format!(
                    "  {} ({})",
                    cap_name(&sanitize(&t.edge_type), cap),
                    at_least(t.edges)
                );
                if let Some(rule) = &t.rule {
                    let _ = write!(line, " — rule {}", cap_name(&sanitize(rule), cap));
                    if t.hidden_rules > 0 {
                        let _ = write!(line, " +{}", t.hidden_rules);
                    }
                }
                let _ = writeln!(
                    line,
                    " — {} → {}",
                    end_labels(&t.src, cap),
                    end_labels(&t.dst, cap)
                );
                line
            })
            .collect()
    };

    // The part that gives way last: how deep the history runs, who may read
    // it, and the calls.
    // `unknown`, not `0`: a history the budget never counted is not a history
    // that is not there, and the two lead a reader to opposite conclusions.
    let mut prefix = match s.commits {
        Some(n) => format!("history: {n} commits\n"),
        None => "history: unknown\n".to_string(),
    };
    if !s.roles.is_empty() {
        let roles: Vec<String> = s
            .roles
            .iter()
            .map(|(name, labels)| {
                if labels.is_empty() {
                    sanitize(name)
                } else {
                    format!("{} ({})", sanitize(name), labels.join(", "))
                }
            })
            .collect();
        let _ = writeln!(prefix, "roles: {}", roles.join(SEP));
    }
    let recipes: Vec<String> = s
        .recipes
        .iter()
        .map(|r| format!("  {}: {}\n", sanitize(&r.question), sanitize(&r.call)))
        .collect();
    let with_recipes = |kept: usize| -> String {
        let mut fixed = prefix.clone();
        if kept > 0 {
            fixed.push_str(BRIEF_RECIPES_HEADING);
            fixed.extend(recipes[..kept].iter().map(String::as_str));
        }
        fixed
    };
    let fixed = with_recipes(recipes.len());

    // Measure one: the listings whole. A store whose brief already fits — every
    // ordinary one — renders exactly the bytes it always did, since nothing
    // below runs.
    let whole = memory_body(
        &header,
        &label_lines(usize::MAX),
        &edge_type_lines(usize::MAX),
        &fixed,
        0,
    );
    if whole.len() <= budget {
        return whole + &tail;
    }

    // Measure two: every name cut to [`BRIEF_NAME_CAP`], and *then* entries
    // dropped against the shorter lines. Cutting before dropping rather than
    // after is the order that does anything: a brief over budget because one
    // name is 250 characters keeps its whole schema once the name is cut,
    // where dropping first would throw away entries to pay for the names
    // inside the few that remain — and by the time dropping alone has run out
    // of entries there are no names left to cut.
    let mut labels = label_lines(BRIEF_NAME_CAP);
    let mut edge_types = edge_type_lines(BRIEF_NAME_CAP);
    let mut dropped = 0;
    loop {
        let body = memory_body(&header, &labels, &edge_types, &fixed, dropped);
        if body.len() <= budget {
            return body + &tail;
        }
        if edge_types.pop().is_none() && labels.pop().is_none() {
            break;
        }
        dropped += 1;
    }

    // Measure three: the worked calls, from the end, and a line that says the
    // brief was cut — without it a session reads a truncated set of recipes as
    // the whole set.
    let truncated = format!(
        "(brief truncated at {} bytes)\n",
        thousands(MAX_BRIEF_BYTES)
    );
    let mut kept = recipes.len();
    loop {
        let body = memory_body(&header, &[], &[], &with_recipes(kept), dropped) + &truncated;
        if body.len() <= budget {
            return body + &tail;
        }
        if kept == 0 {
            // Nothing droppable is left: the header, the history and the roles
            // alone overrun the budget. Whole lines come off the end so the
            // ceiling holds whatever the store is named.
            return cap_bytes(&body, budget) + &tail;
        }
        kept -= 1;
    }
}

/// Longest a name may print in a memory brief that did not fit its budget
/// with every droppable listing entry already gone.
///
/// Sixty characters is longer than any name written to be read and short
/// enough that a line spends its budget on the schema rather than on one
/// identifier. It applies only to the cut round: a store whose names are
/// ordinary never reaches it, and renders exactly what it rendered before.
const BRIEF_NAME_CAP: usize = 60;

/// `name` cut to at most `cap` characters, the last of them `…` when anything
/// came off.
///
/// Counted in characters and cut on a character boundary, so a name of runes
/// is never halved mid-rune. `usize::MAX` is the uncut round and returns the
/// name whole.
fn cap_name(name: &str, cap: usize) -> String {
    if cap == 0 || name.chars().count() <= cap {
        return name.to_string();
    }
    let end = name
        .char_indices()
        .nth(cap - 1)
        .map_or(name.len(), |(i, _)| i);
    format!("{}…", &name[..end])
}

/// A memory store's brief above its `reach` line, for one candidate schema.
fn memory_body(
    header: &str,
    labels: &[String],
    edge_types: &[String],
    fixed: &str,
    dropped: usize,
) -> String {
    let mut out = String::from(header);
    if !labels.is_empty() {
        out.push_str(BRIEF_LABELS_HEADING);
        out.extend(labels.iter().map(String::as_str));
    }
    if !edge_types.is_empty() {
        out.push_str(BRIEF_EDGE_TYPES_HEADING);
        out.extend(edge_types.iter().map(String::as_str));
    }
    if dropped > 0 {
        let _ = writeln!(out, "  … and {dropped} more");
    }
    out.push_str(fixed);
    out
}

/// The labels on one end of an edge type, as one phrase, each cut to `cap`
/// characters. An edge type seen between nodes of no known label — every
/// endpoint tombstoned — says `?` rather than leaving the arrow with nothing
/// on one side.
fn end_labels(labels: &[String], cap: usize) -> String {
    if labels.is_empty() {
        "?".to_string()
    } else {
        labels
            .iter()
            .map(|l| cap_name(&sanitize(l), cap))
            .collect::<Vec<_>>()
            .join("|")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_bytes_keeps_whole_lines_and_never_half_of_one() {
        let text = "aaaa\nbbbb\ncccc\n"; // three five-byte lines
        assert_eq!(cap_bytes(text, 15), text, "the whole text fits exactly");
        assert_eq!(
            cap_bytes(text, 14),
            "aaaa\nbbbb\n",
            "the last line is whole"
        );
        assert_eq!(cap_bytes(text, 10), "aaaa\nbbbb\n");
        assert_eq!(cap_bytes(text, 9), "aaaa\n");
        assert_eq!(
            cap_bytes(text, 4),
            "",
            "a first line too long yields nothing, never a fragment"
        );
        assert_eq!(cap_bytes(text, 0), "");
        // A line with no trailing newline still costs the one it is given.
        assert_eq!(cap_bytes("abc", 4), "abc\n");
        assert_eq!(cap_bytes("abc", 3), "");
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
}
