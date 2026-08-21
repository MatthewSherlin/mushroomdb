/**
 * Vitest global setup: build the mushroomdb binary (once), populate a demo
 * database, and start the HTTP server on an ephemeral port.
 *
 * Provides two values to tests via vitest's inject() API:
 *   - `baseUrl`  — HTTP base URL, e.g. "http://127.0.0.1:54321"
 *   - `wsUrl`    — WebSocket base URL, e.g. "ws://127.0.0.1:54321"
 *   - `skipReason` — non-null string when tests should be skipped
 *
 * If the binary cannot be built, all tests are skipped with a clear message
 * rather than failing.
 */

import { execFileSync, spawn, type ChildProcess } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import type { GlobalSetupContext } from "vitest/node";

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/** Repo root, relative to clients/typescript/ */
const REPO_ROOT = resolve(import.meta.dirname, "..", "..", "..");

/** Where cargo should deposit the binary. */
const CARGO_BIN = join(REPO_ROOT, "target", "debug", "mushroomdb");

/** Cargo binary (from the specified toolchain path). */
const CARGO = join(
  "/Users/nilrehsttam/.rustup/toolchains/stable-aarch64-apple-darwin/bin",
  "cargo",
);

// ---------------------------------------------------------------------------
// State shared between setup and teardown
// ---------------------------------------------------------------------------

let serverProcess: ChildProcess | null = null;
let tmpDbDir: string | null = null;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function tryBuild(): { ok: true } | { ok: false; reason: string } {
  // If the binary already exists, skip build to keep test startup fast.
  try {
    execFileSync(CARGO_BIN, ["--help"], { stdio: "ignore" });
    return { ok: true };
  } catch {
    // Binary missing or not executable — try to build.
  }

  try {
    execFileSync(
      CARGO,
      ["build", "-p", "cli", "--bin", "mushroomdb"],
      {
        cwd: REPO_ROOT,
        stdio: "inherit",
        env: { ...process.env, CARGO_TERM_COLOR: "never" },
      },
    );
    return { ok: true };
  } catch (err) {
    return {
      ok: false,
      reason: `cargo build failed: ${err instanceof Error ? err.message : String(err)}`,
    };
  }
}

function runDemo(dbDir: string): { ok: true } | { ok: false; reason: string } {
  try {
    execFileSync(CARGO_BIN, ["demo", dbDir], {
      stdio: "inherit",
      timeout: 60_000,
    });
    return { ok: true };
  } catch (err) {
    return {
      ok: false,
      reason: `demo failed: ${err instanceof Error ? err.message : String(err)}`,
    };
  }
}

/** Start the server and resolve to the bound address. */
function startServer(dbDir: string): Promise<{ port: number; proc: ChildProcess }> {
  return new Promise((resolve, reject) => {
    const proc = spawn(
      CARGO_BIN,
      ["serve", dbDir, "--addr", "127.0.0.1:0", "--no-ui"],
      { stdio: ["ignore", "pipe", "pipe"] },
    );

    let resolved = false;

    const onData = (chunk: Buffer) => {
      const text = chunk.toString("utf8");
      // Server prints: "listening on http://127.0.0.1:<port>"
      const match = /listening on http:\/\/127\.0\.0\.1:(\d+)/.exec(text);
      if (match && !resolved) {
        resolved = true;
        // Unref the process and its stdio so Node.js can exit when done
        // even if the server is still running (teardown kills it explicitly).
        proc.stdout?.resume();
        proc.stderr?.resume();
        proc.unref();
        resolve({ port: parseInt(match[1]!, 10), proc });
      }
    };

    proc.stdout?.on("data", onData);
    proc.stderr?.on("data", onData); // belt-and-suspenders

    proc.on("exit", (code) => {
      if (!resolved) {
        reject(new Error(`Server exited before ready (code ${code})`));
      }
    });

    proc.on("error", (err) => {
      if (!resolved) reject(err);
    });

    // Timeout if the server never prints its address.
    setTimeout(() => {
      if (!resolved) {
        proc.kill();
        reject(new Error("Timed out waiting for server ready signal"));
      }
    }, 30_000);
  });
}

// ---------------------------------------------------------------------------
// Global setup / teardown
// ---------------------------------------------------------------------------

export default async function setup({ provide }: GlobalSetupContext) {
  // 1. Build (or verify existing) binary.
  const buildResult = tryBuild();
  if (!buildResult.ok) {
    provide("skipReason", buildResult.reason);
    provide("baseUrl", "");
    provide("wsUrl", "");
    return;
  }

  // 2. Populate demo database.
  tmpDbDir = mkdtempSync(join(tmpdir(), "mushroomdb-ts-test-"));
  const demoResult = runDemo(tmpDbDir);
  if (!demoResult.ok) {
    provide("skipReason", demoResult.reason);
    provide("baseUrl", "");
    provide("wsUrl", "");
    return;
  }

  // 3. Start server.
  const { port, proc } = await startServer(tmpDbDir);
  serverProcess = proc;

  const base = `http://127.0.0.1:${port}`;
  const ws = `ws://127.0.0.1:${port}`;
  provide("skipReason", "");
  provide("baseUrl", base);
  provide("wsUrl", ws);

  // 4. Return teardown.
  return async () => {
    if (serverProcess) {
      // Try graceful SIGTERM first, then SIGKILL after 3 s.
      const proc = serverProcess;
      serverProcess = null;
      proc.kill("SIGTERM");
      await new Promise<void>((res) => {
        const t = setTimeout(() => {
          try { proc.kill("SIGKILL"); } catch { /* already gone */ }
          res();
        }, 3_000);
        proc.on("exit", () => { clearTimeout(t); res(); });
      });
    }
    if (tmpDbDir) {
      try {
        rmSync(tmpDbDir, { recursive: true, force: true });
      } catch {
        // best-effort
      }
    }
  };
}
