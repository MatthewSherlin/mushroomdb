#!/usr/bin/env python3
"""What the MCP tool listing costs a session, in bytes — spec §8.5.

A host sends the `tools` array of `tools/list` to the model before the first
turn of every session, so its size is a cost every session pays. This
measures it the one way the published numbers were taken: the array as
compact JSON (no whitespace, non-ASCII kept as UTF-8), counted in bytes,
from a real `mushroomdb mcp` process on an empty store.

    python3 scripts/measure-tool-listing.py --binary target/debug/mushroomdb \
        [--expect-default N] [--expect-all M]

Prints one line per listing. With an `--expect-*` count it exits 1 when the
listing has a different number of tools, so a measurement cannot silently be
of the wrong surface. Needs nothing beyond the standard library.
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
import tempfile

#: A rough conversion, labelled as such wherever it is printed: no tokenizer is
#: called, because this script must run without a model API.
BYTES_PER_TOKEN_ESTIMATE = 4


def tools_list(binary: str, all_tools: bool) -> list[dict]:
    with tempfile.TemporaryDirectory() as store:
        cmd = [binary, "mcp", store] + (["--all-tools"] if all_tools else [])
        requests = [
            {"jsonrpc": "2.0", "id": 1, "method": "initialize",
             "params": {"protocolVersion": "2024-11-05", "capabilities": {},
                        "clientInfo": {"name": "measure-tool-listing", "version": "0"}}},
            {"jsonrpc": "2.0", "method": "notifications/initialized"},
            {"jsonrpc": "2.0", "id": 2, "method": "tools/list"},
        ]
        stdin = "".join(json.dumps(r) + "\n" for r in requests)
        out = subprocess.run(cmd, input=stdin, capture_output=True, text=True,
                             timeout=120, check=True).stdout
    for line in out.splitlines():
        reply = json.loads(line)
        if reply.get("id") == 2:
            return reply["result"]["tools"]
    raise SystemExit(f"no tools/list reply from {cmd}: {out!r}")


def size(tools: list[dict]) -> int:
    return len(json.dumps(tools, separators=(",", ":"), ensure_ascii=False).encode("utf-8"))


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--binary", required=True)
    ap.add_argument("--expect-default", type=int)
    ap.add_argument("--expect-all", type=int)
    a = ap.parse_args()
    failed = False
    for label, all_tools, expect in (("default", False, a.expect_default),
                                     ("--all-tools", True, a.expect_all)):
        tools = tools_list(a.binary, all_tools)
        n = size(tools)
        print(f"{label}: {len(tools)} tools, {n} bytes "
              f"(~{n // BYTES_PER_TOKEN_ESTIMATE} tokens at an estimated "
              f"{BYTES_PER_TOKEN_ESTIMATE} bytes per token)")
        if expect is not None and len(tools) != expect:
            print(f"{label}: expected {expect} tools, got {len(tools)}", file=sys.stderr)
            failed = True
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
