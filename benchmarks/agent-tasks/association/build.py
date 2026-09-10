#!/usr/bin/env python3
"""One deterministic world, written three ways: files, SQLite, and a store.

The association suite compares an agent given a graph against the same agent
given the same facts as JSON files or as a SQLite database. So there is exactly
one generator — `dogfood/synthesize.py`, the shapes the scale runs already use —
and three writers that must not disagree about a single fact. `equivalent()`
loads all three back and says so.

What the three forms share: the base entities as of day 0, a 300-change history
over a 90-day window, and two roles. What only the graph form has: the ten rules
loaded as real linking rules (so the store holds derived edges and their
history) and the 1536-dimension `embedding` the semantic rule needs. That
vector is the only generated field the flat forms leave out — writing 1536
floats per entity into a JSON file the agent is meant to grep would be 50 MB of
noise — so the README names it as the one field only the store has, no rule a
task may ask about reads it, and `equivalent()` checks its length in the store
rather than comparing it across forms.

Each form gets its own self-contained directory under the build's `--out`:
`files/`, `sqlite/` and `graph/`. A form's subject directory is that directory,
copied whole.

"Time" in the store is a WAL commit index; there is no wall clock in it.
`days.json` is the bridge: for each day it records the commit index of that
day's last mutation, which is what `query_at` wants. The store is never
snapshotted — a snapshot would truncate the history the suite exists to ask
about.

CLI:

    python association/build.py --seed 20260910 --scale 2000 --out <dir>
"""

from __future__ import annotations

import argparse
import json
import random
import shutil
import sqlite3
import subprocess
import sys
import time
from datetime import date, timedelta
from pathlib import Path
from typing import Any, Iterable

HERE = Path(__file__).resolve().parent
BENCH = HERE.parent
REPO = BENCH.parent.parent
DOGFOOD = REPO / "dogfood"

if str(DOGFOOD) not in sys.path:
    sys.path.insert(0, str(DOGFOOD))

import synthesize                                   # noqa: E402  (dogfood)
from rules import SIX_RULES                         # noqa: E402  (dogfood)

# --------------------------------------------------------------------------
# the shape of a world
# --------------------------------------------------------------------------

N_DAYS = 90
LAST_DAY = N_DAYS - 1
N_CHANGES = 300
DAY_ZERO = date(2026, 6, 1)
STORE_NAME = "world.mushroomdb"
SQLITE_NAME = "world.sqlite"
# One directory per form under the build's `--out`, so all three are
# self-contained: a form's subject directory is its directory, copied whole.
FORM_DIRS: dict[str, str] = {"files": "files", "sqlite": "sqlite", "graph": "graph"}
FORMS: tuple[str, ...] = tuple(FORM_DIRS)
# `crates/core-storage/src/fs.rs`: what a snapshot leaves in a store directory.
# The association store is never snapshotted — the WAL is the history.
SNAPSHOT_FILES: tuple[str, ...] = ("snapshot.bin", "snapshot.bin.bak")

# The only fields a `set_prop` change touches. Every label carries all five.
MUTABLE_FIELDS: tuple[str, ...] = (
    "status", "industry", "specialties", "location", "size_bucket",
)
OPS: tuple[str, ...] = ("set_prop", "insert_node", "delete_node")
OP_WEIGHTS: tuple[int, ...] = (60, 25, 15)
# The generator only ever emits "published"; a history in which nothing is ever
# drafted or archived would make the status field dead weight.
STATUSES: tuple[str, ...] = ("published", "draft", "archived")
# What a new entity is: Talent / Company / Job in the generator's own 70/20/10.
INSERT_LABELS: tuple[str, ...] = ("Talent", "Company", "Job")
INSERT_WEIGHTS: tuple[int, ...] = (70, 20, 10)

ROLES: list[dict[str, Any]] = [
    {"name": "recruiter", "labels": ["Talent", "Job"]},
    {"name": "client", "labels": ["Company", "Job"]},
]

# The fields every form carries, per label — the README's glossary, what the
# SQLite schema has columns for, and what `equivalent()` compares. The one
# generated field left out is `embedding`: 1536 floats per entity that only the
# store keeps (see the module docstring). Only Talent and Company have one; a
# Job has no `email` either.
COMPARE_FIELDS: dict[str, tuple[str, ...]] = {
    "Talent": ("name", "email", "user_id", "status", "industry", "specialties",
               "design_styles", "size_bucket", "years_of_experience",
               "location", "address"),
    "Company": ("name", "email", "user_id", "status", "industry", "specialties",
                "design_styles", "size_bucket", "company_size", "founded_year",
                "location", "address"),
    "Job": ("name", "user_id", "status", "industry", "specialties",
            "design_styles", "size_bucket", "company_name", "company_id",
            "company_size", "location", "address"),
}

# List-valued fields default to `[]`, never to null, in every form: an entity
# with no design styles has an empty list, it does not lack the field.
LIST_FIELDS: tuple[str, ...] = ("specialties", "design_styles")

# The field only the store has, and what a correct one looks like there.
EMBEDDING = "embedding"
EMBEDDING_DIM = 1536
EMBEDDING_LABELS: tuple[str, ...] = ("Talent", "Company")

ENTITY_FILES: tuple[tuple[str, str], ...] = (
    ("Talent", "talent.json"), ("Company", "company.json"), ("Job", "job.json"),
)


def day_date(day: int) -> str:
    """Day `d` of the window as an ISO date. Day 0 is the base state."""
    return (DAY_ZERO + timedelta(days=day)).isoformat()


def _is_semantic(rule: dict[str, Any]) -> bool:
    """A rule whose predicate reaches for the embedding vector."""
    def walk(p: Any) -> bool:
        if isinstance(p, dict):
            return "VectorSimilar" in p or any(walk(v) for v in p.values())
        if isinstance(p, list):
            return any(walk(v) for v in p)
        return False
    return walk(rule["predicate"])


def store_rules() -> list[dict[str, Any]]:
    """Every rule the store loads — the semantic ones too, for realism."""
    return [dict(r) for r in SIX_RULES]


def task_rules() -> list[dict[str, Any]]:
    """The rules a task may ask about: everything but the semantic ones.

    A semantic rule needs the embedding, which only the graph form has, so a
    question about one could not be answered from files or SQLite.
    """
    return [dict(r) for r in SIX_RULES if not _is_semantic(r)]


# --------------------------------------------------------------------------
# the world
# --------------------------------------------------------------------------


def _scale_run():
    """`dogfood/scale_run.py`, imported late — it needs the binding at import."""
    import scale_run
    return scale_run


def world(seed: int, scale: int = 2000) -> dict[str, Any]:
    """The whole dataset, deterministic in `seed` alone.

    `nodes` are the day-0 entities in the generator's order (Talent, then
    Company, then Job); `changes` is the history in day order; `rules` are the
    rules tasks may ask about; `days` is the date of each day in the window.
    """
    if scale < 3:
        raise ValueError(
            f"scale must be at least 3 (one Talent, one Company, one Job); "
            f"got {scale}. split_scale({scale}) leaves a label empty and the "
            f"cross-label rules would have nothing to derive.")
    n_talent, n_companies, n_jobs = _scale_run().split_scale(scale)
    if min(n_talent, n_companies, n_jobs) < 1:
        raise ValueError(
            f"scale {scale} splits to {n_talent}/{n_companies}/{n_jobs} "
            f"talent/company/job; every label needs at least one entity.")
    nodes = list(synthesize.generate(n_talent, n_companies, n_jobs, seed))
    changes = changelog(nodes, seed, n_talent, n_companies, n_jobs)
    return {
        "nodes": nodes,
        "rules": task_rules(),
        "changes": changes,
        "roles": [dict(r) for r in ROLES],
        "days": [day_date(d) for d in range(N_DAYS)],
    }


def changelog(nodes: list[dict], seed: int, n_talent: int, n_companies: int,
              n_jobs: int) -> list[dict[str, Any]]:
    """`N_CHANGES` mutations over days 1..LAST_DAY, in day order.

    Built forward against a live copy of the world, which is what makes the two
    awkward guarantees free: an entity is never changed twice on the same day
    (a per-day set of touched keys), and a deleted entity is never referred to
    again (deleting drops it from the pool every later change draws from).
    """
    rng = random.Random(f"association-changelog-{seed}")
    live: dict[str, dict] = {
        n["key"]: {"key": n["key"], "label": n["label"], "props": dict(n["props"])}
        for n in nodes
    }
    next_index = {"Talent": n_talent, "Company": n_companies, "Job": n_jobs}

    ops = rng.choices(OPS, weights=OP_WEIGHTS, k=N_CHANGES)
    days = sorted(rng.randint(1, LAST_DAY) for _ in range(N_CHANGES))

    changes: list[dict[str, Any]] = []
    touched: set[str] = set()
    current_day = -1
    for op, day in zip(ops, days):
        if day != current_day:
            current_day, touched = day, set()
        change = _one_change(op, day, rng, live, touched, next_index, n_companies)
        touched.add(change["key"])
        changes.append(change)
    return changes


def _one_change(op: str, day: int, rng: random.Random, live: dict[str, dict],
                touched: set[str], next_index: dict[str, int],
                n_companies: int) -> dict[str, Any]:
    if op == "insert_node":
        node = _fresh_node(rng, next_index, n_companies)
        live[node["key"]] = {"key": node["key"], "label": node["label"],
                             "props": dict(node["props"])}
        return _change(day, "insert_node", node["key"], node=node)

    key = _pick(rng, live, touched)
    if key is None:                     # nothing left to touch today
        node = _fresh_node(rng, next_index, n_companies)
        live[node["key"]] = {"key": node["key"], "label": node["label"],
                             "props": dict(node["props"])}
        return _change(day, "insert_node", node["key"], node=node)

    if op == "delete_node":
        del live[key]
        return _change(day, "delete_node", key)

    field = rng.choice(MUTABLE_FIELDS)
    value = _new_value(rng, field, live[key]["props"].get(field))
    live[key]["props"][field] = value
    return _change(day, "set_prop", key, field=field, value=value)


def _change(day: int, op: str, key: str, field: str | None = None,
            value: Any = None, node: dict | None = None) -> dict[str, Any]:
    return {"day": day, "date": day_date(day), "op": op, "key": key,
            "field": field, "value": value, "node": node}


def _pick(rng: random.Random, live: dict[str, dict],
          touched: set[str]) -> str | None:
    candidates = [k for k in sorted(live) if k not in touched]
    return rng.choice(candidates) if candidates else None


def _new_value(rng: random.Random, field: str, current: Any) -> Any:
    """A value from the generator's own vocabulary, different from the current
    one where that is cheap to arrange — a change that changes nothing teaches
    the suite nothing."""
    for _ in range(5):
        value = _draw(rng, field)
        if value != current:
            return value
    return value


def _draw(rng: random.Random, field: str) -> Any:
    if field == "status":
        return rng.choice(STATUSES)
    if field == "industry":
        return rng.choice(synthesize.INDUSTRIES)
    if field == "size_bucket":
        return rng.randint(1, 5)
    if field == "specialties":
        primary = rng.choice(synthesize.SPECIALTIES)
        pool = [s for s in synthesize.SPECIALTIES if s != primary]
        return [primary, *rng.sample(pool, rng.randint(0, 3))]
    if field == "location":
        # A relocation is to another metro, jittered inside it exactly as the
        # generator places an entity there in the first place. `address` is
        # left as it was: the README says `location` is what a rule reads.
        _, lat, lon = rng.choice(synthesize.METROS)
        return synthesize._jitter(rng, lat, lon)
    raise ValueError(f"no vocabulary for field {field!r}")


def _fresh_node(rng: random.Random, next_index: dict[str, int],
                n_companies: int) -> dict[str, Any]:
    """A new entity from the generator, at an index no base entity used.

    The generator's per-label builders are what keep a late arrival
    indistinguishable from a day-0 one — same vocabularies, same metro
    clustering, and an `embedding` that is still a function of its own key.
    """
    label = rng.choices(INSERT_LABELS, weights=INSERT_WEIGHTS, k=1)[0]
    i = next_index[label]
    next_index[label] = i + 1
    industry = rng.choices(synthesize.INDUSTRIES,
                           weights=synthesize.INDUSTRY_WEIGHTS, k=1)[0]
    if label == "Talent":
        return synthesize._talent(rng, i, industry)
    if label == "Company":
        return synthesize._company(rng, i, industry)
    return synthesize._job(rng, i, industry, n_companies)


# --------------------------------------------------------------------------
# the README every form carries
# --------------------------------------------------------------------------

FORM_NOTES = {
    "files": (
        "## This copy\n\n"
        "`entities/talent.json`, `entities/company.json` and "
        "`entities/job.json` are the **day 0** state — they do not move. The "
        "history is `changes.jsonl`, one change per line in order; the state on "
        "any later day is day 0 with every change up to and including that day "
        "applied. `roles.json` holds the two roles.\n"
    ),
    "sqlite": (
        "## This copy\n\n"
        "`world.sqlite` holds the **day 0** state in `talent`, `company` and "
        "`job` — those rows do not move. The history is the `changes` table, "
        "ordered by `seq`; the state on any later day is day 0 with every change "
        "up to and including that day applied. List fields are JSON text "
        "(`specialties_json`, `design_styles_json`); `location` is the pair of "
        "columns `lat`, `lon`. `roles` holds the two roles. `SCHEMA.md` has the "
        "full schema.\n"
    ),
    "graph": (
        "## This copy\n\n"
        "The data is the store in `world.mushroomdb`, and there are no entity "
        "files. The store already holds the relationships the rules above "
        "derive, and their history: every change was applied to it as its own "
        "commit, in order.\n\n"
        "The store also carries a `SEMANTIC_MATCH` relationship that no rule "
        "above describes: it is derived from an `embedding` vector this copy "
        "has and the other forms do not, and no question asks about it — treat "
        "it as background noise.\n\n"
        "There is no wall clock in the store — a point in its history is a "
        "**commit index**. `days.json` is the map: each entry gives a `day`, its "
        "`date`, and the `commit` index of that day's last change. To ask about "
        "a date, look its `commit` up in `days.json` and ask about the past at "
        "that commit.\n\n"
        "`schema.json` holds the two roles, which are applied to the store.\n"
    ),
}


def render_readme(w: dict[str, Any], form: str) -> str:
    tmpl = (HERE / "README.tmpl.md").read_text()
    rules_md = "\n".join(
        f"- **{r['name']}** — a `{r['src_label']}` is "
        f"**{r['edge_type']}** with a `{r['dst_label']}` when "
        f"{_predicate_prose(r['predicate'])}."
        for r in w["rules"]
    )
    roles_md = "\n".join(
        f"| `{r['name']}` | every {' and '.join(r['labels'])} |" for r in w["roles"]
    )
    subs = {
        "{{N_NODES}}": f"{len(w['nodes']):,}",
        "{{N_CHANGES}}": str(len(w["changes"])),
        "{{N_DAYS}}": str(N_DAYS),
        "{{LAST_DAY}}": str(LAST_DAY),
        "{{DAY_FIRST}}": w["days"][0],
        "{{DAY_LAST}}": w["days"][-1],
        "{{SPECIALTIES}}": ", ".join(f"`{s}`" for s in synthesize.SPECIALTIES),
        "{{DESIGN_STYLES}}": ", ".join(f"`{s}`" for s in synthesize.DESIGN_STYLES),
        "{{RULES}}": rules_md,
        "{{ROLES}}": roles_md,
        "{{FORM_NOTE}}": FORM_NOTES[form],
    }
    out = tmpl
    for token, value in subs.items():
        out = out.replace(token, value)
    left = [t for t in subs if t in out]
    if left:
        raise AssertionError(f"README template tokens left unfilled: {left}")
    return out


def _predicate_prose(predicate: dict[str, Any]) -> str:
    (kind, args), = predicate.items()
    if kind == "FieldEqual":
        return f"`field_equal({args['field']})` holds"
    if kind == "Overlap":
        return (f"`overlap({args['field']}, {args['min']})` holds — their "
                f"`{args['field']}` sets share at least one value and the "
                f"Jaccard index `|A ∩ B| / |A ∪ B|` is at least {args['min']}")
    if kind == "GeoRadius":
        return (f"`geo_radius({args['field']}, {args['km']})` holds — they are "
                f"at most {args['km']} km apart")
    if kind == "NumericWithin":
        tol = args["tolerance"]
        if tol == 0:
            return (f"`numeric_within({args['field']}, 0)` holds — their "
                    f"`{args['field']}` values are equal")
        return (f"`numeric_within({args['field']}, {tol})` holds — their "
                f"`{args['field']}` values differ by at most {tol:g}")
    raise ValueError(f"no prose for predicate {kind}")


# --------------------------------------------------------------------------
# form P: files
# --------------------------------------------------------------------------


def _flat(node: dict[str, Any]) -> dict[str, Any]:
    """One entity as a flat record — exactly the fields the README glossary
    describes, so the file and SQLite forms carry the same columns and nothing
    the README does not explain. Only `embedding` stays behind: 1536 floats the
    store keeps and no rule a task may ask about reads."""
    props = node["props"]
    flat: dict[str, Any] = {"key": node["key"], "label": node["label"]}
    for field in COMPARE_FIELDS[node["label"]]:
        flat[field] = props.get(field, [] if field in LIST_FIELDS else None)
    return flat


def write_files(w: dict[str, Any], directory: str | Path) -> Path:
    """`entities/*.json` (day 0), `changes.jsonl`, `roles.json`, `README.md`.

    The directory is cleared first, like the other two writers: a rebuild at a
    different scale must not leave a stale entity file behind.
    """
    directory = Path(directory)
    if directory.exists():
        shutil.rmtree(directory)
    (directory / "entities").mkdir(parents=True)
    for label, filename in ENTITY_FILES:
        rows = [_flat(n) for n in w["nodes"] if n["label"] == label]
        (directory / "entities" / filename).write_text(
            json.dumps(rows, indent=2, sort_keys=True) + "\n")
    with (directory / "changes.jsonl").open("w") as fh:
        for change in w["changes"]:
            fh.write(json.dumps(_flat_change(change), sort_keys=True) + "\n")
    (directory / "roles.json").write_text(
        json.dumps(w["roles"], indent=2, sort_keys=True) + "\n")
    (directory / "README.md").write_text(render_readme(w, "files"))
    return directory


def _flat_change(change: dict[str, Any]) -> dict[str, Any]:
    out = dict(change)
    if out.get("node") is not None:
        out["node"] = _flat(out["node"])
    return out


def read_files(directory: str | Path) -> tuple[list[dict], list[dict]]:
    """The day-0 entities and the changelog, back out of the file form."""
    directory = Path(directory)
    nodes: list[dict] = []
    for _, filename in ENTITY_FILES:
        nodes.extend(json.loads((directory / "entities" / filename).read_text()))
    changes = [json.loads(line)
               for line in (directory / "changes.jsonl").read_text().splitlines()
               if line.strip()]
    return nodes, changes


# --------------------------------------------------------------------------
# form Q: sqlite
# --------------------------------------------------------------------------

SQLITE_SCHEMA = """
CREATE TABLE talent (
  key TEXT PRIMARY KEY, name TEXT, email TEXT, user_id TEXT,
  status TEXT, industry TEXT,
  specialties_json TEXT, design_styles_json TEXT, size_bucket INTEGER,
  years_of_experience INTEGER, lat REAL, lon REAL, address TEXT);
CREATE TABLE company (
  key TEXT PRIMARY KEY, name TEXT, email TEXT, user_id TEXT,
  status TEXT, industry TEXT,
  specialties_json TEXT, design_styles_json TEXT, size_bucket INTEGER,
  company_size TEXT, founded_year INTEGER, lat REAL, lon REAL, address TEXT);
CREATE TABLE job (
  key TEXT PRIMARY KEY, name TEXT, user_id TEXT, status TEXT, industry TEXT,
  specialties_json TEXT, design_styles_json TEXT, size_bucket INTEGER,
  company_name TEXT, company_id TEXT, company_size TEXT,
  lat REAL, lon REAL, address TEXT);
CREATE TABLE changes (
  seq INTEGER PRIMARY KEY, day INTEGER, date TEXT, op TEXT, key TEXT,
  field TEXT, value_json TEXT, node_json TEXT);
CREATE TABLE roles (name TEXT PRIMARY KEY, labels_json TEXT);
CREATE INDEX changes_key ON changes(key);
CREATE INDEX changes_day ON changes(day);
"""

_TABLE = {"Talent": "talent", "Company": "company", "Job": "job"}
_JSON_COLUMN = {"specialties_json": "specialties",
                "design_styles_json": "design_styles"}


def _columns(label: str) -> tuple[str, ...]:
    """The entity table's columns, derived from the fields every form carries.

    Derived rather than written out a second time: the DDL above is the one
    other place these names appear, and an INSERT naming a column it does not
    declare fails loudly rather than writing a form that is quietly short a
    fact.
    """
    columns = ["key"]
    for field in COMPARE_FIELDS[label]:
        if field == "location":
            columns.extend(("lat", "lon"))
        elif f"{field}_json" in _JSON_COLUMN:
            columns.append(f"{field}_json")
        else:
            columns.append(field)
    return tuple(columns)


_COLUMNS = {table: _columns(label) for label, table in _TABLE.items()}


def write_sqlite(w: dict[str, Any], path: str | Path) -> Path:
    """`world.sqlite`, plus the shared `README.md` and a `SCHEMA.md` beside it."""
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    if path.exists():
        path.unlink()
    con = sqlite3.connect(str(path))
    try:
        con.executescript(SQLITE_SCHEMA)
        for node in w["nodes"]:
            table = _TABLE[node["label"]]
            cols = _COLUMNS[table]
            con.execute(
                f"INSERT INTO {table} ({', '.join(cols)}) "
                f"VALUES ({', '.join('?' * len(cols))})",
                tuple(_column_value(node, c) for c in cols))
        for seq, change in enumerate(w["changes"]):
            con.execute(
                "INSERT INTO changes (seq, day, date, op, key, field, "
                "value_json, node_json) VALUES (?,?,?,?,?,?,?,?)",
                (seq, change["day"], change["date"], change["op"], change["key"],
                 change["field"],
                 None if change["value"] is None else json.dumps(change["value"]),
                 None if change["node"] is None
                 else json.dumps(_flat(change["node"]), sort_keys=True)))
        for role in w["roles"]:
            con.execute("INSERT INTO roles (name, labels_json) VALUES (?, ?)",
                        (role["name"], json.dumps(role["labels"])))
        con.commit()
    finally:
        con.close()
    path.parent.joinpath("README.md").write_text(render_readme(w, "sqlite"))
    path.parent.joinpath("SCHEMA.md").write_text(_schema_md(path.name))
    return path


def _column_value(node: dict[str, Any], column: str) -> Any:
    props = node["props"]
    if column == "key":
        return node["key"]
    if column == "lat":
        return props["location"][0]
    if column == "lon":
        return props["location"][1]
    if column in _JSON_COLUMN:
        return json.dumps(props.get(_JSON_COLUMN[column], []))
    return props.get(column)


def _schema_md(db_name: str) -> str:
    return (
        f"# `{db_name}` schema\n\n"
        "Day-0 entities in `talent`, `company` and `job`; the history in "
        "`changes`; the roles in `roles`. List fields are JSON text; a location "
        "is the `lat`/`lon` pair.\n\n"
        "```sql\n" + SQLITE_SCHEMA.strip() + "\n```\n\n"
        "`changes.seq` is the 0-based order the changes were made in; `day` and "
        "`date` say when. `value_json` is the new value of `field` for a "
        "`set_prop`; `node_json` is the whole new record for an `insert_node`; "
        "both are null for a `delete_node`.\n"
    )


def read_sqlite(path: str | Path) -> tuple[list[dict], list[dict]]:
    """The day-0 entities and the changelog, back out of the SQLite form."""
    con = sqlite3.connect(str(path))
    con.row_factory = sqlite3.Row
    try:
        nodes: list[dict] = []
        for label, table in _TABLE.items():
            for row in con.execute(f"SELECT * FROM {table}"):
                flat = {"key": row["key"], "label": label}
                for column in _COLUMNS[table]:
                    if column in ("key", "lat", "lon"):
                        continue
                    if column in _JSON_COLUMN:
                        flat[_JSON_COLUMN[column]] = json.loads(row[column])
                    else:
                        flat[column] = row[column]
                flat["location"] = [row["lat"], row["lon"]]
                nodes.append(flat)
        changes = []
        for row in con.execute("SELECT * FROM changes ORDER BY seq"):
            changes.append({
                "day": row["day"], "date": row["date"], "op": row["op"],
                "key": row["key"], "field": row["field"],
                "value": None if row["value_json"] is None
                else json.loads(row["value_json"]),
                "node": None if row["node_json"] is None
                else json.loads(row["node_json"]),
            })
    finally:
        con.close()
    return nodes, changes


# --------------------------------------------------------------------------
# form R: the store
# --------------------------------------------------------------------------


def write_store(w: dict[str, Any], directory: str | Path,
                binary: str | Path) -> dict[int, int]:
    """Build the graph form and return `{day: commit index}`.

    The store is never snapshotted: a snapshot truncates the WAL, and the WAL
    *is* the history every time-travel task asks about.
    """
    directory = Path(directory)
    if directory.exists():
        shutil.rmtree(directory)
    directory.mkdir(parents=True)
    store = directory / STORE_NAME

    from mushroomdb import GraphDb
    scale_run = _scale_run()

    by_day: dict[int, list[dict]] = {}
    for change in w["changes"]:
        by_day.setdefault(change["day"], []).append(change)

    days: dict[int, int] = {}
    db = GraphDb.open(str(store))
    try:
        scale_run.ingest_nodes(db, w["nodes"])
        scale_run.declare_rules(db, store_rules())
        days[0] = db.wal_total_commits() - 1
        for day in range(1, N_DAYS):
            for change in by_day.get(day, ()):
                _apply(db, change)
            # A day with no change repeats the previous day's commit: the
            # state on that date genuinely is the previous day's state.
            days[day] = db.wal_total_commits() - 1
    finally:
        db.close()

    schema = directory / "schema.json"
    schema.write_text(json.dumps({"roles": w["roles"]}, indent=2) + "\n")
    applied = subprocess.run(
        [str(binary), "schema", "apply", str(store), str(schema)],
        capture_output=True, text=True, timeout=300)
    if applied.returncode != 0:
        # `check=True` would raise a CalledProcessError that prints the exit
        # code and swallows the reason, which is always in stderr here.
        raise RuntimeError(
            f"`{binary} schema apply` failed ({applied.returncode}) on "
            f"{store}:\n{applied.stderr.strip() or applied.stdout.strip()}")

    (directory / "days.json").write_text(json.dumps(
        [{"day": d, "date": day_date(d), "commit": days[d]} for d in range(N_DAYS)],
        indent=2) + "\n")
    (directory / "README.md").write_text(render_readme(w, "graph"))
    return days


def _apply(db, change: dict[str, Any]) -> None:
    op = change["op"]
    if op == "set_prop":
        db.set_prop(change["key"], change["field"], change["value"])
    elif op == "delete_node":
        db.delete_node(change["key"])
    elif op == "insert_node":
        node = change["node"]
        db.insert_node(node["label"], node["key"], node["props"])
    else:
        raise ValueError(f"unknown op {op!r}")


def read_store(directory: str | Path) -> tuple[list[dict], list[dict]]:
    """The day-0 entities and `days.json`, back out of the graph form.

    `MATCH (n) RETURN n` returns one `{'n': key}` per row — keys, not records —
    so the fields are projected explicitly, one query per label. `embedding` is
    projected as `size(...)` rather than as itself: the length is all anyone
    checks, and 1536 floats per entity across the binding is not.
    """
    directory = Path(directory)
    days = json.loads((directory / "days.json").read_text())
    commit = days[0]["commit"]

    from mushroomdb import GraphDb
    db = GraphDb.open(str(directory / STORE_NAME), read_only=True)
    try:
        nodes: list[dict] = []
        for label, fields in COMPARE_FIELDS.items():
            projection = ", ".join(f"n.{f}" for f in fields)
            rows = db.query_at(
                commit,
                f"MATCH (n:{label}) RETURN n, {projection}, size(n.{EMBEDDING})")
            for row in rows:
                flat = {"key": row["n"], "label": label}
                for field in fields:
                    value = row[f"n.{field}"]
                    if value is None and field in LIST_FIELDS:
                        value = []
                    flat[field] = value
                flat[f"{EMBEDDING}_len"] = row[f"size(n.{EMBEDDING})"]
                nodes.append(flat)
    finally:
        db.close()
    return nodes, days


# --------------------------------------------------------------------------
# do the three forms agree?
# --------------------------------------------------------------------------


def _freeze(value: Any) -> Any:
    if isinstance(value, float):
        return round(value, 9)
    if isinstance(value, list):
        return tuple(_freeze(v) for v in value)
    return value


def _facts(flat_nodes: Iterable[dict]) -> dict[str, tuple]:
    out: dict[str, tuple] = {}
    for node in flat_nodes:
        label = node["label"]
        out[node["key"]] = (
            label,
            tuple((f, _freeze(node.get(f))) for f in COMPARE_FIELDS[label]),
        )
    return out


def _embedding_problems(store_nodes: Iterable[dict]) -> list[str]:
    """`embedding` is the one field only the store has, so it is checked there
    rather than compared: every Talent and Company must carry a full
    `EMBEDDING_DIM` vector, and a Job — which the generator gives none — must
    carry no vector at all."""
    problems: list[str] = []
    wrong: list[str] = []
    unexpected: list[str] = []
    for node in store_nodes:
        length = node.get(f"{EMBEDDING}_len")
        if node["label"] in EMBEDDING_LABELS:
            if length != EMBEDDING_DIM:
                wrong.append(f"{node['key']}={length}")
        elif length is not None:
            unexpected.append(f"{node['key']}={length}")
    if wrong:
        problems.append(f"store: {len(wrong)} embeddings are not "
                        f"{EMBEDDING_DIM} long, e.g. {wrong[:3]}")
    if unexpected:
        problems.append(f"store: {len(unexpected)} entities carry an embedding "
                        f"the generator gives none, e.g. {unexpected[:3]}")
    return problems


def differences(files_dir: str | Path, sqlite_path: str | Path,
                store_dir: str | Path) -> list[str]:
    """Every way the three forms disagree, as one line each. Empty is the goal."""
    file_nodes, file_changes = read_files(files_dir)
    sql_nodes, sql_changes = read_sqlite(sqlite_path)
    store_nodes, days = read_store(store_dir)

    problems: list[str] = []
    p, q, r = _facts(file_nodes), _facts(sql_nodes), _facts(store_nodes)
    for name, other in (("sqlite", q), ("store", r)):
        missing = sorted(set(p) - set(other))
        extra = sorted(set(other) - set(p))
        if missing:
            problems.append(f"{name}: {len(missing)} keys missing, e.g. {missing[:3]}")
        if extra:
            problems.append(f"{name}: {len(extra)} keys not in files, e.g. {extra[:3]}")
        for key in sorted(set(p) & set(other)):
            if p[key] != other[key]:
                problems.append(f"{name}: {key} differs: "
                                f"{_first_field_diff(p[key], other[key])}")
                if len([x for x in problems if x.startswith(name)]) > 5:
                    break

    problems.extend(_embedding_problems(store_nodes))

    if file_changes != sql_changes:
        first = next((i for i, (a, b) in enumerate(zip(file_changes, sql_changes))
                      if a != b), min(len(file_changes), len(sql_changes)))
        problems.append(f"changes: files and sqlite differ at seq {first}")

    if len(days) != N_DAYS:
        problems.append(f"days.json has {len(days)} entries, expected {N_DAYS}")
    for entry in days:
        if entry["date"] != day_date(entry["day"]):
            problems.append(f"days.json: day {entry['day']} dated {entry['date']}")
    for earlier, later in zip(days, days[1:]):
        # A day's commit is the last commit of that day, so time only moves
        # forward; a day nothing happened on repeats the day before's.
        if later["commit"] < earlier["commit"]:
            problems.append(f"days.json: day {later['day']} commit "
                            f"{later['commit']} precedes day {earlier['day']}'s "
                            f"{earlier['commit']}")
    dates = {e["date"] for e in days}
    unknown = sorted({c["date"] for c in file_changes} - dates)
    if unknown:
        problems.append(f"changes name dates days.json does not: {unknown[:3]}")
    return problems


def equivalent(files_dir: str | Path, sqlite_path: str | Path,
               store_dir: str | Path) -> bool:
    """Do the three forms carry the same base facts, history and days?"""
    problems = differences(files_dir, sqlite_path, store_dir)
    for line in problems:
        print(f"  not equivalent: {line}")
    return not problems


def _first_field_diff(a: tuple, b: tuple) -> str:
    for (field, av), (_, bv) in zip(a[1], b[1]):
        if av != bv:
            return f"{field} {av!r} != {bv!r}"
    return f"label {a[0]!r} != {b[0]!r}"


# --------------------------------------------------------------------------
# CLI
# --------------------------------------------------------------------------


def form_paths(out: str | Path) -> dict[str, Path]:
    """Where each form's subject directory lives under a build's `--out`.

    One entry per form, and the SQLite database inside its own directory: all
    three are self-contained, so provisioning an arm is copying one directory.
    """
    out = Path(out)
    paths = {form: out / name for form, name in FORM_DIRS.items()}
    paths["sqlite_db"] = paths["sqlite"] / SQLITE_NAME
    return paths


def build(seed: int, scale: int, out: Path, binary: Path) -> dict[str, Any]:
    """Write all three forms under `out`, one self-contained directory each."""
    out = Path(out)
    out.mkdir(parents=True, exist_ok=True)
    where = form_paths(out)
    timings: dict[str, float] = {}

    t = time.perf_counter()
    w = world(seed, scale)
    timings["world"] = time.perf_counter() - t

    t = time.perf_counter()
    write_files(w, where["files"])
    timings["files"] = time.perf_counter() - t

    t = time.perf_counter()
    write_sqlite(w, where["sqlite_db"])
    timings["sqlite"] = time.perf_counter() - t

    t = time.perf_counter()
    days = write_store(w, where["graph"], binary)
    timings["store"] = time.perf_counter() - t

    # The equivalence read time-travels the store back to day 0, which on a
    # store this deep is the slowest step of the build; say so before it.
    print("checking the three forms agree...", flush=True)
    t = time.perf_counter()
    ok = equivalent(where["files"], where["sqlite_db"], where["graph"])
    timings["equivalent"] = time.perf_counter() - t
    return {"world": w, "days": days, "timings": timings, "equivalent": ok}


def _dir_bytes(path: Path) -> int:
    return sum(p.stat().st_size for p in path.rglob("*") if p.is_file())


def _human(n: int) -> str:
    for unit in ("B", "KiB", "MiB", "GiB"):
        if n < 1024 or unit == "GiB":
            return f"{n:.1f} {unit}" if unit != "B" else f"{n} B"
        n /= 1024.0
    return f"{n} B"


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--seed", type=int, default=20260910)
    ap.add_argument("--scale", type=int, default=2000)
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--binary", type=Path, default=None)
    args = ap.parse_args(argv)

    binary = args.binary
    if binary is None:
        sys.path.insert(0, str(BENCH))
        from subjects import MUSHROOMDB, ensure_binary
        ensure_binary()
        binary = MUSHROOMDB

    result = build(args.seed, args.scale, args.out, binary)
    w, days = result["world"], result["days"]
    where = form_paths(args.out)
    store = where["graph"] / STORE_NAME
    print(f"\nworld: {len(w['nodes'])} nodes, {len(w['changes'])} changes, "
          f"{len(w['rules'])} task rules ({len(store_rules())} in the store), "
          f"{len(w['roles'])} roles")
    for phase, seconds in result["timings"].items():
        print(f"  {phase:<11} {seconds:8.2f}s")
    store_bytes = _dir_bytes(store)
    print(f"store size: {_human(store_bytes)} ({store_bytes} bytes)")
    for form in FORMS:
        print(f"{form + '/':<8} {_human(_dir_bytes(where[form]))}")
    print(f"day 0  -> commit {days[0]}   ({day_date(0)})")
    print(f"day {LAST_DAY} -> commit {days[LAST_DAY]}   ({day_date(LAST_DAY)})")
    print(f"equivalent: {result['equivalent']}")
    return 0 if result["equivalent"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
