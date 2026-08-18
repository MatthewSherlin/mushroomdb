import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  ApiClient,
  ApiError,
  isAbsentEndpoint,
  isKeyNotFound,
} from "./api";

const tokens = readFileSync(
  join(dirname(fileURLToPath(import.meta.url)), "tokens.css"),
  "utf8",
);

function jsonResponse(status: number, body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

function textResponse(status: number, body: string): Response {
  return new Response(body, { status });
}

describe("design tokens", () => {
  it("encodes the brief palette verbatim and no other hues", () => {
    expect(tokens).toMatch(/--ink:\s*#0B0E14;/);
    expect(tokens).toMatch(/--paper:\s*#E8E6E1;/);
    expect(tokens).toMatch(/--structure:\s*#55627A;/);
    expect(tokens).toMatch(/--gold:\s*#E8A33D;/);
    expect(tokens).toMatch(/--signal:\s*#6FC3B8;/);
    const hues = tokens.match(/#[0-9A-Fa-f]{3,8}/g) ?? [];
    expect(hues).toEqual([
      "#0B0E14",
      "#E8E6E1",
      "#55627A",
      "#E8A33D",
      "#6FC3B8",
    ]);
  });
});

describe("ApiClient", () => {
  const fetchMock = vi.fn();
  const client = new ApiClient("http://127.0.0.1:8080");

  beforeEach(() => {
    fetchMock.mockReset();
    vi.stubGlobal("fetch", fetchMock);
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("query POSTs /query?format=json and returns columns/rows", async () => {
    fetchMock.mockResolvedValue(
      jsonResponse(200, { columns: ["t"], rows: [["p1"]] }),
    );

    const result = await client.query(
      "MATCH (t:Person {id: $tid}) RETURN t",
      { tid: "p1" },
    );

    expect(result).toEqual({ columns: ["t"], rows: [["p1"]] });
    expect(fetchMock).toHaveBeenCalledTimes(1);
    const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit];
    expect(url).toBe("http://127.0.0.1:8080/query?format=json");
    expect(init.method).toBe("POST");
    expect(init.headers).toEqual({ "Content-Type": "application/json" });
    expect(init.body).toBe(
      JSON.stringify({
        cypher: "MATCH (t:Person {id: $tid}) RETURN t",
        params: { tid: "p1" },
      }),
    );
  });

  it("query omits params when they are not provided", async () => {
    fetchMock.mockResolvedValue(
      jsonResponse(200, { columns: ["n"], rows: [] }),
    );

    await client.query("MATCH (n) RETURN n LIMIT 1");

    const [, init] = fetchMock.mock.calls[0] as [string, RequestInit];
    expect(init.body).toBe(
      JSON.stringify({ cypher: "MATCH (n) RETURN n LIMIT 1" }),
    );
  });

  it("stats GETs /stats", async () => {
    const body = {
      nodes_live: 60,
      nodes_tombstoned: 0,
      edges: 170,
      rules: [
        { name: "skill_fit", edges: 90, tripped: false, fires: 1 },
      ],
    };
    fetchMock.mockResolvedValue(jsonResponse(200, body));

    await expect(client.stats()).resolves.toEqual(body);
    const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit];
    expect(url).toBe("http://127.0.0.1:8080/stats");
    expect(init.method).toBe("GET");
  });

  it("explain GETs /explain?a=&b=", async () => {
    const body = [
      {
        rule: "skill_fit",
        edge_type: "FIT",
        src_key: "person-01",
        dst_key: "proj-01",
        weight: 1.0,
        predicate: {
          kind: "overlap",
          fields: ["skills"],
          min: 0.3,
          tolerance: null,
          km: null,
          parts: null,
        },
      },
    ];
    fetchMock.mockResolvedValue(jsonResponse(200, body));

    await expect(client.explain("person-01", "proj-01")).resolves.toEqual(body);
    const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit];
    expect(url).toBe(
      "http://127.0.0.1:8080/explain?a=person-01&b=proj-01",
    );
    expect(init.method).toBe("GET");
  });

  it("neighborhood GETs /node/{key}/neighborhood with snake_case query params", async () => {
    const body = { columns: ["key", "label", "depth"], rows: [["b", "Person", 1]] };
    fetchMock.mockResolvedValue(jsonResponse(200, body));

    const result = await client.neighborhood("a/b", {
      depth: 2,
      dir: "out",
      edgeTypes: ["KNOWS", "LIKES"],
    });

    expect(result).toEqual(body);
    const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit];
    expect(url).toBe(
      "http://127.0.0.1:8080/node/a%2Fb/neighborhood?depth=2&dir=out&edge_types=KNOWS%2CLIKES",
    );
    expect(init.method).toBe("GET");
  });

  it("nodeInfo GETs /node/{key} and returns key/label/props", async () => {
    const body = {
      key: "p1",
      label: "Person",
      props: { name: "ada", years: 8, tags: ["x", "y"], ok: true },
    };
    fetchMock.mockResolvedValue(jsonResponse(200, body));

    await expect(client.nodeInfo("a/b")).resolves.toEqual(body);
    const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit];
    expect(url).toBe("http://127.0.0.1:8080/node/a%2Fb");
    expect(init.method).toBe("GET");
  });

  it("nodeEdges GETs /node/{key}/edges and returns the edges array", async () => {
    const body = {
      edges: [
        {
          edge_type: "KNOWS",
          src_key: "p1",
          dst_key: "p2",
          derived: false,
        },
        {
          edge_type: "WORKS_AT",
          src_key: "p1",
          dst_key: "acme",
          derived: true,
        },
      ],
    };
    fetchMock.mockResolvedValue(jsonResponse(200, body));

    await expect(client.nodeEdges("p1")).resolves.toEqual(body);
    const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit];
    expect(url).toBe("http://127.0.0.1:8080/node/p1/edges");
    expect(init.method).toBe("GET");
  });

  it("404 with body prefix node key not found: is a key miss, not an absent endpoint", async () => {
    fetchMock.mockResolvedValue(
      jsonResponse(404, { error: "node key not found: ghost" }),
    );
    const err = await client.nodeInfo("ghost").then(
      () => {
        throw new Error("expected reject");
      },
      (e: unknown) => e,
    );
    expect(err).toBeInstanceOf(ApiError);
    const apiErr = err as ApiError;
    expect(apiErr.status).toBe(404);
    expect(apiErr.error).toBe("node key not found: ghost");
    expect(isKeyNotFound(apiErr)).toBe(true);
    expect(isAbsentEndpoint(apiErr)).toBe(false);
  });

  it("404 without the key-not-found prefix is an absent-endpoint (legacy server)", async () => {
    fetchMock.mockResolvedValue(textResponse(404, "Not Found"));
    const err = await client.nodeEdges("p1").then(
      () => {
        throw new Error("expected reject");
      },
      (e: unknown) => e,
    );
    expect(isKeyNotFound(err)).toBe(false);
    expect(isAbsentEndpoint(err)).toBe(true);
  });

  it("neighborhood omits unset options", async () => {
    fetchMock.mockResolvedValue(
      jsonResponse(200, { columns: ["key"], rows: [] }),
    );

    await client.neighborhood("n1");

    const [url] = fetchMock.mock.calls[0] as [string, RequestInit];
    expect(url).toBe("http://127.0.0.1:8080/node/n1/neighborhood");
  });

  it("ingest POSTs /ingest with label, rows, and wire-shaped opts", async () => {
    const report = {
      inserted: 1,
      row_errors: [[0, "duplicate key p1"]] as [number, string][],
      rules_created: ["auto_fk_person_org_id"],
      skipped_fk_fields: [{ field: "ghost_id", reason: "no matching target keys" }],
    };
    fetchMock.mockResolvedValue(jsonResponse(200, report));

    const rows = [{ id: "p1", org_id: "acme" }];
    const opts = { key_field: "id", auto_fk: { suffix: "_id" } };
    await expect(client.ingest("Person", rows, opts)).resolves.toEqual(report);

    const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit];
    expect(url).toBe("http://127.0.0.1:8080/ingest");
    expect(init.method).toBe("POST");
    expect(init.body).toBe(
      JSON.stringify({ label: "Person", rows, options: opts }),
    );
  });

  it("throws ApiError carrying the server {error} detail on non-2xx", async () => {
    fetchMock.mockResolvedValue(
      jsonResponse(400, { error: "parse: unexpected end of input" }),
    );

    const err = await client.query("MATCH (n)").then(
      () => {
        throw new Error("expected reject");
      },
      (e: unknown) => e,
    );

    expect(err).toBeInstanceOf(ApiError);
    const apiErr = err as ApiError;
    expect(apiErr.status).toBe(400);
    expect(apiErr.error).toBe("parse: unexpected end of input");
    expect(apiErr.message).toBe("parse: unexpected end of input");
  });

  it("throws ApiError for non-2xx without an {error} field", async () => {
    fetchMock.mockResolvedValue(textResponse(422, "Failed to deserialize"));

    const err = await client.stats().then(
      () => {
        throw new Error("expected reject");
      },
      (e: unknown) => e,
    );

    expect(err).toBeInstanceOf(ApiError);
    const apiErr = err as ApiError;
    expect(apiErr.status).toBe(422);
    expect(apiErr.error).toBeUndefined();
    expect(apiErr.message).toBe("HTTP 422");
  });
});
