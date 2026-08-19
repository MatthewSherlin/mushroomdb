#!/usr/bin/env node
"use strict";

const crypto = require("crypto");
const fs = require("fs");
const http = require("http");
const https = require("https");
const os = require("os");
const path = require("path");
const { spawnSync } = require("child_process");

const pkg = require("./package.json");
const SUPPORTED = [
  "darwin-arm64 → aarch64-apple-darwin",
  "darwin-x64 → x86_64-apple-darwin",
  "linux-x64 → x86_64-unknown-linux-gnu",
  "linux-arm64 → aarch64-unknown-linux-gnu",
];

function rustTarget(platform, arch) {
  if (platform === "darwin" && arch === "arm64") return "aarch64-apple-darwin";
  if (platform === "darwin" && arch === "x64") return "x86_64-apple-darwin";
  if (platform === "linux" && arch === "x64") return "x86_64-unknown-linux-gnu";
  if (platform === "linux" && (arch === "arm64" || arch === "aarch64")) {
    return "aarch64-unknown-linux-gnu";
  }
  return null;
}

function fail(msg) {
  process.stderr.write(msg + "\n");
  process.exit(1);
}

function fetchBuffer(url) {
  return new Promise((resolve, reject) => {
    const lib = url.startsWith("https:") ? https : http;
    const req = lib.get(url, (res) => {
      if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
        res.resume();
        fetchBuffer(res.headers.location).then(resolve, reject);
        return;
      }
      if (res.statusCode !== 200) {
        res.resume();
        reject(new Error(`GET ${url} → ${res.statusCode}`));
        return;
      }
      const chunks = [];
      res.on("data", (c) => chunks.push(c));
      res.on("end", () => resolve(Buffer.concat(chunks)));
      res.on("error", reject);
    });
    req.on("error", reject);
  });
}

function checksumOf(buf) {
  return crypto.createHash("sha256").update(buf).digest("hex");
}

function expectedChecksum(sums, filename) {
  const lines = sums.split(/\r?\n/);
  for (const line of lines) {
    const m = line.match(/^([0-9a-fA-F]{64})\s+(\S+)$/);
    if (m && path.basename(m[2]) === filename) return m[1].toLowerCase();
  }
  return null;
}

async function main() {
  const platform = process.env.MUSHROOMDB_FORCE_OS || process.platform;
  const arch = process.env.MUSHROOMDB_FORCE_ARCH || process.arch;
  const target = rustTarget(platform, arch);
  if (!target) {
    fail(
      `unsupported platform: ${platform}-${arch}\nsupported: ${SUPPORTED.join("; ")}`,
    );
  }

  const version = String(pkg.version).replace(/^v/, "");
  const tag = `v${version}`;
  const asset = `mushroomdb-${tag}-${target}.tar.gz`;
  const repo = "MatthewSherlin/graph-db";
  const base =
    process.env.MUSHROOMDB_RELEASE_BASE ||
    `https://github.com/${repo}/releases/download/${tag}`;
  const vendor = path.join(__dirname, "vendor");
  const dest = path.join(vendor, "mushroomdb");
  fs.mkdirSync(vendor, { recursive: true });

  const tarUrl = `${base.replace(/\/$/, "")}/${asset}`;
  const sumsUrl = `${base.replace(/\/$/, "")}/SHA256SUMS`;
  const tarball = await fetchBuffer(tarUrl);
  const sums = (await fetchBuffer(sumsUrl)).toString("utf8");
  const want = expectedChecksum(sums, asset);
  if (!want) {
    fail(`SHA256SUMS has no entry for ${asset}`);
  }
  const got = checksumOf(tarball);
  if (got !== want) {
    fail(`checksum mismatch for ${asset}: got ${got} want ${want}`);
  }

  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "mushroomdb-npm-"));
  const tarPath = path.join(tmp, asset);
  fs.writeFileSync(tarPath, tarball);
  const extracted = spawnSync("tar", ["-xzf", tarPath, "-C", tmp], {
    encoding: "utf8",
  });
  if (extracted.status !== 0) {
    fail(`tar extract failed: ${extracted.stderr || extracted.stdout || extracted.status}`);
  }
  const extractedBin = path.join(tmp, "mushroomdb");
  if (!fs.existsSync(extractedBin)) {
    fail(`tarball ${asset} did not contain ./mushroomdb`);
  }
  fs.copyFileSync(extractedBin, dest);
  fs.chmodSync(dest, 0o755);
  process.stdout.write(`installed ${dest} (${target} ${tag})\n`);
}

main().catch((err) => fail(err.stack || String(err)));
