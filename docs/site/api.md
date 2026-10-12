# API reference

mushroomdb is one engine behind several surfaces. Each has its own page.

| Surface | Reference | Guide |
|---|---|---|
| HTTP, served by `mushroomdb serve` | [HTTP API](http.md) — every route | [Running as a service](service.md) |
| MCP, served by `mushroomdb mcp` | [MCP tools](mcp-tools.md) — every tool and argument | [Agent memory](mcp.md) |
| Python, `pip install mushroomdb` | [Python API](python-api.md) — every method | [Python guide](python.md) |
| Rust, `cargo add mushroomdb` | [docs.rs](https://docs.rs/mushroomdb) — every item | [Rust](rust.md) |
| The `mushroomdb` binary | [CLI](cli.md) — every subcommand | [Quickstart](quickstart.md) |
| TypeScript, `npm install mushroomdb-client` | [TypeScript client](typescript.md) — every method, and what it does not wrap | |

This page used to hold the HTTP and Python references together. Its sections are now in
[HTTP API](http.md), [Python guide](python.md) and, for backup and export from Rust and the
CLI, [Durability](durability.md#backup-and-export-from-rust-and-the-cli).
