import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import { highlightHtml, tokenize } from "./cypher";

const here = dirname(fileURLToPath(import.meta.url));
const src = readFileSync(join(here, "cypher.ts"), "utf8");

describe("module contract", () => {
  it("is a pure module: no DOM, canvas, or cosmos imports", () => {
    expect(src).not.toMatch(
      /from\s+["']@cosmos\.gl|document\.|window\.|HTMLCanvas|getContext\(/,
    );
  });
});

describe("tokenize", () => {
  it("tags MATCH / RETURN as keywords and leaves identifiers plain", () => {
    expect(tokenize("MATCH (n) RETURN n")).toEqual([
      { kind: "keyword", text: "MATCH" },
      { kind: "space", text: " " },
      { kind: "punct", text: "(" },
      { kind: "ident", text: "n" },
      { kind: "punct", text: ")" },
      { kind: "space", text: " " },
      { kind: "keyword", text: "RETURN" },
      { kind: "space", text: " " },
      { kind: "ident", text: "n" },
    ]);
  });

  it("keeps keyword casing and treats match as a keyword", () => {
    expect(tokenize("match n")).toEqual([
      { kind: "keyword", text: "match" },
      { kind: "space", text: " " },
      { kind: "ident", text: "n" },
    ]);
  });

  it("tokenizes strings, numbers, and line comments", () => {
    expect(tokenize("// hi\nRETURN 'a\\'b' 12")).toEqual([
      { kind: "comment", text: "// hi" },
      { kind: "space", text: "\n" },
      { kind: "keyword", text: "RETURN" },
      { kind: "space", text: " " },
      { kind: "string", text: "'a\\'b'" },
      { kind: "space", text: " " },
      { kind: "number", text: "12" },
    ]);
  });
});

describe("highlightHtml", () => {
  it("wraps keywords and escapes markup in the source", () => {
    const html = highlightHtml("RETURN '<b>'");
    expect(html).toContain('<span class="hl-kw">RETURN</span>');
    expect(html).toContain('<span class="hl-str">\'&lt;b&gt;\'</span>');
    expect(html).not.toContain("<b>");
  });

  it("escapes double quotes in string tokens", () => {
    const html = highlightHtml('RETURN "a"');
    expect(html).toContain('<span class="hl-str">&quot;a&quot;</span>');
    expect(html).not.toMatch(/hl-str">"/);
  });
});
