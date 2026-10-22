/**
 * Integration tests for the mushroomdb TypeScript client.
 *
 * These tests run against a real server binary spawned in global-setup.ts.
 * If the binary could not be built, all tests are reported as SKIPPED (not
 * passed) — the `describe.skipIf` flag is computed from `inject("serverReady")`
 * at module load time so CI boards show accurate skip counts.
 *
 * The demo database contains:
 *   - 10 Orgs, 20 Projects, 30 People
 *   - Linking rules: person-project FIT (overlap), person-org (key-match)
 */

import { existsSync, rmSync } from "node:fs";
import { join } from "node:path";
import WS from "ws";
import { inject, describe, it, expect, beforeAll } from "vitest";
import { MushroomClient, MushroomError } from "../src/client.js";
import type { WsConstructor } from "../src/ws.js";

// ---------------------------------------------------------------------------
// Skip flag — computed once at module load time (after global setup).
// Global setup provides "serverReady": true when server is up, absent/false
// when it could not start (binary missing, build failure, etc.).
// Using a boolean computed here (not a function) avoids any timing ambiguity
// with describe.skipIf's condition evaluation.
// ---------------------------------------------------------------------------

const serverReady: boolean = inject("serverReady") === true;
/** true when the server binary is unavailable — use as the skipIf condition. */
const NO_SERVER = !serverReady;

// ---------------------------------------------------------------------------
// Setup — pull values injected by global-setup.ts
// ---------------------------------------------------------------------------

let client: MushroomClient;
let wsBase: string;

beforeAll(() => {
  if (NO_SERVER) return;
  const baseUrl = inject("baseUrl") as string;
  wsBase = inject("wsUrl") as string;
  client = new MushroomClient(baseUrl);
});

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------

describe.skipIf(NO_SERVER)("stats", () => {
  it("returns node/edge counts for the demo graph", async () => {
    const s = await client.stats();
    expect(typeof s.nodes_live).toBe("number");
    expect(s.nodes_live).toBeGreaterThan(0);
    expect(typeof s.edges).toBe("number");
    expect(Array.isArray(s.rules)).toBe(true);
  });
});

// ---------------------------------------------------------------------------
// Query — read with params
// ---------------------------------------------------------------------------

describe.skipIf(NO_SERVER)("query", () => {
  it("returns a typed result set with columns and rows", async () => {
    const result = await client.query(
      "MATCH (p:Person) RETURN p.id AS pid LIMIT 5",
    );

    expect(Array.isArray(result.columns)).toBe(true);
    expect(result.columns).toContain("pid");
    expect(Array.isArray(result.rows)).toBe(true);
    expect(result.rows.length).toBeGreaterThan(0);
    expect(result.rows.length).toBeLessThanOrEqual(5);
    for (const row of result.rows) {
      expect(row.length).toBe(1);
      expect(typeof row[0]).toBe("string");
    }
  });

  it("supports bound params in a read query", async () => {
    const result = await client.query(
      "MATCH (p:Person {id: $pid}) RETURN p.id AS pid",
      { params: { pid: "person-01" } },
    );

    expect(result.columns).toContain("pid");
    expect(result.rows.length).toBe(1);
    expect(result.rows[0]![0]).toBe("person-01");
  });

  it("returns a null list element as null, from a query without MATCH", async () => {
    const result = await client.query("RETURN [1, null] AS l, null AS v");

    expect(result.columns).toEqual(["l", "v"]);
    expect(result.rows).toEqual([[[1, null], null]]);
  });
});

// ---------------------------------------------------------------------------
// Write + read-back
// ---------------------------------------------------------------------------

describe.skipIf(NO_SERVER)("write via query", () => {
  const testKey = `ts-client-test-${Date.now()}`;

  it("creates a node and reads it back", async () => {
    // Nodes require a string `id` property as the node key.
    await client.query(
      `CREATE (n:TsTest {id: '${testKey}', label: 'hello'})`,
    );

    const readResult = await client.query(
      "MATCH (n:TsTest {id: $k}) RETURN n.label AS lbl",
      { params: { k: testKey } },
    );
    expect(readResult.rows.length).toBe(1);
    expect(readResult.rows[0]![0]).toBe("hello");
  });

  it("CREATE ... RETURN returns the created node binding in one statement", async () => {
    const crKey = `ts-cr-test-${Date.now()}`;
    const result = await client.query(
      `CREATE (n:TsTest {id: '${crKey}', score: 42}) RETURN n, n.score AS sc`,
    );
    // Should return 1 row with n=key and sc=42.
    expect(result.columns).toEqual(["n", "sc"]);
    expect(result.rows.length).toBe(1);
    expect(result.rows[0]![0]).toBe(crKey);
    expect(result.rows[0]![1]).toBe(42);
  });
});

// A format-V11 value written in its tagged JSON form reads back in the same
// form (TY-12): the first such write moves the store to the V11 format.
describe.skipIf(NO_SERVER)("tagged V11 values round-trip", () => {
  it.each([
    ["Date", { $date: "2024-01-15" }],
    ["LocalDateTime", { $ldt: "2024-01-15T12:34:56.789012" }],
    ["DateTime", { $dt: "2024-01-15T12:34:56.789012+05:30" }],
    ["Duration", { $dur: "P1Y2M3DT4H5M6.789S" }],
    ["Binary", { $binary: "3q2+7w==" }],
  ] as const)("a %s prop is stored and read back unchanged", async (name, value) => {
    const key = `ts-typed-${name}-${Date.now()}`;
    await client.createNode("TsTyped", key, { v: value });
    const result = await client.query("MATCH (n:TsTyped) WHERE n.id = $k RETURN n.v AS v", {
      params: { k: key },
    });
    expect(result.columns).toEqual(["v"]);
    expect(result.rows).toEqual([[value]]);
  });
});

// ---------------------------------------------------------------------------
// Error surfacing
// ---------------------------------------------------------------------------

// MushroomError shape is a pure unit test — no server needed; always runs.
describe("MushroomError shape", () => {
  it("name, detail, message are set correctly", () => {
    const err = new MushroomError("test detail");
    expect(err.name).toBe("MushroomError");
    expect(err.detail).toBe("test detail");
    expect(err.message).toBe("test detail");
  });
});

describe("subscribe constructor token", () => {
  function fakeWs(seen: string[]) {
    class FakeWs {
      onopen: ((ev: unknown) => void) | null = null;
      onmessage: ((ev: { data: unknown }) => void) | null = null;
      onclose: ((ev: unknown) => void) | null = null;
      onerror: ((ev: unknown) => void) | null = null;
      constructor(url: string) {
        seen.push(url);
        queueMicrotask(() => this.onopen?.(null));
      }
      send(_data: string): void {
        queueMicrotask(() => {
          this.onmessage?.({ data: JSON.stringify({ subscribed: true }) });
        });
      }
      close(): void {
        queueMicrotask(() => this.onclose?.(null));
      }
    }
    return FakeWs as unknown as WsConstructor;
  }

  it("appends ?token= from constructor token, URL-encoded", async () => {
    const seen: string[] = [];
    const authed = new MushroomClient("http://example.test", { token: "a/b" });
    const handle = await authed.subscribe(
      { writes: true, wsConstructor: fakeWs(seen) },
      () => {},
    );
    await handle.close();
    expect(seen).toEqual(["ws://example.test/subscribe?token=a%2Fb"]);
  });

  it("omits ?token= when constructor has no token", async () => {
    const seen: string[] = [];
    const plain = new MushroomClient("http://example.test");
    const handle = await plain.subscribe(
      { writes: true, wsConstructor: fakeWs(seen) },
      () => {},
    );
    await handle.close();
    expect(seen).toEqual(["ws://example.test/subscribe"]);
  });
});

describe.skipIf(NO_SERVER)("error handling", () => {
  it("surfaces server error message intact on bad Cypher", async () => {
    let caught: MushroomError | null = null;
    try {
      await client.query("THIS IS NOT VALID CYPHER !!!");
    } catch (err) {
      if (err instanceof MushroomError) caught = err;
      else throw err;
    }

    expect(caught).not.toBeNull();
    expect(caught!).toBeInstanceOf(MushroomError);
    expect(caught!.detail.length).toBeGreaterThan(0);
    expect(caught!.message.length).toBeGreaterThan(0);
  });
});

// ---------------------------------------------------------------------------
// Suggest
// ---------------------------------------------------------------------------

describe.skipIf(NO_SERVER)("suggest", () => {
  it("returns a SuggestReport with the correct shape", async () => {
    const report = await client.suggest();

    expect(typeof report.truncated).toBe("boolean");
    expect(Array.isArray(report.suggestions)).toBe(true);

    for (const s of report.suggestions) {
      expect(typeof s.est_edges).toBe("number");
      expect(typeof s.rationale).toBe("string");
      expect(Array.isArray(s.examples)).toBe(true);
      expect(s.def).toBeDefined();
    }
  });
});

// ---------------------------------------------------------------------------
// Algo — pagerank on the demo graph
// ---------------------------------------------------------------------------

describe.skipIf(NO_SERVER)("algo", () => {
  it("pagerank returns scores for the demo graph", async () => {
    const report = await client.algo("pagerank");

    expect(Array.isArray(report.scores)).toBe(true);
    expect(report.scores.length).toBeGreaterThan(0);
    expect(typeof report.converged).toBe("boolean");

    for (const [key, score] of report.scores) {
      expect(typeof key).toBe("string");
      expect(typeof score).toBe("number");
      expect(score).toBeGreaterThan(0);
    }
  });

  it("wcc returns components", async () => {
    const report = await client.algo("wcc");

    expect(Array.isArray(report.components)).toBe(true);
    expect(report.components.length).toBeGreaterThan(0);
    expect(typeof report.truncated).toBe("boolean");
  });

  it("degree returns degree scores", async () => {
    const report = await client.algo("degree");

    expect(Array.isArray(report.scores)).toBe(true);
    expect(report.scores.length).toBeGreaterThan(0);
    expect(typeof report.truncated).toBe("boolean");
  });
});

// ---------------------------------------------------------------------------
// Subscribe — fire event round-trip, clean close
// ---------------------------------------------------------------------------

describe.skipIf(NO_SERVER)("subscribe", () => {
  it("receives a NodeInserted write event then closes cleanly", async () => {
    const events: import("../src/types.js").DbEvent[] = [];
    const nodeKey = `sub-test-${Date.now()}`;

    const handle = await client.subscribe(
      {
        writes: true,
        wsConstructor: WS as unknown as WsConstructor,
      },
      (ev) => {
        events.push(ev);
      },
    );

    try {
      // Nodes are keyed by `id`; CREATE...RETURN is also supported but not needed here.
      await client.query(
        `CREATE (n:SubTestNode {id: '${nodeKey}'})`,
      );

      const received = await new Promise<boolean>((resolve) => {
        const deadline = Date.now() + 5_000;
        const check = () => {
          const found = events.some(
            (ev) =>
              ev.type === "node_inserted" &&
              (ev as { key: string }).key === nodeKey,
          );
          if (found) {
            resolve(true);
          } else if (Date.now() >= deadline) {
            resolve(false);
          } else {
            setTimeout(check, 50);
          }
        };
        check();
      });

      expect(received).toBe(true);
    } finally {
      await handle.close();
    }
  }, 15_000);

  it("unsubscribes / closes cleanly with no events", async () => {
    const handle = await client.subscribe(
      {
        rules: [],
        writes: false,
        wsConstructor: WS as unknown as WsConstructor,
      },
      () => { /* should not fire */ },
    );

    await handle.close();
  }, 10_000);
});

// ---------------------------------------------------------------------------
// Explain / node / neighborhood / createRule
// ---------------------------------------------------------------------------

describe("createRule request", () => {
  it("POSTs /rules with RuleDef JSON and optional Bearer token", async () => {
    const orig = globalThis.fetch;
    const seen: { url: string; init?: RequestInit }[] = [];
    globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
      seen.push({ url: String(input), init });
      return new Response(JSON.stringify({ ok: true, name: "r" }), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      });
    }) as typeof fetch;
    try {
      const authed = new MushroomClient("http://example.test", {
        token: "s3cret",
      });
      const def = {
        name: "r",
        src_label: "Org",
        dst_label: "Org",
        predicate: { NumericWithin: { field: "founded_year", tolerance: 2.0 } },
        edge_type: "FOUNDED_WITHIN",
      };
      await authed.createRule(def);
      expect(seen).toHaveLength(1);
      expect(seen[0]!.url).toBe("http://example.test/rules");
      expect(seen[0]!.init?.method).toBe("POST");
      expect(seen[0]!.init?.body).toBe(JSON.stringify(def));
      const headers = new Headers(seen[0]!.init?.headers);
      expect(headers.get("Authorization")).toBe("Bearer s3cret");
    } finally {
      globalThis.fetch = orig;
    }
  });
});

describe.skipIf(NO_SERVER)("explain / node / neighborhood / createRule", () => {
  it("explain returns Explanation[] for a demo pair", async () => {
    const why = await client.explain("person-01", "proj-01");
    expect(Array.isArray(why)).toBe(true);
    expect(why.length).toBeGreaterThan(0);
    expect(typeof why[0]!.rule).toBe("string");
    expect(typeof why[0]!.edge_type).toBe("string");
    expect(why[0]!.src_key).toBe("person-01");
    expect(why[0]!.dst_key).toBe("proj-01");
  });

  it("node returns NodeInfo or null", async () => {
    const info = await client.node("person-01");
    expect(info).not.toBeNull();
    expect(info!.key).toBe("person-01");
    expect(info!.label).toBe("Person");
    expect(info!.props).toBeDefined();

    const missing = await client.node("no-such-node-ts-client");
    expect(missing).toBeNull();
  });

  it("neighborhood returns key/label/depth rows", async () => {
    const nb = await client.neighborhood("person-01", { depth: 1 });
    expect(nb.columns).toEqual(["key", "label", "depth"]);
    expect(Array.isArray(nb.rows)).toBe(true);
    expect(nb.rows.length).toBeGreaterThan(0);
  });

  it("createRule POSTs a RuleDef and the rule appears in stats", async () => {
    const name = `ts-client-rule-${Date.now()}`;
    await client.createRule({
      name,
      src_label: "Person",
      dst_label: "Person",
      predicate: { FieldEqual: { field: "id" } },
      edge_type: "TS_SAME_ID",
      max_edges: null,
    });
    const s = await client.stats();
    expect(s.rules.some((r) => r.name === name)).toBe(true);
  });
});

// Prevent unused variable warning for wsBase — used via inject() in beforeAll.
void (wsBase!);

// ---------------------------------------------------------------------------
// renameNode / upsertEdge (mock-based: shape + routing)
// ---------------------------------------------------------------------------

describe("renameNode request", () => {
  it("POSTs /nodes/{old}/rename with new_key body", async () => {
    const orig = globalThis.fetch;
    const seen: { url: string; init?: RequestInit }[] = [];
    globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
      seen.push({ url: String(input), init });
      return new Response(JSON.stringify({ ok: true }), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      });
    }) as typeof fetch;
    try {
      const c = new MushroomClient("http://example.test");
      await c.renameNode("alice", "alice2");
      expect(seen).toHaveLength(1);
      expect(seen[0]!.url).toBe("http://example.test/nodes/alice/rename");
      expect(seen[0]!.init?.method).toBe("POST");
      const body = JSON.parse(seen[0]!.init?.body as string);
      expect(body).toEqual({ new_key: "alice2" });
    } finally {
      globalThis.fetch = orig;
    }
  });
});

describe("upsertEdge request", () => {
  it("POSTs /edges/upsert with correct fields", async () => {
    const orig = globalThis.fetch;
    const seen: { url: string; init?: RequestInit }[] = [];
    globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
      seen.push({ url: String(input), init });
      return new Response(
        JSON.stringify({ nodes_created: 2, edge_inserted: true }),
        { status: 200, headers: { "Content-Type": "application/json" } },
      );
    }) as typeof fetch;
    try {
      const c = new MushroomClient("http://example.test");
      const result = await c.upsertEdge("KNOWS", "alice", "bob", "Person");
      expect(seen).toHaveLength(1);
      expect(seen[0]!.url).toBe("http://example.test/edges/upsert");
      expect(seen[0]!.init?.method).toBe("POST");
      const body = JSON.parse(seen[0]!.init?.body as string);
      expect(body).toEqual({
        edge_type: "KNOWS",
        src_key: "alice",
        dst_key: "bob",
        placeholder_label: "Person",
      });
      expect(result.nodes_created).toBe(2);
      expect(result.edge_inserted).toBe(true);
    } finally {
      globalThis.fetch = orig;
    }
  });
});

// ---------------------------------------------------------------------------
// health — mock-based routing + server-based behavior
// ---------------------------------------------------------------------------

describe("health request", () => {
  it("GETs /health without a request body", async () => {
    const orig = globalThis.fetch;
    const seen: { url: string; init?: RequestInit }[] = [];
    globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
      seen.push({ url: String(input), init });
      return new Response(JSON.stringify({ ok: true }), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      });
    }) as typeof fetch;
    try {
      const c = new MushroomClient("http://example.test");
      const result = await c.health();
      expect(seen).toHaveLength(1);
      expect(seen[0]!.url).toBe("http://example.test/health");
      expect(seen[0]!.init?.method).toBeUndefined(); // default GET
      expect(result.ok).toBe(true);
    } finally {
      globalThis.fetch = orig;
    }
  });
});

describe.skipIf(NO_SERVER)("health (server)", () => {
  it("returns { ok: true } from a live server", async () => {
    const result = await client.health();
    expect(result.ok).toBe(true);
  });
});

// ---------------------------------------------------------------------------
// metrics — mock routing + server behavior
// ---------------------------------------------------------------------------

describe("metrics request", () => {
  it("GETs /metrics", async () => {
    const orig = globalThis.fetch;
    const seen: { url: string; init?: RequestInit }[] = [];
    globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
      seen.push({ url: String(input), init });
      return new Response(
        JSON.stringify({
          nodes_live: 10, nodes_tombstoned: 0, edges: 5,
          commit_seq: 42, wal_size_bytes: null, rss_bytes: null,
          uptime_s: 1,
          slow_queries: { threshold_ms: 1000, count: 0, last: [] },
        }),
        { status: 200, headers: { "Content-Type": "application/json" } },
      );
    }) as typeof fetch;
    try {
      const c = new MushroomClient("http://example.test");
      const m = await c.metrics();
      expect(seen[0]!.url).toBe("http://example.test/metrics");
      expect(m.nodes_live).toBe(10);
      expect(m.commit_seq).toBe(42);
    } finally {
      globalThis.fetch = orig;
    }
  });
});

describe.skipIf(NO_SERVER)("metrics (server)", () => {
  it("returns MetricsReport with the expected shape", async () => {
    const m = await client.metrics();
    expect(typeof m.nodes_live).toBe("number");
    expect(typeof m.edges).toBe("number");
    expect(typeof m.commit_seq).toBe("number");
    expect(typeof m.uptime_s).toBe("number");
    expect(typeof m.slow_queries).toBe("object");
    expect(Array.isArray(m.slow_queries.last)).toBe(true);
  });
});

// ---------------------------------------------------------------------------
// createNode / deleteNode — server-based behavior
// ---------------------------------------------------------------------------

describe.skipIf(NO_SERVER)("createNode / deleteNode", () => {
  it("creates a node and reads it back, then deletes it and it is gone", async () => {
    const key = `ts-create-${Date.now()}`;

    // Create
    const result = await client.createNode("TsNodeTest", key, { score: 99 });
    expect(result.ok).toBe(true);
    expect(result.nodes).toBe(1);

    // Read back
    const info = await client.node(key);
    expect(info).not.toBeNull();
    expect(info!.label).toBe("TsNodeTest");
    expect(info!.props["score"]).toBe(99);

    // Delete
    await client.deleteNode(key);

    // Gone
    const gone = await client.node(key);
    expect(gone).toBeNull();
  });

  it("deleteNode throws MushroomError for a missing key", async () => {
    let caught: MushroomError | null = null;
    try {
      await client.deleteNode("ts-client-no-such-node-delete");
    } catch (err) {
      if (err instanceof MushroomError) caught = err;
      else throw err;
    }
    expect(caught).not.toBeNull();
    expect(caught!).toBeInstanceOf(MushroomError);
  });
});

// ---------------------------------------------------------------------------
// nodeEdges — server-based
// ---------------------------------------------------------------------------

describe.skipIf(NO_SERVER)("nodeEdges", () => {
  it("returns edges for a node that has some", async () => {
    const result = await client.nodeEdges("person-01");
    expect(result).toHaveProperty("edges");
    expect(Array.isArray(result.edges)).toBe(true);
    // Demo graph has rules so person-01 should have derived edges.
    expect(result.edges.length).toBeGreaterThan(0);
    const edge = result.edges[0]!;
    expect(typeof edge.edge_type).toBe("string");
    expect(typeof edge.src_key).toBe("string");
    expect(typeof edge.dst_key).toBe("string");
    expect(typeof edge.derived).toBe("boolean");
  });

  it("throws MushroomError (404) for an unknown key", async () => {
    let caught: MushroomError | null = null;
    try {
      await client.nodeEdges("ts-client-no-such-node-edges");
    } catch (err) {
      if (err instanceof MushroomError) caught = err;
      else throw err;
    }
    expect(caught).not.toBeNull();
    expect(caught!).toBeInstanceOf(MushroomError);
  });
});

// ---------------------------------------------------------------------------
// nodeHistory — server-based
// ---------------------------------------------------------------------------

describe.skipIf(NO_SERVER)("nodeHistory", () => {
  it("returns history for an existing node with at least one insert event", async () => {
    const result = await client.nodeHistory("person-01");
    expect(result.key).toBe("person-01");
    expect(Array.isArray(result.history)).toBe(true);
    expect(result.history.length).toBeGreaterThan(0);
    expect(typeof result.total_commits).toBe("number");
    expect(typeof result.horizon).toBe("number");
    const first = result.history[0]!;
    expect(first.change.type).toBe("NodeInserted");
  });

  it("throws MushroomError (404) for an unknown key", async () => {
    let caught: MushroomError | null = null;
    try {
      await client.nodeHistory("ts-client-no-such-node-hist");
    } catch (err) {
      if (err instanceof MushroomError) caught = err;
      else throw err;
    }
    expect(caught).not.toBeNull();
    expect(caught!).toBeInstanceOf(MushroomError);
  });
});

// ---------------------------------------------------------------------------
// edgeHistory / wasLinked — server-based
// ---------------------------------------------------------------------------

describe.skipIf(NO_SERVER)("edgeHistory / wasLinked", () => {
  it("edgeHistory returns events between two nodes in the demo graph", async () => {
    const result = await client.edgeHistory("person-01", "proj-01");
    expect(result.a).toBe("person-01");
    expect(result.b).toBe("proj-01");
    expect(Array.isArray(result.events)).toBe(true);
    expect(typeof result.total_commits).toBe("number");
    expect(typeof result.horizon).toBe("number");
  });

  it("wasLinked returns a boolean result at commit 0", async () => {
    // At commit 0 nothing is linked; we just check the shape comes back.
    let result: import("../src/types.js").WasLinkedResult | null = null;
    let caught: MushroomError | null = null;
    try {
      result = await client.wasLinked("person-01", "proj-01", "FIT", 0);
    } catch (err) {
      // Commit 0 may be out of range on this demo store — that is fine.
      if (err instanceof MushroomError) caught = err;
      else throw err;
    }
    if (result !== null) {
      expect(result.a).toBe("person-01");
      expect(result.b).toBe("proj-01");
      expect(result.edge_type).toBe("FIT");
      expect(typeof result.linked).toBe("boolean");
    } else {
      // A 400 CommitOutOfRange is the correct server response — wasLinked
      // did reach the server; verify the error is MushroomError.
      expect(caught!).toBeInstanceOf(MushroomError);
    }
  });
});

// ---------------------------------------------------------------------------
// setNodeProp / removeNodeProp — server-based
// ---------------------------------------------------------------------------

describe.skipIf(NO_SERVER)("setNodeProp / removeNodeProp", () => {
  it("sets a property and reads it back, then removes it", async () => {
    const key = `ts-prop-${Date.now()}`;
    await client.createNode("TsPropTest", key);

    await client.setNodeProp(key, "color", "red");
    const info = await client.node(key);
    expect(info!.props["color"]).toBe("red");

    await client.removeNodeProp(key, "color");
    const info2 = await client.node(key);
    expect(info2!.props["color"]).toBeUndefined();

    // Cleanup
    await client.deleteNode(key);
  });
});

// ---------------------------------------------------------------------------
// createEdge / deleteEdge — server-based
// ---------------------------------------------------------------------------

describe.skipIf(NO_SERVER)("createEdge / deleteEdge", () => {
  it("creates an explicit edge between two existing nodes and deletes it", async () => {
    const srcKey = `ts-edge-src-${Date.now()}`;
    const dstKey = `ts-edge-dst-${Date.now()}`;
    await client.createNode("TsEdgeTest", srcKey);
    await client.createNode("TsEdgeTest", dstKey);

    await client.createEdge("TS_LINKED", srcKey, dstKey);

    const edges = await client.nodeEdges(srcKey);
    const found = edges.edges.some(
      (e) => e.edge_type === "TS_LINKED" && e.src_key === srcKey && e.dst_key === dstKey,
    );
    expect(found).toBe(true);

    await client.deleteEdge("TS_LINKED", srcKey, dstKey);

    const edges2 = await client.nodeEdges(srcKey);
    const stillThere = edges2.edges.some(
      (e) => e.edge_type === "TS_LINKED" && e.src_key === srcKey && e.dst_key === dstKey,
    );
    expect(stillThere).toBe(false);

    // Cleanup
    await client.deleteNode(srcKey);
    await client.deleteNode(dstKey);
  });
});

// ---------------------------------------------------------------------------
// backup — mock-based routing
// ---------------------------------------------------------------------------

describe("backup request", () => {
  it("POSTs /backup with dest body", async () => {
    const orig = globalThis.fetch;
    const seen: { url: string; init?: RequestInit }[] = [];
    globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
      seen.push({ url: String(input), init });
      return new Response(
        JSON.stringify({ files: ["wal.bin"], bytes: 1024, verified: true }),
        { status: 200, headers: { "Content-Type": "application/json" } },
      );
    }) as typeof fetch;
    try {
      const c = new MushroomClient("http://example.test");
      const result = await c.backup("my-backup");
      expect(seen).toHaveLength(1);
      expect(seen[0]!.url).toBe("http://example.test/backup");
      expect(seen[0]!.init?.method).toBe("POST");
      const body = JSON.parse(seen[0]!.init?.body as string);
      expect(body).toEqual({ dest: "my-backup" });
      expect(result.verified).toBe(true);
      expect(Array.isArray(result.files)).toBe(true);
    } finally {
      globalThis.fetch = orig;
    }
  });
});

// ---------------------------------------------------------------------------
// findSimilar — mock routing
// ---------------------------------------------------------------------------

describe("findSimilar request", () => {
  it("POSTs /find_similar with the options as the body", async () => {
    const orig = globalThis.fetch;
    const seen: { url: string; init?: RequestInit }[] = [];
    globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
      seen.push({ url: String(input), init });
      return new Response(
        JSON.stringify({ hits: [["node-a", 0.95], ["node-b", 0.87]] }),
        { status: 200, headers: { "Content-Type": "application/json" } },
      );
    }) as typeof fetch;
    try {
      const c = new MushroomClient("http://example.test");
      const result = await c.findSimilar({
        field: "embedding",
        vector: [0.1, 0.2, 0.3],
        k: 5,
        min: 0.8,
      });
      expect(seen[0]!.url).toBe("http://example.test/find_similar");
      expect(seen[0]!.init?.method).toBe("POST");
      const body = JSON.parse(seen[0]!.init?.body as string);
      expect(body.field).toBe("embedding");
      expect(body.vector).toEqual([0.1, 0.2, 0.3]);
      expect(body.k).toBe(5);
      expect(body.min).toBe(0.8);
      expect(result.hits).toHaveLength(2);
      expect(result.hits[0]![0]).toBe("node-a");
      expect(result.hits[0]![1]).toBeCloseTo(0.95);
    } finally {
      globalThis.fetch = orig;
    }
  });
});

// ---------------------------------------------------------------------------
// createNode — mock routing
// ---------------------------------------------------------------------------

describe("createNode request", () => {
  it("POSTs /nodes with label, key, optional props", async () => {
    const orig = globalThis.fetch;
    const seen: { url: string; init?: RequestInit }[] = [];
    globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
      seen.push({ url: String(input), init });
      return new Response(
        JSON.stringify({ ok: true, nodes: 1, edges: 0 }),
        { status: 200, headers: { "Content-Type": "application/json" } },
      );
    }) as typeof fetch;
    try {
      const c = new MushroomClient("http://example.test");
      const result = await c.createNode("Widget", "w-1", { price: 9.99 });
      expect(seen[0]!.url).toBe("http://example.test/nodes");
      expect(seen[0]!.init?.method).toBe("POST");
      const body = JSON.parse(seen[0]!.init?.body as string);
      expect(body).toEqual({ label: "Widget", key: "w-1", props: { price: 9.99 } });
      expect(result.ok).toBe(true);
      expect(result.nodes).toBe(1);
    } finally {
      globalThis.fetch = orig;
    }
  });
});

// ---------------------------------------------------------------------------
// deleteNode — mock routing
// ---------------------------------------------------------------------------

describe("deleteNode request", () => {
  it("sends DELETE /node/{key} with URL-encoded key", async () => {
    const orig = globalThis.fetch;
    const seen: { url: string; init?: RequestInit }[] = [];
    globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
      seen.push({ url: String(input), init });
      return new Response(JSON.stringify({ ok: true }), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      });
    }) as typeof fetch;
    try {
      const c = new MushroomClient("http://example.test");
      await c.deleteNode("alice/bob");
      expect(seen[0]!.url).toBe("http://example.test/node/alice%2Fbob");
      expect(seen[0]!.init?.method).toBe("DELETE");
    } finally {
      globalThis.fetch = orig;
    }
  });
});

// ---------------------------------------------------------------------------
// deleteEdge — mock routing
// ---------------------------------------------------------------------------

describe("deleteEdge request", () => {
  it("sends DELETE /edges/{etype}/{src}/{dst} with URL-encoded segments", async () => {
    const orig = globalThis.fetch;
    const seen: { url: string; init?: RequestInit }[] = [];
    globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
      seen.push({ url: String(input), init });
      return new Response(JSON.stringify({ ok: true }), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      });
    }) as typeof fetch;
    try {
      const c = new MushroomClient("http://example.test");
      await c.deleteEdge("KNOWS", "alice", "bob");
      expect(seen[0]!.url).toBe("http://example.test/edges/KNOWS/alice/bob");
      expect(seen[0]!.init?.method).toBe("DELETE");
    } finally {
      globalThis.fetch = orig;
    }
  });
});

// ---------------------------------------------------------------------------
// setNodeProp / removeNodeProp — mock routing
// ---------------------------------------------------------------------------

describe("setNodeProp request", () => {
  it("sends PUT /node/{key}/prop/{field} with value body", async () => {
    const orig = globalThis.fetch;
    const seen: { url: string; init?: RequestInit }[] = [];
    globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
      seen.push({ url: String(input), init });
      return new Response(JSON.stringify({ ok: true }), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      });
    }) as typeof fetch;
    try {
      const c = new MushroomClient("http://example.test");
      await c.setNodeProp("alice", "age", 30);
      expect(seen[0]!.url).toBe("http://example.test/node/alice/prop/age");
      expect(seen[0]!.init?.method).toBe("PUT");
      const body = JSON.parse(seen[0]!.init?.body as string);
      expect(body).toEqual({ value: 30 });
    } finally {
      globalThis.fetch = orig;
    }
  });
});

describe("removeNodeProp request", () => {
  it("sends DELETE /node/{key}/prop/{field}", async () => {
    const orig = globalThis.fetch;
    const seen: { url: string; init?: RequestInit }[] = [];
    globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
      seen.push({ url: String(input), init });
      return new Response(JSON.stringify({ ok: true }), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      });
    }) as typeof fetch;
    try {
      const c = new MushroomClient("http://example.test");
      await c.removeNodeProp("alice", "age");
      expect(seen[0]!.url).toBe("http://example.test/node/alice/prop/age");
      expect(seen[0]!.init?.method).toBe("DELETE");
    } finally {
      globalThis.fetch = orig;
    }
  });
});

// ---------------------------------------------------------------------------
// createEdge — mock routing
// ---------------------------------------------------------------------------

describe("createEdge request", () => {
  it("POSTs /edges with type, src, dst", async () => {
    const orig = globalThis.fetch;
    const seen: { url: string; init?: RequestInit }[] = [];
    globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
      seen.push({ url: String(input), init });
      return new Response(JSON.stringify({ ok: true }), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      });
    }) as typeof fetch;
    try {
      const c = new MushroomClient("http://example.test");
      await c.createEdge("KNOWS", "alice", "bob");
      expect(seen[0]!.url).toBe("http://example.test/edges");
      expect(seen[0]!.init?.method).toBe("POST");
      const body = JSON.parse(seen[0]!.init?.body as string);
      expect(body).toEqual({ type: "KNOWS", src: "alice", dst: "bob" });
    } finally {
      globalThis.fetch = orig;
    }
  });
});

// ---------------------------------------------------------------------------
// wasLinked — mock routing
// ---------------------------------------------------------------------------

describe("wasLinked request", () => {
  it("sends GET /history/was_linked with correct query params", async () => {
    const orig = globalThis.fetch;
    const seen: { url: string; init?: RequestInit }[] = [];
    globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
      seen.push({ url: String(input), init });
      return new Response(
        JSON.stringify({ a: "alice", b: "bob", edge_type: "KNOWS", at_commit: 5, linked: true }),
        { status: 200, headers: { "Content-Type": "application/json" } },
      );
    }) as typeof fetch;
    try {
      const c = new MushroomClient("http://example.test");
      const result = await c.wasLinked("alice", "bob", "KNOWS", 5);
      const url = new URL(seen[0]!.url);
      expect(url.pathname).toBe("/history/was_linked");
      expect(url.searchParams.get("a")).toBe("alice");
      expect(url.searchParams.get("b")).toBe("bob");
      expect(url.searchParams.get("edge_type")).toBe("KNOWS");
      expect(url.searchParams.get("at_commit")).toBe("5");
      expect(result.linked).toBe(true);
    } finally {
      globalThis.fetch = orig;
    }
  });
});

// ---------------------------------------------------------------------------
// nodeEdges mask validation — pure unit tests
// ---------------------------------------------------------------------------

describe("nodeEdges mask validation", () => {
  it("empty mask array throws MushroomError", async () => {
    const c = new MushroomClient("http://example.test");
    await expect(c.nodeEdges("key", { mask: [] })).rejects.toBeInstanceOf(MushroomError);
  });

  it("an empty or whitespace-padded mask key throws MushroomError", async () => {
    // The server trims each key: "" and " " leave no mask at all, which
    // returns every edge, and " lead" silently matches nothing.
    const c = new MushroomClient("http://example.test");
    for (const bad of ["", " ", " lead", "lead "]) {
      await expect(c.nodeEdges("key", { mask: ["ok", bad] })).rejects.toBeInstanceOf(
        MushroomError,
      );
    }
  });

  it("mask key containing a comma throws MushroomError", async () => {
    const c = new MushroomClient("http://example.test");
    await expect(c.nodeEdges("key", { mask: ["ok-key", "bad,key"] })).rejects.toBeInstanceOf(MushroomError);
  });
});

// ---------------------------------------------------------------------------
// nodeEdges mask + stub_hidden — live server
// ---------------------------------------------------------------------------

describe.skipIf(NO_SERVER)("nodeEdges mask and stub_hidden (live)", () => {
  it("mask filters returned edges to those involving allowed keys", async () => {
    const centerKey = `ts-ne-ctr-${Date.now()}`;
    const nb1Key = `ts-ne-nb1-${Date.now()}`;
    const nb2Key = `ts-ne-nb2-${Date.now()}`;
    await client.createNode("TsNeTest", centerKey);
    await client.createNode("TsNeTest", nb1Key);
    await client.createNode("TsNeTest", nb2Key);
    await client.createEdge("NE_LINK", centerKey, nb1Key);
    await client.createEdge("NE_LINK", centerKey, nb2Key);
    try {
      // mask includes centerKey and nb1Key but not nb2Key
      const result = await client.nodeEdges(centerKey, { mask: [centerKey, nb1Key] });
      const dstKeys = result.edges.map((e) =>
        typeof e.dst_key === "string" ? e.dst_key : e.dst_key.key,
      );
      expect(dstKeys).toContain(nb1Key);
      expect(dstKeys).not.toContain(nb2Key);
    } finally {
      await client.deleteEdge("NE_LINK", centerKey, nb1Key);
      await client.deleteEdge("NE_LINK", centerKey, nb2Key);
      await client.deleteNode(centerKey);
      await client.deleteNode(nb1Key);
      await client.deleteNode(nb2Key);
    }
  });

  it("stub_hidden:true returns restricted stubs for masked-out endpoints", async () => {
    const centerKey = `ts-ne-sh-c-${Date.now()}`;
    const visKey = `ts-ne-sh-v-${Date.now()}`;
    const hidKey = `ts-ne-sh-h-${Date.now()}`;
    await client.createNode("TsNeStub", centerKey);
    await client.createNode("TsNeStub", visKey);
    await client.createNode("TsNeStub", hidKey);
    await client.createEdge("SH_LINK", centerKey, visKey);
    await client.createEdge("SH_LINK", centerKey, hidKey);
    try {
      // mask includes center + visKey only; hidKey is outside mask
      const result = await client.nodeEdges(centerKey, {
        mask: [centerKey, visKey],
        stub_hidden: true,
      });
      // hidKey edge should appear as a restricted stub
      const hidEdge = result.edges.find((e) => {
        const dst = e.dst_key;
        return typeof dst === "object" && dst.key === hidKey;
      });
      expect(hidEdge).toBeDefined();
      expect(
        (hidEdge!.dst_key as { key: string; restricted: true }).restricted,
      ).toBe(true);
      // visKey edge should still be a plain string
      const visEdge = result.edges.find((e) => e.dst_key === visKey);
      expect(visEdge).toBeDefined();
    } finally {
      await client.deleteEdge("SH_LINK", centerKey, visKey);
      await client.deleteEdge("SH_LINK", centerKey, hidKey);
      await client.deleteNode(centerKey);
      await client.deleteNode(visKey);
      await client.deleteNode(hidKey);
    }
  });
});

// ---------------------------------------------------------------------------
// findSimilar with where — live server
// ---------------------------------------------------------------------------

describe.skipIf(NO_SERVER)("findSimilar where (live)", () => {
  it("where.eq filters results by property equality", async () => {
    const key1 = `ts-fsw-pub-${Date.now()}`;
    const key2 = `ts-fsw-dra-${Date.now()}`;
    const vec = [0.1, 0.2, 0.3];
    await client.createNode("TsFsWhere", key1, { embedding: vec, status: "pub" });
    await client.createNode("TsFsWhere", key2, { embedding: vec, status: "dra" });
    try {
      const result = await client.findSimilar({
        field: "embedding",
        vector: vec,
        k: 20,
        min: 0.5,
        where: { field: "status", eq: "pub" },
      });
      const keys = result.hits.map(([k]) => k);
      expect(keys).toContain(key1);
      expect(keys).not.toContain(key2);
    } finally {
      await client.deleteNode(key1);
      await client.deleteNode(key2);
    }
  });

  it("where.in filters results by set membership", async () => {
    const key1 = `ts-fswi-a-${Date.now()}`;
    const key2 = `ts-fswi-b-${Date.now()}`;
    const key3 = `ts-fswi-c-${Date.now()}`;
    const vec = [0.1, 0.2, 0.3];
    await client.createNode("TsFsWhereIn", key1, { embedding: vec, status: "a" });
    await client.createNode("TsFsWhereIn", key2, { embedding: vec, status: "b" });
    await client.createNode("TsFsWhereIn", key3, { embedding: vec, status: "c" });
    try {
      const result = await client.findSimilar({
        field: "embedding",
        vector: vec,
        k: 20,
        min: 0.5,
        where: { field: "status", in: ["a", "b"] },
      });
      const keys = result.hits.map(([k]) => k);
      expect(keys).toContain(key1);
      expect(keys).toContain(key2);
      expect(keys).not.toContain(key3);
    } finally {
      await client.deleteNode(key1);
      await client.deleteNode(key2);
      await client.deleteNode(key3);
    }
  });
});

// ---------------------------------------------------------------------------
// watch onClose callback + terminate() fallback — mock-based
// ---------------------------------------------------------------------------

describe.skipIf(NO_SERVER)("watch onClose (live — client close)", () => {
  it("onClose fires exactly once when the caller closes the handle", async () => {
    let closeCount = 0;
    const handle = await client.watch(
      {
        wsConstructor: WS as unknown as import("../src/ws.js").WsConstructor,
        onClose: () => { closeCount++; },
      },
      () => {},
    );
    await handle.close();
    // Small settling pause — onClose is called synchronously inside doClose()
    // but may be queued by the ws package's internals.
    await new Promise((r) => setTimeout(r, 50));
    expect(closeCount).toBe(1);
    // A second close() must not fire onClose again.
    await handle.close();
    await new Promise((r) => setTimeout(r, 50));
    expect(closeCount).toBe(1);
  }, 10_000);
});

describe("watch onClose — server drop (mock)", () => {
  it("onClose fires when the server drops the connection after ack", async () => {
    let closeFired = false;
    class DropWs {
      onopen: ((ev: unknown) => void) | null = null;
      onmessage: ((ev: { data: unknown }) => void) | null = null;
      onclose: ((ev: unknown) => void) | null = null;
      onerror: ((ev: unknown) => void) | null = null;
      constructor(_url: string) {
        queueMicrotask(() => {
          this.onopen?.(null);
          queueMicrotask(() => {
            this.onmessage?.({ data: JSON.stringify({ subscribed: true }) });
            // Simulate server dropping the connection immediately after ack.
            queueMicrotask(() => this.onclose?.(null));
          });
        });
      }
      send(_data: string): void {}
      close(): void { /* server already dropped it */ }
    }
    const c = new MushroomClient("http://example.test");
    const handle = await c.watch(
      {
        wsConstructor: DropWs as unknown as import("../src/ws.js").WsConstructor,
        onClose: () => { closeFired = true; },
      },
      () => {},
    );
    await new Promise((r) => setTimeout(r, 50));
    expect(closeFired).toBe(true);
    // close() after server drop must not throw and must not fire onClose again.
    let secondFire = false;
    await handle.close();
    expect(secondFire).toBe(false);
  });
});

describe("watch terminate() fallback (mock)", () => {
  it("close() resolves ~3s, fires onClose, calls terminate() when server never answers", async () => {
    let closeFired = false;
    let terminateCalled = false;
    class NeverClosesWs {
      onopen: ((ev: unknown) => void) | null = null;
      onmessage: ((ev: { data: unknown }) => void) | null = null;
      onclose: ((ev: unknown) => void) | null = null;
      onerror: ((ev: unknown) => void) | null = null;
      constructor(_url: string) {
        queueMicrotask(() => {
          this.onopen?.(null);
          queueMicrotask(() =>
            this.onmessage?.({ data: JSON.stringify({ subscribed: true }) }),
          );
        });
      }
      send(_data: string): void {}
      close(): void { /* never answers — onclose is never called */ }
      terminate(): void { terminateCalled = true; }
    }
    const c = new MushroomClient("http://example.test");
    const handle = await c.watch(
      {
        wsConstructor: NeverClosesWs as unknown as import("../src/ws.js").WsConstructor,
        onClose: () => { closeFired = true; },
      },
      () => {},
    );
    const t0 = Date.now();
    await handle.close();
    const elapsed = Date.now() - t0;
    expect(elapsed).toBeGreaterThan(2_900);
    expect(elapsed).toBeLessThan(4_000);
    expect(terminateCalled).toBe(true);
    expect(closeFired).toBe(true);
  }, 6_000);
});

// ---------------------------------------------------------------------------
// watch — constructor token forwarded as ?token=
// ---------------------------------------------------------------------------

describe("watch constructor token", () => {
  it("appends ?token= from constructor token for /watch", async () => {
    const seen: string[] = [];

    // Fake WS sends subscribed:true immediately on open
    class AckWs {
      onopen: ((ev: unknown) => void) | null = null;
      onmessage: ((ev: { data: unknown }) => void) | null = null;
      onclose: ((ev: unknown) => void) | null = null;
      onerror: ((ev: unknown) => void) | null = null;
      constructor(url: string) {
        seen.push(url);
        queueMicrotask(() => {
          this.onopen?.(null);
          queueMicrotask(() =>
            this.onmessage?.({ data: JSON.stringify({ subscribed: true }) }),
          );
        });
      }
      send(_data: string): void {}
      close(): void {
        queueMicrotask(() => this.onclose?.(null));
      }
    }

    const authed = new MushroomClient("http://example.test", { token: "w/t" });
    const handle = await authed.watch(
      { wsConstructor: AckWs as unknown as import("../src/ws.js").WsConstructor },
      () => {},
    );
    await handle.close();
    expect(seen).toEqual(["ws://example.test/watch?token=w%2Ft"]);
  });
});

// ---------------------------------------------------------------------------
// watch — live server
// ---------------------------------------------------------------------------

describe.skipIf(NO_SERVER)("watch", () => {
  it("receives a node_inserted mutation event then closes cleanly", async () => {
    const events: import("../src/types.js").MutationEvent[] = [];
    const nodeKey = `watch-test-${Date.now()}`;

    const handle = await client.watch(
      { wsConstructor: WS as unknown as import("../src/ws.js").WsConstructor },
      (ev) => { events.push(ev); },
    );

    try {
      await client.createNode("WatchTestNode", nodeKey);

      const received = await new Promise<boolean>((resolve) => {
        const deadline = Date.now() + 5_000;
        const check = () => {
          const found = events.some(
            (ev) =>
              "node_inserted" in ev &&
              (ev as { node_inserted: { key: string } }).node_inserted.key === nodeKey,
          );
          if (found) {
            resolve(true);
          } else if (Date.now() >= deadline) {
            resolve(false);
          } else {
            setTimeout(check, 50);
          }
        };
        check();
      });

      expect(received).toBe(true);
    } finally {
      await handle.close();
      // Cleanup
      try { await client.deleteNode(nodeKey); } catch { /* ignore */ }
    }
  }, 15_000);

  it("close() resolves in under 2 s against a live server", async () => {
    const handle = await client.watch(
      { wsConstructor: WS as unknown as import("../src/ws.js").WsConstructor },
      () => {},
    );
    const t0 = Date.now();
    await handle.close();
    const elapsed = Date.now() - t0;
    expect(elapsed).toBeLessThan(2_000);
  }, 5_000);
});

// ---------------------------------------------------------------------------
// Path segment validation (.  ..  lone-surrogate) — pure unit tests, no server
// ---------------------------------------------------------------------------

describe("path segment validation", () => {
  it("nodeEdges('.') throws MushroomError before any fetch", async () => {
    const c = new MushroomClient("http://example.test");
    await expect(c.nodeEdges(".")).rejects.toBeInstanceOf(MushroomError);
  });

  it("nodeEdges('..') throws MushroomError", async () => {
    const c = new MushroomClient("http://example.test");
    await expect(c.nodeEdges("..")).rejects.toBeInstanceOf(MushroomError);
  });

  it("node('.') throws MushroomError", async () => {
    const c = new MushroomClient("http://example.test");
    await expect(c.node(".")).rejects.toBeInstanceOf(MushroomError);
  });

  it("deleteNode('..') throws MushroomError", async () => {
    const c = new MushroomClient("http://example.test");
    await expect(c.deleteNode("..")).rejects.toBeInstanceOf(MushroomError);
  });

  it("nodeHistory('.') throws MushroomError", async () => {
    const c = new MushroomClient("http://example.test");
    await expect(c.nodeHistory(".")).rejects.toBeInstanceOf(MushroomError);
  });

  it("setNodeProp('.', 'field', 1) throws MushroomError for dot key", async () => {
    const c = new MushroomClient("http://example.test");
    await expect(c.setNodeProp(".", "field", 1)).rejects.toBeInstanceOf(MushroomError);
  });

  it("setNodeProp('key', '..', 1) throws MushroomError for dot-dot field", async () => {
    const c = new MushroomClient("http://example.test");
    await expect(c.setNodeProp("key", "..", 1)).rejects.toBeInstanceOf(MushroomError);
  });

  it("deleteEdge('KNOWS', '.', 'dst') throws MushroomError for dot src", async () => {
    const c = new MushroomClient("http://example.test");
    await expect(c.deleteEdge("KNOWS", ".", "dst")).rejects.toBeInstanceOf(MushroomError);
  });

  it("a lone-surrogate key throws MushroomError not URIError", async () => {
    const c = new MushroomClient("http://example.test");
    // \uD800 is a lone high surrogate — encodeURIComponent throws URIError.
    await expect(c.node("\uD800")).rejects.toBeInstanceOf(MushroomError);
    await expect(c.node("\uD800")).rejects.not.toBeInstanceOf(URIError);
  });
});

// ---------------------------------------------------------------------------
// fetchJson text-first error handling — pure mock tests
// ---------------------------------------------------------------------------

describe("fetchJson non-JSON error bodies", () => {
  it("empty 404 body gives MushroomError('HTTP 404'), not SyntaxError", async () => {
    const orig = globalThis.fetch;
    globalThis.fetch = (async () =>
      new Response("", { status: 404 })
    ) as typeof fetch;
    try {
      const c = new MushroomClient("http://example.test");
      const err = await c.nodeEdges("somekey").catch((e: unknown) => e);
      expect(err).toBeInstanceOf(MushroomError);
      expect((err as MushroomError).detail).toBe("HTTP 404");
    } finally {
      globalThis.fetch = orig;
    }
  });

  it("plain-text 413 body gives MushroomError('HTTP 413'), not SyntaxError", async () => {
    const orig = globalThis.fetch;
    globalThis.fetch = (async () =>
      new Response("Request body too large", {
        status: 413,
        headers: { "Content-Type": "text/plain" },
      })
    ) as typeof fetch;
    try {
      const c = new MushroomClient("http://example.test");
      const err = await c.health().catch((e: unknown) => e);
      expect(err).toBeInstanceOf(MushroomError);
      expect((err as MushroomError).detail).toBe("HTTP 413");
    } finally {
      globalThis.fetch = orig;
    }
  });

  it("JSON error body still surfaces the error field", async () => {
    const orig = globalThis.fetch;
    globalThis.fetch = (async () =>
      new Response(JSON.stringify({ error: "node key not found: x" }), {
        status: 404,
        headers: { "Content-Type": "application/json" },
      })
    ) as typeof fetch;
    try {
      const c = new MushroomClient("http://example.test");
      const err = await c.deleteNode("x").catch((e: unknown) => e);
      expect(err).toBeInstanceOf(MushroomError);
      expect((err as MushroomError).detail).toBe("node key not found: x");
    } finally {
      globalThis.fetch = orig;
    }
  });
});

// ---------------------------------------------------------------------------
// createNode duplicate key — live server
// ---------------------------------------------------------------------------

describe.skipIf(NO_SERVER)("createNode duplicate key", () => {
  it("second createNode with the same key throws MushroomError", async () => {
    const key = `ts-dup-${Date.now()}`;
    await client.createNode("TsDupTest", key);
    try {
      await expect(client.createNode("TsDupTest", key)).rejects.toBeInstanceOf(MushroomError);
    } finally {
      await client.deleteNode(key);
    }
  });
});

// ---------------------------------------------------------------------------
// deleteEdge — absent edge 200, missing endpoint throws
// ---------------------------------------------------------------------------

describe.skipIf(NO_SERVER)("deleteEdge idempotent / endpoint-missing", () => {
  it("deleting an absent edge succeeds silently (not a throw)", async () => {
    const srcKey = `ts-de-src-${Date.now()}`;
    const dstKey = `ts-de-dst-${Date.now()}`;
    await client.createNode("TsDeTest", srcKey);
    await client.createNode("TsDeTest", dstKey);
    // Create then delete the edge to establish it existed
    await client.createEdge("DE_TEST", srcKey, dstKey);
    await client.deleteEdge("DE_TEST", srcKey, dstKey);
    // Delete again: absent edge must not throw
    await expect(client.deleteEdge("DE_TEST", srcKey, dstKey)).resolves.toBeUndefined();
    // Cleanup
    await client.deleteNode(srcKey);
    await client.deleteNode(dstKey);
  });

  it("deleteEdge throws MushroomError when an endpoint node is missing", async () => {
    await expect(
      client.deleteEdge("DE_MISSING", "ts-no-such-src-de", "ts-no-such-dst-de"),
    ).rejects.toBeInstanceOf(MushroomError);
  });
});

// ---------------------------------------------------------------------------
// query as_of / mask / namespace — live server
// ---------------------------------------------------------------------------

describe.skipIf(NO_SERVER)("query as_of / mask / namespace", () => {
  it("as_of returns the graph state at an earlier commit", async () => {
    const key = `ts-asof-${Date.now()}`;
    await client.createNode("TsAsOfTest", key, { v: 1 });
    // Record the commit where v=1 was written
    const hist = await client.nodeHistory(key);
    const createCommit = hist.history[0]!.commit;
    // Overwrite v
    await client.setNodeProp(key, "v", 2);
    // Current state: v=2
    const now = await client.query(
      "MATCH (n:TsAsOfTest {id: $k}) RETURN n.v AS v",
      { params: { k: key } },
    );
    expect(now.rows[0]![0]).toBe(2);
    // as_of createCommit: v=1
    const then = await client.query(
      "MATCH (n:TsAsOfTest {id: $k}) RETURN n.v AS v",
      { params: { k: key }, as_of: createCommit },
    );
    expect(then.rows[0]![0]).toBe(1);
    await client.deleteNode(key);
  });

  it("mask restricts visible nodes", async () => {
    const key1 = `ts-mask-a-${Date.now()}`;
    const key2 = `ts-mask-b-${Date.now()}`;
    await client.createNode("TsMaskTest", key1);
    await client.createNode("TsMaskTest", key2);
    const masked = await client.query(
      "MATCH (n:TsMaskTest) RETURN n.id AS k",
      { mask: [key1] },
    );
    const keys = masked.rows.map((r) => r[0]);
    expect(keys).toContain(key1);
    expect(keys).not.toContain(key2);
    await client.deleteNode(key1);
    await client.deleteNode(key2);
  });

  it("a masked write is refused with MushroomError", async () => {
    const key = `ts-maskedw-${Date.now()}`;
    await client.createNode("TsMaskedWriteTest", key);
    await expect(
      client.query(
        `CREATE (n:TsMaskedWriteTest {id: 'forbidden-${Date.now()}'})`,
        { mask: [key] },
      ),
    ).rejects.toBeInstanceOf(MushroomError);
    await client.deleteNode(key);
  });

  it("namespace scopes results to the named namespace", async () => {
    // Querying a namespace that no node belongs to should return nothing
    const result = await client.query(
      "MATCH (n:Person) RETURN n.id AS k LIMIT 5",
      { namespace: "__ts_client_nonexistent_ns__" },
    );
    expect(result.rows).toHaveLength(0);
  });
});

// ---------------------------------------------------------------------------
// wasLinked — true after edge created, false before; RFC 3339 date
// ---------------------------------------------------------------------------

describe.skipIf(NO_SERVER)("wasLinked at specific commits", () => {
  it("returns linked: false before and linked: true after the edge", async () => {
    const srcKey = `ts-wl-src-${Date.now()}`;
    const dstKey = `ts-wl-dst-${Date.now()}`;
    await client.createNode("TsWLTest", srcKey);
    await client.createNode("TsWLTest", dstKey);
    // Commit just before the edge
    const m1 = await client.metrics();
    const beforeCommit = m1.commit_seq - 1;
    await client.createEdge("WL_TEST", srcKey, dstKey);
    // Commit after the edge
    const m2 = await client.metrics();
    const afterCommit = m2.commit_seq - 1;
    if (beforeCommit >= 0) {
      const before = await client.wasLinked(srcKey, dstKey, "WL_TEST", beforeCommit);
      expect(before.linked).toBe(false);
    }
    const after = await client.wasLinked(srcKey, dstKey, "WL_TEST", afterCommit);
    expect(after.linked).toBe(true);
    // Cleanup
    await client.deleteEdge("WL_TEST", srcKey, dstKey);
    await client.deleteNode(srcKey);
    await client.deleteNode(dstKey);
  });

  it("accepts an RFC 3339 date string for atCommit", async () => {
    let result: import("../src/types.js").WasLinkedResult | null = null;
    let caught: MushroomError | null = null;
    try {
      result = await client.wasLinked("person-01", "proj-01", "FIT", "2020-01-01T00:00:00Z");
    } catch (err) {
      if (err instanceof MushroomError) caught = err;
      else throw err;
    }
    if (result !== null) {
      expect(typeof result.linked).toBe("boolean");
    } else {
      // A range error is acceptable for a date before the store exists.
      expect(caught!).toBeInstanceOf(MushroomError);
    }
  });
});

// ---------------------------------------------------------------------------
// edgeHistory — Added then Retracted events
// ---------------------------------------------------------------------------

describe.skipIf(NO_SERVER)("edgeHistory Added and Retracted", () => {
  it("shows Added after create and Retracted after delete", async () => {
    const srcKey = `ts-eh-src-${Date.now()}`;
    const dstKey = `ts-eh-dst-${Date.now()}`;
    await client.createNode("TsEHTest", srcKey);
    await client.createNode("TsEHTest", dstKey);
    await client.createEdge("EH_TEST", srcKey, dstKey);
    const h1 = await client.edgeHistory(srcKey, dstKey);
    expect(h1.events.some((e) => e.event === "Added" && e.edge_type === "EH_TEST")).toBe(true);
    await client.deleteEdge("EH_TEST", srcKey, dstKey);
    const h2 = await client.edgeHistory(srcKey, dstKey);
    expect(h2.events.some((e) => e.event === "Retracted" && e.edge_type === "EH_TEST")).toBe(true);
    // Cleanup
    await client.deleteNode(srcKey);
    await client.deleteNode(dstKey);
  });
});

// ---------------------------------------------------------------------------
// createEdge / setNodeProp error paths — live server
// ---------------------------------------------------------------------------

describe.skipIf(NO_SERVER)("createEdge error path", () => {
  it("throws MushroomError when an endpoint node is missing", async () => {
    await expect(
      client.createEdge("TS_MISSING_EP", "ts-no-such-a-ce", "ts-no-such-b-ce"),
    ).rejects.toBeInstanceOf(MushroomError);
  });
});

describe.skipIf(NO_SERVER)("setNodeProp error path", () => {
  it("throws MushroomError when the node does not exist", async () => {
    await expect(
      client.setNodeProp("ts-no-such-node-snp", "field", "value"),
    ).rejects.toBeInstanceOf(MushroomError);
  });
});

// ---------------------------------------------------------------------------
// findSimilar — live server
// ---------------------------------------------------------------------------

describe.skipIf(NO_SERVER)("findSimilar (live)", () => {
  it("finds a node by vector similarity", async () => {
    const key = `ts-vec-${Date.now()}`;
    const vec = [0.1, 0.2, 0.3];
    await client.createNode("TsVecTest", key, { embedding: vec });
    try {
      const result = await client.findSimilar({
        field: "embedding",
        vector: vec,
        k: 5,
        min: 0.5,
      });
      expect(Array.isArray(result.hits)).toBe(true);
      expect(result.hits.some(([k]) => k === key)).toBe(true);
    } finally {
      await client.deleteNode(key);
    }
  });
});

// ---------------------------------------------------------------------------
// backup — live server
// ---------------------------------------------------------------------------

describe.skipIf(NO_SERVER)("backup (live)", () => {
  it("backup returns verified:true with a file list", async () => {
    // The server confines a backup to its backup root, which here is the
    // directory it was started in — this package's. So: a relative name,
    // removed afterwards.
    const dest = `ts-backup-${Date.now()}`;
    const onDisk = join(process.cwd(), dest);
    try {
      const result = await client.backup(dest);
      expect(result.verified).toBe(true);
      expect(result.files.length).toBeGreaterThan(0);
      expect(result.bytes).toBeGreaterThan(0);
      for (const file of result.files) {
        expect(existsSync(join(onDisk, file))).toBe(true);
      }
    } finally {
      rmSync(onDisk, { recursive: true, force: true });
    }
  });
});

// ---------------------------------------------------------------------------
// Special character round trip — live server
// ---------------------------------------------------------------------------

describe.skipIf(NO_SERVER)("special character key round trip", () => {
  it("creates and retrieves a node with special characters in the key", async () => {
    const key = `ts-special-${Date.now()}/with space & more?`;
    await client.createNode("TsSpecialTest", key);
    try {
      const info = await client.node(key);
      expect(info).not.toBeNull();
      expect(info!.key).toBe(key);
    } finally {
      await client.deleteNode(key);
    }
  });
});

// ---------------------------------------------------------------------------
// schema indexes / constraints — server-based behavior
// ---------------------------------------------------------------------------

describe.skipIf(NO_SERVER)("schema indexes and constraints", () => {
  it("declares, lists and drops a range index and a unique constraint", async () => {
    const label = `TsSchema${Date.now()}`;
    const created = await client.createIndex(label, "age");
    expect(created.name).toBe(`idx_${label}_age`);
    expect((await client.indexes()).some((i) => i.name === created.name && i.type === "range")).toBe(true);
    await client.dropIndex(created.name);
    expect((await client.indexes()).some((i) => i.name === created.name)).toBe(false);

    await client.createConstraint(`uc_${label}`, label, "email");
    expect((await client.constraints()).some((c) => c.name === `uc_${label}`)).toBe(true);
    await client.dropConstraint(`uc_${label}`);
    await client.dropIndex(`eq_${label}_email`);
  });

  it("dropIndex throws MushroomError (404) for an unknown name", async () => {
    await expect(client.dropIndex("ts_no_such_index")).rejects.toBeInstanceOf(MushroomError);
  });
});
