#!/usr/bin/env node
"use strict";

const { spawnSync } = require("child_process");
const fs = require("fs");
const path = require("path");

const bin = path.resolve(path.join(__dirname, "..", "vendor", "mushroomdb"));

// Two flags that answer "where is this package", so an installer can ask once
// instead of making every invocation pay for `npx`.
//
// Resolving the package costs npx a cache check, a version resolve and a Node
// process of its own — around half a second — and a Claude Code hook would pay
// that before every prompt and every edit. `mushroomdb install` and the plugin's
// hooks/run.sh ask once and write the answer down.
//
// Prefer `--print-binary`. This script is only a shim: it starts a Node
// runtime and then spawns the native binary anyway, and that Node startup is
// most of the cost. Measured warm, `--version`: npx 514 ms, `node <this file>`
// 118 ms, the binary directly 7 ms. `--print-launcher` stays as the fallback
// for an install whose vendored binary was never fetched.

// The native binary for this platform, which postinstall put beside us.
// Exits 1 when it is not there, so a caller can fall through to the launcher.
if (process.argv[2] === "--print-binary") {
  if (!fs.existsSync(bin)) {
    process.stderr.write("mushroomdb binary is missing at " + bin + "\n");
    process.exit(1);
  }
  process.stdout.write(bin + "\n");
  process.exit(0);
}

// This script's own path. Answered before the vendor check on purpose: where
// this file is stays true whether or not the binary beside it was fetched.
if (process.argv[2] === "--print-launcher") {
  process.stdout.write(__filename + "\n");
  process.exit(0);
}

if (!fs.existsSync(bin)) {
  process.stderr.write(
    "mushroomdb binary is missing; re-run npm install (postinstall fetches the GitHub Release asset)\n",
  );
  process.exit(1);
}
const result = spawnSync(bin, process.argv.slice(2), { stdio: "inherit" });
if (result.error) {
  process.stderr.write(result.error.message + "\n");
  process.exit(1);
}
process.exit(result.status === null ? 1 : result.status);
