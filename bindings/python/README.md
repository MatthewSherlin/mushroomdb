# mushroomdb (Python)

Python bindings for [mushroomdb](https://github.com/MatthewSherlin/mushroomdb) —
the embedded graph database where edges are declared, not inserted.

```python
import mushroomdb
db = mushroomdb.GraphDb.open("./db")
db.insert_node("Org", "org-01", {"founded_year": 2010})
```

Full documentation, the rules tour, and benchmarks live in the
[main repository](https://github.com/MatthewSherlin/mushroomdb).
