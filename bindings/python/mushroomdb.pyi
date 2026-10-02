"""Type stubs for the mushroomdb Python bindings (PyO3 extension module).

Kept in step with `bindings/python/src/lib.rs`; `tests/test_parity.py` fails
if a public method is missing here.
"""

from __future__ import annotations

from os import PathLike
from types import TracebackType
from typing import Any, Literal, Sequence

Scalar = int | float | str | bool | list[Any] | dict[str, Any]
"""A value the store can hold: int, float, str, bool, list, or dict."""

Params = dict[str, Scalar] | Sequence[tuple[str, Scalar]] | None
"""Query parameters: a name→value dict, a list of (name, value) tuples, or None."""

Row = dict[str, Any]
"""One result row, keyed by RETURN alias."""

class MushroomError(RuntimeError):
    """Base class for every error the engine raises.

    It subclasses `RuntimeError`, so every `except RuntimeError` written
    against an earlier release keeps catching exactly what it caught.

    Each subclass carries `code`, a stable snake_case string equal to the
    engine's variant name, and the failing variant's own fields as attributes.
    `code` is the compatibility surface: classes may be added, a code is never
    respelled. `str(e)` is the message the engine has always produced, so
    existing logs and substring checks keep working.

    `code` is `None` on this base, which is never raised directly.
    """

    code: str | None

class KeyNotFound(MushroomError):
    """No node with this key."""

    code: str
    key: str

class DuplicateKey(MushroomError):
    """A node with this key already exists."""

    code: str
    key: str

class IoError(MushroomError):
    """The store's filesystem refused a read or a write.

    The engine variant carries an unnamed `std::io::Error`, whose own text is
    already the message, so this class adds no attribute of its own.
    """

    code: str

class Corrupt(MushroomError):
    """The store's on-disk state did not parse."""

    code: str
    detail: str

class RuleInvalid(MushroomError):
    """The rule definition was rejected."""

    code: str
    detail: str

class RuleOwned(MushroomError):
    """The edge belongs to a rule and is not writable by hand."""

    code: str
    detail: str

class RuleNotFound(MushroomError):
    """No rule by this name."""

    code: str
    name: str

class QueryError(MushroomError):
    """The Cypher statement failed.

    `detail` is also the message: a Python caller has never seen a prefix.
    """

    code: str
    detail: str

class IngestError(MushroomError):
    """The batch was rejected before anything landed."""

    code: str
    detail: str

class ReadOnly(MushroomError):
    """This handle never writes.

    Raised by a write on an as-of instance and by a write on a handle
    `scoped()` produced.
    """

    code: str

class CommitOutOfRange(MushroomError):
    """The commit is outside the retained range `floor..total`.

    `floor` is the oldest commit still reachable — `0` when nothing has been
    pruned — and `total` is the exclusive upper bound.
    """

    code: str
    commit: int
    total: int
    floor: int

class ViewPropReadOnly(MushroomError):
    """The property is managed by a view and cannot be written directly."""

    code: str
    view_name: str

class CasConflict(MushroomError):
    """A compare-and-set precondition was not satisfied."""

    code: str
    key: str
    expected: int
    actual: int

class MaskedReadOnly(MushroomError):
    """A write statement reached a scoped or masked query path, which is read-only."""

    code: str

class RoleWriteDenied(MushroomError):
    """A role-bound write was denied.

    `reason` is also the message.
    """

    code: str
    reason: str

class MushroomBusy(MushroomError):
    """Another process holds the store's write lock.

    Nothing was written, so retrying later is always safe. Raised only by write
    calls: opening read-only and reading never take the lock.

    `holder` is the holding process id when the platform makes it cheaply
    knowable and `None` otherwise — a diagnostic hint, never something to
    branch on.
    """

    code: str
    holder: int | None

class NamespaceImmutable(MushroomError):
    """A namespace is set at insert and fixed for the node's lifetime.

    `from_` carries a trailing underscore because `from` is a Python keyword.
    """

    code: str
    key: str
    from_: str
    to: str

class CrossNamespace(MushroomError):
    """A hand-written edge would cross a namespace boundary.

    Only a global rule — one with no `namespace` — may derive one.
    """

    code: str
    src: str
    src_ns: str
    dst: str
    dst_ns: str

class CommitTimeNotMonotonic(MushroomError):
    """An asserted commit time would move the store's recorded history backwards.

    Raised by `record_commits_at` only. The live clock is never held to this, so
    an NTP step backwards still commits — a caller asserting "this happened at T"
    is stating a fact and can be held to it, and a clock cannot.

    Backfill in chronological order: resolution walks commit order, so a later
    commit carrying an earlier instant would silently widen every answer after
    it. Equal instants are allowed — that is what a shared day means.
    """

    code: str
    supplied_ms: int
    newest_ms: int

class NoRecordedTime(MushroomError):
    """A date was given to a history call on a store that records no commit times.

    Written by a release before 0.6.11, or its `commit_times.bin` sidecar was
    removed. The store is intact and every index-addressed read still works;
    only date resolution is unavailable, so pass a frame index instead.

    Deliberately distinct from `Corrupt`: an absent map is not a damaged one,
    and collapsing the two would make "your times are broken" read as "this
    store never had any".
    """

    code: str

class TimeBeforeFloor(MushroomError):
    """The date predates the oldest commit time the store still records.

    `floor_commit` is the oldest frame the map describes and `floor_ms` is its
    recorded time. Retrying with an earlier instant fails the same way — the
    entries below it were dropped when the WAL horizon moved.
    """

    code: str
    floor_ms: int
    floor_commit: int

class GraphDb:
    """An embedded mushroomdb store.

    One writer at a time across processes. A handle sees commits made through
    it plus, after `refresh()`, everything other processes have committed.

    `scoped()` returns a read-only child of this handle that applies a scope to
    every read — see its docstring for the contract.
    """

    @staticmethod
    def open(path: str | PathLike[str], read_only: bool = False) -> GraphDb:
        """Open (creating if needed) the database rooted at `path`.

        A read-write handle holds the store's cross-process write lock for as
        long as it is open, so only one exists at a time across all processes.
        If another is already open this polls for two seconds and then raises
        `MushroomBusy` — **at `open`, not at the first write**. Nothing was
        written, so retrying later is always safe.

        Pass `read_only=True` for a handle that never writes and never takes
        the lock: it opens immediately even while another process is writing,
        every mutation raises `ReadOnly`, and `refresh()` still follows the
        writer's commits.

        The supported multi-process arrangements are in
        `docs/site/multiprocess.md`. Keep a store several processes open on a
        local disk: advisory locks over a network filesystem are not supported.
        """

    def scoped(
        self,
        role: str | None = None,
        namespace: str | None = None,
        keys: Sequence[str] | None = None,
    ) -> GraphDb:
        """A child handle that scopes **every** read and refuses every write.

        It shares this handle's store and mutex — not a second open, no second
        lock, one small allocation — so `close()` on either name closes both.

        At least one leg is required; none raises `ValueError`. Legs intersect,
        so `scoped()` on a scoped handle narrows further and never widens, and
        `keys=[]` narrows to nothing. An unknown `role` raises here, not on the
        first read.

        Every read obeys one contract: the subject is checked first, so a key
        outside the scope is indistinguishable from a key that does not exist —
        `node_info` answers `None`, `node_edges` raises `KeyNotFound`,
        `node_history` is empty. Then every other node the answer would mention
        — neighbour, endpoint, candidate, evidence — is filtered to the scope,
        so `degree` counts only visible neighbours and `find_similar`,
        `pairwise_similar` and `search_hybrid` score only visible candidates.

        The scope resolves per read, so a handle held across a write is never
        stale. `refresh()` is permitted. `has_vector_rule` and
        `is_index_enabled` answer unscoped: they are schema, not node data.

        A read that answers about the whole store takes no mask, so it is
        **refused** on a scoped handle with `ValueError` rather than narrowed:
        `pagerank`, `connected_components`, `degree_centrality`, `communities`,
        `search`, `fulltext_pairs`, `rules`, `suggest_rules`, `recall`,
        `schema_report`, `identity_clusters` and `roles`. `is_fulltext_enabled`
        answers: it is a schema fact about a pair you named.

        ```python
        s = db.scoped(role="reader-a")
        t = db.scoped(namespace="tenant-a", keys=visible_ids)
        ```
        """

    def insert_node(
        self,
        label: str,
        key: str,
        props: dict[str, Scalar],
        namespace: str | None = None,
    ) -> None:
        """Insert a new node; raises if `key` is already live.

        `namespace` is the namespace the node is created in — set at insert and
        immutable. Omitted means the `default` namespace.
        """

    def upsert_node(
        self, label: str, key: str, props: dict[str, Scalar]
    ) -> Literal["inserted", "updated"]:
        """Insert `key` if absent, otherwise update it in place.

        Returns `"inserted"` or `"updated"`. On update, only the fields present
        in `props` whose value differs from the stored one are written — fields
        you do not pass are left untouched, and unchanged fields produce no WAL
        record, so rules do not re-fire needlessly. Changed fields go in one
        `set_props` call, so a mid-list refusal leaves the node untouched.

        Raises `ValueError` if `key` already exists under a different label:
        relabelling a node is not an upsert, and ignoring the mismatch would
        hide a caller bug.
        """

    def insert_edge(self, edge_type: str, src: str, dst: str) -> bool:
        """Insert a user-owned edge; False if it already existed."""

    def insert_edge_upsert(
        self, edge_type: str, src: str, dst: str, placeholder_label: str
    ) -> dict[str, Any]:
        """Insert an edge, auto-creating any missing endpoint.

        Each missing endpoint is created as a plain node with label
        `placeholder_label` and no properties. Rules fire and last-change is
        updated for each auto-created node.

        Returns `{"nodes_created": N, "edge_inserted": bool}`.
        """

    def delete_edge(self, edge_type: str, src: str, dst: str) -> bool:
        """Delete a user-owned edge; raises for a rule-derived edge."""

    def delete_node(self, key: str) -> dict[str, int]:
        """Delete a live node and every edge incident on it.

        Returns `{"manual_edges": N, "derived_edges": M}`, counting the
        user-inserted and rule-derived edges removed. Raises `KeyNotFound` for
        an unknown or already-deleted key.
        """

    def set_prop(self, key: str, field: str, value: Scalar | None) -> None:
        """Set or overwrite a single property.

        `value=None` removes the field, exactly as `remove_prop` does: Python
        has no distinct "null property" and the store has no null value, so
        `None` means absent rather than stored-as-null.
        """

    def remove_prop(self, key: str, field: str) -> bool:
        """Remove a property.

        `True` if the field was present and removed, `False` if it was already
        absent. Raises `KeyNotFound` for an unknown or deleted key.

        Removing a field a rule watches retracts the edges that field derived.
        """

    def set_props_many(
        self, rows: Sequence[tuple[str, dict[str, Scalar | None]]]
    ) -> dict[str, int]:
        """Set properties on many existing nodes in one commit.

        `rows` is a sequence of `(key, props)` tuples. Only values that differ
        from what is stored are written; a `None` value removes the property.
        Returns `{"nodes", "props_set", "props_removed"}` — what was written
        after the comparison, so a call that changed nothing returns zeros and
        writes nothing at all.

        **Existing nodes only.** An unknown key raises `KeyNotFound` and
        nothing is written, even when that key's row would have changed
        nothing. Use `ingest_batch` to create nodes.

        **One commit, and rules fire in it.** Every change is one WAL frame.
        Rules re-fire on each changed property inside that commit; when one
        derives or retracts an edge the engine appends one more frame of
        history markers, so `wal_total_commits()` moves by 1, or by 2 — never
        by the number of nodes.

        The comparison and the write happen under one lock acquisition, so no
        other writer can come between them.

        A key that appears twice is a `ValueError`: merge its dicts first.
        `1` and `1.0` are different values. A view-owned property, or `ns` set
        to a different namespace, refuses the whole call. Keep a call under
        about 10,000 rows — it is one frame and one fsync.

        **This is a raw property write, like `set_prop`.** On an entity node,
        `name` and the two alias lists are maintained by `upsert_entity` and
        `remember`: a `name` written here is not reflected in `aliases` until
        the next describing write.

        ```python
        db.set_props_many([("alice", {"score": 3}), ("bob", {"score": 5, "old": None})])
        # {"nodes": 2, "props_set": 2, "props_removed": 1}
        ```
        """

    def query(
        self,
        cypher: str,
        params: Params = None,
        role: str | None = None,
        namespace: str | None = None,
    ) -> list[Row]:
        """Execute a read query and return one dict per row, keyed by RETURN alias.

        Values must be `int`, `float`, `str`, `bool`, `list` or `dict`.
        Parameters are bound, never interpolated, so string values are safe
        against injection.

        `role` answers as one of the store's roles (from `roles.json`) and
        `namespace` from one namespace only. They **intersect** — a namespace
        can only narrow what a role already allows, so a role bound to
        `tenant-a` asked for `tenant-b` answers with nothing — and either one
        makes the call a read, so a write statement raises `MaskedReadOnly`.

        Prefer `scoped()` when the scope belongs to the caller rather than to
        this one call; these arguments are its per-call form.
        """

    def query_with_params(
        self, cypher: str, params: Sequence[tuple[str, Scalar]]
    ) -> list[Row]:
        """Back-compat alias for `query(cypher, params=[...])` with a tuple list.

        Each element of `params` is a `(name, value)` tuple. Values must be
        `int`, `float`, `str`, `bool`, or a `list` of those. Prefer `query`,
        which accepts the same tuple list and a dict besides.
        """

    def query_write(self, cypher: str, params: Params = None) -> list[Row]:
        """Execute a Cypher write statement.

        CREATE / MATCH…SET / MATCH…DELETE / MATCH…DETACH DELETE / MERGE.

        Returns a one-row result dict with keys `created`, `properties_set` and
        `deleted`, unless the statement carries its own `RETURN` projection, in
        which case the projected rows are returned instead.

        `params` takes the same shapes as `query`: `None`, a dict, or a list of
        `(name, value)` tuples.
        """

    def query_at(
        self,
        commit: int,
        cypher: str,
        params: Params = None,
        role: str | None = None,
        namespace: str | None = None,
    ) -> list[Row]:
        """Time-travel read: run `cypher` against the graph as of `commit`.

        `role` and `namespace` intersect the same way as live `query`.
        """

    def rename_node(self, old: str, new: str) -> None:
        """Rename a node's key.

        The dense id — edges, history, last-change — is unchanged, so nothing
        the old key was party to is lost.

        Raises `KeyNotFound` if `old` is unknown, or `DuplicateKey` if `new` is
        already live.
        """

    def create_rule(self, rule: dict[str, Any], if_not_exists: bool = False) -> bool:
        """Register a linking rule; False when `if_not_exists` skips a duplicate.

        A `"namespace"` key scopes the rule to one namespace; omitted is global.
        """

    def explain(self, a: str, b: str) -> list[Row]:
        """Why are `a` and `b` linked? One dict per derived edge between them.

        Each is `{rule, edge_type, src_key, dst_key, weight, predicate}`.
        `predicate` is the snake_case summary shape, which `create_rule`
        accepts verbatim — an explanation round-trips into a new rule.

        On a `scoped()` handle: either endpoint hidden raises `KeyNotFound`,
        and an explanation whose evidence path crosses a hidden node is omitted
        entirely rather than redacted.
        """

    def neighbors(self, key: str, edge_type: str, direction: str) -> list[str]:
        """One-hop neighbour keys along `edge_type`.

        `direction` is `"out"` or `"in"`. This API is one directed hop, so
        `"both"` raises `ValueError` — unlike `degree`, which accepts it.
        """

    def node_info(self, key: str) -> dict[str, Any] | None:
        """`{key, label, props}` for a live node, or `None` for an unknown key.

        Contrast `node_edges`, which raises `KeyNotFound` for the same miss.
        The asymmetry mirrors the Rust API — `Option` versus `Result` — and is
        deliberate, not a Python invention.
        """

    def node_edges(self, key: str) -> list[Row]:
        """Edges incident on `key`. Raises `KeyNotFound` for an unknown key.

        Each dict is `{edge_type, src_key, dst_key, derived}`.

        `node_info` answers `None` on the same miss, because Rust returns
        `Option` there and `Result` here. The asymmetry is the core API.
        """

    def node_history(self, key: str) -> Row:
        """Per-node change history: `{key, history, total_commits, horizon}`.

        `horizon` is the oldest commit still retained; events before it were
        pruned and are not in `history`.
        """

    def wal_total_commits(self) -> int:
        """Total number of committed WAL frames visible in the current horizon."""

    def edge_history(self, a: str, b: str) -> Row:
        """Per-edge change history between `a` and `b`.

        Returns `{a, b, events, total_commits, horizon}`, where each event is
        `{edge_type, commit, event, rule}`. `event` is `"Added"` or
        `"Retracted"`; `rule` is the rule name for a derived edge and `None`
        for a manually written one; `horizon` is the oldest commit still
        retained — events before it were pruned and are not in `events`.
        """

    def was_linked(
        self, a: str, b: str, edge_type: str, at_commit: int | str
    ) -> bool:
        """Whether `a` and `b` were linked by `edge_type` at or before `at_commit`.

        `at_commit` takes a 0-based frame index, or an RFC 3339 date string —
        `"2026-06-19"`, `"2026-06-19T12:00:00Z"`, offsets accepted — which
        resolves to the last commit at or before that instant. A `bool` is
        rejected rather than read as `int`. Raises `NoRecordedTime` when the
        store records no times and `TimeBeforeFloor` when the date predates the
        oldest it has.
        """

    def edges_at(self, key: str, commit: int | str) -> list[Row]:
        """Every edge incident on `key` at WAL `commit`, from one WAL scan.

        Returns `{edge_type, src, dst, derived, rule}` dicts sorted by
        `(edge_type, src, dst)`. Raises for a commit outside the horizon.

        `commit` takes a 0-based frame index, or an RFC 3339 date string —
        `"2026-06-19"`, `"2026-06-19T12:00:00Z"` — which resolves to the last
        commit at or before that instant. Prefer the date when the question
        names one: guessing an index for a date is how a plausible wrong graph
        gets returned.
        """

    def record_commits_at(self, unix_ms: int | None) -> None:
        """Record subsequent commits as having happened at `unix_ms`.

        `None` goes back to the system clock. For **backfilled history**: a
        mirror importing rows that already carry their own timestamps, or a
        replay of events from months ago. Without this every imported commit is
        stamped "now", so a store holding a year of history answers every date
        question with `TimeBeforeFloor` — the data is there and no date reaches
        it.

        Sticky until changed or cleared, because a day of backfilled rows
        genuinely shares one instant. Import in chronological order: an instant
        earlier than anything already recorded raises
        `CommitTimeNotMonotonic`.

        Not available over HTTP or MCP. Asserting when a commit happened
        rewrites the store's apparent history, which is not something a role
        token models.
        """

    def resolve_date(self, date: str) -> int:
        """The 0-based frame index a date resolves to.

        The last commit at or before `date`, ready to hand to `edges_at` or
        `was_linked`. Raises `ValueError`-shaped `QueryError` when `date` will
        not parse, `NoRecordedTime` on a store that records none, and
        `TimeBeforeFloor` when it predates the oldest recorded.
        """

    def commit_time_ms(self, commit: int) -> int | None:
        """The recorded wall-clock time of a 0-based frame index, in unix ms.

        `None` when the store records no time for it — a pre-0.6.11 store, a
        frame below the horizon, or a damaged sidecar.
        """

    def what_if_set_prop(self, key: str, field: str, value: Any) -> dict[str, list[Row]]:
        """Derived edges a `set_prop(key, field, value)` would change.

        Returns `{"lost": [...], "gained": [...]}`, each entry shaped like an
        `edges_at` row. Writes nothing.
        """

    def enable_index(self, label: str, field: str) -> None:
        """Enable an equality index on `(label, field)`."""

    def disable_index(self, label: str, field: str) -> None:
        """Disable the equality index on `(label, field)`."""

    def enable_multiplicity(self) -> None:
        """Start recording a per-pair insert count on this store.

        Opt-in, and **one way**: it writes a declaration record the previous
        release's decoder cannot read. Left alone, a 0.6.9 binary would not
        refuse such a store — it would treat the first such record as a corrupt
        WAL tail, replay only the commits before it, and persist that
        truncation.

        So the call also takes a snapshot, at a format version 0.6.9 does not
        know, and it takes it **before** writing the record. A 0.6.9 binary
        stops at `snapshot: unsupported version 10` and leaves the WAL exactly
        as it found it. The cost is one full snapshot write; the snapshot keeps
        the WAL, so every commit `open_at` could reach before the call it can
        still reach after it.

        One thing the snapshot does move: a store that archives its WAL decides
        at its **first** archive whether `open_at` may reach into archives, and
        it says no whenever a snapshot it cannot vouch for already exists. A
        snapshot this handle took itself, keeping the WAL, is one it can vouch
        for — so opting in and taking that first archive in the *same* session
        keeps that reach. Across sessions it does not, which is the answer any
        store with a prior snapshot gets.

        A store that never calls this contains no such record, keeps writing
        the old snapshot version, and stays readable by 0.6.9 indefinitely.
        Refused on a scoped handle.

        **Not atomic.** If this raises, the store may be opted in anyway: the
        record can already be in the log with only its fsync having failed, or
        the snapshot alone can be enough for the next open to finish the job.
        The snapshot is written first in every case, so an older binary refuses
        such a store by name rather than truncating it — nothing is lost. But
        the exception means "outcome unknown", not "nothing happened": reopen
        and call `is_multiplicity_enabled()` to find out where the store stands.
        There is no call that opts it back out.
        """

    def is_multiplicity_enabled(self) -> bool:
        """Whether this store records insert-count multiplicity.

        A store-wide flag naming no node, so it answers on a scoped handle.
        """

    def is_index_enabled(self, label: str, field: str) -> bool:
        """Whether `(label, field)` currently has an equality index."""

    def has_vector_rule(self, field: str) -> bool:
        """Whether an approximate (HNSW) VectorSimilar rule covers `field`.

        A capability probe to call before `find_similar`: `True` means the
        native ANN index is active and `find_similar` will use it unless you
        pass `exact=True` or a `where=` predicate; `False` means no such rule
        covers `field`, so `find_similar` is an O(n) brute-force scan — which
        is exact by construction.

        Unscoped on a `scoped()` handle: this is a schema fact, not node data.
        """

    def find_similar(
        self,
        field: str,
        vector: Sequence[float],
        label: str | None = None,
        k: int = 10,
        min: float = 0.8,
        mask: Sequence[str] | None = None,
        where: dict | None = None,
        exact: bool = False,
    ) -> list[tuple[str, float]]:
        """The `k` nearest nodes to `vector` by cosine similarity on `field`.

        Returns `(node_key, similarity)` tuples sorted score-descending, kept
        when `score >= min`. Scores are cosine similarity in `[-1, 1]`; a
        distance of `1 - sim` is the caller's conversion — the engine does not
        speak distance.

        **Which arguments make the answer exact.** `exact=True` and a `where=`
        predicate each force a GEMM brute-force over the candidate set. A
        `mask=` allow-list — and a `scoped()` handle — do **not**: they narrow
        which nodes may be returned without changing which kernel runs, so the
        answer stays approximate when an HNSW rule covers `field`. Under a mask
        the beam widens until it has `k` visible hits, so the result is never
        short while more visible hits exist, but it is still not guaranteed to
        be the true top `k`. If you need an exhaustive answer over the visible
        set, pass `exact=True` alongside the mask.

        When no approximate VectorSimilar rule covers `field` (check with
        `has_vector_rule`), every call is already an exact brute-force scan.

        `label=None` searches every label. `where` is `{"field": …, "eq": …}`
        or `{"field": …, "in": [...]}` — exactly one of `eq`/`in`; anything
        else raises `ValueError` before reaching the engine. It uses the
        property index only when `label` is also set and
        `enable_index(label, where["field"])` is on; otherwise it is a
        correct-but-slower scan.

        Candidates are `label ∩ mask ∩ where`, intersected further by the
        handle's scope. A candidate whose embedding is missing, zero-norm or a
        different length than the query is skipped; a zero-norm query returns
        `[]`.

        **`min` defaults to `0.8`**, the same floor the MCP `find_similar` tool
        and HTTP `POST /find_similar` apply. It was `0.0` here through 0.6, so
        a call that never named `min` now drops every hit below `0.8` and
        nothing raises. Pass `min=0.0` for the old behaviour.
        """

    def pairwise_similar(
        self,
        keys: Sequence[str],
        field: str,
        k: int = 10,
        min: float = 0.0,
    ) -> list[tuple[str, list[tuple[str, float]]]]:
        """Exact per-key cosine top-k among `keys`. Self excluded. Never HNSW.

        Each key in `keys` is scored only against that same set, so this is a
        closed comparison, not a search of the store. Writes no edges.

        Scores are cosine similarity in `[-1, 1]`, kept when `score >= min` —
        the same unit and inequality as `find_similar`. A distance of
        `1 - sim` is the caller's conversion here too; convert after the call
        rather than baking a distance threshold into `min`.

        Treat the result as a map keyed by source: outer order is first-seen
        packed keys, not a zip with the input. A source with no neighbour above
        `min` still appears as `(src, [])`, so "present, nothing similar" stays
        distinct from "omitted".

        Unknown keys, missing embeddings, zero-norm and wrong-dimension vectors
        are omitted as both query and candidate. Duplicate keys collapse to
        first-seen order. Empty `keys` returns `[]`. More than 8192 unique
        resolved keys raises.

        On a `scoped()` handle hidden keys are dropped from the input set
        *before* the matmul, so a hidden vector can neither influence a score
        nor appear as a neighbour.
        """

    def degree(
        self,
        key: str,
        edge_type: str | None = None,
        direction: str = "both",
        multiplicity: bool = False,
    ) -> int:
        """Unique directed degree of `key`, or its insert count.

        `direction` is `"out"`, `"in"` or `"both"` — the sum of unique
        out-neighbours and unique in-neighbours, so a reciprocal pair counts 2
        at each endpoint, not the size of the undirected neighbour set.

        Adjacency is a set, so the default is a unique-neighbour count, not a
        row count of duplicate pairs. An unknown `edge_type` yields 0; an
        unknown `key` raises `KeyNotFound`.

        `multiplicity=True` sums the persisted per-pair insert count instead,
        so a pair inserted three times contributes 3. It answers the unique
        count on a store that never called `enable_multiplicity` — the argument
        is a readout preference, not a demand the store cannot meet. On a scoped
        handle it sums visible pairs only, for the same reason the unique count
        does: an unfiltered total discloses hidden neighbours by arithmetic.
        """

    def degrees(
        self,
        keys: Sequence[str] | None = None,
        label: str | None = None,
        where: dict | None = None,
        edge_type: str | None = None,
        direction: str = "both",
        limit: int | None = None,
        multiplicity: bool = False,
    ) -> list[tuple[str, int]]:
        """Unique directed degree for a key subset or a label scan.

        The universe is `keys` if given, else `label`, else every live node.
        `where` is the same dict shape as `find_similar` and intersects.
        `limit` applies after sorting degree descending, key ascending.

        Unknown keys are omitted rather than raising, and `keys=[]` returns
        `[]` — the contrast with `degree`, which raises for an unknown key.
        """

    def search_hybrid(
        self,
        text_field: str,
        query_text: str,
        vector_field: str,
        vector: Sequence[float],
        label: str | None = None,
        k: int = 10,
    ) -> list[tuple[str, float]]:
        """Reciprocal-rank fusion over fulltext and vector similarity.

        Fuses up to `4*k` fulltext hits on `text_field` for `query_text` with
        up to `4*k` vector hits on `vector_field` for `vector`, using
        Reciprocal Rank Fusion (constant 60). An empty `vector` skips the
        vector leg and answers from the text leg alone.

        Returns `[(node_key, fused_score)]` sorted score-descending, ties by
        key. The fused score is a rank-fusion number, not a similarity: it is
        not comparable with a `find_similar` score.

        **This call takes no exactness argument**, so its vector leg is the
        approximate one whenever an HNSW rule covers `vector_field`. To fuse an
        exact vector leg, run `find_similar(..., exact=True)` yourself and fuse
        it with a text search at the call site.

        On a `scoped()` handle both legs are filtered *before* fusion, so the
        ranks are the ranks of the visible corpus and `k` is honoured.
        """

    def get_edge_prop(
        self, edge_type: str, src_key: str, dst_key: str, field: str
    ) -> Any | None:
        """Read a single property from an edge — a rule's `score` weight, say.

        `None` when the edge does not exist, when the field is absent, or when
        any key cannot be resolved. The three are not distinguished, which is
        also what makes this safe on a `scoped()` handle: a hidden endpoint
        answers as an unresolvable key does.
        """

    def ingest_batch(
        self,
        nodes: Sequence[dict[str, Any]],
        edges: Sequence[dict[str, str]] | None = None,
        on_conflict: Literal["error", "skip", "replace"] = "error",
    ) -> dict[str, Any]:
        """Atomically ingest nodes and edges in a single WAL commit.

        `on_conflict` says what a node key that is already taken means:
        `"error"` (the default) rejects the whole frame with `DuplicateKey`;
        `"skip"` leaves the stored node untouched and counts it in `skipped`;
        `"replace"` makes its properties exactly the supplied props — fields
        absent from them are removed — and counts it in `replaced`. A label
        that differs from the stored one, and an `ns` that would move the node,
        are row errors under `"replace"`, not silent rewrites.

        Two properties sit outside "exactly", because neither is the caller's
        to supply: `ns`, which is immutable, and any property a view owns,
        which is kept rather than removed (supplying one is a row error, so
        omitting it is not a request to delete it). Each field kept that way is
        counted in `kept_view_owned`, while the row still counts in `replaced`
        and raises no row error.

        Edges take the argument too, but it changes nothing for them:
        adjacency is a set, so a duplicate edge is already a silent no-op under
        every policy, and these edge dicts carry no properties to replace.

        The report is `{inserted, edges_inserted, skipped, replaced,
        kept_view_owned, row_errors, rules_created, skipped_fk_fields}`, where
        `row_errors` is a list of `(index into nodes, why)`. `edges_inserted`
        counts only newly written edges; a duplicate edge is a silent no-op and
        is not counted.

        For large datasets keep each call to 10,000 nodes or fewer. One call
        with 100,000+ nodes serialises a single giant WAL frame whose fsync
        dominates and negates the batching. Chunk at the call site.
        """

    def batch_edges(
        self,
        inserts: Sequence[dict[str, str]] | None = None,
        deletes: Sequence[dict[str, str]] | None = None,
    ) -> dict[str, int]:
        """Atomically apply edge inserts and deletes in a single WAL commit.

        `inserts` and `deletes` are each a list of `{edge_type, src, dst}`
        dicts naming user-owned edges. All of them commit in one fsync, which
        is what this API is for: the maintenance pattern where one property
        update causes many retractions and additions would otherwise serialise
        one WAL fsync per `insert_edge` / `delete_edge` call.

        Returns `{"edges_inserted": N, "edges_deleted": M}`.
        """

    def pagerank(
        self,
        damping: float = 0.85,
        max_iters: int = 50,
        tol: float = 1e-06,
        edge_type: str | None = None,
        direction: Literal["out", "in", "both"] = "out",
        budget_ms: int = 0,
        weight_prop: str | None = None,
        min_weight: float | None = None,
    ) -> dict[str, Any]:
        """PageRank over the whole store.

        Returns `{"scores": [(key, score), …], "converged": bool}`, one row
        per live node, highest first, ties by key.

        `direction` is `"out"` (rank flows along each edge, the default),
        `"in"` or `"both"`. `edge_type=None` follows every edge type.
        `weight_prop` names an edge property to weight by, and `min_weight`
        drops edges below it.

        **`budget_ms` defaults to `0` — no time limit.** The engine's own
        default is a 5-second wall-clock budget, under which a loaded machine
        returns a different answer for the same store; `0` means the same
        store always gives the same scores. Pass a budget to bound the call,
        and read `converged`.

        Refused on a `scoped()` handle with `ValueError`: no algorithm takes a
        mask, and a visible node's score would be computed over hidden edges.

            top = db.pagerank(edge_type="CITES")["scores"][:10]
        """

    def connected_components(
        self,
        edge_type: str | None = None,
        budget_ms: int = 0,
        weight_prop: str | None = None,
        min_weight: float | None = None,
    ) -> dict[str, Any]:
        """Weakly connected components.

        Returns `{"components": [(key, component), …], "truncated": bool}`,
        one row per live node. `component` is an identifier shared by every
        node in the same component; compare them, do not parse them.

        `edge_type=None` follows every edge type; direction is ignored.
        `budget_ms` defaults to `0` (no limit) — see `pagerank`.

        Refused on a `scoped()` handle with `ValueError`.
        """

    def degree_centrality(
        self,
        edge_type: str | None = None,
        direction: Literal["out", "in", "both"] = "both",
        budget_ms: int = 0,
        weight_prop: str | None = None,
        min_weight: float | None = None,
    ) -> dict[str, Any]:
        """Degree centrality: every node's degree, highest first.

        Returns `{"scores": [(key, degree), …], "truncated": bool}`.

        **Not `degree()` or `degrees()`.** Those answer for the keys you name
        and can be scoped, masked and filtered; this ranks the whole store.
        `direction` is `"both"` (the default), `"out"` or `"in"`.
        `budget_ms` defaults to `0` (no limit) — see `pagerank`.

        Refused on a `scoped()` handle with `ValueError`; `degrees()` is the
        scoped way to ask.
        """

    def communities(
        self,
        edge_types: Sequence[str] | None = None,
        weight_prop: str | None = None,
        min_weight: float | None = None,
        resolution: float = 1.0,
        max_passes: int = 10,
        max_sweeps: int = 20,
        budget_ms: int = 0,
        node_label: str | None = None,
    ) -> dict[str, Any]:
        """Communities by modularity (Louvain).

        Returns `{"communities": [{"id", "members", "internal_weight",
        "cohesion"}, …], "modularity": float, "truncated": bool}`.

        `edge_types` is a **list** — unlike the other three algorithms, which
        take one `edge_type` — and `None` or `[]` follows every edge type.
        `node_label` restricts the pass to one label. `resolution` above 1.0
        favours smaller communities. `budget_ms` defaults to `0` (no limit) —
        see `pagerank`.

        Modularity is global: adding unrelated nodes can move an existing
        community. For "which keys are one entity", use `identity_clusters`.

        Refused on a `scoped()` handle with `ValueError`.
        """

    def enable_fulltext(self, label: str, field: str, if_not_exists: bool = False) -> bool:
        """Declare a full-text index on `(label, field)`. Returns `True` when it
        was newly enabled.

        The engine's call is not idempotent: enabling a pair that is already
        enabled raises `RuleInvalid`. With `if_not_exists=True` it returns
        `False` instead, so a caller that declares its schema at boot can call
        this every time.

        The declaration is logged; the index itself is rebuilt on every open,
        so each declared pair adds to how long the store takes to open.

            db.enable_fulltext("Doc", "body", if_not_exists=True)
        """

    def disable_fulltext(self, label: str, field: str) -> None:
        """Drop the full-text index on `(label, field)` and its postings.

        Raises `RuleNotFound` when the pair is not enabled; its `.name` is
        `"fulltext(label,field)"`.
        """

    def is_fulltext_enabled(self, label: str, field: str) -> bool:
        """Whether `(label, field)` has a full-text index.

        A schema fact about a pair you named, so it answers on a `scoped()`
        handle too, as `is_index_enabled` does.
        """

    def fulltext_pairs(self) -> list[tuple[str, str]]:
        """Every `(label, field)` pair with a full-text index, sorted.

        Refused on a `scoped()` handle with `ValueError`: it enumerates labels
        the scope may hide. `is_fulltext_enabled` answers about one pair.
        """

    def search(self, field: str, query: str, k: int = 0) -> list[tuple[str, float]]:
        """Full-text search on `field`. Returns `[(key, score), …]`, BM25,
        highest first, ties by key.

        The query grammar: space-separated terms are ANDed; `OR` between
        terms; `"a phrase"`; `-term` excludes; `prefix*` matches a prefix.

        **Keyed by field alone.** If two labels are indexed on the same field
        name, both are searched; filter the keys yourself.

        `k=0` (the default) returns every hit; a positive `k` stops at the
        best `k`.

        Refused on a `scoped()` handle with `ValueError`: the index takes no
        mask. `search_hybrid` is the scoped way to search text.
        """

    def rules(self) -> list[dict[str, Any]]:
        """Every rule the store holds, as a list of dicts sorted by name.

        Each dict is a rule definition in the shape `create_rule` accepts —
        `name`, `src_label`, `dst_label`, `predicate` (the externally-tagged
        form, `{"FieldEqual": {"field": "team"}}`), `edge_type`,
        `weight_prop`, `max_edges`, `approximate`, `via_label`, `via_edge`,
        `via_dir`, `namespace` — so a listed rule can be deleted and
        recreated from its own listing. Unset fields are `None`.

        Refused on a `scoped()` handle with `ValueError`: a rule names labels,
        fields and a namespace the scope may hide. `has_vector_rule` answers
        about one field.
        """

    def delete_rule(self, name: str) -> None:
        """Delete a rule and retract every edge it derived.

        Raises `RuleNotFound` (with `.name`) when no rule has that name.
        """

    def rebuild_rule(self, name: str) -> None:
        """Re-derive every edge of one rule from the store as it is now.

        The only way out of a tripped rule: `stats()` reports `tripped` when a
        rule hit its `max_edges` cap and stopped deriving.

        Raises `RuleNotFound` (with `.name`) when no rule has that name.
        """

    def suggest_rules(self) -> dict[str, Any]:
        """What rules the store's own data suggests. Creates nothing.

        Returns `{"suggestions": [...], "total": int, "bookkeeping_hidden":
        int, "truncated": bool}`. Each suggestion has `name`, `src_label`,
        `dst_label`, `edge_type`, `predicate` (one clause of text),
        `est_edges`, `examples` (`[src, dst, score]` lists), `rationale`, and
        **`create_rule_args`: pass that dict to `create_rule` unchanged.** It
        carries an explicit `weight_prop`, so the rule it creates here is the
        rule the MCP `create_rule` tool creates from the same suggestion.

        Proposals over fields the store writes for itself — `ns`, `kind`,
        `ts`, `source`, `provisional`, `id`, `aliases`, `alias_keys` — are
        dropped and counted in `bookkeeping_hidden`. The list is not capped
        (the MCP tool shows five); `truncated` means the engine's 5-second
        budget ran out and a second call may find more.

        Every proposal is a global rule: it links across namespaces.

        Refused on a `scoped()` handle with `ValueError`.
        """

    def remember(
        self,
        text: str,
        about: Sequence[str] | None = None,
        kind: Literal["note", "decision", "todo"] | None = None,
        ts: int | None = None,
        source: str | None = None,
        entities: Sequence[dict[str, Any]] | None = None,
        facts: Sequence[dict[str, str]] | None = None,
    ) -> dict[str, Any]:
        """Write a note the store can later `recall`, and whatever it names.

        `about` is the keys the note is about. A key that does not exist is
        created as a provisional `Entity` rather than refusing the call, and
        listed under `provisional` in the report.

        `entities` is a list of `{"key", "label", "props"?, "aliases"?}`
        dicts — entities you recognised in the text, created or updated in the
        same commit. `facts` is a list of `{"subject", "predicate", "object"}`
        dicts — relationships among them, written as edges. Either may be a
        tuple or any other sequence, as `about` may.

        `kind` is `"note"` (the default), `"decision"` or `"todo"`. `ts` is
        Unix seconds and defaults to now; it is part of the note's key, so the
        same text at the same `ts` is the same note. `source` defaults to
        `"agent"`.

        Returns the report as a dict: `note` (the note's key — **not** `key`,
        which is what the MCP tool's JSON reply calls it), `created`,
        `matched`, `derived`, `provisional`, `provisional_capped` (keys past
        the per-call cap of 20, **not** created), `fulltext_declared`,
        `same_as` and `same_as_lost` (each a list of `{"a", "b", "score"}`:
        the identity links this call made, and the ones it retracted). A
        `score` is the raw float (`0.6666666666666666`); the MCP tool prints
        the same score to two decimals.

        Raises `IngestError` when the text is empty or over 4,000 characters,
        the `kind` is unknown, an entity's `props` carry `aliases` or
        `alias_keys`, or a name is empty or only whitespace — a key in
        `about`, an entity's `key` or `label`, or a fact's `subject`,
        `predicate` or `object`; nothing is written. Its `.detail` is the
        bare sentence; the message carries an `ingest error: ` prefix in
        front of it.

        A store this binding creates has no memory schema. `remember`
        declares full-text on `Note.text` and on each new entity label's
        `name` itself, so `recall` works after the first call.

        ```python
        r = db.remember("Matthew is driving 0.7", about=["matthew"],
                        entities=[{"key": "v0.7", "label": "Release"}],
                        facts=[{"subject": "matthew", "predicate": "WORKS_ON", "object": "v0.7"}])
        r["note"]         # "note:…"
        r["provisional"]  # ["matthew"]
        ```
        """

    def recall(self, topic: str) -> dict[str, Any]:
        """What the store holds about a topic, as rows.

        Returns `{"indexed": bool, "terms": int, "hits": [...]}`. Each hit is
        `{"key", "label", "summary", "covered", "score"}`, best first:
        `covered` is how many of the topic's `terms` the node's own text
        holds, and it leads the ranking — the fused `score` is nearly flat.
        A hit covers at least half the terms. At most six hits. `terms == 0`
        with hits present means the topic was all stopwords: its words were
        searched together and every hit's `covered` is `0`. `summary` is
        the first 120 characters of the node's `text`, `summary` or `name`,
        or `None` when it has none of them.

        `indexed` is `False` when the store declares no full-text index at
        all, so no topic can match — a different answer from an empty `hits`.
        `remember` declares `Note.text` on its first call.

        This is the MCP `recall` tool's ranking, as data; the tool renders the
        same rows as a digest, and this method does not offer the digest.

        **The rows are raw stored content.** `key`, `label` and `summary` are
        unsanitized: only the digest renderer replaces control characters,
        line separators and bidi or zero-width characters with spaces. If you
        render a row into an assistant's context, that sanitisation is yours
        to do — a stored line break can otherwise forge a line of your
        prompt. This binding exposes no helper for it.

        Refused on a `scoped()` handle with `ValueError`.
        """

    def upsert_entity(
        self,
        key: str,
        props: dict[str, Scalar],
        label: str | None = None,
        aliases: Sequence[str] | None = None,
        namespace: str | None = None,
    ) -> dict[str, Any]:
        """Create or update one entity by key, the way the memory tools do.

        Unlike `upsert_node`, this maintains the entity's two identity lists:
        `aliases`, which the store derives from the current key and `name` and
        recomputes on every call — never accumulated, never yours to set — and
        `alias_keys`, the `aliases` you pass, kept as written and accumulating.
        It also sets `id` to the key and clears a `provisional` mark
        `remember` left.

        `label` is required to create and optional to update. **An update
        never changes a label**: one that differs from the stored label
        raises `IngestError` and nothing in `props` is written.

        `namespace` is the namespace a created node lands in. On an existing
        node its own namespace is a no-op and another raises
        `NamespaceImmutable`. A `props["ns"]` that disagrees with `namespace`
        is a `ValueError`.

        Returns `{"key", "label", "created", "updated_fields",
        "same_as_lost"}`. `same_as_lost` lists the identity links this update
        retracted, as `{"a", "b", "score"}`: a changed `name` can take a
        full-name link below the floor. A `score` is the raw float
        (`0.6666666666666666`); the MCP tool prints it to two decimals.

        `aliases` or `alias_keys` inside `props` raise `IngestError`: the
        store maintains both. So does a `key` that is empty or only
        whitespace, and a create under such a `label`. An `IngestError`'s
        `.detail` is the bare sentence; its message carries an
        `ingest error: ` prefix.

        ```python
        db.upsert_entity("ada", {"name": "Ada Lovelace"}, label="Person", aliases=["Countess"])
        ```
        """

    def schema_report(self, budget_ms: int = 1000) -> dict[str, Any]:
        """What the store holds and how it is wired.

        Returns a dict: `brief` (`nodes` and `edges` counts, `labels` each
        with its fields, `edge_types` each with its endpoints, `commits`,
        `roles`, `recipes`, `partial`), `rules` (each with its predicate in
        one clause and its namespace), `fulltext` and `indexes` (`[label,
        field]` lists), `provisional` (how many nodes `remember` named and
        nothing has described) and `provisional_sample` (the first ten keys).

        `budget_ms` bounds the counting; a spent budget sets
        `brief["partial"]` and the counts are then lower bounds. **`0` is not
        "no limit" here**, as it is for the graph algorithms: it is a budget
        already spent, and returns a partial report. For an unhurried report
        pass a large value, such as `60_000`.

        The MCP `schema` tool renders this same report. The names under
        `rules`, `fulltext`, `indexes` and `provisional_sample` are raw stored
        content, unsanitized, as `recall`'s rows are.

        Refused on a `scoped()` handle with `ValueError`.
        """

    def forget(
        self,
        key: str | None = None,
        prop: str | None = None,
        fact: dict[str, str] | None = None,
    ) -> dict[str, Any]:
        """Forget a node, one property, or one fact — exactly one of the three.

        - `forget(key=k)` tombstones the node and every edge on it.
        - `forget(key=k, prop=p)` removes one property.
        - `forget(fact={"subject", "predicate", "object"})` retracts one
          hand-written edge.

        Returns the report as a dict: `mode` (`"node"`, `"prop"` or
        `"fact"`), `target`, `changed` (`False` when there was nothing to
        forget), `manual_edges`, `derived_edges`, `notes` and `notes_total`
        (notes that still say it — **listed, never deleted**), `history_floor`,
        `prop` in prop mode, `aliases_rewritten` (a forgotten `name` took its
        words out of `aliases` in the same commit) and `alias_keys_remain`
        (forgotten `aliases` left declared aliases behind in `alias_keys`).

        **This is a tombstone, not a redaction.** `node_history`,
        `edge_history`, `edges_at` and `was_linked` still read what was
        forgotten, from `history_floor` on, until the log is pruned.

        Raises `ValueError` for any other combination of arguments,
        `KeyNotFound` for an unknown key or fact endpoint, and `RuleOwned` for
        a fact a rule derived. That refusal's message and its `.detail` are
        the same whole sentence — which rule owns the edge and the fields it
        reads — with no `edge is rule-owned: ` prefix in front of it. Nothing
        is written in any of the three.
        """

    def identity_clusters(self, floor: float = 0.6) -> dict[str, Any]:
        """Which keys are one entity: `SAME_AS` links resolved into identities.

        Returns `{"clusters": [...], "linked": int, "claims": int, "floor":
        float}`. Each cluster is `{"canonical", "members", "weakest"}`: every
        pair of `members` is linked at `floor` or above, `canonical` is the
        oldest member and `members[0]`, and `weakest` is the lowest pairwise
        score inside it. `claims` counts unordered pairs — both directions of
        a link are one claim. `weakest` is the raw float
        (`0.6666666666666666`); the MCP tool prints it to two decimals.

        Empty on a store with no `SAME_AS` edges. The identity preset that
        derives them is applied with `mushroomdb schema apply <db>
        --memory-identity`, on the command line, with this handle closed; a
        `SAME_AS` rule made with `create_rule` is the other way.

        Raises `ValueError` for a `floor` that is not a finite number.

        Refused on a `scoped()` handle with `ValueError`.
        """

    def stats(self) -> dict[str, Any]:
        """Node and edge counts, `history_floor`, `namespaces`, and per-rule figures.

        `namespaces` lists every namespace with at least one live node and its
        count. Each rule reports its provenance size, trip latch and fire
        counter. The shape matches the HTTP `/stats` JSON response.

        On a `scoped()` handle the counts stay store-wide; only the namespace
        roster narrows to the scope.
        """

    def roles(self) -> list[dict[str, Any]]:
        """The roles `roles.json` defines: `name`, `labels`, `keys`, `namespaces`, `visible_where`.

        `namespaces` is `None` for a role bound to no namespace, which means
        every one. `visible_where` is `None` or `{"field", "eq", "in"}`.

        `[]` when no roles are defined, and **raises `Corrupt` when `roles.json`
        was corrupt at open** — an unrestricted store answers `[]` too, so a
        poisoned sidecar must not read as "nothing is restricted here". Refused
        on a `scoped()` handle: a role definition names node keys, namespaces
        and the other roles in the store.

        That refusal is a plain **`ValueError`**, not a `MushroomError` — a
        refused read is the one kind of refusal here that is not a typed engine
        error, and the whole-store reads (`pagerank`, `search`, `rules` and the
        others that say so) raise the same. `ReadOnly` means *a
        scoped handle never writes*, and `roles()` is a read; calling it on a
        scoped handle is caller misuse, the same kind of thing as `scoped()`'s
        empty-scope `ValueError`. So a sidecar wrapping its boot-time role
        check in `except MushroomError` will not catch this one: catch
        `ValueError` too, or call `roles()` before narrowing the handle.
        """

    def snapshot(self) -> None:
        """Write a durable snapshot and truncate the WAL tail.

        The next `GraphDb.open()` on the same path then loads the snapshot
        directly and skips WAL replay, which is what makes reopening a large
        store quick.

        A snapshot needs the store's write lock, because it replaces the WAL a
        peer may be appending to. A handle that does not hold the lock raises
        `MushroomBusy` rather than snapshotting around another process's
        not-yet-durable bytes.
        """

    @staticmethod
    def restore(src: str | PathLike[str], dst: str | PathLike[str]) -> dict[str, Any]:
        """Seed the store directory `dst` from the backup `src`, and say what it did.

        `src` is a backup directory or a directory of them, where `latest` wins
        outright and otherwise the newest by mtime does. The copy is staged
        inside `dst` and opened there before anything is moved into place, so a
        backup that does not open leaves `dst` as it was found.

        Returns `{outcome, from, files, bytes}`. `outcome` is `"restored"`,
        `"already_present"` — `dst` already holds a store, which is **refused,
        not merged** — or `"empty"`, meaning nothing under `src` looks like a
        store. A caller that requires a fresh restore must read it; a sidecar
        rebuilding on boot can call this every time and ignore it.

        Raises `IoError` when a copy, an install or the staged open failed; the
        message names both directories.
        """

    def refresh(self) -> int:
        """Apply other processes' commits; return how many were applied.

        A handle does not poll the store, so another process's writes stay
        invisible until you call this. Rules fire and derived edges appear
        exactly as they would on a fresh open.

        Writes nothing, so a `read_only=True` handle and a `scoped()` one may
        both call it.

        A commit another process is still writing is left for the next call: a
        partial trailing frame is a wait, not an error, and the return value
        counts only the complete frames applied.

        A refresh that finds nothing new costs two filesystem metadata calls
        and an integer compare — it reads no file contents at all — so polling
        on an interval is cheap. See `docs/site/multiprocess.md`.
        """

    def close(self) -> None:
        """Close the handle and release the store.

        A `scoped()` child shares the one store, so either name closes both.
        """

    def __enter__(self) -> GraphDb: ...
    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc_value: BaseException | None,
        traceback: TracebackType | None,
    ) -> bool: ...
