#!/usr/bin/env node
"use strict";

const { spawnSync } = require("child_process");
const fs = require("fs");
const path = require("path");

// `--print-launcher`: answer with this script's own absolute path and stop.
//
// It is how an installer takes `npx` off the hot path. Resolving the package
// costs npx a cache check, a version resolve and a Node process of its own —
// around half a second — and a Claude Code hook would pay that before every
// prompt and every edit. `mushroomdb install` asks once, writes down
// `node <this file>`, and every later invocation starts here instead.
//
// Handled before the vendor check on purpose: the answer is where this script
// is, which is true whether or not the binary beside it was fetched.
if (process.argv[2] === "--print-launcher") {
  process.stdout.write(__filename + "\n");
  process.exit(0);
}

const bin = path.join(__dirname, "..", "vendor", "mushroomdb");
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
