"""MushroomDB PropertyGraphStore for LlamaIndex.

Built against llama-index-core 0.14.24.

Binding surface gaps worked around in this file:
- No native vector similarity (find_similar missing): cosine similarity is
  computed in Python over embeddings stored as list-valued node properties.
- No MERGE Cypher keyword: upsert is implemented as insert_node + set_prop
  fallback when the key already exists.
- No label-rename primitive: when upsert_nodes is called for a key that
  already exists with a different label (e.g. a placeholder created by
  upsert_relations with label "entity" is later updated with label "Person"),
  the store performs a delete + reinsert, restoring all user-owned edges
  afterwards. Rule-derived edges are not restored (they are re-derived by the
  engine automatically).
- 'id' is a reserved Cypher parameter name: query parameters use 'k', 'v',
  'p0'..'pN' as parameter names instead.

Reserved sentinel properties
-----------------------------
The following property names are used internally and must not appear in
user-supplied ``EntityNode.properties`` or ``ChunkNode.properties`` — they
will be silently overwritten on every upsert:

  ``__id__``        — the node's primary key (= LabelledNode.id)
  ``__type__``      — "entity" or "chunk"
  ``__name__``      — EntityNode.name
  ``__text__``      — ChunkNode.text
  ``__embedding__`` — the embedding vector (list of floats)
"""

from __future__ import annotations

import math
from typing import Any, Dict, List, Optional, Sequence, Tuple

import mushroomdb
from llama_index.core.graph_stores.types import (
    ChunkNode,
    EntityNode,
    LabelledNode,
    PropertyGraphStore,
    Relation,
)
from llama_index.core.vector_stores.types import VectorStoreQuery

# Sentinel property names stored alongside user properties inside mushroomdb.
_PROP_ID = "__id__"
_PROP_TYPE = "__type__"
_PROP_NAME = "__name__"
_PROP_TEXT = "__text__"
_PROP_EMBEDDING = "__embedding__"

_TYPE_ENTITY = "entity"
_TYPE_CHUNK = "chunk"


def _cosine(a: List[float], b: List[float]) -> float:
    """Pure-Python cosine similarity, used in lieu of a native find_similar."""
    dot = sum(x * y for x, y in zip(a, b))
    mag_a = math.sqrt(sum(x * x for x in a))
    mag_b = math.sqrt(sum(x * x for x in b))
    if mag_a == 0.0 or mag_b == 0.0:
        return 0.0
    return dot / (mag_a * mag_b)


def _escape_str(s: str) -> str:
    """Escape a string for inline Cypher single-quoted literals."""
    return s.replace("\\", "\\\\").replace("'", "\\'")


class MushroomDBPropertyGraphStore(PropertyGraphStore):
    """LlamaIndex ``PropertyGraphStore`` backed by mushroomdb (embedded mode).

    Parameters
    ----------
    path:
        Filesystem path where mushroomdb will store its WAL/snapshot files.
        The directory is created automatically by the binding if it does not
        exist.

    Notes
    -----
    **Reserved sentinel properties** — the following property names are managed
    by this class and must not be used in user-supplied node ``properties``
    dicts (they are silently overwritten on every upsert):

    ============ ==============================================================
    ``__id__``        Primary key (= ``LabelledNode.id``)
    ``__type__``      Node kind: ``"entity"`` or ``"chunk"``
    ``__name__``      ``EntityNode.name``
    ``__text__``      ``ChunkNode.text``
    ``__embedding__`` Embedding vector (list of floats)
    ============ ==============================================================

    **Label updates** — the mushroomdb binding has no label-rename primitive.
    When a node with an existing key is upserted with a *different* label (the
    common case: a placeholder node created with label ``"entity"`` by
    ``upsert_relations`` is later given its real label via ``upsert_nodes``),
    the implementation deletes the node and reinserts it with the new label,
    then restores all user-owned incident edges.  Rule-derived edges are NOT
    restored (the engine re-derives them automatically).

    **Vector queries** — ``vector_query`` scans all embedded nodes in Python
    (no native ANN in the binding as of 0.1.2).  This is fine for typical
    knowledge-graph sizes; it does not scale to millions of nodes.

    **Relation properties are not persisted** — the mushroomdb binding (0.1.2)
    has no edge-property API (``insert_edge`` accepts only type/src/dst;
    ``SET r.field`` is rejected by the Cypher planner; no ``set_edge_prop``
    method exists).  ``Relation.properties`` supplied to ``upsert_relations``
    are silently dropped.  ``get_triplets`` and ``get_rel_map`` always return
    ``Relation`` objects with ``properties={}``.  This is a binding gap, not a
    design choice; it will be fixed when native edge-property support is added
    to the binding (queued alongside ANN).
    """

    supports_structured_queries: bool = True
    supports_vector_queries: bool = True

    def __init__(self, path: str, **kwargs: Any) -> None:
        self._path = path
        self._db = mushroomdb.GraphDb.open(path)

    # ------------------------------------------------------------------
    # Internal helpers
    # ------------------------------------------------------------------

    @property
    def client(self) -> Any:
        return self._db

    def _node_to_props(self, node: LabelledNode) -> Dict[str, Any]:
        """Flatten a LabelledNode into the property dict we persist."""
        props: Dict[str, Any] = dict(node.properties)
        props[_PROP_ID] = node.id
        if isinstance(node, EntityNode):
            props[_PROP_TYPE] = _TYPE_ENTITY
            props[_PROP_NAME] = node.name
        elif isinstance(node, ChunkNode):
            props[_PROP_TYPE] = _TYPE_CHUNK
            props[_PROP_TEXT] = node.text
        if node.embedding is not None:
            props[_PROP_EMBEDDING] = node.embedding
        return props

    def _info_to_node(self, info: Dict[str, Any]) -> LabelledNode:
        """Reconstruct a LabelledNode from a mushroomdb node_info dict."""
        raw: Dict[str, Any] = dict(info["props"])
        ntype = raw.pop(_PROP_TYPE, _TYPE_ENTITY)
        nid = raw.pop(_PROP_ID, info["key"])
        label: str = info["label"]
        embedding: Optional[List[float]] = raw.pop(_PROP_EMBEDDING, None) or None

        if ntype == _TYPE_CHUNK:
            text = raw.pop(_PROP_TEXT, "")
            return ChunkNode(
                text=text,
                id_=nid,
                label=label,
                embedding=embedding,
                properties=raw,
            )
        else:
            name = raw.pop(_PROP_NAME, nid)
            return EntityNode(
                name=name,
                label=label,
                embedding=embedding,
                properties=raw,
            )

    def _upsert_one_node(self, node: LabelledNode) -> None:
        props = self._node_to_props(node)
        existing = self._db.node_info(node.id)
        if existing is None:
            self._db.insert_node(node.label, node.id, props)
        elif existing["label"] == node.label:
            # Same label — just update properties in place.
            for k, v in props.items():
                self._db.set_prop(node.id, k, v)
        else:
            # Label changed (common case: placeholder "entity" → real label).
            # mushroomdb has no rename primitive, so we delete + reinsert and
            # restore all user-owned incident edges.  Rule-derived edges are
            # re-derived automatically by the engine.
            edges = self._db.node_edges(node.id)
            user_edges = [e for e in edges if not e["derived"]]
            self._db.query_write(
                f"MATCH (n) WHERE n.{_PROP_ID} = '{_escape_str(node.id)}' DETACH DELETE n"
            )
            # Merge stored props with the incoming props (incoming wins).
            merged = dict(existing["props"])
            merged.update(props)
            self._db.insert_node(node.label, node.id, merged)
            for e in user_edges:
                # Edge properties are not restored — the binding has no
                # edge-property API, so they were never stored to begin with.
                self._db.insert_edge(e["edge_type"], e["src_key"], e["dst_key"])

    def _ensure_node(self, node_id: str) -> None:
        """Create a placeholder entity node if the key does not exist."""
        if self._db.node_info(node_id) is None:
            self._db.insert_node(
                _TYPE_ENTITY,
                node_id,
                {_PROP_ID: node_id, _PROP_TYPE: _TYPE_ENTITY, _PROP_NAME: node_id},
            )

    def _run_triplet_query(self, cypher: str) -> List[Tuple[LabelledNode, Relation, LabelledNode]]:
        rows = self._db.query(cypher)
        triplets: List[Tuple[LabelledNode, Relation, LabelledNode]] = []
        node_cache: Dict[str, LabelledNode] = {}

        def cached_node(nid: str) -> LabelledNode:
            if nid not in node_cache:
                info = self._db.node_info(nid)
                node_cache[nid] = (
                    self._info_to_node(info)
                    if info
                    else EntityNode(name=nid, label=_TYPE_ENTITY)
                )
            return node_cache[nid]

        for row in rows:
            src_id = row.get("a.__id__")
            rel_type = row.get("type(r)")
            dst_id = row.get("b.__id__")
            if src_id and rel_type and dst_id:
                rel = Relation(label=rel_type, source_id=src_id, target_id=dst_id)
                triplets.append((cached_node(src_id), rel, cached_node(dst_id)))

        return triplets

    # ------------------------------------------------------------------
    # PropertyGraphStore abstract methods
    # ------------------------------------------------------------------

    def upsert_nodes(self, nodes: Sequence[LabelledNode]) -> None:
        for node in nodes:
            self._upsert_one_node(node)

    def upsert_relations(self, relations: List[Relation]) -> None:
        """Upsert relations into the graph.

        .. warning::
            ``Relation.properties`` are **silently dropped**.  The mushroomdb
            0.1.2 binding has no edge-property API (``insert_edge`` accepts
            only type/src/dst; no ``set_edge_prop`` exists).  This is a
            binding gap queued for a future release.
        """
        for rel in relations:
            self._ensure_node(rel.source_id)
            self._ensure_node(rel.target_id)
            # insert_edge returns False (not an error) for duplicate edges.
            # rel.properties are not persisted — binding gap, see class docstring.
            self._db.insert_edge(rel.label, rel.source_id, rel.target_id)

    def get(
        self,
        properties: Optional[dict] = None,
        ids: Optional[List[str]] = None,
    ) -> List[LabelledNode]:
        seen: set = set()
        results: List[LabelledNode] = []

        if ids:
            for nid in ids:
                if nid in seen:
                    continue
                info = self._db.node_info(nid)
                if info:
                    seen.add(nid)
                    results.append(self._info_to_node(info))

        if properties:
            conditions = []
            params: List[Tuple[str, Any]] = []
            for i, (k, v) in enumerate(properties.items()):
                pname = f"p{i}"
                conditions.append(f"n.{k} = ${pname}")
                params.append((pname, v))
            where = " AND ".join(conditions)
            cypher = f"MATCH (n) WHERE {where} RETURN n.{_PROP_ID} LIMIT 10000"
            col = f"n.{_PROP_ID}"
            rows = self._db.query_with_params(cypher, params)
            for row in rows:
                nid = row.get(col)
                if nid and nid not in seen:
                    info = self._db.node_info(nid)
                    if info:
                        seen.add(nid)
                        results.append(self._info_to_node(info))

        return results

    def get_triplets(
        self,
        entity_names: Optional[List[str]] = None,
        relation_names: Optional[List[str]] = None,
        properties: Optional[dict] = None,
        ids: Optional[List[str]] = None,
    ) -> List[Tuple[LabelledNode, Relation, LabelledNode]]:
        # Build the MATCH pattern
        if relation_names:
            rel_pat = "[r:" + "|".join(relation_names) + "]"
        else:
            rel_pat = "[r]"

        # Build WHERE clause
        conditions = []
        if entity_names:
            names_lit = ", ".join(f"'{_escape_str(n)}'" for n in entity_names)
            conditions.append(f"a.{_PROP_NAME} IN [{names_lit}]")
        if ids:
            ids_lit = ", ".join(f"'{_escape_str(i)}'" for i in ids)
            conditions.append(f"a.{_PROP_ID} IN [{ids_lit}]")

        where = (" WHERE " + " AND ".join(conditions)) if conditions else ""
        cypher = (
            f"MATCH (a)-{rel_pat}->(b){where} "
            f"RETURN a.{_PROP_ID}, type(r), b.{_PROP_ID} LIMIT 10000"
        )

        triplets = self._run_triplet_query(cypher)

        # Post-filter by properties if requested
        if properties:
            triplets = [
                (src, rel, dst)
                for src, rel, dst in triplets
                if all(src.properties.get(k) == v for k, v in properties.items())
            ]

        return triplets

    def get_rel_map(
        self,
        graph_nodes: List[LabelledNode],
        depth: int = 2,
        limit: int = 30,
        ignore_rels: Optional[List[str]] = None,
    ) -> List[Tuple[LabelledNode, Relation, LabelledNode]]:
        if not graph_nodes:
            return []

        ignore_set = set(ignore_rels or [])
        current_ids = {node.id for node in graph_nodes}
        visited_edges: set = set()
        triplets: List[Tuple[LabelledNode, Relation, LabelledNode]] = []
        node_cache: Dict[str, LabelledNode] = {node.id: node for node in graph_nodes}

        def cached_node(nid: str) -> LabelledNode:
            if nid not in node_cache:
                info = self._db.node_info(nid)
                node_cache[nid] = (
                    self._info_to_node(info)
                    if info
                    else EntityNode(name=nid, label=_TYPE_ENTITY)
                )
            return node_cache[nid]

        for _ in range(depth):
            if not current_ids or len(triplets) >= limit:
                break

            ids_lit = ", ".join(f"'{_escape_str(i)}'" for i in current_ids)
            cypher = (
                f"MATCH (a)-[r]->(b) WHERE a.{_PROP_ID} IN [{ids_lit}] "
                f"RETURN a.{_PROP_ID}, type(r), b.{_PROP_ID} LIMIT {limit}"
            )
            rows = self._db.query(cypher)
            next_ids: set = set()

            for row in rows:
                src_id = row.get(f"a.{_PROP_ID}")
                rel_type = row.get("type(r)")
                dst_id = row.get(f"b.{_PROP_ID}")
                if not (src_id and rel_type and dst_id):
                    continue
                if rel_type in ignore_set:
                    continue
                edge_key = (src_id, rel_type, dst_id)
                if edge_key in visited_edges:
                    continue
                visited_edges.add(edge_key)
                rel = Relation(label=rel_type, source_id=src_id, target_id=dst_id)
                triplets.append((cached_node(src_id), rel, cached_node(dst_id)))
                next_ids.add(dst_id)
                if len(triplets) >= limit:
                    break

            # Continue BFS only from newly discovered nodes
            current_ids = next_ids - {node.id for node in graph_nodes}

        return triplets

    def delete(
        self,
        entity_names: Optional[List[str]] = None,
        relation_names: Optional[List[str]] = None,
        properties: Optional[dict] = None,
        ids: Optional[List[str]] = None,
    ) -> None:
        targets: set = set()

        if ids:
            targets.update(ids)

        if entity_names:
            names_lit = ", ".join(f"'{_escape_str(n)}'" for n in entity_names)
            cypher = f"MATCH (n) WHERE n.{_PROP_NAME} IN [{names_lit}] RETURN n.{_PROP_ID} LIMIT 10000"
            for row in self._db.query(cypher):
                nid = row.get(f"n.{_PROP_ID}")
                if nid:
                    targets.add(nid)

        if properties:
            conditions = []
            params: List[Tuple[str, Any]] = []
            for i, (k, v) in enumerate(properties.items()):
                pname = f"p{i}"
                conditions.append(f"n.{k} = ${pname}")
                params.append((pname, v))
            where = " AND ".join(conditions)
            cypher = f"MATCH (n) WHERE {where} RETURN n.{_PROP_ID} LIMIT 10000"
            for row in self._db.query_with_params(cypher, params):
                nid = row.get(f"n.{_PROP_ID}")
                if nid:
                    targets.add(nid)

        # DETACH DELETE nodes (removes their incident edges too)
        for nid in targets:
            lit = _escape_str(nid)
            self._db.query_write(
                f"MATCH (n) WHERE n.{_PROP_ID} = '{lit}' DETACH DELETE n"
            )

        # Delete relations by type (without deleting nodes)
        if relation_names:
            for rel_name in relation_names:
                cypher = (
                    f"MATCH (a)-[r:{rel_name}]->(b) "
                    f"RETURN a.{_PROP_ID}, b.{_PROP_ID} LIMIT 10000"
                )
                rows = self._db.query(cypher)
                deletes = [
                    {"edge_type": rel_name, "src": r.get(f"a.{_PROP_ID}"), "dst": r.get(f"b.{_PROP_ID}")}
                    for r in rows
                    if r.get(f"a.{_PROP_ID}") and r.get(f"b.{_PROP_ID}")
                ]
                if deletes:
                    self._db.batch_edges(deletes=deletes)

    def structured_query(
        self,
        query: str,
        param_map: Optional[Dict[str, Any]] = None,
    ) -> Any:
        """Pass a Cypher statement through to mushroomdb.

        Read queries (MATCH/RETURN) are sent via ``query`` /
        ``query_with_params``.  If mushroomdb raises ``RuntimeError`` (which
        it does for write statements sent to the read-only path), the method
        automatically retries via ``query_write``.  This means both read and
        write Cypher is accepted transparently — callers do not need to
        distinguish between them.

        Note: mushroomdb supports a subset of openCypher.  ``MERGE``, ``id()``,
        and double-quoted string literals are not supported.  Parameters must
        not use ``id`` as a name (it is a reserved keyword in the parser).
        """
        params = list((param_map or {}).items())
        try:
            if params:
                return self._db.query_with_params(query, params)
            return self._db.query(query)
        except RuntimeError:
            # Likely a write statement — try query_write
            if params:
                return self._db.query_write(query, params)
            return self._db.query_write(query)

    def vector_query(
        self,
        query: VectorStoreQuery,
        **kwargs: Any,
    ) -> Tuple[List[LabelledNode], List[float]]:
        """Cosine similarity search over stored embeddings.

        mushroomdb does not expose a native find_similar / ANN API in the
        Python binding (as of 0.1.2). Embeddings are stored as list-valued
        node properties and the top-k ranking is computed in Python. For
        datasets of reasonable knowledge-graph size this is acceptable; a
        native binding method would replace this loop.
        """
        q_emb = query.query_embedding
        if not q_emb:
            return [], []

        k = max(1, query.similarity_top_k or 1)

        # Fetch all nodes that have an embedding stored
        col = f"n.{_PROP_ID}"
        rows = self._db.query(
            f"MATCH (n) WHERE n.{_PROP_EMBEDDING} IS NOT NULL RETURN {col} LIMIT 10000"
        )

        candidates: List[Tuple[float, Dict[str, Any]]] = []
        for row in rows:
            nid = row.get(col)
            if not nid:
                continue
            info = self._db.node_info(nid)
            if not info:
                continue
            emb: Optional[List[float]] = info["props"].get(_PROP_EMBEDDING)
            if emb and len(emb) == len(q_emb):
                score = _cosine(q_emb, emb)
                candidates.append((score, info))

        candidates.sort(key=lambda x: x[0], reverse=True)
        top = candidates[:k]
        nodes = [self._info_to_node(info) for _, info in top]
        scores = [score for score, _ in top]
        return nodes, scores

    def persist(
        self,
        persist_path: str,
        fs: Any = None,
    ) -> None:
        """Flush the WAL and write a durable snapshot.

        The ``persist_path`` argument is accepted for API compatibility but
        ignored — mushroomdb is embedded and already knows its own path.
        The ``fs`` argument (fsspec filesystem) is not applicable to an
        embedded store and is silently ignored.
        """
        self._db.snapshot()

    # ------------------------------------------------------------------
    # Async stubs — thin wrappers around the sync methods
    # ------------------------------------------------------------------

    async def aupsert_nodes(self, nodes: List[LabelledNode]) -> None:
        self.upsert_nodes(nodes)

    async def aupsert_relations(self, relations: List[Relation]) -> None:
        self.upsert_relations(relations)

    async def aget(
        self,
        properties: Optional[dict] = None,
        ids: Optional[List[str]] = None,
    ) -> List[LabelledNode]:
        return self.get(properties=properties, ids=ids)

    async def aget_triplets(
        self,
        entity_names: Optional[List[str]] = None,
        relation_names: Optional[List[str]] = None,
        properties: Optional[dict] = None,
        ids: Optional[List[str]] = None,
    ) -> List[Tuple[LabelledNode, Relation, LabelledNode]]:
        return self.get_triplets(
            entity_names=entity_names,
            relation_names=relation_names,
            properties=properties,
            ids=ids,
        )

    async def aget_rel_map(
        self,
        graph_nodes: List[LabelledNode],
        depth: int = 2,
        limit: int = 30,
        ignore_rels: Optional[List[str]] = None,
    ) -> List[Tuple[LabelledNode, Relation, LabelledNode]]:
        return self.get_rel_map(
            graph_nodes=graph_nodes, depth=depth, limit=limit, ignore_rels=ignore_rels
        )

    async def adelete(
        self,
        entity_names: Optional[List[str]] = None,
        relation_names: Optional[List[str]] = None,
        properties: Optional[dict] = None,
        ids: Optional[List[str]] = None,
    ) -> None:
        self.delete(
            entity_names=entity_names,
            relation_names=relation_names,
            properties=properties,
            ids=ids,
        )

    async def astructured_query(
        self,
        query: str,
        param_map: Optional[Dict[str, Any]] = None,
    ) -> Any:
        return self.structured_query(query, param_map=param_map)

    async def avector_query(
        self,
        query: VectorStoreQuery,
        **kwargs: Any,
    ) -> Tuple[List[LabelledNode], List[float]]:
        return self.vector_query(query, **kwargs)

    def __del__(self) -> None:
        try:
            self._db.close()
        except Exception:
            pass
