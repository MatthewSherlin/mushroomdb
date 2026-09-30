//! The session brief on a memory store, after it left `repograph`.
//!
//! The brief is what a `SessionStart` hook prepends to a session and the host
//! caches for the whole of it, so it is byte-stable for a given store by
//! contract, not by luck. That makes it the one thing in this move with an
//! exact oracle: the same store must render the same bytes it rendered before.
use core_api::digest::UNTRUSTED_FRAMING;
use core_api::memory::brief::{brief, render, BriefOptions, MAX_BRIEF_BYTES};
use core_api::memory_schema::memory_defaults;
use core_api::{GraphDb, Value};
use std::time::Duration;

fn tmp(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "mdb-membrief-{name}-{}-{nanos}",
        std::process::id()
    ))
}

/// A store at `dir`, opened for writing.
fn open(dir: &std::path::Path) -> GraphDb<core_storage::fs::RealFs> {
    GraphDb::open(dir).expect("open")
}

/// A memory store with two labels, one edge type and one note.
fn store(name: &str) -> GraphDb<core_storage::fs::RealFs> {
    let mut db = GraphDb::open(&tmp(name)).unwrap();
    db.apply_schema(&memory_defaults()).unwrap();
    db.insert_node("Person", "matthew", vec![]).unwrap();
    db.set_prop("matthew", "name", Value::Str("Matthew Sherlin".into()))
        .unwrap();
    db.insert_node("Project", "v0.7", vec![]).unwrap();
    db.set_prop("v0.7", "name", Value::Str("the write path".into()))
        .unwrap();
    db.insert_node("Note", "note-1", vec![]).unwrap();
    db.set_prop(
        "note-1",
        "text",
        Value::Str("Matthew wants 0.7 to focus on the write path".into()),
    )
    .unwrap();
    // The one edge type. Without it a spent budget counts no nodes and no
    // edges, which is the empty-store test, and the brief renders
    // `EMPTY_BRIEF` rather than a partial schema.
    db.insert_edge("WORKS_ON", "matthew", "v0.7").unwrap();
    db
}

#[test]
fn a_memory_store_brief_names_its_labels_and_its_edge_count() {
    let db = store("labels");
    let b = brief(&db, &BriefOptions::default());
    let text = render(&b, "query '<cypher>'");
    for want in ["labels:", "Person", "Project", "Note", "reach the graph:"] {
        assert!(text.contains(want), "brief missing {want}: {text}");
    }
    assert_eq!(b.nodes, 3, "three nodes: {b:?}");
}

#[test]
fn the_brief_is_byte_stable_for_the_same_store() {
    let db = store("stable");
    let a = render(&brief(&db, &BriefOptions::default()), "query '<cypher>'");
    let b = render(&brief(&db, &BriefOptions::default()), "query '<cypher>'");
    assert_eq!(a, b, "the host caches this for a whole session");
}

#[test]
fn the_brief_never_exceeds_its_budget_however_long_the_names_are() {
    // The cap is hard, not advisory: a store whose names are hundreds of bytes
    // long spends the budget inside the lines that remain, which is why the
    // renderer cuts names and then drops entries and then drops recipes.
    let mut db = GraphDb::open(&tmp("huge")).unwrap();
    db.apply_schema(&memory_defaults()).unwrap();
    for i in 0..40 {
        let key = format!("{}-{i}", "n".repeat(250));
        db.insert_node("Person", &key, vec![]).unwrap();
        db.set_prop(&key, "name", Value::Str("x".repeat(250)))
            .unwrap();
    }
    let text = render(&brief(&db, &BriefOptions::default()), "query '<cypher>'");
    assert!(
        text.len() <= MAX_BRIEF_BYTES + "reach the graph: query '<cypher>'\n".len(),
        "brief ran to {} bytes: {text}",
        text.len()
    );
}

#[test]
fn a_spent_budget_reports_lower_bounds_rather_than_wrong_totals() {
    let db = store("partial");
    let b = brief(
        &db,
        &BriefOptions {
            budget: Duration::ZERO,
        },
    );
    assert!(b.partial, "a spent budget must say so: {b:?}");
    assert!(
        b.commits.is_none(),
        "history is the first thing dropped: {b:?}"
    );
    let text = render(&b, "query '<cypher>'");
    assert!(text.contains("(partial)"), "the reader must see it: {text}");
    assert!(
        text.contains("history: unknown"),
        "unknown is not zero: {text}"
    );
}

// ---------------------------------------------------------------------------
// Moved from `tests/repograph.rs` with the code they test.
// ---------------------------------------------------------------------------

/// A store with no files but plenty in it — a memory graph — is not an empty
/// store, and gets a header and a way in rather than "run ingest-git".
#[test]
fn brief_on_a_store_with_no_files_still_says_how_to_reach_it() {
    let dir = tmp("brief-memory-only");
    let mut db = open(&dir);
    db.insert_node(
        "Person",
        "person:1",
        vec![("name".into(), core_api::Value::Str("Ada".to_string()))],
    )
    .expect("a node that is not a file");
    db.insert_node(
        "Person",
        "person:2",
        vec![("name".into(), core_api::Value::Str("Grace".to_string()))],
    )
    .expect("another");
    db.insert_edge("KNOWS", "person:1", "person:2")
        .expect("edge");

    let b = brief(&db, &BriefOptions::default());
    assert_eq!(b.nodes, 2);
    assert_eq!(b.edges, 1);

    let text = render(&b, "explore <target>");
    assert!(
        text.lines()
            .nth(1)
            .expect("a header")
            .starts_with("mushroomdb brief — 2 nodes · 1 edge · 1 label"),
        "a store with a graph in it is not an empty store: {text}"
    );
    assert!(
        text.ends_with("reach the graph: explore <target>\n"),
        "{text}"
    );
}

// ---------------------------------------------------------------------------
// `brief` on a memory store: the schema, and one worked call per question
// kind.
//
// The first association run showed the graph arm spending turn after turn
// probing Cypher for the store's labels and edge types before it could ask
// anything, and never reaching for the purpose-built tools at all. Everything
// below is that failure, written down: what the schema section has to name,
// and that each question kind arrives already worked into a call.
// ---------------------------------------------------------------------------

/// The seeded memory store every test below reads: two labels, one rule that
/// derives an edge type from a key match, a hand-written edge type, and two
/// roles.
///
/// `person:ada` is deliberately the alphabetically first `Person`, so the
/// worked calls that name "a key with edges" name a key the test can predict.
fn seeded_memory_store(name: &str) -> core_api::GraphDb<core_storage::fs::RealFs> {
    use core_api::schema::Schema;
    use core_api::{Predicate, RoleDef, RuleDef, Value};

    let dir = tmp(name);
    let mut db = open(&dir);
    db.apply_schema(&Schema {
        fulltext: vec![],
        indexes: vec![],
        rules: vec![RuleDef {
            name: "assigned_to".into(),
            src_label: "Person".into(),
            dst_label: "Project".into(),
            predicate: Predicate::KeyMatch {
                field: "project_id".into(),
            },
            edge_type: "ASSIGNED_TO".into(),
            weight_prop: None,
            max_edges: None,
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
            namespace: None,
        }],
        views: vec![],
        roles: vec![
            RoleDef {
                name: "auditor".into(),
                keys: vec![],
                labels: vec!["Person".into(), "Project".into()],
                visible_where: None,
                namespaces: None,
                write: None,
            },
            RoleDef {
                name: "analyst".into(),
                keys: vec![],
                labels: vec!["Person".into()],
                visible_where: None,
                namespaces: None,
                write: None,
            },
        ],
    })
    .expect("schema");

    for (key, name, stage) in [
        ("project:apollo", "Apollo", "live"),
        ("project:borealis", "Borealis", "draft"),
    ] {
        db.insert_node(
            "Project",
            key,
            vec![
                ("name".into(), Value::Str(name.to_string())),
                ("stage".into(), Value::Str(stage.to_string())),
            ],
        )
        .expect("project");
    }
    // All three on one project, so the `how many` recipe — which filters on
    // `n >= HOW_MANY_MIN`, three — has an answer on the store it was rendered
    // from. A fixture whose own worked call returns nothing cannot pin that
    // the call works.
    for (key, name, team, project) in [
        ("person:ada", "Ada", "core", "project:apollo"),
        ("person:bob", "Bob", "core", "project:apollo"),
        ("person:cy", "Cy", "web", "project:apollo"),
    ] {
        db.insert_node(
            "Person",
            key,
            vec![
                ("name".into(), Value::Str(name.to_string())),
                ("team".into(), Value::Str(team.to_string())),
                ("project_id".into(), Value::Str(project.to_string())),
            ],
        )
        .expect("person");
    }
    db.insert_edge("KNOWS", "person:ada", "person:bob")
        .expect("edge");
    db.insert_edge("KNOWS", "person:bob", "person:cy")
        .expect("edge");
    db
}

/// A memory store whose busiest source label reaches a second label by two
/// edge types, not one — unlike [`seeded_memory_store`], where `Person` only
/// ever reaches `Project` by `ASSIGNED_TO`. This is the shape that exercises
/// the `all_of:`/`label:` form of the `as of` and `relationships` recipes
/// rather than their single-type `edge_type:` fallback.
fn seeded_memory_store_with_two_source_edge_types(
    name: &str,
) -> core_api::GraphDb<core_storage::fs::RealFs> {
    use core_api::schema::Schema;
    use core_api::{Predicate, RuleDef, Value};

    let dir = tmp(name);
    let mut db = open(&dir);
    db.apply_schema(&Schema {
        fulltext: vec![],
        indexes: vec![],
        rules: vec![RuleDef {
            name: "assigned_to".into(),
            src_label: "Person".into(),
            dst_label: "Project".into(),
            predicate: Predicate::KeyMatch {
                field: "project_id".into(),
            },
            edge_type: "ASSIGNED_TO".into(),
            weight_prop: None,
            max_edges: None,
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
            namespace: None,
        }],
        views: vec![],
        roles: vec![],
    })
    .expect("schema");

    for key in ["project:apollo", "project:borealis"] {
        db.insert_node("Project", key, vec![]).expect("project");
    }
    for (key, project) in [
        ("person:ada", "project:apollo"),
        ("person:bob", "project:apollo"),
    ] {
        db.insert_node(
            "Person",
            key,
            vec![("project_id".into(), Value::Str(project.to_string()))],
        )
        .expect("person");
    }
    // A second, hand-written edge type between the same two labels the rule
    // already connects — `Project` stays the label `ASSIGNED_TO` and
    // `REVIEWED` both most often reach, so the census picks up both.
    db.insert_edge("REVIEWED", "person:ada", "project:apollo")
        .expect("edge");
    db.insert_edge("REVIEWED", "person:bob", "project:apollo")
        .expect("edge");
    db
}

/// The schema section: every label with its property names and node count,
/// every edge type with the rule that derives it, the labels it runs between
/// and how many there are, how deep the history runs, and who may read it.
#[test]
fn brief_on_a_memory_store_prints_the_schema() {
    let db = seeded_memory_store("brief-memory-schema");
    let b = brief(&db, &BriefOptions::default());
    let s = &b;

    assert_eq!(s.nodes, 5);
    assert_eq!(
        s.labels
            .iter()
            .map(|l| (l.label.as_str(), l.nodes))
            .collect::<Vec<_>>(),
        vec![("Person", 3), ("Project", 2)],
        "most populous first"
    );
    assert_eq!(
        s.labels[0].props,
        vec!["name".to_string(), "project_id".into(), "team".into()],
        "the union of the label's property names, sorted"
    );
    assert_eq!(
        s.edge_types
            .iter()
            .map(|t| (t.edge_type.as_str(), t.rule.as_deref(), t.edges))
            .collect::<Vec<_>>(),
        vec![("ASSIGNED_TO", Some("assigned_to"), 3), ("KNOWS", None, 2),],
        "most numerous first, and a derived type names its rule"
    );
    assert_eq!(s.edge_types[0].src, vec!["Person".to_string()]);
    assert_eq!(s.edge_types[0].dst, vec!["Project".to_string()]);
    assert_eq!(
        s.roles,
        vec![
            ("analyst".to_string(), vec!["Person".to_string()]),
            (
                "auditor".to_string(),
                vec!["Person".to_string(), "Project".to_string()]
            ),
        ],
        "roles.json, sorted by name"
    );

    let text = render(&b, "query '<cypher>'");
    assert!(text.starts_with(UNTRUSTED_FRAMING), "{text}");
    for line in [
        "mushroomdb brief — 5 nodes · 5 edges · 2 labels",
        "labels:",
        "  Person (3) — name, project_id, team",
        "  Project (2) — name, stage",
        "edge types:",
        "  ASSIGNED_TO (3) — rule assigned_to — Person → Project",
        "  KNOWS (2) — Person → Person",
        &format!("history: {} commits", s.commits.expect("counted")),
        "roles: analyst (Person) · auditor (Person, Project)",
    ] {
        assert!(
            text.lines().any(|l| l == line),
            "the schema must print {line:?}:\n{text}"
        );
    }
    assert!(
        s.commits.is_some_and(|c| c > 0),
        "a store that was written to has a history"
    );
    assert!(
        text.ends_with("reach the graph: query '<cypher>'\n"),
        "{text}"
    );
}

/// One worked call per question kind, in order, with the store's own keys,
/// labels and edge types already substituted in — so the call can be made
/// rather than researched.
#[test]
fn brief_on_a_memory_store_works_one_call_per_question_kind() {
    let db = seeded_memory_store("brief-memory-recipes");
    let b = brief(&db, &BriefOptions::default());
    let s = &b;

    let got: Vec<(&str, &str)> = s
        .recipes
        .iter()
        .map(|r| (r.question.as_str(), r.call.as_str()))
        .collect();
    assert_eq!(
        got,
        vec![
            (
                "why",
                "explain_association person:ada project:apollo — returns each \
                 relationship's rule and the values the two share, so there is no \
                 need to fetch raw lists to compare by hand",
            ),
            // Person is the only source label, and only `ASSIGNED_TO` runs
            // from it to the target label (`Project`) its edges most often
            // reach — `KNOWS` lands on `Person`, not `Project` — so there is
            // only one type to name and the recipe shows `edge_type:` rather
            // than a one-element `all_of:`.
            (
                "relationships",
                "node_edges person:ada all_of: [ASSIGNED_TO] label: Project — or \
                 edge_type: ASSIGNED_TO for one type's partner keys",
            ),
            // The newest commit `edges_at` accepts, which is one below the
            // count on a store nothing has pruned — the off-by-one that made
            // this recipe `CommitOutOfRange` is pinned by
            // `the_as_of_recipe_names_a_commit_edges_at_accepts`.
            (
                "as of",
                // The note names where `at` comes from. Two time-travel cells were
                // lost to an agent picking an arbitrary late commit for a date;
                // v0.6.11 lets `at` take the date, so the note points there rather
                // than sending the agent off to derive a commit.
                &*format!(
                    "edges_at person:ada {} edge_type: ASSIGNED_TO — or pass a date \
                     in place of the number: `2026-06-19`, `2026-06-19T12:00:00Z`. \
                     It resolves to the last commit at or before that instant, so \
                     there is no commit to go and find",
                    s.commits.expect("counted") - 1
                ),
            ),
            // `project_id`, not `name`: the field the `assigned_to` rule
            // reads, so the call shown is one that would really lose and gain
            // an edge. Only the new value stays a placeholder.
            (
                "what if",
                "what_if person:ada project_id <value> edge_type: ASSIGNED_TO — the \
                 partners that would be lost or gained under that type",
            ),
            // `key(n)`, not `n.key`: a node's key is not a property, so
            // `n.key` renders a column of nulls. Pinned by
            // `every_cypher_recipe_answers_on_the_store_it_came_from`.
            (
                "who may see",
                "query 'MATCH (n:Person) RETURN key(n) LIMIT 20' role: analyst",
            ),
            (
                "how many",
                "MATCH (a:Person)-[:ASSIGNED_TO]->(b:Project) WITH key(b) AS b_key, \
                 count(a) AS n WHERE n >= 3 RETURN b_key, n",
            ),
            // The seventh recipe: Person is the only label that is ever a
            // source, and its edges land on Project (3) more than on Person
            // itself (2, via KNOWS) — so the pair is (Person, Project), and
            // only ASSIGNED_TO runs between them.
            (
                "linked by all of",
                "MATCH (a:Person)-[:ASSIGNED_TO]->(b:Project) WITH b, count(DISTINCT a) \
                 AS n WHERE n >= 1 RETURN key(b), n ORDER BY n DESC LIMIT 20 — add \
                 `WHERE a.<field> = …` before WITH to filter the source side; one \
                 MATCH with comma-separated patterns intersects, separate MATCHes \
                 do not",
            ),
        ],
        "every placeholder the store can fill is filled"
    );

    let text = render(&b, "query '<cypher>'");
    assert!(text.contains("ask in one call:\n"), "{text}");
    for (question, call) in &got {
        assert!(
            text.lines().any(|l| l == format!("  {question}: {call}")),
            "the brief must print the worked call for {question:?}:\n{text}"
        );
    }
    // The two tools this section exists to point at are named where a reader
    // meets them, not left to a tool search.
    assert!(
        text.contains("edges_at ") && text.contains("what_if "),
        "{text}"
    );
}

/// Binding: when the key's own source label runs more than one edge type to
/// the label it most often reaches, `as of` and `relationships` show the
/// `all_of:`/`label:` intersection form rather than the single-type
/// `edge_type:` fallback, and `what if` always names `edge_type:` — the
/// one-call form the time-travel question needs, over the grouped view that
/// caps at 10 partners a type and never lets an agent ask for the
/// intersection directly.
#[test]
fn as_of_relationships_and_what_if_show_the_one_call_intersection_form() {
    let db = seeded_memory_store_with_two_source_edge_types("brief-memory-intersection");
    let b = brief(&db, &BriefOptions::default());
    let s = &b;

    let call = |question: &str| {
        s.recipes
            .iter()
            .find(|r| r.question == question)
            .unwrap_or_else(|| panic!("no {question:?} recipe: {:?}", s.recipes))
            .call
            .clone()
    };

    let as_of = call("as of");
    assert!(as_of.contains("all_of:"), "{as_of}");
    assert!(as_of.contains("label:"), "{as_of}");

    let relationships = call("relationships");
    assert!(relationships.contains("all_of:"), "{relationships}");
    assert!(relationships.contains("label:"), "{relationships}");

    let what_if = call("what if");
    assert!(what_if.contains("edge_type:"), "{what_if}");
}

/// Binding: the `as of` recipe is a call that *works*, not a line that reads
/// like one — the commit it names is inside `edges_at`'s accepted range.
///
/// `history: N commits` counts commits; `edges_at`'s `at` is a zero-based WAL
/// index whose valid range is `wal_horizon_floor..total_commits`. The count
/// and the last index are off by one, so substituting the count produced a
/// worked example that answered `CommitOutOfRange` on every store there has
/// ever been — the one failure mode a recipe must not have, since a session
/// that copies it learns the tool is broken and goes back to probing Cypher.
///
/// So the test does not read the recipe: it *runs* it. The commit argument is
/// parsed back out of the rendered brief — the bytes a session actually sees —
/// and handed to the engine.
#[test]
fn the_as_of_recipe_names_a_commit_edges_at_accepts() {
    let db = seeded_memory_store("brief-memory-as-of");
    let text = render(&brief(&db, &BriefOptions::default()), "query '<cypher>'");

    let line = text
        .lines()
        .find_map(|l| l.strip_prefix("  as of: edges_at "))
        .expect("the brief prints an `as of` recipe");
    // The call carries the intersection form after the commit —
    // `all_of:`/`edge_type:`/`label:` and a trailing note — so only the
    // first two whitespace-separated tokens are `<key>` and `<commit>`.
    let mut tokens = line.split_whitespace();
    let key = tokens.next().expect("edges_at <key> <commit> …");
    let at: u64 = tokens
        .next()
        .expect("edges_at <key> <commit> …")
        .parse()
        .expect("the commit argument is a number");

    let edges = db
        .edges_at(key, at)
        .unwrap_or_else(|e| panic!("the brief's own worked call must answer: {e}"));
    assert!(
        !edges.is_empty(),
        "the recipe names a key with edges at that commit, or it teaches nothing"
    );

    // And it is the *latest* commit: one past it is out of range, which is
    // what pins the off-by-one rather than merely stepping back far enough to
    // stop failing.
    assert!(
        db.edges_at(key, at + 1).is_err(),
        "the recipe must name the newest commit edges_at accepts, got {at}"
    );
}

/// Binding: a store whose history is gone prints no `as of` recipe at all.
///
/// A WAL-truncating snapshot leaves `wal_horizon_floor == total_commits` — an
/// empty range, in which *every* commit index is out of range. Measured on the
/// association store: `mushroomdb snapshot --truncate` takes the brief from 95
/// s to 0.66 s, so this is the shape a large store will actually be in, not a
/// corner. The five remaining calls still work; a sixth that could not would
/// teach the session the tool is broken.
#[test]
fn a_store_with_no_reachable_history_shows_no_as_of_recipe() {
    let mut db = seeded_memory_store("brief-memory-truncated");
    db.snapshot().expect("fold the WAL into a snapshot");

    let b = brief(&db, &BriefOptions::default());
    let s = &b;
    let questions: Vec<&str> = s.recipes.iter().map(|r| r.question.as_str()).collect();

    // The schema itself is untouched — truncation costs history, not shape.
    assert!(!s.labels.is_empty() && !s.edge_types.is_empty());

    let text = render(&b, "query '<cypher>'");
    if s.commits == Some(0) {
        assert!(
            !questions.contains(&"as of"),
            "no history means no `as of` call to show: {questions:?}"
        );
        assert!(!text.contains("edges_at "), "{text}");
        assert!(text.contains("history: 0 commits"), "{text}");
    } else {
        // Archives survived, so history did: then the recipe must still be
        // callable, which is the same invariant the test above pins.
        let line = text
            .lines()
            .find_map(|l| l.strip_prefix("  as of: edges_at "))
            .expect("history means an `as of` recipe");
        let mut tokens = line.split_whitespace();
        let key = tokens.next().expect("edges_at <key> <commit> …");
        let at: u64 = tokens
            .next()
            .expect("edges_at <key> <commit> …")
            .parse()
            .expect("a commit number");
        db.edges_at(key, at)
            .unwrap_or_else(|e| panic!("the brief's own worked call must answer: {e}"));
    }
    // Whatever happened to the history, the other five calls are still there.
    for question in ["why", "relationships", "what if", "who may see", "how many"] {
        assert!(
            questions.contains(&question),
            "{question:?} does not depend on history: {questions:?}"
        );
    }
}

/// Timing probe for the memory-store brief on a real store. Ignored by
/// default; point `MUSHROOMDB_BENCH_STORE` at a store directory and run with
/// `--ignored`, the same contract `tests/edges_at.rs` uses.
///
/// The number that matters is the *delta* over opening the store, which every
/// hook pays whatever it then asks. `brief` was walking every edge through
/// `all_edges_for_export` — three `String`s and a provenance entry apiece —
/// which on the association store's 1.29 M derived edges cost seven seconds in
/// debug on top of the open. `edge_type_census` replaced that walk; this
/// reports what the replacement costs, so a regression has a number to fail
/// against rather than a feeling.
#[test]
#[ignore]
fn memory_brief_bench_on_large_store() {
    let Ok(path) = std::env::var("MUSHROOMDB_BENCH_STORE") else {
        eprintln!("MUSHROOMDB_BENCH_STORE unset — skipping");
        return;
    };

    let t0 = std::time::Instant::now();
    let db = core_api::GraphDb::open_with_options(
        std::path::Path::new(&path),
        core_api::OpenOptions {
            auto_migrate: false,
            repair_wal: false,
            read_only: true,
        },
    )
    .expect("open the bench store read-only");
    let opened = t0.elapsed();
    eprintln!("open (read-only):      {opened:?}");

    let t1 = std::time::Instant::now();
    let census = db.edge_type_census();
    let census_took = t1.elapsed();
    eprintln!("edge_type_census:      {census_took:?}");

    let t2 = std::time::Instant::now();
    let total = db.wal_total_commits().expect("wal commits");
    eprintln!("wal_total_commits:     {:?}", t2.elapsed());

    let t3 = std::time::Instant::now();
    let b = brief(&db, &BriefOptions::default());
    let built = t3.elapsed();
    eprintln!("brief (whole):         {built:?}");

    let text = render(&b, "query '<cypher>'");
    let s = &b;
    eprintln!(
        "{} nodes, {} labels, {} edge types, {} edges, {total} wal commits, brief {} B",
        s.nodes,
        s.labels.len(),
        census.len(),
        b.edges,
        text.len()
    );
    assert!(text.len() <= MAX_BRIEF_BYTES);
    // Two budgets, because the brief's time is spent on two different things.
    //
    // The census is what this change owns — the walk that used to be
    // `all_edges_for_export` — and on 1.29 M edges it is a tenth of a second
    // in debug. That is the number a regression would blow, so it gets the
    // tight budget.
    //
    // The rest is `wal_total_commits`, which re-reads the WAL to learn the
    // commit the `as of` recipe may name. On this store — 166 MiB of WAL and
    // no snapshot — that is ~2.3 s, and it is also why opening the store costs
    // ninety. On a store anyone has ever snapshotted the WAL is empty and the
    // scan is free: `mushroomdb snapshot --truncate` takes the whole
    // `mushroomdb brief` on this store from 95 s to 0.7 s. So the whole-brief
    // budget is the hook's five seconds, measured in debug, which is three to
    // five times slower than the binary the hook runs.
    assert!(
        census_took < std::time::Duration::from_secs(1),
        "the edge-type census took {census_took:?} on {} edges",
        b.edges
    );
    assert!(
        built < std::time::Duration::from_secs(5),
        "describing the store took {built:?} (open was {opened:?})"
    );
}

/// The same store renders the same bytes, inside the same budget — the brief
/// is cached for a whole session, so two prompts must not disagree about what
/// the store is.
#[test]
fn a_memory_brief_is_byte_stable_and_within_budget() {
    let db = seeded_memory_store("brief-memory-stable");
    let one = render(&brief(&db, &BriefOptions::default()), "query '<cypher>'");
    let two = render(&brief(&db, &BriefOptions::default()), "query '<cypher>'");
    assert_eq!(one, two, "the same store renders the same bytes");
    assert!(one.len() <= MAX_BRIEF_BYTES, "{}", one.len());
    assert!(
        !one.contains("ago"),
        "no relative times: the brief must be byte-stable across prompts"
    );
    assert_eq!(one.matches(UNTRUSTED_FRAMING).count(), 1, "{one}");
}

/// A store with more schema than the budget holds loses schema lines from the
/// end, counted — and never loses a worked call, which is the part that saves
/// the session a round trip.
#[test]
fn a_wide_memory_schema_is_counted_off_and_keeps_every_worked_call() {
    use core_api::Value;

    let dir = tmp("brief-memory-wide");
    let mut db = open(&dir);
    for i in 0..300 {
        db.insert_node(
            &format!("Label{i:03}"),
            &format!("node:{i:03}"),
            vec![("name".into(), Value::Str(format!("n{i}")))],
        )
        .expect("node");
    }
    db.insert_edge("KNOWS", "node:000", "node:001")
        .expect("edge");

    let b = brief(&db, &BriefOptions::default());
    let text = render(&b, "query '<cypher>'");
    assert!(text.len() <= MAX_BRIEF_BYTES, "{}", text.len());
    let marker = text
        .lines()
        .find(|l| l.starts_with("  … and "))
        .expect("a cut listing says how much it cut");
    let dropped: usize = marker
        .trim_start_matches("  … and ")
        .trim_end_matches(" more")
        .parse()
        .expect("a counted marker");
    assert!(dropped > 0 && dropped < 300, "{marker}");
    assert!(
        text.lines().filter(|l| l.starts_with("  ")).count() > 0,
        "some schema survives: {text}"
    );
    for question in [
        "why",
        "relationships",
        "as of",
        "what if",
        "who may see",
        "how many",
    ] {
        assert!(
            text.contains(&format!("  {question}: ")),
            "the {question:?} call must survive the cut:\n{text}"
        );
    }
    assert!(
        text.ends_with("reach the graph: query '<cypher>'\n"),
        "{text}"
    );
    // Exactly one edge type — `KNOWS` — runs in this whole store, so there is
    // nothing for `linked by all of` to intersect and the recipe is omitted.
    assert!(
        !text.contains("linked by all of"),
        "a single edge type has nothing to intersect:\n{text}"
    );
}

/// Binding: the 4,000-byte cap is a ceiling, not a loop that stops when it
/// runs out of lines to drop.
///
/// A store whose labels and edge types are 250 characters each spends the
/// budget *inside* the lines rather than across them: dropping every listing
/// entry still left 5,986 bytes, because the header, the recipes and the
/// roles all quote those same names. The names are cut, and the worked calls
/// come off from the end with the brief saying it was truncated.
#[test]
fn a_brief_of_very_long_names_is_still_capped_at_four_thousand_bytes() {
    use core_api::Value;

    let dir = tmp("brief-memory-long-names");
    let mut db = open(&dir);
    let long = |prefix: &str, i: usize| format!("{prefix}{i:03}{}", "X".repeat(240));
    for i in 0..12 {
        for n in 0..2 {
            db.insert_node(
                &long("Label", i),
                &format!("node:{i:03}:{n}"),
                vec![(long("prop", i), Value::Str("v".to_string()))],
            )
            .expect("node");
        }
    }
    for i in 0..12 {
        db.insert_edge(
            &long("EDGETYPE", i),
            &format!("node:{i:03}:0"),
            &format!("node:{i:03}:1"),
        )
        .expect("edge");
    }

    let b = brief(&db, &BriefOptions::default());
    let text = render(&b, "query '<cypher>'");
    assert!(
        text.len() <= MAX_BRIEF_BYTES,
        "the cap is hard: {} bytes\n{text}",
        text.len()
    );
    // Nothing was left to cut, so the worked calls came off from the end —
    // and the brief says it was cut rather than reading as a whole one.
    assert!(
        text.contains("(brief truncated at 4,000 bytes)\n"),
        "a brief that lost recipes says so:\n{text}"
    );
    let all = b.recipes.len();
    let printed = text
        .lines()
        .filter(|l| {
            l.starts_with("  ")
                && b.recipes
                    .iter()
                    .any(|r| l.starts_with(&format!("  {}: ", r.question)))
        })
        .count();
    assert!(
        printed < all,
        "the recipes are cut from the end: {printed} of {all} printed\n{text}"
    );
    // And the reach line is still the last thing a reader sees.
    assert!(
        text.ends_with("reach the graph: query '<cypher>'\n"),
        "{text}"
    );

    // The cut round itself, on a store whose schema fits once its names do:
    // every entry survives, each name ending in the ellipsis that says it was
    // cut, where the uncut listing would have had to drop entries instead.
    let dir = tmp("brief-memory-long-names-narrow");
    let mut narrow = open(&dir);
    for i in 0..14 {
        narrow
            .insert_node(&long("Label", i), &format!("node:{i:03}"), vec![])
            .expect("node");
    }
    let nb = brief(&narrow, &BriefOptions::default());
    let ntext = render(&nb, "query '<cypher>'");
    assert!(
        ntext.len() <= MAX_BRIEF_BYTES,
        "{} bytes\n{ntext}",
        ntext.len()
    );
    assert_eq!(
        ntext.matches("XXX…").count(),
        14,
        "every label line survives with its name cut:\n{ntext}"
    );
    assert!(
        !ntext.contains("  … and "),
        "cutting the names kept every entry:\n{ntext}"
    );
}

/// Binding: a store with only one edge type in it altogether has nothing to
/// intersect, so `linked by all of` is not one of the recipes — a recipe of
/// a single pattern would demonstrate the wrong thing (that shape already
/// exists: it is the `how many` recipe).
#[test]
fn a_store_with_a_single_edge_type_omits_the_linked_by_all_of_recipe() {
    use core_api::Value;

    let dir = tmp("brief-memory-single-edge-type");
    let mut db = open(&dir);
    for (key, name) in [("person:ada", "Ada"), ("person:bob", "Bob")] {
        db.insert_node(
            "Person",
            key,
            vec![("name".into(), Value::Str(name.to_string()))],
        )
        .expect("person");
    }
    db.insert_edge("KNOWS", "person:ada", "person:bob")
        .expect("edge");

    let b = brief(&db, &BriefOptions::default());
    let s = &b;
    assert_eq!(s.edge_types.len(), 1, "one edge type in the whole store");
    assert!(
        !s.recipes.iter().any(|r| r.question == "linked by all of"),
        "a single edge type has nothing to intersect: {:?}",
        s.recipes
    );

    let text = render(&b, "query '<cypher>'");
    assert!(!text.contains("linked by all of"), "{text}");
}

/// A memory store whose busiest label is one the first role cannot see —
/// the shape of the real association store, where `client` reads only
/// `Company` and `Job` while `Talent` is the most populous label.
///
/// Picking the role and the probe label independently rendered
/// `MATCH (n:Talent) … role: client`, which answers zero rows.
fn role_scoped_memory_store(name: &str) -> core_api::GraphDb<core_storage::fs::RealFs> {
    use core_api::schema::Schema;
    use core_api::{Predicate, RoleDef, RuleDef, Value};

    let dir = tmp(name);
    let mut db = open(&dir);
    db.apply_schema(&Schema {
        fulltext: vec![],
        indexes: vec![],
        rules: vec![RuleDef {
            name: "works_at".into(),
            src_label: "Talent".into(),
            dst_label: "Company".into(),
            predicate: Predicate::KeyMatch {
                field: "company_id".into(),
            },
            edge_type: "WORKS_AT".into(),
            weight_prop: None,
            max_edges: None,
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
            namespace: None,
        }],
        views: vec![],
        roles: vec![
            // Sorts first, and sees neither the busiest label nor the label
            // the busiest edge type starts at.
            RoleDef {
                name: "client".into(),
                keys: vec![],
                labels: vec!["Company".into(), "Job".into()],
                visible_where: None,
                namespaces: None,
                write: None,
            },
            RoleDef {
                name: "recruiter".into(),
                keys: vec![],
                labels: vec!["Company".into(), "Job".into(), "Talent".into()],
                visible_where: None,
                namespaces: None,
                write: None,
            },
        ],
    })
    .expect("schema");

    for key in ["company:acme", "company:globex"] {
        db.insert_node("Company", key, vec![]).expect("company");
    }
    for (key, company) in [
        ("job:backend", "company:acme"),
        ("job:frontend", "company:acme"),
        ("job:sre", "company:globex"),
    ] {
        db.insert_node("Job", key, vec![]).expect("job");
        db.insert_edge("POSTED", company, key).expect("posted");
    }
    // Three at acme, so the `how many` recipe's `n >= 3` filter has an answer.
    for (key, company) in [
        ("talent:ada", "company:acme"),
        ("talent:bob", "company:acme"),
        ("talent:cy", "company:acme"),
        ("talent:di", "company:globex"),
        ("talent:eve", "company:globex"),
    ] {
        db.insert_node(
            "Talent",
            key,
            vec![("company_id".into(), Value::Str(company.to_string()))],
        )
        .expect("talent");
    }
    db
}

/// Run one recipe's call if it is Cypher, and say how many rows it answered.
///
/// `None` for a recipe that is a tool call rather than a query. A role, where
/// the recipe names one, is resolved to its mask and the query runs under it —
/// which is what the session copying the line will experience.
fn run_recipe_cypher(
    db: &core_api::GraphDb<core_storage::fs::RealFs>,
    call: &str,
) -> Option<core_query::ResultSet> {
    let params = std::collections::BTreeMap::new();
    let (cypher, role) = if let Some(rest) = call.strip_prefix("query '") {
        let (cypher, tail) = rest.split_once('\'').expect("a closed quote");
        (cypher, tail.strip_prefix(" role: "))
    } else if call.starts_with("MATCH ") {
        // The `linked by all of` recipe appends a one-line note after its
        // query, separated by " — "; every other `MATCH` call has no such
        // suffix, so this is a no-op for them.
        (call.split(" — ").next().unwrap_or(call), None)
    } else {
        return None;
    };
    let rs = match role {
        Some(role) => {
            let mask = db
                .mask_for_role(role)
                .unwrap_or_else(|e| panic!("the recipe names a role the store has: {role}: {e}"));
            db.query_masked(cypher, &params, &mask)
        }
        None => db.query(cypher, &params),
    }
    .unwrap_or_else(|e| panic!("the brief's own worked call must run: {call:?}: {e}"));
    Some(rs)
}

/// Binding: every Cypher recipe the brief renders answers on the very store it
/// was computed from — at least one row, and no null cell in it.
///
/// Two ways a recipe can read like a call and answer nothing, both of which
/// shipped:
///
/// - `RETURN n.key`. A node's key is not a property, so the query runs, returns
///   the right number of rows, and every cell is null.
/// - a role and a label chosen independently. On the association store that
///   rendered `MATCH (n:Talent) … role: client`, and `client` sees only
///   `Company` and `Job` — zero rows.
///
/// So the test does not read the recipes: it runs them, under the role each
/// names, and looks at the cells.
#[test]
fn every_cypher_recipe_answers_on_the_store_it_came_from() {
    for db in [
        seeded_memory_store("brief-recipes-run"),
        role_scoped_memory_store("brief-recipes-roles"),
    ] {
        let b = brief(&db, &BriefOptions::default());
        let s = &b;
        let text = render(&b, "query '<cypher>'");
        let mut ran = 0;
        for r in &s.recipes {
            assert!(
                text.lines()
                    .any(|l| l == format!("  {}: {}", r.question, r.call)),
                "the brief must print {:?}:\n{text}",
                r.question
            );
            let Some(rs) = run_recipe_cypher(&db, &r.call) else {
                continue;
            };
            ran += 1;
            assert!(
                !rs.is_empty(),
                "the {:?} recipe answers no rows: {}",
                r.question,
                r.call
            );
            for i in 0..rs.len() {
                assert!(
                    rs.row(i).iter().all(Option::is_some),
                    "the {:?} recipe answers a null cell — a column that is not \
                     a property: {} → {:?}",
                    r.question,
                    r.call,
                    rs.row(i)
                );
            }
        }
        assert_eq!(ran, 3, "the brief renders three Cypher recipes");
    }
}

/// Binding: the memory-store brief runs under the same budget the two
/// rankings do, and an exhausted one costs counts and history rather than the
/// brief.
///
/// The hook that renders this has five seconds. Nothing in the memory path was
/// watching a clock: the node pass, the edge-type census and `wal_total_commits`
/// each ran to completion however long they took, and the last of those
/// re-reads the whole WAL. A brief that arrives late is a brief the session
/// never sees.
#[test]
fn a_memory_brief_out_of_budget_is_partial_rather_than_late() {
    let db = seeded_memory_store("brief-memory-budget");

    let full = brief(&db, &BriefOptions::default());
    let fs = &full;
    assert!(!fs.partial, "the default budget is ample for five nodes");
    assert!(fs.commits.is_some(), "and the history is counted");

    let b = brief(
        &db,
        &BriefOptions {
            budget: std::time::Duration::ZERO,
        },
    );
    let s = &b;
    assert!(s.partial, "a spent budget is reported, never hidden");
    assert_eq!(
        s.commits, None,
        "wal_total_commits re-reads the WAL — not on a spent budget"
    );
    assert!(
        !s.recipes.iter().any(|r| r.question == "as of"),
        "no commit index is known, so there is no `as of` call to show: {:?}",
        s.recipes
    );

    let text = render(&b, "query '<cypher>'");
    assert!(text.starts_with(UNTRUSTED_FRAMING), "{text}");
    assert!(
        text.lines().next_back() == Some("reach the graph: query '<cypher>'"),
        "the reach line survives a spent budget:\n{text}"
    );
    assert!(
        text.lines().any(|l| l == "history: unknown"),
        "an uncounted history says so rather than reading as zero:\n{text}"
    );
    assert!(
        text.lines().next().is_some() && text.contains("(partial)"),
        "the header marks a partial brief:\n{text}"
    );
    assert!(text.contains("ask in one call:"), "{text}");
}

/// Binding: a partial schema's counts render as lower bounds.
///
/// Built by hand rather than timed, so the assertion is about the rendering
/// and not about how fast the machine running the tests happens to be.
#[test]
fn a_partial_schema_renders_its_counts_as_lower_bounds() {
    use core_api::memory::brief::{EdgeTypeBrief, LabelBrief, Recipe, SchemaBrief};

    let schema = SchemaBrief {
        nodes: 7,
        edges: 9,
        labels: vec![LabelBrief {
            label: "Person".into(),
            nodes: 7,
            props: vec!["name".into()],
            hidden_props: 0,
        }],
        edge_types: vec![EdgeTypeBrief {
            edge_type: "KNOWS".into(),
            rule: None,
            hidden_rules: 0,
            src: vec!["Person".into()],
            dst: vec!["Person".into()],
            edges: 9,
        }],
        commits: None,
        roles: Vec::new(),
        recipes: vec![Recipe {
            question: "relationships".into(),
            call: "node_edges person:ada".into(),
        }],
        partial: true,
    };
    let text = render(&schema, "query '<cypher>'");

    assert!(
        text.lines()
            .next_back()
            .is_some_and(|l| l.contains("reach the graph")),
        "{text}"
    );
    for line in [
        "  Person (≥ 7) — name",
        "  KNOWS (≥ 9) — Person → Person",
        "history: unknown",
    ] {
        assert!(
            text.lines().any(|l| l == line),
            "a partial brief must print {line:?}:\n{text}"
        );
    }
    assert!(
        text.lines()
            .any(|l| l.starts_with("mushroomdb brief — ") && l.ends_with("(partial)")),
        "{text}"
    );
}

/// Binding: the vector payload is not a schema property a session should
/// query. It is hundreds of floats, it is never what a question is about, and
/// a brief that names it invites a `RETURN n.embedding` that returns a wall of
/// numbers.
#[test]
fn the_brief_hides_the_embedding_prop() {
    use core_api::Value;

    let dir = tmp("brief-embedding");
    let mut db = open(&dir);
    for key in ["doc:one", "doc:two"] {
        db.insert_node(
            "Doc",
            key,
            vec![
                ("title".into(), Value::Str(key.to_string())),
                (
                    "embedding".into(),
                    Value::List(vec![Value::Float(0.1), Value::Float(0.2)]),
                ),
            ],
        )
        .expect("doc");
    }

    let b = brief(&db, &BriefOptions::default());
    let s = &b;
    assert_eq!(
        s.labels[0].props,
        vec!["title".to_string()],
        "the vector payload is not listed"
    );
    assert_eq!(
        s.labels[0].hidden_props, 0,
        "and it is not counted off either — it is hidden, not deferred"
    );

    let text = render(&b, "query '<cypher>'");
    assert!(!text.contains("embedding"), "{text}");
}

/// Binding: the role recipe names a label the role it names can actually see.
#[test]
fn the_role_recipe_probes_a_label_that_role_can_see() {
    let db = role_scoped_memory_store("brief-role-visible");
    let b = brief(&db, &BriefOptions::default());
    let s = &b;

    assert_eq!(
        s.labels[0].label, "Talent",
        "the busiest label is the one no role sorted first can see"
    );
    let call = &s
        .recipes
        .iter()
        .find(|r| r.question == "who may see")
        .expect("a store with roles shows the role recipe")
        .call;

    let role = call
        .rsplit(" role: ")
        .next()
        .expect("the recipe names a role");
    let visible: &[String] = &s
        .roles
        .iter()
        .find(|(n, _)| n == role)
        .expect("a real role")
        .1;
    let label = call
        .split_once("MATCH (n:")
        .and_then(|(_, rest)| rest.split_once(')'))
        .expect("the recipe probes a label")
        .0;
    assert!(
        visible.iter().any(|l| l == label),
        "role {role} cannot see {label}: {call}"
    );
}
