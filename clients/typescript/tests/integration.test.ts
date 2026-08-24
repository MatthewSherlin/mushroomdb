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

// Prevent unused variable warning for wsBase — used via inject() in beforeAll.
void (wsBase!);
