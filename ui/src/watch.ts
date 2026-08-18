export type MutationEvent =
  | { node_inserted: { label: string; key: string } }
  | { prop_set: { key: string; field: string } }
  | { prop_removed: { key: string; field: string } }
  | { edge_inserted: { edge_type: string; src: string; dst: string } }
  | { edge_deleted: { edge_type: string; src: string; dst: string } }
  | { node_deleted: { key: string } }
  | { rule_created: { name: string } }
  | { rule_deleted: { name: string } }
  | { rule_rebuilt: { name: string } }
  | { batch_applied: { ops: number } }
  | { ingested: { label: string; inserted: number } };

export type WatchSocket = {
  addEventListener(
    type: string,
    listener: (event: { data?: unknown }) => void,
  ): void;
  removeEventListener(
    type: string,
    listener: (event: { data?: unknown }) => void,
  ): void;
  close(): void;
};

export type WatchBackoff = {
  initialMs: number;
  maxMs: number;
};

export type WatchClientOptions = {
  url: string;
  onConnected?: () => void;
  onEvent?: (event: MutationEvent) => void;
  onLagged?: (skipped: number) => void;
  createWebSocket?: (url: string) => WatchSocket;
  backoff?: WatchBackoff;
};

export class WatchClient {
  private readonly url: string;
  private readonly onConnected: (() => void) | undefined;
  private readonly onEvent: ((event: MutationEvent) => void) | undefined;
  private readonly onLagged: ((skipped: number) => void) | undefined;
  private readonly createWebSocket: (url: string) => WatchSocket;
  private readonly initialMs: number;
  private readonly maxMs: number;

  private socket: WatchSocket | undefined;
  private attempt = 0;
  private closedByUser = false;
  private awaitingAck = true;
  private reconnectTimer: ReturnType<typeof setTimeout> | undefined;

  constructor(options: WatchClientOptions) {
    this.url = options.url;
    this.onConnected = options.onConnected;
    this.onEvent = options.onEvent;
    this.onLagged = options.onLagged;
    this.createWebSocket = options.createWebSocket ?? defaultWebSocket;
    this.initialMs = options.backoff?.initialMs ?? 250;
    this.maxMs = options.backoff?.maxMs ?? 8000;
    this.connect();
  }

  close(): void {
    this.closedByUser = true;
    if (this.reconnectTimer !== undefined) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = undefined;
    }
    const socket = this.socket;
    this.detach();
    socket?.close();
  }

  private connect(): void {
    if (this.closedByUser) {
      return;
    }
    this.awaitingAck = true;
    const socket = this.createWebSocket(this.url);
    this.socket = socket;
    socket.addEventListener("message", this.handleMessage);
    socket.addEventListener("close", this.handleClose);
  }

  private readonly handleMessage = (event: { data?: unknown }): void => {
    if (typeof event.data !== "string") {
      return;
    }
    const frame = parseWatchFrame(event.data);
    if (this.awaitingAck) {
      if (frame.kind === "ack") {
        this.awaitingAck = false;
        this.attempt = 0;
        this.onConnected?.();
      }
      // Pre-ack frames are dropped — the ack confirms the server is ready for this connection.
      return;
    }
    if (frame.kind === "event") {
      this.onEvent?.(frame.event);
    } else if (frame.kind === "lagged") {
      this.onLagged?.(frame.n);
    }
  };

  private readonly handleClose = (): void => {
    this.detach();
    this.scheduleReconnect();
  };

  private detach(): void {
    const socket = this.socket;
    if (socket === undefined) {
      return;
    }
    socket.removeEventListener("message", this.handleMessage);
    socket.removeEventListener("close", this.handleClose);
    this.socket = undefined;
  }

  private scheduleReconnect(): void {
    if (this.closedByUser || this.reconnectTimer !== undefined) {
      return;
    }
    const delay = Math.min(this.maxMs, this.initialMs * 2 ** this.attempt);
    this.attempt += 1;
    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = undefined;
      this.connect();
    }, delay);
  }
}

function defaultWebSocket(url: string): WatchSocket {
  return new WebSocket(url);
}

type WatchFrame =
  | { kind: "ack" }
  | { kind: "lagged"; n: number }
  | { kind: "event"; event: MutationEvent }
  | { kind: "ignore" };

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isString(value: unknown): value is string {
  return typeof value === "string";
}

function isFiniteNumber(value: unknown): value is number {
  return typeof value === "number" && Number.isFinite(value);
}

function parseWatchFrame(raw: string): WatchFrame {
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return { kind: "ignore" };
  }
  if (!isRecord(parsed)) {
    return { kind: "ignore" };
  }
  if (parsed.subscribed === true) {
    return { kind: "ack" };
  }
  if (isFiniteNumber(parsed.lagged)) {
    return { kind: "lagged", n: parsed.lagged };
  }
  const event = parseMutationEvent(parsed);
  if (event !== undefined) {
    return { kind: "event", event };
  }
  return { kind: "ignore" };
}

function parseMutationEvent(
  obj: Record<string, unknown>,
): MutationEvent | undefined {
  if ("node_inserted" in obj) {
    const v = obj.node_inserted;
    if (isRecord(v) && isString(v.label) && isString(v.key)) {
      return { node_inserted: { label: v.label, key: v.key } };
    }
    return undefined;
  }
  if ("prop_set" in obj) {
    const v = obj.prop_set;
    if (isRecord(v) && isString(v.key) && isString(v.field)) {
      return { prop_set: { key: v.key, field: v.field } };
    }
    return undefined;
  }
  if ("prop_removed" in obj) {
    const v = obj.prop_removed;
    if (isRecord(v) && isString(v.key) && isString(v.field)) {
      return { prop_removed: { key: v.key, field: v.field } };
    }
    return undefined;
  }
  if ("edge_inserted" in obj) {
    const v = obj.edge_inserted;
    if (
      isRecord(v) &&
      isString(v.edge_type) &&
      isString(v.src) &&
      isString(v.dst)
    ) {
      return {
        edge_inserted: { edge_type: v.edge_type, src: v.src, dst: v.dst },
      };
    }
    return undefined;
  }
  if ("edge_deleted" in obj) {
    const v = obj.edge_deleted;
    if (
      isRecord(v) &&
      isString(v.edge_type) &&
      isString(v.src) &&
      isString(v.dst)
    ) {
      return {
        edge_deleted: { edge_type: v.edge_type, src: v.src, dst: v.dst },
      };
    }
    return undefined;
  }
  if ("node_deleted" in obj) {
    const v = obj.node_deleted;
    if (isRecord(v) && isString(v.key)) {
      return { node_deleted: { key: v.key } };
    }
    return undefined;
  }
  if ("rule_created" in obj) {
    const v = obj.rule_created;
    if (isRecord(v) && isString(v.name)) {
      return { rule_created: { name: v.name } };
    }
    return undefined;
  }
  if ("rule_deleted" in obj) {
    const v = obj.rule_deleted;
    if (isRecord(v) && isString(v.name)) {
      return { rule_deleted: { name: v.name } };
    }
    return undefined;
  }
  if ("rule_rebuilt" in obj) {
    const v = obj.rule_rebuilt;
    if (isRecord(v) && isString(v.name)) {
      return { rule_rebuilt: { name: v.name } };
    }
    return undefined;
  }
  if ("batch_applied" in obj) {
    const v = obj.batch_applied;
    if (isRecord(v) && isFiniteNumber(v.ops)) {
      return { batch_applied: { ops: v.ops } };
    }
    return undefined;
  }
  if ("ingested" in obj) {
    const v = obj.ingested;
    if (isRecord(v) && isString(v.label) && isFiniteNumber(v.inserted)) {
      return { ingested: { label: v.label, inserted: v.inserted } };
    }
    return undefined;
  }
  return undefined;
}
