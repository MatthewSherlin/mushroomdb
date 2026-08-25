export type QueryParam = string | number | boolean;

export type JsonCell = string | number | boolean | null | JsonCell[];

export type QueryResult = {
  columns: string[];
  rows: JsonCell[][];
};

export type RuleStats = {
  name: string;
  edges: number;
  tripped: boolean;
  fires: number;
};

export type Stats = {
  nodes_live: number;
  nodes_tombstoned: number;
  edges: number;
  rules: RuleStats[];
};

export type PredicateKind =
  | "key_match"
  | "field_equal"
  | "overlap"
  | "all"
  | "numeric_within"
  | "geo_radius"
  | "vector_similar";

/** Wire `PredicateSummary`. Option fields are present-null, never omitted. */
export type PredicateSummary = {
  kind: PredicateKind;
  fields: string[];
  min: number | null;
  tolerance: number | null;
  km: number | null;
  parts: PredicateSummary[] | null;
};

export type Explanation = {
  rule: string;
  edge_type: string;
  src_key: string;
  dst_key: string;
  weight: number | null;
  predicate: PredicateSummary | null;
};

export type FkSkip = {
  field: string;
  reason: string;
};

export type IngestReport = {
  inserted: number;
  row_errors: [number, string][];
  rules_created: string[];
  skipped_fk_fields: FkSkip[];
};

export type AutoFkOpt = false | "off" | { suffix: string };

export type IngestOptions = {
  key_field?: string;
  auto_fk?: AutoFkOpt;
};

export type IngestRow = Record<string, unknown>;

export type NeighborhoodDir = "in" | "out" | "both";

export type NeighborhoodOpts = {
  depth?: number;
  dir?: NeighborhoodDir;
  edgeTypes?: string[];
};

/** Wire `NodeInfo`. `props` uses the same untagged Value JSON as `/query`. */
export type NodeInfo = {
  key: string;
  label: string;
  props: Record<string, JsonCell>;
};

/** Wire `EdgeInfo` from `GET /node/{key}/edges`. */
export type EdgeInfo = {
  edge_type: string;
  src_key: string;
  dst_key: string;
  derived: boolean;
};

export type NodeEdges = {
  edges: EdgeInfo[];
};

/** Discriminator for T1's 404 register. Exact prefix, including the colon. */
export const KEY_NOT_FOUND_PREFIX = "node key not found:";

export class ApiError extends Error {
  readonly status: number;
  readonly error: string | undefined;
  readonly body: unknown;

  constructor(status: number, body: unknown) {
    const detail = errorDetail(body);
    super(detail ?? `HTTP ${status}`);
    this.name = "ApiError";
    this.status = status;
    this.error = detail;
    this.body = body;
  }
}

export function isKeyNotFound(err: unknown): boolean {
  return (
    err instanceof ApiError &&
    err.status === 404 &&
    typeof err.error === "string" &&
    err.error.startsWith(KEY_NOT_FOUND_PREFIX)
  );
}

/** 404 that is not a key miss — missing route / old server / static fallback. */
export function isAbsentEndpoint(err: unknown): boolean {
  return err instanceof ApiError && err.status === 404 && !isKeyNotFound(err);
}

/** Page `?token=` value, if present and non-empty. */
export function pageToken(search?: string): string | undefined {
  const raw =
    search ??
    (typeof window !== "undefined" ? window.location.search : "");
  const value = new URLSearchParams(raw).get("token");
  if (value === null || value === "") {
    return undefined;
  }
  return value;
}

export class ApiClient {
  private readonly baseUrl: string;
  private readonly token: string | undefined;

  constructor(baseUrl = "", token?: string) {
    this.baseUrl = baseUrl.replace(/\/$/, "");
    this.token = token;
  }

  query(
    cypher: string,
    params?: Record<string, QueryParam>,
  ): Promise<QueryResult> {
    const body: { cypher: string; params?: Record<string, QueryParam> } = {
      cypher,
    };
    if (params !== undefined) {
      body.params = params;
    }
    return this.request<QueryResult>("POST", "/query?format=json", body);
  }

  stats(): Promise<Stats> {
    return this.request<Stats>("GET", "/stats");
  }

  explain(a: string, b: string): Promise<Explanation[]> {
    const qs = new URLSearchParams({ a, b });
    return this.request<Explanation[]>("GET", `/explain?${qs.toString()}`);
  }

  nodeInfo(key: string): Promise<NodeInfo> {
    return this.request<NodeInfo>(
      "GET",
      `/node/${encodeURIComponent(key)}`,
    );
  }

  nodeEdges(key: string): Promise<NodeEdges> {
    return this.request<NodeEdges>(
      "GET",
      `/node/${encodeURIComponent(key)}/edges`,
    );
  }

  neighborhood(
    key: string,
    opts: NeighborhoodOpts = {},
  ): Promise<QueryResult> {
    const qs = new URLSearchParams();
    if (opts.depth !== undefined) {
      qs.set("depth", String(opts.depth));
    }
    if (opts.dir !== undefined) {
      qs.set("dir", opts.dir);
    }
    if (opts.edgeTypes !== undefined) {
      qs.set("edge_types", opts.edgeTypes.join(","));
    }
    const query = qs.toString();
    const path = `/node/${encodeURIComponent(key)}/neighborhood${
      query ? `?${query}` : ""
    }`;
    return this.request<QueryResult>("GET", path);
  }

  ingest(
    label: string,
    rows: IngestRow[],
    opts?: IngestOptions,
  ): Promise<IngestReport> {
    const body: {
      label: string;
      rows: IngestRow[];
      options?: IngestOptions;
    } = { label, rows };
    if (opts !== undefined) {
      body.options = opts;
    }
    return this.request<IngestReport>("POST", "/ingest", body);
  }

  private async request<T>(
    method: "GET" | "POST",
    path: string,
    body?: unknown,
  ): Promise<T> {
    const init: RequestInit = { method };
    const headers: Record<string, string> = {};
    if (body !== undefined) {
      headers["Content-Type"] = "application/json";
      init.body = JSON.stringify(body);
    }
    if (this.token !== undefined) {
      headers.Authorization = `Bearer ${this.token}`;
    }
    if (Object.keys(headers).length > 0) {
      init.headers = headers;
    }
    const res = await fetch(`${this.baseUrl}${path}`, init);
    const parsed = await readBody(res);
    if (res.status < 200 || res.status >= 300) {
      throw new ApiError(res.status, parsed);
    }
    return parsed as T;
  }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function errorDetail(body: unknown): string | undefined {
  if (!isRecord(body) || typeof body.error !== "string") {
    return undefined;
  }
  return body.error;
}

async function readBody(res: Response): Promise<unknown> {
  const text = await res.text();
  if (text === "") {
    return undefined;
  }
  try {
    return JSON.parse(text) as unknown;
  } catch {
    return text;
  }
}
