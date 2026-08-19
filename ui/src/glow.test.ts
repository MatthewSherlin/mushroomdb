import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import { GLOW_MS, GlowQueue, bornEdgeIds } from "./glow";

const glowSrc = readFileSync(
  join(dirname(fileURLToPath(import.meta.url)), "glow.ts"),
  "utf8",
);

describe("module contract", () => {
  it("is a pure module: no DOM, canvas, or cosmos imports", () => {
    expect(glowSrc).not.toMatch(
      /from\s+["']@cosmos\.gl|document\.|window\.|HTMLCanvas|getContext\(/,
    );
  });
});

describe("dev-only test hooks", () => {
  it("installs window.__testHooks only inside an import.meta.env.DEV branch", () => {
    const explorer = readFileSync(
      join(dirname(fileURLToPath(import.meta.url)), "explorer.ts"),
      "utf8",
    );
    expect(explorer).toMatch(/if \(import\.meta\.env\.DEV\) \{[\s\S]*__testHooks/);
    const unguarded = explorer.replace(
      /if \(import\.meta\.env\.DEV\) \{[\s\S]*?^\s{4}\}/m,
      "",
    );
    expect(unguarded).not.toContain("__testHooks");
  });
});

describe("bornEdgeIds", () => {
  it("returns ids present after apply but not before, sorted", () => {
    expect(bornEdgeIds(["KNOWS|a|b"], ["KNOWS|a|b", "FIT|a|c", "related|d|a"])).toEqual(
      ["FIT|a|c", "related|d|a"],
    );
  });

  it("returns [] when the edge set did not grow", () => {
    expect(bornEdgeIds(["a"], ["a"])).toEqual([]);
    expect(bornEdgeIds(["a", "b"], ["a"])).toEqual([]);
  });
});

describe("GlowQueue", () => {
  it("schedules a 600ms pulse and expires it", () => {
    const q = new GlowQueue();
    q.schedule(["e1", "e2"], 1_000);
    expect(GLOW_MS).toBe(600);
    expect(q.active(1_000)).toEqual(["e1", "e2"]);
    expect(q.active(1_599)).toEqual(["e1", "e2"]);
    expect(q.active(1_600)).toEqual([]);
  });

  it("re-scheduling the same id extends the pulse from the new now", () => {
    const q = new GlowQueue();
    q.schedule(["e1"], 1_000);
    q.schedule(["e1"], 1_400);
    expect(q.active(1_600)).toEqual(["e1"]);
    expect(q.active(2_000)).toEqual([]);
  });

  it("prune drops expired ids and reports whether any remain", () => {
    const q = new GlowQueue();
    q.schedule(["live"], 1_000);
    q.schedule(["dead"], 200);
    expect(q.prune(900)).toBe(true);
    expect(q.active(900)).toEqual(["live"]);
    expect(q.prune(1_600)).toBe(false);
    expect(q.active(1_600)).toEqual([]);
  });

  it("nextExpiry is the soonest until, or undefined when empty", () => {
    const q = new GlowQueue();
    expect(q.nextExpiry()).toBeUndefined();
    q.schedule(["a"], 100, 50);
    q.schedule(["b"], 100, 200);
    expect(q.nextExpiry()).toBe(150);
  });
});
