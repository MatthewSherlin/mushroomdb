# Python quickstart

Create a graph inside Python, with no server process. Use Python 3.10 or later.

```sh no-run
python3 -m venv .venv
.venv/bin/pip install mushroomdb
```

Save this as `quickstart.py`:

```python
from pathlib import Path
from tempfile import mkdtemp
from mushroomdb import GraphDb

store = Path(mkdtemp(prefix="mushroomdb-quickstart-"))
with GraphDb.open(store) as db:
    db.insert_node("Person", "alice", {"name": "Alice"})
    db.insert_node("Person", "bob", {"name": "Bob"})
    db.insert_edge("KNOWS", "alice", "bob")
    rows = db.query("MATCH (a:Person)-[:KNOWS]->(b) RETURN a.name, b.name")
    assert rows == [{"a.name": "Alice", "b.name": "Bob"}]
    print("Alice knows Bob")
print(f"Store retained at {store}")
```

```sh no-run
.venv/bin/python quickstart.py
```

The context manager closes the handle. The fresh store stays at the printed
path; open it again with `GraphDb.open(path)` to continue using it. The query
returns dictionaries keyed by the projected column names.

## What's next

- [Python guide](python.md) and [API reference](python-api.md).
- [Agent memory](quickstart.md): the `db.memory` namespace.
- [Deployment](deployment.md): run a service for several clients.
