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

class GraphDb:
    """An embedded mushroomdb store.

    One writer process per store; a handle sees only the commits made through
    it. Reopen to pick up another process's writes.
    """

    @staticmethod
    def open(path: str | PathLike[str]) -> GraphDb:
        """Open (creating if needed) the database rooted at `path`."""

    def insert_node(self, label: str, key: str, props: dict[str, Scalar]) -> None:
        """Insert a new node; raises if `key` is already live."""

    def upsert_node(
        self, label: str, key: str, props: dict[str, Scalar]
    ) -> Literal["inserted", "updated"]:
        """Insert `key` if absent, else update only the provided changed fields."""

    def insert_edge(self, edge_type: str, src: str, dst: str) -> bool:
        """Insert a user-owned edge; False if it already existed."""

    def insert_edge_upsert(
        self, edge_type: str, src: str, dst: str, placeholder_label: str
    ) -> dict[str, Any]:
        """Insert an edge, auto-creating missing endpoints as placeholder nodes."""

    def delete_edge(self, edge_type: str, src: str, dst: str) -> bool:
        """Delete a user-owned edge; raises for a rule-derived edge."""

    def delete_node(self, key: str) -> dict[str, int]:
        """Delete a node and its edges; returns `{manual_edges, derived_edges}`."""

    def set_prop(self, key: str, field: str, value: Scalar | None) -> None:
        """Set a property; `None` removes the field."""

    def remove_prop(self, key: str, field: str) -> bool:
        """Remove a property; False if it was already absent."""

    def query(self, cypher: str, params: Params = None) -> list[Row]:
        """Execute a read query and return one dict per row."""

    def query_with_params(
        self, cypher: str, params: Sequence[tuple[str, Scalar]]
    ) -> list[Row]:
        """Back-compat alias for `query(cypher, params)` with a tuple list."""

    def query_write(self, cypher: str, params: Params = None) -> list[Row]:
        """Execute a Cypher write statement (CREATE / SET / DELETE / MERGE)."""

    def query_at(self, commit: int, cypher: str, params: Params = None) -> list[Row]:
        """Time-travel read: run `cypher` against the graph as of `commit`."""

    def rename_node(self, old: str, new: str) -> None:
        """Rename a node's key, preserving its edges and history."""

    def create_rule(self, rule: dict[str, Any], if_not_exists: bool = False) -> bool:
        """Register a linking rule; False when `if_not_exists` skips a duplicate."""

    def explain(self, a: str, b: str) -> list[Row]:
        """Why are `a` and `b` linked? One dict per derived edge."""

    def neighbors(self, key: str, edge_type: str, direction: str) -> list[str]:
        """One-hop neighbour keys along `edge_type` ('out' or 'in')."""

    def node_info(self, key: str) -> dict[str, Any] | None:
        """`{key, label, props}` for a live node, or None if absent."""

    def node_edges(self, key: str) -> list[Row]:
        """Edges incident on `key`; raises for an unknown key."""

    def node_history(self, key: str) -> list[Row]:
        """Per-node change history since the last truncating snapshot."""

    def was_linked(self, a: str, b: str, edge_type: str, at_commit: int) -> bool:
        """Whether `a` and `b` were linked by `edge_type` at or before `at_commit`."""

    def enable_index(self, label: str, field: str) -> None:
        """Enable an equality index on `(label, field)`."""

    def disable_index(self, label: str, field: str) -> None:
        """Disable the equality index on `(label, field)`."""

    def is_index_enabled(self, label: str, field: str) -> bool:
        """Whether `(label, field)` currently has an equality index."""

    def has_vector_rule(self, field: str) -> bool:
        """Whether an approximate (HNSW) VectorSimilar rule covers `field`."""

    def find_similar(
        self,
        field: str,
        vector: Sequence[float],
        label: str | None = None,
        k: int = 10,
        min: float = 0.0,
        mask: Sequence[str] | None = None,
    ) -> list[tuple[str, float]]:
        """The `k` nearest nodes to `vector` by cosine similarity on `field`."""

    def search_hybrid(
        self,
        text_field: str,
        query_text: str,
        vector_field: str,
        vector: Sequence[float],
        label: str | None = None,
        k: int = 10,
    ) -> list[tuple[str, float]]:
        """Reciprocal-rank fusion over fulltext and vector similarity."""

    def get_edge_prop(
        self, edge_type: str, src_key: str, dst_key: str, field: str
    ) -> Any | None:
        """Read a single property from an edge, or None if absent."""

    def ingest_batch(
        self,
        nodes: Sequence[dict[str, Any]],
        edges: Sequence[dict[str, str]] | None = None,
    ) -> dict[str, Any]:
        """Atomically ingest nodes and edges in a single WAL commit."""

    def batch_edges(
        self,
        inserts: Sequence[dict[str, str]] | None = None,
        deletes: Sequence[dict[str, str]] | None = None,
    ) -> dict[str, int]:
        """Atomically apply edge inserts and deletes in a single WAL commit."""

    def stats(self) -> dict[str, Any]:
        """Node/edge counts plus per-rule provenance size, latch, and fires."""

    def snapshot(self) -> None:
        """Write a durable snapshot and truncate the WAL tail."""

    def close(self) -> None:
        """Close the handle and release the store."""

    def __enter__(self) -> GraphDb: ...
    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc_value: BaseException | None,
        traceback: TracebackType | None,
    ) -> bool: ...
