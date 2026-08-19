import fs from "node:fs";
import { createRequire } from "node:module";
import path from "node:path";
import type { Plugin } from "vite";
import { defineConfig } from "vitest/config";

const require = createRequire(import.meta.url);
const glBenchEsm = require.resolve("gl-bench/dist/gl-bench.module.js");
const api = "http://127.0.0.1:8080";

const proxy = {
  "/query": api,
  "/stats": api,
  "/explain": api,
  "/node": api,
  "/ingest": api,
  "/watch": { target: api, ws: true },
};

/** Vite 8 serves disk files at `/@fs/...` but rewrites some imports to `/node_modules/...`. */
function serveRootNodeModules(): Plugin {
  return {
    name: "serve-root-node-modules",
    configureServer(server) {
      server.middlewares.use((req, _res, next) => {
        const url = req.url;
        if (url === undefined) {
          next();
          return;
        }
        const q = url.indexOf("?");
        const pathname = decodeURIComponent(q === -1 ? url : url.slice(0, q));
        const search = q === -1 ? "" : url.slice(q);
        if (!pathname.startsWith("/node_modules/")) {
          next();
          return;
        }
        const file = path.join(server.config.root, pathname);
        if (fs.existsSync(file) && fs.statSync(file).isFile()) {
          req.url = `/@fs${file}${search}`;
        }
        next();
      });
    },
  };
}

export default defineConfig({
  plugins: [serveRootNodeModules()],
  test: {
    environment: "node",
    exclude: ["**/node_modules/**", "**/dist/**", "**/e2e/**"],
  },
  // cosmos.gl imports `gl-bench`'s browser UMD build, which has no ESM default.
  resolve: {
    alias: {
      "gl-bench": glBenchEsm,
    },
  },
  server: { proxy },
  preview: { proxy },
  build: {
    rolldownOptions: {
      output: {
        manualChunks(id: string) {
          if (
            id.includes("node_modules/@cosmos.gl") ||
            id.includes("/@cosmos.gl/") ||
            id.includes("node_modules/@luma.gl") ||
            id.includes("node_modules/@math.gl") ||
            id.includes("node_modules/@probe.gl")
          ) {
            return "cosmos";
          }
        },
      },
    },
  },
});
