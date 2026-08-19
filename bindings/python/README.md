# mushroomdb Python bindings

Embedded bindings over the Rust core. Not an HTTP client.

```python
from mushroomdb import GraphDb

with GraphDb.open("./db") as db:
    db.insert_node("org-01", "Org", {"founded_year": 2010})
    db.create_rule({
        "name": "founded_within",
        "src_label": "Org",
        "dst_label": "Org",
        "predicate": {"NumericWithin": {"field": "founded_year", "tolerance": 2.0}},
        "edge_type": "FOUNDED_WITHIN",
        "weight_prop": "score",
        "max_edges": None,
    })
    print(db.query("MATCH (n:Org) RETURN n"))
```

Local develop (venv in this directory):

```
python3 -m venv .venv
.venv/bin/pip install maturin pytest
.venv/bin/maturin develop
.venv/bin/pytest
```
