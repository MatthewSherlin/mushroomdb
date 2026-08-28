"""MushroomDB GraphStore for LangChain.

Built against langchain-community 0.4.2 / langchain-core 1.6.1.

Implements: ``GraphStore`` (``langchain_community.graphs.graph_store.GraphStore``).

Abstract contract fulfilled:
  - ``get_schema``         (property) -> str
  - ``get_structured_schema`` (property) -> Dict[str, Any]
  - ``query(query, params={})`` -> List[Dict[str, Any]]
  - ``refresh_schema()``   -> None
  - ``add_graph_documents(graph_documents, include_source=False)`` -> None

Binding gaps (mushroomdb 0.1.2 — same as the LlamaIndex integration):
  - **No edge-property API**: ``Relationship.properties`` are silently dropped.
    ``insert_edge`` accepts only type/src/dst; no ``set_edge_prop`` exists.
  - **No MERGE keyword**: upsert is ``insert_node`` on first write, then
    ``set_prop`` per field on update.
  - **``id`` is a reserved Cypher parameter name**: avoid it in ``params``
    passed to ``query()``.
  - **``DELETE r`` silently no-ops**: use ``delete_edge``/``batch_edges`` to
    remove edges.
  - **Single-quoted string literals only**: double-quoted strings cause a lex
    error inside the Cypher parser.

Reserved sentinel property
--------------------------
``__id__`` — the node's string key as stored in mushroomdb.  Do not use this
name in your node ``properties`` dicts; it will be overwritten on every upsert.

When ``include_source=True`` in ``add_graph_documents``, the source Document is
stored as a ``Document``-labelled node and each extracted entity node receives a
``MENTIONS`` edge from the Document to the entity.
"""

from __future__ import annotations

from typing import Any, Dict, List, Optional

import mushroomdb
from langchain_community.graphs.graph_document import GraphDocument, Node
from langchain_community.graphs.graph_store import GraphStore

_PROP_ID = "__id__"


def _escape_str(s: str) -> str:
    """Escape a string for inline Cypher single-quoted literals."""
    return s.replace("\\", "\\\\").replace("'", "\\'")


class MushroomDBGraphStore(GraphStore):
    """LangChain ``GraphStore`` backed by mushroomdb (embedded mode).

    Implements ``langchain_community.graphs.graph_store.GraphStore``
    (langchain-community 0.4.2).

    Parameters
    ----------
    path:
        Filesystem path where mushroomdb will store its WAL/snapshot files.
        The directory is created automatically by the binding if it does not
        exist.

    Notes
    -----
    **Reserved sentinel property** — ``__id__`` is written to every node to
    store its string key.  Do not use ``__id__`` in user-supplied node
    ``properties``; it will be silently overwritten on every upsert.

    **Relation properties are not persisted** — mushroomdb 0.1.2 has no
    edge-property API (``insert_edge`` accepts only type/src/dst; no
    ``set_edge_prop`` exists; ``SET r.field`` is rejected by the Cypher
    planner).  ``Relationship.properties`` passed to
    ``add_graph_documents`` are silently dropped.  This is a binding gap,
    not a design choice.

    **Cypher subset** — ``query()`` is a direct Cypher passthrough.  mushroomdb
    implements a subset of openCypher; the following are *not* supported:
    ``MERGE``, ``id()`` function, double-quoted string literals, and the
    parameter name ``id`` (reserved by the parser).

    **Single-process** — mushroomdb is an embedded store; open at most one
    ``MushroomDBGraphStore`` per database path per process.
    """

    def __init__(self, path: str, **kwargs: Any) -> None:
        self._path = path
        self._db = mushroomdb.GraphDb.open(path)
        self._schema: str = ""
        self._structured_schema: Dict[str, Any] = {}

    # ------------------------------------------------------------------
    # Schema properties (abstract in GraphStore)
    # ------------------------------------------------------------------

    @property
    def get_schema(self) -> str:
        """Return the current schema as a human-readable string.

        Call ``refresh_schema()`` first to ensure it reflects the current
        database state.
        """
        return self._schema

    @property
    def get_structured_schema(self) -> Dict[str, Any]:
        """Return the current schema as a structured dict.

        Shape (mirrors Neo4jGraph convention)::

            {
                "node_props": {"Person": ["age", "name"], "Movie": ["title"]},
                "rel_props": {},   # always empty — binding gap
                "relationships": [
                    {"start": "Person", "type": "LIKES", "end": "Movie"}
                ],
            }

        Call ``refresh_schema()`` first to ensure it reflects the current
        database state.
        """
        return self._structured_schema

    # ------------------------------------------------------------------
    # refresh_schema
    # ------------------------------------------------------------------

    def refresh_schema(self) -> None:
        """Recompute schema strings from the live database state.

        Scans all nodes and edges.  For large graphs this will be slower
        than a native schema introspection endpoint; at knowledge-graph
        scale (thousands of nodes) it is acceptable.
        """
        node_props: Dict[str, set] = {}
        rel_prop_keys: Dict[str, set] = {}
        seen_rels: set = set()
        relationships: List[Dict[str, str]] = []

        # Collect all node keys, then fetch label + props via node_info.
        try:
            rows = self._db.query(f"MATCH (n) RETURN n.{_PROP_ID} LIMIT 100000")
        except RuntimeError:
            rows = []

        for row in rows:
            nid = row.get(f"n.{_PROP_ID}")
            if not nid:
                continue
            info = self._db.node_info(nid)
            if not info:
                continue
            label: str = info["label"]
            if label not in node_props:
                node_props[label] = set()
            for k in info["props"]:
                if not k.startswith("__"):
                    node_props[label].add(k)

        # Collect all edges.
        try:
            edge_rows = self._db.query(
                f"MATCH (a)-[r]->(b) RETURN a.{_PROP_ID}, type(r), b.{_PROP_ID} LIMIT 100000"
            )
        except RuntimeError:
            edge_rows = []

        for row in edge_rows:
            src_id = row.get(f"a.{_PROP_ID}")
            rel_type = row.get("type(r)")
            dst_id = row.get(f"b.{_PROP_ID}")
            if not (src_id and rel_type and dst_id):
                continue
            if rel_type not in rel_prop_keys:
                rel_prop_keys[rel_type] = set()
            src_info = self._db.node_info(src_id)
            dst_info = self._db.node_info(dst_id)
            if src_info and dst_info:
                key = (src_info["label"], rel_type, dst_info["label"])
                if key not in seen_rels:
                    seen_rels.add(key)
                    relationships.append({
                        "start": src_info["label"],
                        "type": rel_type,
                        "end": dst_info["label"],
                    })

        # Structured schema dict (mirrors Neo4jGraph convention).
        self._structured_schema = {
            "node_props": {lbl: sorted(props) for lbl, props in node_props.items()},
            "rel_props": {},
            "relationships": relationships,
        }

        # Human-readable schema string.
        lines = ["Node labels and properties:"]
        if node_props:
            for lbl, props in sorted(node_props.items()):
                prop_str = ", ".join(sorted(props)) if props else "(none)"
                lines.append(f"  ({lbl} {{{prop_str}}})")
        else:
            lines.append("  (none)")

        lines.append("")
        lines.append("Relationship types:")
        if relationships:
            seen_strs: set = set()
            for rel in relationships:
                s = f"  (:{rel['start']})-[:{rel['type']}]->(:{rel['end']})"
                if s not in seen_strs:
                    seen_strs.add(s)
                    lines.append(s)
        else:
            lines.append("  (none)")

        self._schema = "\n".join(lines)

    # ------------------------------------------------------------------
    # query — Cypher passthrough
    # ------------------------------------------------------------------

    def query(self, query: str, params: dict = {}) -> List[Dict[str, Any]]:
        """Execute a Cypher query against mushroomdb.

        Parameters
        ----------
        query:
            Cypher query string.  mushroomdb supports a subset of
            openCypher — no ``MERGE``, no ``id()`` function, single-quoted
            string literals only.
        params:
            Optional parameter dict.  Keys must not be ``'id'`` (reserved
            by the mushroomdb Cypher parser).  Values are bound positionally
            as ``$name`` placeholders.

        Returns
        -------
        List[Dict[str, Any]]
            One dict per result row.  Write statements (``CREATE``,
            ``DELETE``, ``SET``) that raise a ``RuntimeError`` on the
            read path are automatically retried via ``query_write``.
        """
        param_list = list(params.items()) if params else []
        try:
            if param_list:
                return self._db.query_with_params(query, param_list)
            return self._db.query(query)
        except RuntimeError:
            result = (
                self._db.query_write(query, param_list)
                if param_list
                else self._db.query_write(query)
            )
            return result or []

    # ------------------------------------------------------------------
    # add_graph_documents
    # ------------------------------------------------------------------

    def add_graph_documents(
        self,
        graph_documents: List[GraphDocument],
        include_source: bool = False,
    ) -> None:
        """Load ``GraphDocument`` objects into mushroomdb.

        Parameters
        ----------
        graph_documents:
            Each ``GraphDocument`` carries ``nodes`` (List[Node]),
            ``relationships`` (List[Relationship]), and ``source`` (a
            LangChain ``Document``).
        include_source:
            When ``True``, the ``source`` Document is inserted as a
            ``Document``-labelled node and each entity node gains a
            ``MENTIONS`` edge from the Document to the entity.

        Notes
        -----
        ``Relationship.properties`` are **silently dropped** — the
        mushroomdb 0.1.2 binding has no edge-property API.
        """
        for gd in graph_documents:
            source_key: Optional[str] = None

            if include_source and gd.source is not None:
                source_key = self._ingest_source(gd.source)

            for node in gd.nodes:
                self._upsert_node(node)
                if source_key is not None:
                    # Document -[:MENTIONS]-> entity
                    self._db.insert_edge("MENTIONS", source_key, str(node.id))

            for rel in gd.relationships:
                src_key = str(rel.source.id)
                dst_key = str(rel.target.id)
                # Ensure endpoint nodes exist (may already be present from gd.nodes).
                self._ensure_node_stub(rel.source)
                self._ensure_node_stub(rel.target)
                # rel.properties silently dropped — binding gap; see class docstring.
                self._db.insert_edge(rel.type, src_key, dst_key)

    # ------------------------------------------------------------------
    # Internal helpers
    # ------------------------------------------------------------------

    def _upsert_node(self, node: Node) -> None:
        """Insert or update a Node from a GraphDocument."""
        key = str(node.id)
        label = node.type or "Node"
        props = dict(node.properties)
        props[_PROP_ID] = key

        existing = self._db.node_info(key)
        if existing is None:
            self._db.insert_node(label, key, props)
        elif existing["label"] == label:
            for k, v in props.items():
                self._db.set_prop(key, k, v)
        else:
            # Label changed: delete + reinsert, preserving user-owned edges.
            edges = self._db.node_edges(key)
            user_edges = [e for e in edges if not e.get("derived", False)]
            self._db.query_write(
                f"MATCH (n) WHERE n.{_PROP_ID} = '{_escape_str(key)}' DETACH DELETE n"
            )
            merged = dict(existing["props"])
            merged.update(props)
            self._db.insert_node(label, key, merged)
            for e in user_edges:
                self._db.insert_edge(e["edge_type"], e["src_key"], e["dst_key"])

    def _ensure_node_stub(self, node: Node) -> None:
        """Create a stub node if the key doesn't exist yet."""
        if self._db.node_info(str(node.id)) is None:
            self._upsert_node(node)

    def _ingest_source(self, source: Any) -> str:
        """Persist a LangChain Document as a 'Document'-labelled node.

        Returns the node key used.
        """
        page_content: str = getattr(source, "page_content", "") or ""
        metadata: dict = dict(getattr(source, "metadata", {}) or {})

        # Prefer an explicit source/id from metadata; fall back to content hash.
        key = metadata.get("source") or metadata.get("id") or (
            f"doc_{abs(hash(page_content)) % (10 ** 9)}"
        )
        key = str(key)

        props = dict(metadata)
        props[_PROP_ID] = key
        props["page_content"] = page_content

        existing = self._db.node_info(key)
        if existing is None:
            self._db.insert_node("Document", key, props)
        else:
            for k, v in props.items():
                self._db.set_prop(key, k, v)
        return key

    def __del__(self) -> None:
        try:
            self._db.close()
        except Exception:
            pass
