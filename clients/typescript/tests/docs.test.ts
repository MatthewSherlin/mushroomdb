// The TypeScript page names every method the client has.
//
// docs/site/typescript.md has a table with one row a public method of
// MushroomClient, written `client.<name>(`. A method added to the class and
// not to the page fails here by name; so does a row for a method that is gone.

import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import { MushroomClient } from "../src/index";

const here = dirname(fileURLToPath(import.meta.url));
const page = readFileSync(join(here, "../../../docs/site/typescript.md"), "utf8");

// `private` is a compile-time word in TypeScript: these four are on the
// prototype at run time, and are not part of what a caller uses.
const PRIVATE = ["url", "wsUrl", "authHeaders", "fetchJson"];

const methods = Object.getOwnPropertyNames(MushroomClient.prototype)
  .filter((name) => name !== "constructor" && !PRIVATE.includes(name))
  .sort();

const documented = [...page.matchAll(/`client\.([A-Za-z]+)\(/g)].map((m) => m[1]);

describe("docs/site/typescript.md", () => {
  it("finds the client's methods at all", () => {
    expect(methods.length).toBeGreaterThanOrEqual(12);
  });

  it("has a row for every public method of MushroomClient", () => {
    expect(methods.filter((name) => !documented.includes(name))).toEqual([]);
  });

  it("has no row for a method the client does not have", () => {
    expect([...new Set(documented)].filter((name) => !methods.includes(name))).toEqual([]);
  });
});
