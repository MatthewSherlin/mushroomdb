/**
 * Graph store — the pure-logic brain every view renders from.
 *
 * No DOM / canvas / @cosmos.gl imports. Views (T3+) fetch and feed this module.
 *
 * ## Chosen merge API
 *
 * The HTTP `/node/{k}/neighborhood` endpoint returns nodes only
 * (`columns: ["key","label","depth"]`) — never edges. Edges for the canvas
 * come from `GET /node/{k}/edges` (legacy: neighborhood + `/explain`,
 * with user etypes collapsed to `related`) via `mergeNeighborhoodWithEdges`.
 *
 * - `fromNeighborhood(rootKey, result)` — ingest a neighborhood ResultSet.
 *   Adds/updates those nodes and ensures the root exists. Does **not** add
 *   edges. After this call `needsEdges(rootKey)` is true.
 * - `needsEdges(rootKey)` — true iff the key is a visible node and
 *   `mergeNeighborhoodWithEdges` has not been applied for it since the last
 *   `fromNeighborhood` (or ever, if that merge has never run).
 * - `mergeNeighborhoodWithEdges(root, perEtypeNeighbors, dir?)` — for each
 *   etype → neighbor keys: ensure stub nodes exist, then add directed edges.
 *   `dir: "out"` (default) stores `etype|root|nbr`; `dir: "in"` stores
 *   `etype|nbr|root`. A typed (non-`related`) merge drops any legacy
 *   `related` edge between the same endpoints, both orientations. Re-merge
 *   is idempotent. Clears `needsEdges(root)`.
 * - `mergeQueryGraph(columns, rows)` — ingest `/query?format=json`. The
 *   server serializes `RETURN n` as the node key string. A `key`+`label`
 *   pair is bound when those columns exist; otherwise every string cell in
 *   a non-dotted column is a key (`label`/`depth` reserved, not harvested).
 *   Does not add edges.
 *
 * Binding invariant: after any merge, every edge's `src` and `dst` exist as
 * nodes. Edge identity is `etype|src|dst` — rematch never duplicates.
 *
 * ## Watch application
 *
 * - `node_inserted` — add only if any existing edge references the key
 *   (defensive; the endpoint invariant normally makes this a re-label) OR
 *   the key is in `dirty` (dirty-interest). Else ignore. Visible nodes
 *   have their label updated.
 * - `prop_set` / `prop_removed` — if the node is visible, mark it dirty.
 * - `edge_inserted` / `edge_deleted` — only if **both** endpoints are
 *   visible. New edges have unknown provenance (`derived` unset); the view
 *   refetches `/explain`. Existing provenance is not overwritten.
 * - `node_deleted` — drop the node, its incident edges, its dirty mark,
 *   and fix selection if it pointed at the node or an incident edge.
 * - `rule_created` / `rule_deleted` / `rule_rebuilt` — mark **all** visible
 *   node keys dirty (derived edges may have changed). Provenance cache is
 *   kept until the view refetches explain.
 * - `batch_applied` / `ingested` — no-op; inner per-record events are
 *   already applied.
 * - `lagged()` — mark every visible node key dirty (resync).
 */
import type { Explanation, JsonCell, QueryResult } from "./api";
import type { MutationEvent } from "./watch";

export type GraphNode = {
  key: string;
  label: string;
  props?: Record<string, unknown>;
};

export type GraphEdge = {
  etype: string;
  src: string;
  dst: string;
  derived?: boolean;
  explanation?: Explanation;
};

export type Selection =
  | { kind: "node"; id: string }
  | { kind: "edge"; id: string }
  | null;

export type Rgba = readonly [number, number, number, number];

export type CosmosSnapshot = {
  pointKeys: string[];
  pointColors: Rgba[];
  links: [number, number][];
  linkColors: Rgba[];
};

export type EdgeDir = "in" | "out";

/** Design Brief tokens as cosmos-style 0–1 RGBA. Hexes match `tokens.css`. */
export const COLOR = {
  ink: hexRgba("#0B0E14"),
  paper: hexRgba("#E8E6E1"),
  structure: hexRgba("#55627A"),
  gold: hexRgba("#E8A33D"),
  signal: hexRgba("#6FC3B8"),
} as const;

export function edgeId(etype: string, src: string, dst: string): string {
  return `${etype}|${src}|${dst}`;
}

export class GraphStore {
  readonly nodes = new Map<string, GraphNode>();
  readonly edges = new Map<string, GraphEdge>();
  readonly dirty = new Set<string>();
  selection: Selection = null;

  /** Roots whose typed neighbors have been merged since the last fromNeighborhood. */
  private readonly edgesReady = new Set<string>();

  fromNeighborhood(rootKey: string, result: QueryResult): void {
    const keyIdx = result.columns.indexOf("key");
    const labelIdx = result.columns.indexOf("label");
    if (keyIdx >= 0) {
      for (const row of result.rows) {
        const key = stringCell(row[keyIdx]);
        if (key === undefined) {
          continue;
        }
        const label = labelIdx >= 0 ? (stringCell(row[labelIdx]) ?? "") : "";
        this.upsertNode(key, label);
      }
    }
    this.ensureNode(rootKey);
    this.edgesReady.delete(rootKey);
    this.dirty.delete(rootKey);
  }

  needsEdges(rootKey: string): boolean {
    return this.nodes.has(rootKey) && !this.edgesReady.has(rootKey);
  }

  mergeNeighborhoodWithEdges(
    root: string,
    perEtypeNeighbors: Record<string, string[]>,
    dir: EdgeDir = "out",
  ): void {
    this.ensureNode(root);
    for (const [etype, neighbors] of Object.entries(perEtypeNeighbors)) {
      for (const nbr of neighbors) {
        this.ensureNode(nbr);
        const src = dir === "out" ? root : nbr;
        const dst = dir === "out" ? nbr : root;
        this.upsertEdge(etype, src, dst);
        // USER_ETYPE ("related") — typed merge supersedes the legacy ghost.
        if (etype !== "related") {
          this.dropLegacyRelated(src, dst);
        }
      }
    }
    this.edgesReady.add(root);
  }

  mergeQueryGraph(columns: string[], rows: JsonCell[][]): void {
    const keyIdx = columns.indexOf("key");
    const labelIdx = columns.indexOf("label");
    if (keyIdx >= 0) {
      for (const row of rows) {
        const key = stringCell(row[keyIdx]);
        if (key === undefined) {
          continue;
        }
        const label = labelIdx >= 0 ? (stringCell(row[labelIdx]) ?? "") : "";
        this.upsertNode(key, label);
      }
      return;
    }
    for (const row of rows) {
      for (let i = 0; i < columns.length; i++) {
        const col = columns[i];
        if (
          col === undefined ||
          col.includes(".") ||
          col === "label" ||
          col === "depth"
        ) {
          continue;
        }
        const key = stringCell(row[i]);
        if (key !== undefined) {
          this.upsertNode(key, "");
        }
      }
    }
  }

  setProvenance(id: string, explanation: Explanation | null): void {
    const edge = this.edges.get(id);
    if (edge === undefined) {
      return;
    }
    if (explanation === null) {
      edge.derived = false;
      delete edge.explanation;
      return;
    }
    edge.derived = true;
    edge.explanation = explanation;
  }

  derivedEdges(ruleName?: string): GraphEdge[] {
    const out: GraphEdge[] = [];
    for (const id of [...this.edges.keys()].sort()) {
      const edge = this.edges.get(id);
      if (edge === undefined || edge.derived !== true) {
        continue;
      }
      if (ruleName !== undefined && edge.explanation?.rule !== ruleName) {
        continue;
      }
      out.push(edge);
    }
    return out;
  }

  setDerived(id: string, derived: boolean): void {
    const edge = this.edges.get(id);
    if (edge === undefined) {
      return;
    }
    edge.derived = derived;
  }

  applyNodeInfo(
    key: string,
    label: string,
    props: Record<string, unknown>,
  ): void {
    const node = this.nodes.get(key);
    if (node === undefined) {
      return;
    }
    if (label !== "") {
      node.label = label;
    }
    node.props = props;
    this.dirty.delete(key);
  }

  setNodeProps(key: string, props: Record<string, unknown> | undefined): void {
    const node = this.nodes.get(key);
    if (node === undefined) {
      return;
    }
    if (props === undefined) {
      delete node.props;
      return;
    }
    node.props = props;
  }

  select(selection: Selection): void {
    this.selection = selection;
  }

  markDirty(key: string): void {
    this.dirty.add(key);
  }

  apply(event: MutationEvent): void {
    if ("node_inserted" in event) {
      this.onNodeInserted(event.node_inserted);
      return;
    }
    if ("prop_set" in event) {
      this.markVisibleDirty(event.prop_set.key);
      return;
    }
    if ("prop_removed" in event) {
      this.markVisibleDirty(event.prop_removed.key);
      return;
    }
    if ("edge_inserted" in event) {
      this.onEdgeInserted(event.edge_inserted);
      return;
    }
    if ("edge_deleted" in event) {
      this.onEdgeDeleted(event.edge_deleted);
      return;
    }
    if ("node_deleted" in event) {
      this.onNodeDeleted(event.node_deleted.key);
      return;
    }
    if (
      "rule_created" in event ||
      "rule_deleted" in event ||
      "rule_rebuilt" in event
    ) {
      this.markAllDirty();
      return;
    }
    // batch_applied / ingested: inner events already applied.
  }

  lagged(): void {
    this.markAllDirty();
  }

  toCosmos(): CosmosSnapshot {
    const pointKeys = [...this.nodes.keys()].sort();
    const index = new Map<string, number>();
    for (let i = 0; i < pointKeys.length; i++) {
      index.set(pointKeys[i]!, i);
    }

    const selectedNode =
      this.selection?.kind === "node" ? this.selection.id : undefined;
    const selectedEdge =
      this.selection?.kind === "edge" ? this.selection.id : undefined;

    const pointColors: Rgba[] = pointKeys.map((key) =>
      key === selectedNode ? COLOR.signal : COLOR.paper,
    );

    const links: [number, number][] = [];
    const linkColors: Rgba[] = [];
    for (const id of [...this.edges.keys()].sort()) {
      const edge = this.edges.get(id);
      if (edge === undefined) {
        continue;
      }
      const srcIdx = index.get(edge.src);
      const dstIdx = index.get(edge.dst);
      if (srcIdx === undefined || dstIdx === undefined) {
        continue;
      }
      links.push([srcIdx, dstIdx]);
      const incidentToSelection =
        selectedEdge === id ||
        selectedNode === edge.src ||
        selectedNode === edge.dst;
      if (incidentToSelection) {
        linkColors.push(COLOR.signal);
      } else if (edge.derived === true) {
        linkColors.push(COLOR.gold);
      } else {
        linkColors.push(COLOR.structure);
      }
    }

    return { pointKeys, pointColors, links, linkColors };
  }

  private upsertNode(key: string, label: string): GraphNode {
    const existing = this.nodes.get(key);
    if (existing === undefined) {
      const node: GraphNode = { key, label };
      this.nodes.set(key, node);
      return node;
    }
    if (label !== "") {
      existing.label = label;
    }
    return existing;
  }

  private ensureNode(key: string): GraphNode {
    return this.upsertNode(key, "");
  }

  private upsertEdge(etype: string, src: string, dst: string): GraphEdge {
    const id = edgeId(etype, src, dst);
    const existing = this.edges.get(id);
    if (existing !== undefined) {
      return existing;
    }
    const edge: GraphEdge = { etype, src, dst };
    this.edges.set(id, edge);
    return edge;
  }

  private onNodeInserted(ev: { label: string; key: string }): void {
    const existing = this.nodes.get(ev.key);
    if (existing !== undefined) {
      if (ev.label !== "") {
        existing.label = ev.label;
      }
      return;
    }
    if (this.dirty.has(ev.key) || this.edgeReferences(ev.key)) {
      this.nodes.set(ev.key, { key: ev.key, label: ev.label });
    }
  }

  private edgeReferences(key: string): boolean {
    for (const edge of this.edges.values()) {
      if (edge.src === key || edge.dst === key) {
        return true;
      }
    }
    return false;
  }

  private markVisibleDirty(key: string): void {
    if (this.nodes.has(key)) {
      this.dirty.add(key);
    }
  }

  private markAllDirty(): void {
    for (const key of this.nodes.keys()) {
      this.dirty.add(key);
    }
  }

  private onEdgeInserted(ev: {
    edge_type: string;
    src: string;
    dst: string;
  }): void {
    if (!this.nodes.has(ev.src) || !this.nodes.has(ev.dst)) {
      return;
    }
    this.upsertEdge(ev.edge_type, ev.src, ev.dst);
  }

  private onEdgeDeleted(ev: {
    edge_type: string;
    src: string;
    dst: string;
  }): void {
    if (!this.nodes.has(ev.src) || !this.nodes.has(ev.dst)) {
      return;
    }
    this.removeEdge(edgeId(ev.edge_type, ev.src, ev.dst));
  }

  private onNodeDeleted(key: string): void {
    this.nodes.delete(key);
    this.dirty.delete(key);
    this.edgesReady.delete(key);
    for (const [id, edge] of [...this.edges.entries()]) {
      if (edge.src === key || edge.dst === key) {
        this.removeEdge(id);
      }
    }
    if (this.selection?.kind === "node" && this.selection.id === key) {
      this.selection = null;
    }
  }

  /** Drop legacy `related` ghosts both ways — synthesis was directionless. */
  private dropLegacyRelated(src: string, dst: string): void {
    this.removeEdge(edgeId("related", src, dst));
    if (src !== dst) {
      this.removeEdge(edgeId("related", dst, src));
    }
  }

  private removeEdge(id: string): void {
    if (!this.edges.has(id)) {
      return;
    }
    this.edges.delete(id);
    this.clearSelectionIfEdge(id);
  }

  private clearSelectionIfEdge(id: string): void {
    if (this.selection?.kind === "edge" && this.selection.id === id) {
      this.selection = null;
    }
  }
}

function stringCell(cell: JsonCell | undefined): string | undefined {
  return typeof cell === "string" && cell !== "" ? cell : undefined;
}

function hexRgba(hex: string): Rgba {
  const n = Number.parseInt(hex.slice(1), 16);
  return [
    ((n >> 16) & 255) / 255,
    ((n >> 8) & 255) / 255,
    (n & 255) / 255,
    1,
  ];
}
