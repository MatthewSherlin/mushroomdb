#!/usr/bin/env node
"use strict";

const crypto = require("crypto");
const fs = require("fs");
const http = require("http");
const https = require("https");
const path = require("path");
const zlib = require("zlib");

const pkg = require("./package.json");
const SUPPORTED = [
  "darwin-arm64 → aarch64-apple-darwin",
  "darwin-x64 → x86_64-apple-darwin",
  "linux-x64 → x86_64-unknown-linux-gnu",
  "linux-x64 with musl → x86_64-unknown-linux-musl",
  "linux-arm64 → aarch64-unknown-linux-gnu",
  "win32-x64 → x86_64-pc-windows-msvc",
];

// Whether this Linux is a musl one (Alpine). Node reports the glibc it runs
// on; a Node built for musl reports none. MUSHROOMDB_FORCE_LIBC overrides.
function isMusl() {
  const forced = process.env.MUSHROOMDB_FORCE_LIBC;
  if (forced) return forced === "musl";
  try {
    const header = process.report.getReport().header;
    return !header.glibcVersionRuntime;
  } catch (_) {
    return false;
  }
}

function rustTarget(platform, arch) {
  if (platform === "darwin" && arch === "arm64") return "aarch64-apple-darwin";
  if (platform === "darwin" && arch === "x64") return "x86_64-apple-darwin";
  if (platform === "win32" && arch === "x64") return "x86_64-pc-windows-msvc";
  if (platform === "linux" && arch === "x64") {
    return isMusl() ? "x86_64-unknown-linux-musl" : "x86_64-unknown-linux-gnu";
  }
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

// The bytes of the file named `name` inside a gzipped tarball, or null.
//
// Read here and not by running `tar`: there is not always one, and where
// there are two — Git Bash on Windows puts GNU tar ahead of the system's —
// the one found first reads a path that starts `C:` as a remote host.
function fileFromTarball(tarball, name) {
  const tar = zlib.gunzipSync(tarball);
  for (let at = 0; at + 512 <= tar.length; ) {
    const header = tar.subarray(at, at + 512);
    if (header.every((b) => b === 0)) break;
    const entry = header.subarray(0, 100).toString("utf8").replace(/\0.*$/, "");
    const size = parseInt(header.subarray(124, 136).toString("utf8").trim(), 8) || 0;
    const type = String.fromCharCode(header[156] || 48);
    const body = at + 512;
    if ((type === "0" || type === "\0") && path.posix.basename(entry) === name) {
      if (body + size > tar.length) return null;
      return tar.subarray(body, body + size);
    }
    at = body + Math.ceil(size / 512) * 512;
  }
  return null;
}

function checksumOf(buf) {
  return crypto.createHash("sha256").update(buf).digest("hex");
}

function expectedChecksum(sums, filename) {
  const lines = sums.split(/\r?\n/);
  for (const line of lines) {
    // `sha256sum` marks a file read in binary mode with `*` before its
    // name, and on Windows that is every file.
    const m = line.match(/^([0-9a-fA-F]{64})\s+\*?(\S+)$/);
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
  const repo = "MatthewSherlin/mushroomdb";
  const base =
    process.env.MUSHROOMDB_RELEASE_BASE ||
    `https://github.com/${repo}/releases/download/${tag}`;
  const vendor = path.join(__dirname, "vendor");
  const binName = platform === "win32" ? "mushroomdb.exe" : "mushroomdb";
  const dest = path.join(vendor, binName);
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

  const binary = fileFromTarball(tarball, binName);
  if (!binary) {
    fail(`tarball ${asset} did not contain ./${binName}`);
  }
  fs.writeFileSync(dest, binary);
  fs.chmodSync(dest, 0o755);
  process.stdout.write(`installed ${dest} (${target} ${tag})\n`);
}

main().catch((err) => fail(err.stack || String(err)));
