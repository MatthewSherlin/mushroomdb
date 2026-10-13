# mushroomdb

**The graph that stays true — and knows who's allowed to see it.**

mushroomdb is an embedded graph database for agents and apps that reason over entities. You
declare a relationship as a rule, once; every write after that derives the matching edges, scores
them, and retracts them when they stop being true.

This package is the launcher: it downloads the native `mushroomdb` binary for your platform from
the project's GitHub Releases and runs it. macOS (Apple Silicon and Intel), Linux x86_64 (glibc and musl), Linux arm64 and Windows x86_64 are built.

```sh
npx mushroomdb install --db ./memory   # give Claude Code, Cursor or Codex a memory
npx mushroomdb demo ./db               # write a small worked graph
npx mushroomdb serve ./db              # the explorer and the HTTP API at 127.0.0.1:8080
```

- Documentation: <https://matthewsherlin.github.io/mushroomdb/docs/>
- Source and issues: <https://github.com/MatthewSherlin/mushroomdb>
- The TypeScript client for the HTTP API is a separate package, `mushroomdb-client`.

Dual-licensed under MIT or Apache-2.0, at your option.
