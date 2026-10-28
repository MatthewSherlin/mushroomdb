# Docker quickstart

Run the HTTP server in a container, then create and query a two-person graph
from your host. You need Docker and curl. These plain graph operations use the
released image; unreleased features require an image built from that source.

Create a fresh volume and start a container. Explicit `serve` arguments omit
the image's default demo data, so this store starts empty:

```sh no-run
quickstart_volume=$(docker volume create)
quickstart_token=quickstart-local-token
quickstart_container=$(docker run --detach \
  --publish 127.0.0.1:8080:8080 \
  --mount "source=$quickstart_volume,target=/data" \
  --env "MUSHROOMDB_TOKEN=$quickstart_token" \
  ghcr.io/matthewsherlin/mushroomdb:v0.9.2 \
  serve /data --addr 0.0.0.0:8080)
curl --fail --retry 30 --retry-all-errors --retry-delay 1 \
  http://127.0.0.1:8080/health
```

The published host port is loopback-only. The server binds all container
interfaces and receives a token; authenticated requests send that same token.

```sh no-run
curl --fail --silent --show-error \
  -H "Authorization: Bearer $quickstart_token" \
  -H 'Content-Type: application/json' \
  --data-binary @- 'http://127.0.0.1:8080/query?format=json' <<'JSON'
{"cypher":"CREATE (:Person {id: 'alice', name: 'Alice'})-[:KNOWS]->(:Person {id: 'bob', name: 'Bob'})"}
JSON
curl --fail --silent --show-error \
  -H "Authorization: Bearer $quickstart_token" \
  -H 'Content-Type: application/json' \
  --data '{"cypher":"MATCH (a:Person)-[:KNOWS]->(b) RETURN a.name, b.name"}' \
  'http://127.0.0.1:8080/query?format=json'
```

The second response has `columns` equal to `["a.name","b.name"]` and `rows`
equal to `[["Alice","Bob"]]`. The container writes to the retained volume:

```sh no-run
docker stop "$quickstart_container"
printf '%s\n' "$quickstart_volume"
```

Stopping the container preserves the volume. Keep its printed name to mount
the same data next time. Commands address only the container created above.

## What's next

- [Service reference](service.md): HTTP flags, backup and restore.
- [Agent memory](quickstart.md): use the graph as application memory.
- [Deployment](deployment.md) and [self-hosting](self-host.md).
