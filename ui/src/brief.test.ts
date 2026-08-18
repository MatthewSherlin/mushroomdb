import { readdirSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

const here = dirname(fileURLToPath(import.meta.url));
const ALLOWED_HEX = new Set([
  "#0B0E14",
  "#E8E6E1",
  "#55627A",
  "#E8A33D",
  "#6FC3B8",
]);

const HEX = /#[0-9A-Fa-f]{3,8}/g;
const RGB = /\brgba?\(/;
const HSL = /\bhsla?\(/;

function sourceFiles(): string[] {
  return readdirSync(here).filter(
    (name) =>
      (name.endsWith(".ts") || name.endsWith(".css")) &&
      !name.endsWith(".test.ts"),
  );
}

describe("design brief guards", () => {
  it("allows only the five brief hexes in shipped source (no inline hue drift)", () => {
    const found: string[] = [];
    for (const name of sourceFiles()) {
      const text = readFileSync(join(here, name), "utf8");
      if (name.endsWith(".css")) {
        expect(text, name).not.toMatch(RGB);
        expect(text, name).not.toMatch(HSL);
      }
      for (const hex of text.match(HEX) ?? []) {
        if (!ALLOWED_HEX.has(hex)) {
          found.push(`${name}:${hex}`);
        }
      }
    }
    expect(found).toEqual([]);
  });

  it("uses Space Grotesk only for the wordmark and panel titles", () => {
    const css = readFileSync(join(here, "style.css"), "utf8");
    const hits = [...css.matchAll(/([^{}]+)\{[^}]*font-family:\s*"Space Grotesk"/g)];
    const selectors = hits.map((m) => m[1]!.trim());
    expect(selectors).toEqual([".wordmark-name", ".panel-title"]);
  });

  it("puts data surfaces in IBM Plex Mono", () => {
    const css = readFileSync(join(here, "style.css"), "utf8");
    for (const sel of [
      ".label-chip",
      ".hover-card",
      ".error-strip",
      ".ticker-last",
      ".console-hint",
      ".console-hl",
      ".console-table",
      ".console-error",
      ".rules-name",
      ".why-rule",
      ".why-line",
      ".why-tok",
      ".rules-tripped",
    ]) {
      expect(css, sel).toMatch(
        new RegExp(
          `${sel.replace(".", "\\.")}[^{]*\\{[^}]*IBM Plex Mono|${sel.replace(".", "\\.")},`,
        ),
      );
    }
  });

  it("keeps a --signal outline on :focus-visible plus a forced-colors fallback", () => {
    const css = readFileSync(join(here, "style.css"), "utf8");
    expect(css).toMatch(
      /:focus-visible\s*\{[^}]*outline:\s*2px solid var\(--signal\)/,
    );
    expect(css).toMatch(
      /\.console-input:focus-visible\s*\{[^}]*outline:\s*2px solid var\(--signal\)/,
    );
    expect(css).toMatch(
      /\.console-input:focus-visible\s*\{[^}]*box-shadow:\s*inset 0 0 0 2px var\(--signal\)/,
    );
    expect(css).toMatch(/@media \(forced-colors: active\)/);
    expect(css).toMatch(/outline:\s*2px solid CanvasText/);
  });

  it("disables slide-over transitions under prefers-reduced-motion", () => {
    const css = readFileSync(join(here, "style.css"), "utf8");
    expect(css).toMatch(/@media \(prefers-reduced-motion: reduce\)/);
    expect(css).toMatch(/\.why,\s*\.rules\s*\{[^}]*transition:\s*none/);
  });

  it("keeps the layout floor at 1024px and drawers off the rail", () => {
    const css = readFileSync(join(here, "style.css"), "utf8");
    expect(css).toMatch(/min-width:\s*1024px/);
    expect(css).toMatch(/\.console\s*\{[^}]*left:\s*48px/);
    expect(css).toMatch(/\.rules\s*\{[^}]*left:\s*48px/);
    expect(css).toMatch(
      /\.rules\s*\{[^}]*transform:\s*translateX\(calc\(-100% - 48px\)\)/,
    );
  });
});
