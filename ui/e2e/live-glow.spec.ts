import { execFileSync } from "node:child_process";
import { readdirSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { expect, test } from "@playwright/test";
import { loadDemoNeighborhood, waitConnected } from "./helpers";

const ingestBody = JSON.stringify({
  label: "Person",
  rows: [
    {
      id: "person-96",
      name: "Person 96",
      org_id: "org-01",
      project_id: "proj-01",
      skills: ["s01", "s02", "s03"],
    },
  ],
});

test.describe("live glow", () => {
  test("production dist does not ship __testHooks", () => {
    const dist = join(dirname(fileURLToPath(import.meta.url)), "..", "dist");
    const assets = join(dist, "assets");
    const files = readdirSync(assets).filter((n) => n.endsWith(".js"));
    expect(files.length).toBeGreaterThan(0);
    for (const name of files) {
      const text = readFileSync(join(assets, name), "utf8");
      expect(text, name).not.toContain("__testHooks");
    }
  });

  test("ingest grows the canvas and the ticker; glow hook only in dev", async ({
    page,
    baseURL,
  }) => {
    await page.goto("/");
    await waitConnected(page);
    await loadDemoNeighborhood(page);
    const before = await page.locator(".label-chip").count();
    expect(before).toBeGreaterThan(0);

    execFileSync(
      "curl",
      [
        "-sS",
        "-X",
        "POST",
        `${baseURL}/ingest`,
        "-H",
        "content-type: application/json",
        "-d",
        ingestBody,
      ],
      { encoding: "utf8" },
    );

    await expect(page.locator(".ticker-last")).toContainText(/person-96|ingested/, {
      timeout: 20_000,
    });
    await expect
      .poll(async () => page.locator(".label-chip").count(), { timeout: 20_000 })
      .toBeGreaterThan(before);

    const hooks = await page.evaluate(() => window.__testHooks);
    // Production serve --ui ui/dist: hooks must be absent.
    // Vite dev: schedule should record born derived edges.
    if (hooks === undefined) {
      expect(hooks).toBeUndefined();
    } else {
      expect(hooks.glowScheduled.length).toBeGreaterThan(0);
    }
  });
});
