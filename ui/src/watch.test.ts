import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { WatchClient, type MutationEvent, type WatchSocket } from "./watch";

type WsListener = (event: { data?: string }) => void;

class ScriptedWebSocket implements WatchSocket {
  url: string;
  readyState = 0;
  private readonly listeners = new Map<string, Set<WsListener>>();

  constructor(url: string) {
    this.url = url;
  }

  addEventListener(type: string, listener: WsListener): void {
    let set = this.listeners.get(type);
    if (!set) {
      set = new Set();
      this.listeners.set(type, set);
    }
    set.add(listener);
  }

  removeEventListener(type: string, listener: WsListener): void {
    this.listeners.get(type)?.delete(listener);
  }

  close(): void {
    if (this.readyState === 3) {
      return;
    }
    this.readyState = 3;
    this.dispatch("close");
  }

  open(): void {
    this.readyState = 1;
    this.dispatch("open");
  }

  receive(data: string): void {
    this.dispatch("message", data);
  }

  private dispatch(type: string, data?: string): void {
    for (const listener of this.listeners.get(type) ?? []) {
      listener({ data });
    }
  }
}

const VARIANTS: MutationEvent[] = [
  { node_inserted: { label: "A", key: "k" } },
  { prop_set: { key: "k", field: "n" } },
  { prop_removed: { key: "k", field: "n" } },
  { edge_inserted: { edge_type: "E", src: "a", dst: "b" } },
  { edge_deleted: { edge_type: "E", src: "a", dst: "b" } },
  { node_deleted: { key: "k" } },
  { rule_created: { name: "skill_fit" } },
  { rule_deleted: { name: "skill_fit" } },
  { rule_rebuilt: { name: "skill_fit" } },
  { batch_applied: { ops: 3 } },
  { ingested: { label: "Person", inserted: 2 } },
];

describe("WatchClient", () => {
  const sockets: ScriptedWebSocket[] = [];
  const createWebSocket = (url: string): ScriptedWebSocket => {
    const ws = new ScriptedWebSocket(url);
    sockets.push(ws);
    return ws;
  };

  beforeEach(() => {
    sockets.length = 0;
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  function start(
    extras: {
      onConnected?: () => void;
      onReconnecting?: () => void;
      onEvent?: (event: MutationEvent) => void;
      onLagged?: (n: number) => void;
    } = {},
  ): WatchClient {
    return new WatchClient({
      url: "ws://127.0.0.1:8080/watch",
      createWebSocket,
      backoff: { initialMs: 100, maxMs: 400 },
      ...extras,
    });
  }

  it("does not emit connected until the subscribed ack", () => {
    const onConnected = vi.fn();
    const onEvent = vi.fn();
    start({ onConnected, onEvent });

    expect(sockets).toHaveLength(1);
    sockets[0].open();
    expect(onConnected).not.toHaveBeenCalled();

    sockets[0].receive(JSON.stringify({ node_inserted: { label: "A", key: "k" } }));
    expect(onConnected).not.toHaveBeenCalled();
    expect(onEvent).not.toHaveBeenCalled();

    sockets[0].receive(JSON.stringify({ subscribed: true }));
    expect(onConnected).toHaveBeenCalledTimes(1);
    expect(onEvent).not.toHaveBeenCalled();
  });

  it("parses each snake_case MutationEvent variant after ack", () => {
    const events: MutationEvent[] = [];
    start({ onEvent: (event) => events.push(event) });
    sockets[0].open();
    sockets[0].receive(JSON.stringify({ subscribed: true }));

    for (const frame of VARIANTS) {
      sockets[0].receive(JSON.stringify(frame));
    }

    expect(events).toEqual(VARIANTS);
  });

  it("routes {lagged:n} to onLagged", () => {
    const onLagged = vi.fn();
    start({ onLagged });
    sockets[0].open();
    sockets[0].receive(JSON.stringify({ subscribed: true }));
    sockets[0].receive(JSON.stringify({ lagged: 7 }));
    expect(onLagged).toHaveBeenCalledTimes(1);
    expect(onLagged).toHaveBeenCalledWith(7);
  });

  it("emits reconnecting on unexpected close before the next ack", async () => {
    const onConnected = vi.fn();
    const onReconnecting = vi.fn();
    start({ onConnected, onReconnecting });
    sockets[0].open();
    sockets[0].receive(JSON.stringify({ subscribed: true }));
    sockets[0].close();
    expect(onReconnecting).toHaveBeenCalledTimes(1);
    expect(onConnected).toHaveBeenCalledTimes(1);
  });

  it("reconnects after an unexpected close and waits for ack again", async () => {
    const onConnected = vi.fn();
    start({ onConnected });
    sockets[0].open();
    sockets[0].receive(JSON.stringify({ subscribed: true }));
    expect(onConnected).toHaveBeenCalledTimes(1);

    sockets[0].close();
    expect(sockets).toHaveLength(1);

    await vi.advanceTimersByTimeAsync(99);
    expect(sockets).toHaveLength(1);
    await vi.advanceTimersByTimeAsync(1);
    expect(sockets).toHaveLength(2);

    sockets[1].open();
    expect(onConnected).toHaveBeenCalledTimes(1);
    sockets[1].receive(JSON.stringify({ subscribed: true }));
    expect(onConnected).toHaveBeenCalledTimes(2);
  });

  it("caps reconnect backoff", async () => {
    start();
    sockets[0].close();
    await vi.advanceTimersByTimeAsync(100);
    expect(sockets).toHaveLength(2);

    sockets[1].close();
    await vi.advanceTimersByTimeAsync(200);
    expect(sockets).toHaveLength(3);

    sockets[2].close();
    await vi.advanceTimersByTimeAsync(400);
    expect(sockets).toHaveLength(4);

    sockets[3].close();
    await vi.advanceTimersByTimeAsync(399);
    expect(sockets).toHaveLength(4);
    await vi.advanceTimersByTimeAsync(1);
    expect(sockets).toHaveLength(5);
  });

  it("close() shuts the socket and does not reconnect", async () => {
    const client = start();
    sockets[0].open();
    sockets[0].receive(JSON.stringify({ subscribed: true }));

    client.close();
    expect(sockets[0].readyState).toBe(3);

    await vi.advanceTimersByTimeAsync(10_000);
    expect(sockets).toHaveLength(1);
  });
});
