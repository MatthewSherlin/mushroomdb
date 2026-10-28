"""Graph retrieval for a language-model prompt, offline, with no API keys.

The pipeline a retrieval-augmented generation app runs, in one script:

1. Take text chunks and "extract" the entities each one mentions. The
   extraction here is a fixed lookup, standing in for a model call.
2. Store chunks and entities as nodes and the mentions as edges.
3. Embed each chunk with a deterministic hashed bag-of-words vector, standing
   in for an embedding model.
4. Answer a question: find the nearest chunks with ``find_similar``, then walk
   the graph from the entities they mention to pull in related chunks that no
   vector would have matched.
5. Print the assembled context a prompt would carry.

It uses the core ``mushroomdb`` Python API only. ``langchain-mushroomdb`` needs
``langchain-core`` from a package index, which would break "runs offline".

    pip install mushroomdb
    python graph_retrieval.py
"""

from __future__ import annotations

import hashlib
import math
import tempfile

from mushroomdb import GraphDb

DIMENSIONS = 64

CHUNKS = {
    "c1": "Ada Lovelace wrote the first program for the Analytical Engine.",
    "c2": "Charles Babbage designed the Analytical Engine, a mechanical computer.",
    "c3": "Babbage and Lovelace corresponded about the machine for years.",
    "c4": "The Difference Engine computed polynomial tables by finite differences.",
    "c5": "Mushrooms grow from a mycelium network in damp, dark soil.",
}

# Stand-in for entity extraction: chunk id -> entities it mentions.
ENTITIES = {
    "c1": ["Ada Lovelace", "Analytical Engine"],
    "c2": ["Charles Babbage", "Analytical Engine"],
    "c3": ["Charles Babbage", "Ada Lovelace"],
    "c4": ["Charles Babbage", "Difference Engine"],
    "c5": [],
}


def embed(text: str) -> list[float]:
    """A unit-length hashed bag-of-words vector; the same text always maps to the same vector."""
    vector = [0.0] * DIMENSIONS
    for word in text.lower().replace(".", " ").replace(",", " ").split():
        digest = hashlib.sha256(word.encode()).digest()
        vector[int.from_bytes(digest[:4], "big") % DIMENSIONS] += 1.0
    norm = math.sqrt(sum(x * x for x in vector)) or 1.0
    return [x / norm for x in vector]


def build(db: GraphDb) -> None:
    for entity in sorted({name for names in ENTITIES.values() for name in names}):
        db.insert_node("Entity", entity, {"name": entity})
    for chunk_id, text in CHUNKS.items():
        db.insert_node("Chunk", chunk_id, {"text": text, "embedding": embed(text)})
        for entity in ENTITIES[chunk_id]:
            db.insert_edge("MENTIONS", chunk_id, entity)


def retrieve(db: GraphDb, question: str, k: int = 2) -> list[tuple[str, str]]:
    """Return (chunk id, why) pairs: nearest chunks first, then chunks one entity away."""
    hits = db.find_similar("embedding", embed(question), label="Chunk", k=k, min=0.1)
    found = [(key, f"vector match, similarity {score:.2f}") for key, score in hits]
    seen = {key for key, _ in found}
    for key, _ in hits:
        rows = db.query(
            "MATCH (c:Chunk {key: $key})-[:MENTIONS]->(e:Entity)<-[:MENTIONS]-(o:Chunk) "
            "RETURN DISTINCT key(o) AS other, key(e) AS via ORDER BY other",
            {"key": key},
        )
        for row in rows:
            if row["other"] not in seen:
                seen.add(row["other"])
                found.append((row["other"], f"shares {row['via']} with {key}"))
    return found


def main() -> None:
    with tempfile.TemporaryDirectory() as directory, GraphDb.open(f"{directory}/store") as db:
        build(db)
        question = "Who wrote a program for the Analytical Engine?"
        print(f"Question: {question}\n")
        for chunk_id, why in retrieve(db, question):
            node = db.node_info(chunk_id)
            print(f"[{chunk_id}] ({why})\n    {node['props']['text']}")


if __name__ == "__main__":
    main()
