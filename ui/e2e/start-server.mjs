#!/usr/bin/env node
import { spawn, spawnSync } from "node:child_process";
import { mkdtempSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repo = join(here, "..", "..");
const bin = join(repo, "target", "debug", "mushroomdb");
const dist = join(repo, "ui", "dist");
const port =
  process.env.E2E_PORT ?? String(49152 + Math.floor(Math.random() * 16383));
process.env.E2E_PORT = port;

if (!existsSync(bin)) {
  console.error(`missing ${bin} — cargo build -p cli --bin mushroomdb`);
  process.exit(1);
}
if (!existsSync(join(dist, "index.html"))) {
  console.error(`missing ${dist}/index.html — npm run build`);
  process.exit(1);
}

const db = mkdtempSync(join(tmpdir(), "mushroomdb-e2e-"));
const demo = spawnSync(bin, ["demo", db], { stdio: "inherit" });
if (demo.status !== 0) {
  process.exit(demo.status ?? 1);
}
// /ingest inserts nodes only. Seed a real user edge for the demo-flow spec.
const seedBin = join(repo, "target", "debug", "examples", "insert_edge");
if (!existsSync(seedBin)) {
  const built = spawnSync(
    "cargo",
    ["build", "-p", "core-api", "--example", "insert_edge"],
    { cwd: repo, stdio: "inherit" },
  );
  if (built.status !== 0) {
    process.exit(built.status ?? 1);
  }
}
const seed = spawnSync(
  seedBin,
  [db, "KNOWS", "person-01", "person-02"],
  { stdio: "inherit" },
);
if (seed.status !== 0) {
  process.exit(seed.status ?? 1);
}

const child = spawn(
  bin,
  ["serve", db, "--ui", dist, "--addr", `127.0.0.1:${port}`],
  { stdio: "inherit" },
);
const stop = () => {
  child.kill("SIGTERM");
};
process.on("SIGTERM", stop);
process.on("SIGINT", stop);
child.on("exit", (code) => {
  process.exit(code ?? 1);
});
