/**
 * Live-mode helpers: ticker buffer, watch-dot machine, lagged resync keys,
 * and the T6 edge_inserted decision.
 *
 * ## Partial-endpoint / not-fully-expanded edge_inserted
 *
 * `store.apply` (T2) only upserts an edge when **both** endpoints are
 * already visible — it never fabricates a missing node. T6 adds dirtying:
 *
 * - Both visible and neither `needsEdges` (fully expanded): apply the
 *   event so the live edge appears (glow path).
 * - Otherwise, if a visible endpoint is not fully expanded **or** the
 *   other end is off-canvas: `markDirty` the visible endpoint(s) and do
 *   **not** apply — no stub node, no unclassified edge. A later expand /
 *   lagged resync refetches the neighborhood.
 */
import { EXPLAIN_CONCURRENCY, mapPool } from "./classify";
import { expandNode, type ExpandApi } from "./expand";
import { bornEdgeIds } from "./glow";
import { edgeId, type GraphStore } from "./store";
import type { MutationEvent } from "./watch";

export const TICKER_CAP = 20;

export const FLASH_MS = 280;

export type WatchDot = "idle" | "connected" | "reconnecting" | "flash";

export type DotAction = "connected" | "reconnecting" | "event" | "flash_end";

export class TickerBuffer {
  private readonly cap: number;
  private readonly items: string[] = [];

  constructor(cap = TICKER_CAP) {
    this.cap = cap;
  }

  push(line: string): void {
    this.items.push(line);
    if (this.items.length > this.cap) {
      this.items.shift();
    }
  }

  lines(): readonly string[] {
    return this.items;
  }

  last(): string | undefined {
    return this.items[this.items.length - 1];
  }
}

export function formatTickerLine(event: MutationEvent): string {
  if ("node_inserted" in event) {
    return `node inserted ${event.node_inserted.key}`;
  }
  if ("prop_set" in event) {
    return `prop set ${event.prop_set.key}.${event.prop_set.field}`;
  }
  if ("prop_removed" in event) {
    return `prop removed ${event.prop_removed.key}.${event.prop_removed.field}`;
  }
  if ("edge_inserted" in event) {
    const e = event.edge_inserted;
    return `edge inserted ${e.edge_type} ${e.src} → ${e.dst}`;
  }
  if ("edge_deleted" in event) {
    const e = event.edge_deleted;
    return `edge deleted ${e.edge_type} ${e.src} → ${e.dst}`;
  }
  if ("node_deleted" in event) {
    return `node deleted ${event.node_deleted.key}`;
  }
  if ("rule_created" in event) {
    return `rule created ${event.rule_created.name}`;
  }
  if ("rule_deleted" in event) {
    return `rule deleted ${event.rule_deleted.name}`;
  }
  if ("rule_rebuilt" in event) {
    return `rule rebuilt ${event.rule_rebuilt.name}`;
  }
  if ("batch_applied" in event) {
    return `batch applied ${event.batch_applied.ops}`;
  }
  return `ingested ${event.ingested.label} ${event.ingested.inserted}`;
}

export function formatLaggedLine(skipped: number): string {
  return `lagged ${skipped}`;
}

export function nextDot(
  current: WatchDot,
  action: DotAction,
  reducedMotion = false,
): WatchDot {
  if (action === "connected") {
    return "connected";
  }
  if (action === "reconnecting") {
    return "reconnecting";
  }
  if (action === "flash_end") {
    return current === "flash" ? "connected" : current;
  }
  if (current !== "connected" && current !== "flash") {
    return current;
  }
  return reducedMotion ? "connected" : "flash";
}

export function watchUrl(location: {
  protocol: string;
  host: string;
  search?: string;
}): string {
  const scheme = location.protocol === "https:" ? "wss:" : "ws:";
  const token = new URLSearchParams(location.search ?? "").get("token");
  const qs =
    token !== null && token !== ""
      ? `?token=${encodeURIComponent(token)}`
      : "";
  return `${scheme}//${location.host}/watch${qs}`;
}

export function applyLiveEvent(store: GraphStore, event: MutationEvent): void {
  if (!("edge_inserted" in event)) {
    store.apply(event);
    return;
  }
  const { src, dst } = event.edge_inserted;
  const srcVisible = store.nodes.has(src);
  const dstVisible = store.nodes.has(dst);
  const ready =
    srcVisible &&
    dstVisible &&
    !store.needsEdges(src) &&
    !store.needsEdges(dst);
  if (ready) {
    store.apply(event);
    return;
  }
  if (srcVisible) {
    store.markDirty(src);
  }
  if (dstVisible) {
    store.markDirty(dst);
  }
}

export function resyncKeys(store: GraphStore): string[] {
  return [...store.nodes.keys()].sort();
}

export function triggersResync(event: MutationEvent): boolean {
  return (
    "ingested" in event ||
    "node_inserted" in event ||
    "edge_inserted" in event
  );
}

export function derivedEdgeIds(store: GraphStore): string[] {
  return store.derivedEdges().map((e) => edgeId(e.etype, e.src, e.dst));
}

export function glowBornDerived(
  before: readonly string[],
  after: readonly string[],
  reducedMotion: boolean,
): string[] {
  if (reducedMotion) {
    return [];
  }
  return bornEdgeIds(before, after);
}

/** Ingest / lagged bursts: one run at a time, at most one queued behind it. */
export class ResyncGate {
  private running = false;
  private pending = false;

  request(run: () => Promise<void>): void {
    if (this.running) {
      this.pending = true;
      return;
    }
    void this.drain(run);
  }

  private async drain(run: () => Promise<void>): Promise<void> {
    this.running = true;
    try {
      do {
        this.pending = false;
        await run();
      } while (this.pending);
    } finally {
      this.running = false;
    }
  }
}

export async function resyncNeighborhoods(
  store: GraphStore,
  api: ExpandApi,
  reducedMotion: boolean,
): Promise<string[]> {
  store.lagged();
  const before = derivedEdgeIds(store);
  const keys = resyncKeys(store);
  await mapPool(keys, EXPLAIN_CONCURRENCY, (key) =>
    expandNode(store, api, key, 1),
  );
  return glowBornDerived(before, derivedEdgeIds(store), reducedMotion);
}
