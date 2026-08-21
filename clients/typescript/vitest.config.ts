import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    globalSetup: "./tests/global-setup.ts",
    // Use node environment (no DOM shim needed).
    environment: "node",
    // Tests that spawn and talk to a real server need more time.
    testTimeout: 30_000,
    hookTimeout: 30_000,
    // forceExit: vitest 4.x keeps its internal Vite dev server alive for up to
    // 10 s after tests complete — a known upstream issue in vitest ≤4.x.
    // The WS lifecycle in ws.ts is correct: handle.close() calls ws.close()
    // and awaits the 'close' event via closePromise; subscribe tests guard with
    // try/finally. The spawned server proc is unref()'d after startup so it
    // does not block Node exit. This flag suppresses the Vite-internal delay
    // only and is NOT masking a real open-handle leak. Revisit on vitest 5
    // upgrade — remove if tests exit cleanly without it.
    forceExit: true,
  },
});
