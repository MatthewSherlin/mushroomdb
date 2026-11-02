"""An assistant's retrieval step that only ever sees what its user may see.

Two people ask the same question of one graph, in one script:

1. Store documents with an embedding. A rule, declared once, links documents
   that share topics, so nobody writes those edges by hand.
2. Declare two roles. ``support`` sees documents marked ``access: all``;
   ``finance`` sees every document.
3. Answer through a handle scoped to the asker's role: vector search, then a
   walk along the derived edges. Both steps obey the scope, so the app has no
   filtering step to forget.
4. Revoke a document with one write. The next read no longer returns it.

It runs offline with no API keys. The embedding is a deterministic hashed
bag-of-words vector, standing in for an embedding model, and the printed context
is what a prompt would carry.

    pip install mushroomdb
    python permissioned_assistant.py
"""

from __future__ import annotations

import hashlib
import json
import math
import pathlib
import tempfile

from mushroomdb import GraphDb

DIMENSIONS = 64

DOCS = {
    "postmortem": {
        "access": "all",
        "topics": ["acme", "billing"],
        "text": "Postmortem: the billing outage delayed the Acme renewal invoices.",
    },
    "runbook": {
        "access": "all",
        "topics": ["billing", "operations"],
        "text": "Runbook: how to replay failed invoices after a billing outage.",
    },
    "contract-dispute": {
        "access": "restricted",
        "topics": ["acme", "legal"],
        "text": "Legal is disputing the liability cap, so the Acme renewal is at risk.",
    },
    "salary-bands": {
        "access": "restricted",
        "topics": ["compensation"],
        "text": "Salary bands for next year, by level.",
    },
}

ROLES = {
    "version": 4,
    "roles": [
        {
            "name": "support",
            "labels": ["Doc"],
            "keys": [],
            "visible_where": {"field": "access", "eq": "all"},
        },
        {"name": "finance", "labels": ["Doc"], "keys": []},
    ],
}

QUESTION = "Why is the Acme renewal at risk?"


def embed(text: str) -> list[float]:
    """A unit-length hashed bag-of-words vector; the same text always maps to the same vector."""
    vector = [0.0] * DIMENSIONS
    for word in text.lower().replace(".", " ").replace(",", " ").replace("?", " ").split():
        digest = hashlib.sha256(word.encode()).digest()
        vector[int.from_bytes(digest[:4], "big") % DIMENSIONS] += 1.0
    norm = math.sqrt(sum(x * x for x in vector)) or 1.0
    return [x / norm for x in vector]


def build(path: str) -> None:
    with GraphDb.open(path) as db:
        db.create_rule(
            {
                "name": "shared_topic",
                "src_label": "Doc",
                "dst_label": "Doc",
                "predicate": {"kind": "overlap", "fields": ["topics"], "min": 0.3},
                "edge_type": "RELATED",
                "weight_prop": "score",
                "max_edges": 5,
            }
        )
        for key, doc in DOCS.items():
            db.insert_node("Doc", key, {**doc, "embedding": embed(doc["text"])})
    # A store reads its roles when it opens.
    pathlib.Path(path, "roles.json").write_text(json.dumps(ROLES))


def retrieve(handle: GraphDb, question: str, k: int = 2) -> list[str]:
    """The nearest documents, then the documents a rule linked to them."""
    hits = handle.find_similar("embedding", embed(question), label="Doc", k=k, min=0.1, exact=True)
    found = [key for key, _ in hits]
    for key, _ in hits:
        for other in handle.neighbors(key, "RELATED", "out"):
            if other not in found:
                found.append(other)
    return found


def show(role: str, handle: GraphDb) -> None:
    print(f"{role} asks: {QUESTION}")
    for key in retrieve(handle, QUESTION):
        print(f"  [{key}] {handle.node_info(key)['props']['text']}")
    print()


def main() -> None:
    with tempfile.TemporaryDirectory() as directory:
        path = f"{directory}/store"
        build(path)
        with GraphDb.open(path) as db:
            support = db.scoped(role="support")
            finance = db.scoped(role="finance")

            print("One graph, one question, two answers.\n")
            show("support", support)
            show("finance", finance)

            print("Why did retrieval connect these two documents?")
            for row in finance.explain("postmortem", "contract-dispute")[:1]:
                predicate = row["predicate"]
                print(
                    f"  rule {row['rule']}: {predicate['kind']} on {predicate['fields'][0]}, "
                    f"score {row['weight']:.2f}"
                )
            print()

            print("To support, a hidden document does not exist:")
            print(f"  node_info('contract-dispute') -> {support.node_info('contract-dispute')}\n")

            print("Revoke the postmortem with one write, then ask again.\n")
            db.set_prop("postmortem", "access", "restricted")
            show("support", support)


if __name__ == "__main__":
    main()
