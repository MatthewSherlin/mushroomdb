#!/usr/bin/env node
"use strict";

const { spawnSync } = require("child_process");
const fs = require("fs");
const path = require("path");

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
