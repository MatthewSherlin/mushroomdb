# mushroomdb-cli

The `mushroomdb` command-line binary: embedded graph database with incremental
association rules, a Cypher subset, hybrid search, and a built-in MCP stdio
server for AI agents.

```sh
cargo install mushroomdb-cli
mushroomdb demo ./db      # seed a demo store
mushroomdb serve ./db     # http api (the explorer ui is in the npx, docker and release builds, not this one)
mushroomdb mcp ./db       # mcp stdio server for agents
```

Full documentation: <https://matthewsherlin.github.io/mushroomdb/docs/>

mcp-name: io.github.MatthewSherlin/mushroomdb
