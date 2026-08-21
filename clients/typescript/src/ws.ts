/**
 * WebSocket subscription over `GET /subscribe`.
 *
 * # Protocol (matches crates/server/src/subscribe.rs)
 *
 * 1. Connect to `ws[s]://<host>/subscribe`.
 * 2. Server waits for one JSON subscribe message: `{rules?, writes?}`.
 * 3. Server responds with `{"subscribed":true}`.
 * 4. Server streams DbEvent JSON frames until the connection closes.
 *
 * # Reconnection
 *
 * Auto-reconnect is NOT implemented in v1. When the connection drops
 * (network error, server restart), no further events are delivered. The
 * caller is responsible for reconnecting if required.
 *
 * # Lagged events
 *
 * If the server's per-subscriber queue overflows, it emits a
 * `{"type":"lagged","missed":N}` frame. This is passed to `onEvent` like any
 * other event. For lossless consumers: on receiving a `lagged` event, re-read
 * the affected graph state via a query.
 *
 * # Node.js usage
 *
 * The browser WebSocket global is not present in Node < 21. Pass the `ws`
 * package's WebSocket class via `opts.wsConstructor`:
 *
 * ```ts
 * import WS from 'ws';
 * const handle = await subscribe(wsUrl, { writes: true, wsConstructor: WS as WsConstructor }, onEvent);
 * ```
 */

import { MushroomError } from "./types.js";
import type { DbEvent, SubscribeMessage } from "./types.js";

export type { DbEvent };

/**
 * Minimal WebSocket-like interface required by subscribe().
 * Satisfied by both browser WebSocket and the `ws` npm package.
 */
export interface WsLike {
  send(data: string): void;
  close(): void;
  set onopen(handler: ((ev: unknown) => void) | null);
  set onmessage(handler: ((ev: { data: unknown }) => void) | null);
  set onclose(handler: ((ev: unknown) => void) | null);
  set onerror(handler: ((ev: unknown) => void) | null);
}

/** Constructor type for both browser WebSocket and the `ws` package. */
export type WsConstructor = new (url: string) => WsLike;

/** Options for {@link subscribe}. */
export interface SubscribeOptions extends SubscribeMessage {
  /**
   * Custom WebSocket constructor.
   *
   * **Node.js only** — required when `globalThis.WebSocket` is not available
   * (Node < 21). Example:
   *
   * ```ts
   * import WS from 'ws';
   * { wsConstructor: WS as WsConstructor }
   * ```
   *
   * In the browser the native `WebSocket` global is used automatically.
   */
  wsConstructor?: WsConstructor;
}

/** Handle returned by {@link subscribe}. */
export interface SubscribeHandle {
  /**
   * Close the WebSocket connection.
   *
   * Returns a promise that resolves when the connection is fully closed.
   * Always await this before ending a test or shutting down to avoid
   * dangling handles that keep the event loop alive.
   */
  close(): Promise<void>;
}

/** Coerce an unknown message `data` value to a UTF-8 string. */
function dataToString(data: unknown): string {
  if (typeof data === "string") return data;
  // Node.js ws package delivers Buffer objects for text frames.
  if (data != null && typeof (data as { toString?: unknown }).toString === "function") {
    return (data as { toString(): string }).toString();
  }
  return String(data);
}

/** Resolve the WebSocket constructor: explicit option → global. */
// eslint-disable-next-line @typescript-eslint/no-explicit-any
function resolveWsConstructor(opt?: WsConstructor): WsConstructor {
  if (opt) return opt;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const g = globalThis as any;
  if (typeof g["WebSocket"] === "function") return g["WebSocket"] as WsConstructor;
  throw new Error(
    "No WebSocket implementation available. " +
      "In Node.js < 21, install the `ws` package and pass " +
      "`wsConstructor: WS as WsConstructor` in the options.",
  );
}

/**
 * Open a `GET /subscribe` WebSocket and begin streaming {@link DbEvent}s.
 *
 * Resolves when the server acknowledges the subscribe message
 * (`{"subscribed":true}`). Rejects on connection failure or if the server
 * returns an error (e.g. unknown rule name).
 *
 * @param wsUrl   Full WebSocket URL, e.g. `ws://127.0.0.1:8080/subscribe`.
 * @param opts    Subscribe options — rules, writes flag, optional wsConstructor.
 * @param onEvent Callback invoked for each {@link DbEvent}, including `lagged`.
 */
export async function subscribe(
  wsUrl: string,
  opts: SubscribeOptions,
  onEvent: (event: DbEvent) => void,
): Promise<SubscribeHandle> {
  const WS = resolveWsConstructor(opts.wsConstructor);
  const ws = new WS(wsUrl);

  return new Promise<SubscribeHandle>((resolve, reject) => {
    let subscribed = false;
    let closeResolve: (() => void) | null = null;

    const closePromise = new Promise<void>((res) => {
      closeResolve = res;
    });

    ws.onopen = () => {
      const msg: SubscribeMessage = {
        rules: opts.rules ?? [],
        writes: opts.writes ?? false,
      };
      ws.send(JSON.stringify(msg));
    };

    ws.onmessage = (ev: { data: unknown }) => {
      let text: string;
      try {
        text = dataToString(ev.data);
      } catch {
        return; // unreadable frame — skip
      }

      let parsed: unknown;
      try {
        parsed = JSON.parse(text);
      } catch {
        return; // unparseable frame — skip
      }

      const frame = parsed as Record<string, unknown>;

      if (!subscribed) {
        if (frame["subscribed"] === true) {
          subscribed = true;
          resolve({
            close(): Promise<void> {
              ws.close();
              return closePromise;
            },
          });
        } else if (typeof frame["error"] === "string") {
          reject(new MushroomError(frame["error"] as string));
          ws.close();
        } else {
          reject(new Error("Unexpected subscribe response: " + text));
          ws.close();
        }
      } else {
        onEvent(frame as unknown as DbEvent);
      }
    };

    ws.onerror = (err: unknown) => {
      if (!subscribed) {
        reject(
          err instanceof Error
            ? err
            : new Error("WebSocket error before subscribe ack"),
        );
      }
    };

    ws.onclose = () => {
      if (!subscribed) {
        reject(new Error("WebSocket closed before subscribe ack"));
      }
      closeResolve?.();
    };
  });
}
