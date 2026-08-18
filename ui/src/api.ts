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

export type Explanation = {
  rule: string;
  edge_type: string;
  src_key: string;
  dst_key: string;
  weight: number | null;
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

export class ApiClient {
  private readonly baseUrl: string;

  constructor(baseUrl = "") {
    this.baseUrl = baseUrl.replace(/\/$/, "");
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
    if (body !== undefined) {
      init.headers = { "Content-Type": "application/json" };
      init.body = JSON.stringify(body);
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
