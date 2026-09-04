#!/usr/bin/env node
// SDK-level MCP handshake check.
//
// Speaks to a real mushroomdb MCP server over stdio using the official
// @modelcontextprotocol/sdk client — not a hand-rolled JSON-RPC probe — so a
// break in the wire protocol, the tool list, or the reported serverInfo shows
// up the same way it would to a real assistant host.
//
// Two ways to point it at a server:
//   --command <bin> --db <dir>     spawn `<bin> mcp <dir>` directly
//   --config <file> --server <name>  read command/args for mcpServers.<name>
//                                     from an .mcp.json-shaped file
//
// Optional checks:
//   --expect-version <v>   serverInfo.version must equal <v>
//   --expect-tools a,b,c   every named tool must be in tools/list
//   --call <tool> <json>   after the base checks, call one more tool and
//                          print its text content (so a caller can grep it)
//
// On success: prints `ok: <n> tools, serverInfo <name> <version>` and exits 0.
// Any failure — connection, an assertion, a malformed argument — throws,
// prints the error to stderr, and exits 1. Nothing here decides pass/fail by
// parsing its own stdout; every check is an assertion that throws.

import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";
import { readFileSync } from "node:fs";

const SERVER_NAME = "mushroomdb";

// The SDK's default request timeout (60s) covers a warm process fine, but the
// very first request — initialize — has to wait out the child process too.
// When that child is `npx -y mushroomdb@<v> …` on a cold cache (no local
// tarball yet, prewarm skipped or itself timed out), fetching, extracting,
// and resolving the platform-specific native binary can genuinely take
// longer than 60s. Give the handshake alone a longer budget so a slow-but-
// real npx download doesn't read as a broken server.
const CONNECT_TIMEOUT_MS = 120_000;

function parseArgs(argv) {
  const opts = { expectTools: null, call: null };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    switch (a) {
      case "--command":
        opts.command = argv[++i];
        break;
      case "--db":
        opts.db = argv[++i];
        break;
      case "--config":
        opts.config = argv[++i];
        break;
      case "--server":
        opts.server = argv[++i];
        break;
      case "--expect-version":
        opts.expectVersion = argv[++i];
        break;
      case "--expect-tools":
        opts.expectTools = (argv[++i] ?? "")
          .split(",")
          .map((s) => s.trim())
          .filter(Boolean);
        break;
      case "--call":
        opts.call = { tool: argv[++i], argsJson: argv[++i] };
        break;
      default:
        throw new Error(`unrecognized argument: ${a}`);
    }
  }
  return opts;
}

/** Resolve the (command, args) to spawn from the parsed options. */
function resolveServerParams(opts) {
  if (opts.command) {
    if (!opts.db) {
      throw new Error("--command requires --db <dir>");
    }
    return { command: opts.command, args: ["mcp", opts.db] };
  }
  if (opts.config) {
    if (!opts.server) {
      throw new Error("--config requires --server <name>");
    }
    const raw = readFileSync(opts.config, "utf8");
    const config = JSON.parse(raw);
    const entry = config?.mcpServers?.[opts.server];
    if (!entry || typeof entry.command !== "string") {
      throw new Error(
        `${opts.config}: mcpServers.${opts.server} is missing or has no command`,
      );
    }
    return { command: entry.command, args: entry.args ?? [] };
  }
  throw new Error("either --command <bin> --db <dir> or --config <file> --server <name> is required");
}

async function main() {
  const opts = parseArgs(process.argv.slice(2));
  const { command, args } = resolveServerParams(opts);

  const transport = new StdioClientTransport({ command, args });
  const client = new Client(
    { name: "mushroomdb-mcp-handshake", version: "1.0.0" },
    { capabilities: {} },
  );

  try {
    await client.connect(transport, { timeout: CONNECT_TIMEOUT_MS });

    const serverInfo = client.getServerVersion();
    if (!serverInfo) {
      throw new Error("server did not report serverInfo during initialize");
    }
    if (serverInfo.name !== SERVER_NAME) {
      throw new Error(
        `serverInfo.name mismatch: expected "${SERVER_NAME}", got "${serverInfo.name}"`,
      );
    }
    if (opts.expectVersion && serverInfo.version !== opts.expectVersion) {
      throw new Error(
        `serverInfo.version mismatch: expected "${opts.expectVersion}", got "${serverInfo.version}"`,
      );
    }

    const { tools } = await client.listTools();
    if (opts.expectTools) {
      const names = new Set(tools.map((t) => t.name));
      const missing = opts.expectTools.filter((name) => !names.has(name));
      if (missing.length > 0) {
        throw new Error(`tools/list is missing expected tool(s): ${missing.join(", ")}`);
      }
    }

    // Every handshake calls `map` on the target store (the CI job points it
    // at an empty one) so a break in the tool-call path — not just tools/list
    // — fails the same way a real client would see it.
    const mapResult = await client.callTool({ name: "map", arguments: {} });
    if (mapResult.isError) {
      throw new Error(`map tool call returned an error: ${JSON.stringify(mapResult.content)}`);
    }

    if (opts.call) {
      let callArgs;
      try {
        callArgs = JSON.parse(opts.call.argsJson);
      } catch (err) {
        throw new Error(`--call ${opts.call.tool}: could not parse JSON args: ${err.message}`);
      }
      const callResult = await client.callTool({ name: opts.call.tool, arguments: callArgs });
      if (callResult.isError) {
        throw new Error(
          `${opts.call.tool} tool call returned an error: ${JSON.stringify(callResult.content)}`,
        );
      }
      const text = (callResult.content ?? [])
        .filter((c) => c.type === "text")
        .map((c) => c.text)
        .join("\n");
      console.log(`call ${opts.call.tool}:`);
      console.log(text);
    }

    console.log(`ok: ${tools.length} tools, serverInfo ${serverInfo.name} ${serverInfo.version}`);
  } finally {
    await client.close();
  }
}

main().catch((err) => {
  console.error(err && err.stack ? err.stack : String(err));
  process.exit(1);
});
