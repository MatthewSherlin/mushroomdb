import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    globalSetup: "./tests/global-setup.ts",
    // Use node environment (no DOM shim needed).
    environment: "node",
    // Tests that spawn and talk to a real server need more time.
    testTimeout: 30_000,
    hookTimeout: 30_000,
    // Force-exit after all tests complete so the spawned server process
    // (and vitest's internal Vite server) don't keep the runner alive.
    forceExit: true,
  },
});
