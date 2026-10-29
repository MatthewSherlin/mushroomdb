// Transaction API against a mock HTTP server that speaks the server's /tx/*
// routes: begin, run statements, then commit or roll back by transaction id.

import { createServer, type IncomingMessage, type Server } from "node:http";
import type { AddressInfo } from "node:net";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import {
  ConstraintViolationError,
  MushroomClient,
  MushroomError,
  TransactionConflictError,
  TransactionExpiredError,
} from "../src/index";

interface Seen {
  method: string;
  path: string;
  body: unknown;
  auth: string | undefined;
}

type Reply = { status: number; body?: unknown; destroy?: boolean };

let server: Server;
let client: MushroomClient;
let seen: Seen[];
let handler: (req: Seen) => Reply;

async function readBody(req: IncomingMessage): Promise<unknown> {
  const chunks: Buffer[] = [];
  for await (const chunk of req) chunks.push(chunk as Buffer);
  const text = Buffer.concat(chunks).toString("utf8");
  return text ? JSON.parse(text) : null;
}

beforeEach(async () => {
  seen = [];
  server = createServer(async (req, res) => {
    const entry: Seen = {
      method: req.method ?? "",
      path: req.url ?? "",
      body: await readBody(req),
      auth: req.headers.authorization,
    };
    seen.push(entry);
    const reply = handler(entry);
    if (reply.destroy) {
      req.socket.destroy();
      return;
    }
    res.statusCode = reply.status;
    res.setHeader("content-type", "application/json");
    res.end(reply.body === undefined ? "" : JSON.stringify(reply.body));
  });
  await new Promise<void>((ok) => server.listen(0, "127.0.0.1", ok));
  const port = (server.address() as AddressInfo).port;
  client = new MushroomClient(`http://127.0.0.1:${port}`, { token: "t0k" });
});

afterEach(async () => {
  await new Promise<void>((ok) => server.close(() => ok()));
});

/** A tiny in-memory stand-in for the server: buffers CREATEs, applies on commit. */
function statefulServer(): { committed: string[] } {
  const committed: string[] = [];
  let buffer: string[] = [];
  handler = (req) => {
    if (req.path === "/tx/begin") {
      buffer = [];
      return { status: 200, body: { tx_id: "abc123", expires_at: 1700000000 } };
    }
    if (req.path === "/tx/abc123/run") {
      const { cypher, params } = req.body as {
        cypher: string;
        params: Record<string, string>;
      };
      if (cypher.startsWith("CREATE")) {
        buffer.push(params.id);
        return { status: 200, body: { columns: [], rows: [] } };
      }
      return {
        status: 200,
        body: { columns: ["id"], rows: buffer.map((id) => [id]) },
      };
    }
    if (req.path === "/tx/abc123/commit") {
      committed.push(...buffer);
      return { status: 200, body: { bookmark: 42 } };
    }
    if (req.path === "/tx/abc123/rollback") {
      buffer = [];
      return { status: 200, body: {} };
    }
    return { status: 404, body: { error: "not found" } };
  };
  return { committed };
}

describe("beginTransaction", () => {
  it("commits: begin, run, commit returns the bookmark", async () => {
    const store = statefulServer();
    const tx = await client.beginTransaction();
    expect(tx.txId).toBe("abc123");
    await tx.execute("CREATE (:P {id: $id})", { id: "p1" });
    expect(await tx.commit()).toBe(42);
    expect(store.committed).toEqual(["p1"]);
    expect(seen.map((s) => `${s.method} ${s.path}`)).toEqual([
      "POST /tx/begin",
      "POST /tx/abc123/run",
      "POST /tx/abc123/commit",
    ]);
    expect(seen[1].body).toEqual({
      cypher: "CREATE (:P {id: $id})",
      params: { id: "p1" },
    });
    expect(seen.every((s) => s.auth === "Bearer t0k")).toBe(true);
  });

  it("rolls back: nothing is committed", async () => {
    const store = statefulServer();
    const tx = await client.beginTransaction();
    await tx.execute("CREATE (:P {id: $id})", { id: "p1" });
    await tx.rollback();
    expect(store.committed).toEqual([]);
    expect(seen[seen.length - 1].path).toBe("/tx/abc123/rollback");
  });

  it("reads its own writes through execute", async () => {
    statefulServer();
    const tx = await client.beginTransaction();
    await tx.execute("CREATE (:P {id: $id})", { id: "p1" });
    const out = await tx.execute("MATCH (p:P) RETURN p.id AS id");
    expect(out.rows).toEqual([["p1"]]);
  });

  it("refuses use after the transaction is finished", async () => {
    statefulServer();
    const tx = await client.beginTransaction();
    await tx.commit();
    await expect(tx.execute("RETURN 1")).rejects.toBeInstanceOf(MushroomError);
    await expect(tx.commit()).rejects.toBeInstanceOf(MushroomError);
    expect(seen.map((s) => s.path)).toEqual(["/tx/begin", "/tx/abc123/commit"]);
    await tx.rollback(); // a no-op, not an error
    expect(seen.map((s) => s.path)).toEqual(["/tx/begin", "/tx/abc123/commit"]);
  });

  const beginThen = (after: Reply | (() => Reply)) => {
    handler = (req) =>
      req.path === "/tx/begin"
        ? { status: 200, body: { tx_id: "abc123", expires_at: 1 } }
        : typeof after === "function"
          ? after()
          : after;
  };

  it("after a commit network failure the outcome is unknown and the handle refuses use", async () => {
    beginThen({ status: 200, destroy: true });
    const tx = await client.beginTransaction();
    const err = await tx.commit().catch((e: unknown) => e);
    expect(err).toBeInstanceOf(Error);
    expect(err).not.toBeInstanceOf(MushroomError);
    const count = seen.length;
    await expect(tx.commit()).rejects.toThrow(/outcome is unknown/);
    await expect(tx.execute("RETURN 1")).rejects.toThrow(/outcome is unknown/);
    await expect(tx.rollback()).rejects.toThrow(/outcome is unknown/);
    expect(seen.length).toBe(count);
  });

  it("after a rollback network failure a retry is allowed", async () => {
    let calls = 0;
    beginThen(() => (++calls === 1 ? { status: 200, destroy: true } : { status: 200, body: {} }));
    const tx = await client.beginTransaction();
    await expect(tx.rollback()).rejects.toBeInstanceOf(Error);
    expect(tx.isFinished).toBe(false);
    await expect(tx.rollback()).resolves.toBeUndefined();
    expect(tx.isFinished).toBe(true);
  });

  it("a network failure mid-transaction surfaces from execute and leaves the handle open", async () => {
    let calls = 0;
    beginThen(() =>
      ++calls === 1 ? { status: 200, destroy: true } : { status: 200, body: { columns: [], rows: [] } },
    );
    const tx = await client.beginTransaction();
    await expect(tx.execute("RETURN 1")).rejects.toBeInstanceOf(Error);
    await expect(tx.execute("RETURN 1")).resolves.toEqual({ columns: [], rows: [] });
  });

  it("a second commit while one is in flight throws without a request", async () => {
    beginThen({ status: 200, body: { bookmark: 7 } });
    const tx = await client.beginTransaction();
    const first = tx.commit();
    await expect(tx.commit()).rejects.toThrow(/in flight/);
    expect(await first).toBe(7);
    expect(seen.filter((s) => s.path.endsWith("/commit")).length).toBe(1);
  });

  it("a 404 on begin says the server does not support transactions", async () => {
    handler = () => ({ status: 404 });
    const err = await client.beginTransaction().catch((e: unknown) => e);
    expect(err).toBeInstanceOf(MushroomError);
    expect(err).not.toBeInstanceOf(TransactionExpiredError);
    expect((err as MushroomError).detail).toMatch(/does not support transactions/);
  });

  it("maps 409 on commit to TransactionConflictError", async () => {
    handler = (req) =>
      req.path === "/tx/begin"
        ? { status: 200, body: { tx_id: "abc123", expires_at: 1 } }
        : { status: 409, body: { error: "node changed since begin" } };
    const tx = await client.beginTransaction();
    const err = await tx.commit().catch((e: unknown) => e);
    expect(err).toBeInstanceOf(TransactionConflictError);
    expect(err).toBeInstanceOf(MushroomError);
    expect((err as TransactionConflictError).detail).toBe("node changed since begin");
    expect(tx.isFinished).toBe(true);
  });

  it("a busy 409 is retryable: not a conflict, and the handle stays open", async () => {
    let commits = 0;
    handler = (req) => {
      if (req.path === "/tx/begin") return { status: 200, body: { tx_id: "abc123", expires_at: 1 } };
      if (req.path.endsWith("/commit")) {
        commits += 1;
        return commits === 1
          ? { status: 409, body: { error: "transaction busy", code: "busy" } }
          : { status: 200, body: { bookmark: 5 } };
      }
      return { status: 409, body: { error: "transaction busy", code: "busy" } };
    };
    const tx = await client.beginTransaction();
    const runErr = await tx.execute("RETURN 1").catch((e: unknown) => e);
    expect(runErr).toBeInstanceOf(MushroomError);
    expect(runErr).not.toBeInstanceOf(TransactionConflictError);
    expect((runErr as MushroomError).code).toBe("busy");
    const err = await tx.commit().catch((e: unknown) => e);
    expect(err).toBeInstanceOf(MushroomError);
    expect(err).not.toBeInstanceOf(TransactionConflictError);
    expect(tx.isFinished).toBe(false);
    // The same handle commits once the server is free.
    await expect(tx.commit()).resolves.toBe(5);
  });

  it("a 409 with code conflict is a TransactionConflictError", async () => {
    handler = (req) =>
      req.path === "/tx/begin"
        ? { status: 200, body: { tx_id: "abc123", expires_at: 1 } }
        : { status: 409, body: { error: "CAS conflict", code: "conflict" } };
    const tx = await client.beginTransaction();
    const err = await tx.commit().catch((e: unknown) => e);
    expect(err).toBeInstanceOf(TransactionConflictError);
    expect((err as TransactionConflictError).code).toBe("conflict");
    expect(tx.isFinished).toBe(true);
  });

  it("a 409 with code constraint is a ConstraintViolationError, not a conflict, and ends the handle", async () => {
    handler = (req) =>
      req.path === "/tx/begin"
        ? { status: 200, body: { tx_id: "abc123", expires_at: 1 } }
        : {
            status: 409,
            body: { error: "unique constraint uniq_name violated on name", code: "constraint" },
          };
    const tx = await client.beginTransaction();
    const err = await tx.commit().catch((e: unknown) => e);
    expect(err).toBeInstanceOf(ConstraintViolationError);
    expect(err).toBeInstanceOf(MushroomError);
    expect(err).not.toBeInstanceOf(TransactionConflictError);
    expect((err as ConstraintViolationError).code).toBe("constraint");
    expect((err as ConstraintViolationError).status).toBe(409);
    expect(tx.isFinished).toBe(true);
  });

  it("transaction(callback) rethrows a constraint refusal at commit as ConstraintViolationError", async () => {
    handler = (req) => {
      if (req.path === "/tx/begin") return { status: 200, body: { tx_id: "abc123", expires_at: 1 } };
      if (req.path.endsWith("/commit")) {
        return { status: 409, body: { error: "unique constraint", code: "constraint" } };
      }
      return { status: 200, body: { columns: [], rows: [] } };
    };
    const err = await client
      .transaction(async (tx) => {
        await tx.execute("CREATE (:Person {id: 'dup', name: 'taken'})");
      })
      .catch((e: unknown) => e);
    expect(err).toBeInstanceOf(ConstraintViolationError);
    expect(err).not.toBeInstanceOf(TransactionConflictError);
  });

  it("an auto-commit query refused by a constraint is a ConstraintViolationError", async () => {
    handler = () => ({ status: 409, body: { error: "unique constraint", code: "constraint" } });
    const err = await client.query("CREATE (:Person {id: 'dup', name: 'taken'})").catch((e: unknown) => e);
    expect(err).toBeInstanceOf(ConstraintViolationError);
    expect((err as ConstraintViolationError).code).toBe("constraint");
  });

  it("a commit error that ended the transaction on the server finishes the handle", async () => {
    // A 400 from the engine at commit (a namespace or view-owned refusal): the
    // server consumed the transaction, so GET /tx/{id} is 404.
    handler = (req) => {
      if (req.path === "/tx/begin") return { status: 200, body: { tx_id: "abc123", expires_at: 1 } };
      if (req.path.endsWith("/commit")) return { status: 400, body: { error: "namespace refused" } };
      if (req.method === "GET") return { status: 404, body: { error: "unknown transaction" } };
      return { status: 200, body: { columns: [], rows: [] } };
    };
    const tx = await client.beginTransaction();
    const err = await tx.commit().catch((e: unknown) => e);
    expect(err).toBeInstanceOf(MushroomError);
    expect((err as MushroomError).status).toBe(400);
    expect(tx.isFinished).toBe(true);
    expect(seen.some((s) => s.method === "GET" && s.path === "/tx/abc123")).toBe(true);
    await expect(tx.execute("RETURN 1")).rejects.toThrow(/already finished/);
  });

  it("a commit refused before it reached the transaction leaves the handle open", async () => {
    // A 503 from load shedding: the server still holds the transaction.
    let commits = 0;
    handler = (req) => {
      if (req.path === "/tx/begin") return { status: 200, body: { tx_id: "abc123", expires_at: 1 } };
      if (req.path.endsWith("/commit")) {
        commits += 1;
        return commits === 1
          ? { status: 503, body: { error: "server at its concurrency limit; retry" } }
          : { status: 200, body: { bookmark: 7 } };
      }
      if (req.method === "GET") {
        return { status: 200, body: { tx_id: "abc123", expires_at: 1, op_count: 0 } };
      }
      return { status: 200, body: { columns: [], rows: [] } };
    };
    const tx = await client.beginTransaction();
    const err = await tx.commit().catch((e: unknown) => e);
    expect((err as MushroomError).status).toBe(503);
    expect(tx.isFinished).toBe(false);
    await expect(tx.commit()).resolves.toBe(7);
  });

  it("a commit error while the server still runs the commit makes the outcome unknown", async () => {
    // A 408: the 120 s deadline hit but the commit is still running on the
    // server, which answers GET /tx/{id} with 409 busy while a request holds it.
    handler = (req) => {
      if (req.path === "/tx/begin") return { status: 200, body: { tx_id: "abc123", expires_at: 1 } };
      if (req.path.endsWith("/commit")) return { status: 408, body: { error: "request timed out" } };
      if (req.method === "GET" && req.path === "/tx/abc123") {
        return { status: 409, body: { error: "transaction busy", code: "busy" } };
      }
      return { status: 500, body: { error: "unexpected request" } };
    };
    const tx = await client.beginTransaction();
    await tx.commit().catch(() => undefined);
    expect(seen.filter((r) => r.method === "GET").length).toBe(1);
    expect(tx.isFinished).toBe(false);
    await expect(tx.execute("RETURN 1")).rejects.toThrow(/outcome is unknown/);
  });

  it("maps 404 on commit to TransactionExpiredError", async () => {
    handler = (req) =>
      req.path === "/tx/begin"
        ? { status: 200, body: { tx_id: "abc123", expires_at: 1 } }
        : { status: 404, body: { error: "unknown transaction" } };
    const tx = await client.beginTransaction();
    await expect(tx.commit()).rejects.toBeInstanceOf(TransactionExpiredError);
  });

  it("treats rollback of an expired transaction as done", async () => {
    handler = (req) =>
      req.path === "/tx/begin"
        ? { status: 200, body: { tx_id: "abc123", expires_at: 1 } }
        : { status: 410, body: { error: "expired" } };
    const tx = await client.beginTransaction();
    await expect(tx.rollback()).resolves.toBeUndefined();
  });

  it("a statement error is a plain MushroomError and leaves the handle open", async () => {
    handler = (req) =>
      req.path === "/tx/begin"
        ? { status: 200, body: { tx_id: "abc123", expires_at: 1 } }
        : { status: 400, body: { error: "bad cypher" } };
    const tx = await client.beginTransaction();
    const err = await tx.execute("BAD").catch((e: unknown) => e);
    expect(err).toBeInstanceOf(MushroomError);
    expect(err).not.toBeInstanceOf(TransactionConflictError);
    expect(tx.isFinished).toBe(false);
  });
});

describe("transaction(callback)", () => {
  it("commits when the callback resolves and returns its value", async () => {
    const store = statefulServer();
    const value = await client.transaction(async (tx) => {
      await tx.execute("CREATE (:P {id: $id})", { id: "p1" });
      return "done";
    });
    expect(value).toBe("done");
    expect(store.committed).toEqual(["p1"]);
  });

  it("rolls back and rethrows when the callback throws", async () => {
    const store = statefulServer();
    const boom = new Error("boom");
    await expect(
      client.transaction(async (tx) => {
        await tx.execute("CREATE (:P {id: $id})", { id: "p1" });
        throw boom;
      }),
    ).rejects.toBe(boom);
    expect(store.committed).toEqual([]);
    const paths = seen.map((s) => s.path);
    expect(paths).toContain("/tx/abc123/rollback");
    expect(paths).not.toContain("/tx/abc123/commit");
  });

  it("rethrows the callback's error even when the rollback call fails", async () => {
    handler = (req) =>
      req.path === "/tx/begin"
        ? { status: 200, body: { tx_id: "abc123", expires_at: 1 } }
        : { status: 500, body: { error: "rollback broke" } };
    const boom = new Error("boom");
    await expect(
      client.transaction(async () => {
        throw boom;
      }),
    ).rejects.toBe(boom);
  });

  it("attaches a failed rollback (HTTP 500) to the original error as rollbackError", async () => {
    handler = (req) =>
      req.path === "/tx/begin"
        ? { status: 200, body: { tx_id: "abc123", expires_at: 1 } }
        : { status: 500, body: { error: "rollback broke" } };
    const boom = new Error("boom");
    const err = await client
      .transaction(async () => {
        throw boom;
      })
      .catch((e: unknown) => e);
    expect(err).toBe(boom);
    expect((err as { rollbackError?: MushroomError }).rollbackError?.detail).toBe("rollback broke");
  });

  it("attaches a network-level rollback failure to the original error", async () => {
    handler = (req) =>
      req.path === "/tx/begin"
        ? { status: 200, body: { tx_id: "abc123", expires_at: 1 } }
        : { status: 200, destroy: true };
    const boom = new Error("boom");
    const err = await client
      .transaction(async () => {
        throw boom;
      })
      .catch((e: unknown) => e);
    expect(err).toBe(boom);
    expect((err as { rollbackError?: unknown }).rollbackError).toBeInstanceOf(Error);
  });

  it("surfaces a commit conflict as TransactionConflictError", async () => {
    handler = (req) => {
      if (req.path === "/tx/begin") {
        return { status: 200, body: { tx_id: "abc123", expires_at: 1 } };
      }
      if (req.path.endsWith("/commit")) {
        return { status: 409, body: { error: "conflict" } };
      }
      return { status: 200, body: { columns: [], rows: [] } };
    };
    await expect(
      client.transaction(async (tx) => {
        await tx.execute("MATCH (n) SET n.x = 1");
      }),
    ).rejects.toBeInstanceOf(TransactionConflictError);
    expect(seen.map((s) => s.path)).not.toContain("/tx/abc123/rollback");
  });

  it("does not commit again when the callback committed itself", async () => {
    statefulServer();
    await client.transaction(async (tx) => {
      await tx.commit();
    });
    expect(seen.filter((s) => s.path.endsWith("/commit")).length).toBe(1);
  });
});
