# Security policy

## Posture

mushroomdb is local-first by design. The embedded Rust core has no network
stack; it reads and writes a local directory only. When you run
`mushroomdb serve`, the HTTP server binds to `127.0.0.1:8080` by default (not
`0.0.0.0`). Binding to a non-loopback interface requires `--token` or
`MUSHROOMDB_TOKEN`. Loopback binds may omit a token. When a token is
configured, every HTTP request except `GET /health` must present
`Authorization: Bearer <token>`, `?token=`, or `Cookie: mushroomdb_token=`
(WebSocket `/watch` and `/subscribe` take `?token=`). Open the explorer at
`http://host:8080/?token=…` so the UI can attach that token to API fetches
and WebSockets; HTML responses set the cookie so `/assets/*` load. The
process refuses to start if `--addr` is not loopback and no token is set.

The `mushroomdb mcp` mode reads and writes to a local database via stdio
only. It opens no sockets.

## Reporting a vulnerability

Report security issues through GitHub's private advisory channel:

1. Go to the repository on GitHub.
2. Click **Security** > **Advisories** > **Report a vulnerability**.
3. Fill in the description, affected version (or commit), and any
   reproduction steps.

Do not open a public issue for a vulnerability. We will acknowledge reports
within three business days and aim to publish a fix and advisory within 90
days of confirmation.

## Scope

- Memory safety issues in the Rust core (storage, rule engine, server)
- WAL or snapshot parsing that could cause data corruption on malformed input
- Path traversal or unintended file writes via the `--ui` or `--db` arguments
- Denial of service via crafted Cypher queries or ingest payloads

Out of scope: issues that require local write access to the database
directory (the threat model assumes the OS user has that access), or
performance-only regressions without a safety component.

## Supported versions

Pre-v0.1 — no stable release yet. Fixes land on `main`.
