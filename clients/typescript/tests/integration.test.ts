/**
 * Integration tests for the mushroomdb TypeScript client.
 *
 * These tests run against a real server binary spawned in global-setup.ts.
 * If the binary could not be built, all tests are skipped with an explanatory
 * message.
 *
 * The demo database contains:
 *   - 10 Orgs, 20 Projects, 30 People
 *   - Linking rules: person-project FIT (overlap), person-org (key-match)
 */

import WS from "ws";
import { inject, describe, it, expect, beforeAll } from "vitest";
import { MushroomClient, MushroomError } from "../src/client.js";
import type { WsConstructor } from "../src/ws.js";

// ---------------------------------------------------------------------------
// Setup — pull values injected by global-setup.ts
// ---------------------------------------------------------------------------

let client: MushroomClient;
let wsBase: string;

beforeAll(() => {
  const skipReason = inject("skipReason") as string;
  if (skipReason) {
    return;
  }
  const baseUrl = inject("baseUrl") as string;
  wsBase = inject("wsUrl") as string;
  client = new MushroomClient(baseUrl);
});

/** Skip helper: returns true and logs when the binary is unavailable. */
function shouldSkip(): boolean {
  const reason = inject("skipReason") as string;
  if (reason) {
    console.warn(`[SKIP] mushroomdb binary unavailable: ${reason}`);
    return true;
  }
  return false;
}

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------

describe("stats", () => {
  it("returns node/edge counts for the demo graph", async () => {
    if (shouldSkip()) return;

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

describe("query", () => {
  it("returns a typed result set with columns and rows", async () => {
    if (shouldSkip()) return;

    const result = await client.query(
      "MATCH (p:Person) RETURN p.id AS pid LIMIT 5",
    );

    expect(Array.isArray(result.columns)).toBe(true);
    expect(result.columns).toContain("pid");
    expect(Array.isArray(result.rows)).toBe(true);
    expect(result.rows.length).toBeGreaterThan(0);
    expect(result.rows.length).toBeLessThanOrEqual(5);
    // Each row should have one cell for "pid"
    for (const row of result.rows) {
      expect(row.length).toBe(1);
      expect(typeof row[0]).toBe("string");
    }
  });

  it("supports bound params in a read query", async () => {
    if (shouldSkip()) return;

    const result = await client.query(
      "MATCH (p:Person {id: $pid}) RETURN p.id AS pid",
      { params: { pid: "person-01" } },
    );

    expect(result.columns).toContain("pid");
    expect(result.rows.length).toBe(1);
    expect(result.rows[0]![0]).toBe("person-01");
  });
});

// ---------------------------------------------------------------------------
// Write + read-back
// ---------------------------------------------------------------------------

describe("write via query", () => {
  const testKey = `ts-client-test-${Date.now()}`;

  it("creates a node and reads it back", async () => {
    if (shouldSkip()) return;

    // Write — CREATE without RETURN (the parser does not support CREATE+RETURN).
    // The server requires a string 'id' property as the node key.
    await client.query(
      `CREATE (n:TsTest {id: '${testKey}', label: 'hello'})`,
    );

    // Read back
    const readResult = await client.query(
      "MATCH (n:TsTest {id: $k}) RETURN n.label AS lbl",
      { params: { k: testKey } },
    );
    expect(readResult.rows.length).toBe(1);
    expect(readResult.rows[0]![0]).toBe("hello");
  });
});

// ---------------------------------------------------------------------------
// Error surfacing
// ---------------------------------------------------------------------------

describe("error handling", () => {
  it("surfaces server error message intact on bad Cypher", async () => {
    if (shouldSkip()) return;

    let caught: MushroomError | null = null;
    try {
      await client.query("THIS IS NOT VALID CYPHER !!!");
    } catch (err) {
      if (err instanceof MushroomError) caught = err;
      else throw err;
    }

    expect(caught).not.toBeNull();
    expect(caught!).toBeInstanceOf(MushroomError);
    // Server returns a non-empty error detail string.
    expect(caught!.detail.length).toBeGreaterThan(0);
    // The detail should mention the query error in some way.
    expect(caught!.message.length).toBeGreaterThan(0);
  });

  it("MushroomError.name is MushroomError", () => {
    const err = new MushroomError("test detail");
    expect(err.name).toBe("MushroomError");
    expect(err.detail).toBe("test detail");
    expect(err.message).toBe("test detail");
  });
});

// ---------------------------------------------------------------------------
// Suggest
// ---------------------------------------------------------------------------

describe("suggest", () => {
  it("returns a SuggestReport with the correct shape", async () => {
    if (shouldSkip()) return;

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

describe("algo", () => {
  it("pagerank returns scores for the demo graph", async () => {
    if (shouldSkip()) return;

    const report = await client.algo("pagerank");

    expect(Array.isArray(report.scores)).toBe(true);
    expect(report.scores.length).toBeGreaterThan(0);
    expect(typeof report.converged).toBe("boolean");

    // Each entry: [node_key: string, score: number]
    for (const [key, score] of report.scores) {
      expect(typeof key).toBe("string");
      expect(typeof score).toBe("number");
      expect(score).toBeGreaterThan(0);
    }
  });

  it("wcc returns components", async () => {
    if (shouldSkip()) return;

    const report = await client.algo("wcc");

    expect(Array.isArray(report.components)).toBe(true);
    expect(report.components.length).toBeGreaterThan(0);
    expect(typeof report.truncated).toBe("boolean");
  });

  it("degree returns degree scores", async () => {
    if (shouldSkip()) return;

    const report = await client.algo("degree");

    expect(Array.isArray(report.scores)).toBe(true);
    expect(report.scores.length).toBeGreaterThan(0);
    expect(typeof report.truncated).toBe("boolean");
  });
});

// ---------------------------------------------------------------------------
// Subscribe — fire event round-trip, clean close
// ---------------------------------------------------------------------------

describe("subscribe", () => {
  it("receives a NodeInserted write event then closes cleanly", async () => {
    if (shouldSkip()) return;

    const events: import("../src/types.js").DbEvent[] = [];
    const nodeKey = `sub-test-${Date.now()}`;

    // Open subscription before issuing the write so we don't miss it.
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
      // Issue a write (no RETURN — parser limitation).
      // Server requires a string 'id' property as the node key.
      await client.query(
        `CREATE (n:SubTestNode {id: '${nodeKey}'})`,
      );

      // Wait for the NodeInserted event (up to 5 s).
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
      // Always close — ensures no dangling WebSocket handle.
      await handle.close();
    }
  }, 15_000); // extend timeout for slow CI

  it("unsubscribes / closes cleanly with no events", async () => {
    if (shouldSkip()) return;

    // Open with no subscriptions (empty rules, writes=false).
    const handle = await client.subscribe(
      {
        rules: [],
        writes: false,
        wsConstructor: WS as unknown as WsConstructor,
      },
      () => {
        // should not fire
      },
    );

    // Immediately close — must not hang.
    await handle.close();
  }, 10_000);
});
