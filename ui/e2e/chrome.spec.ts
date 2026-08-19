import { readdirSync, statSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { expect, test, type Page } from "@playwright/test";
import { loadDemoNeighborhood, waitConnected } from "./helpers";

const SDD = join(
  dirname(fileURLToPath(import.meta.url)),
  "..",
  "..",
  ".superpowers",
  "sdd",
  "2026-08-19-plan-9-launch",
);

const MAIN_BUDGET = 250 * 1024;

test.describe("chrome", () => {
  test("rail names, zoom controls, and edge legend stay wired to the store", async ({
    page,
  }) => {
    await page.goto("/");
    await expect(page.getByRole("button", { name: "Explore" })).toBeVisible();
    await expect(page.getByRole("button", { name: "Console" })).toBeVisible();
    await expect(page.getByRole("button", { name: "Rules" })).toBeVisible();
    await waitConnected(page);
    await loadDemoNeighborhood(page);

    const toolbar = page.getByRole("toolbar", { name: "Canvas" });
    await expect(toolbar).toBeVisible();
    const zoomIn = toolbar.getByRole("button", { name: "Zoom in" });
    const zoomOut = toolbar.getByRole("button", { name: "Zoom out" });
    const fit = toolbar.getByRole("button", { name: "Fit" });
    await expect(zoomIn).toBeVisible();
    await expect(zoomOut).toBeVisible();
    await expect(fit).toBeVisible();
    await zoomIn.focus();
    await expect(zoomIn).toBeFocused();
    await page.keyboard.press("Tab");
    await expect(zoomOut).toBeFocused();
    await page.keyboard.press("Tab");
    await expect(fit).toBeFocused();

    const legend = page.locator(".legend");
    await expect(legend).toBeVisible();
    await expect(legend).toContainText("ORG");
    const before = await legend.locator(".legend-item").count();
    expect(before).toBeGreaterThan(0);

    await page.getByRole("button", { name: "Edges" }).click();
    await expect(page.locator(".legend-list")).toBeHidden();
    await page.getByRole("button", { name: "Edges" }).click();
    await expect(page.locator(".legend-list")).toBeVisible();
    await expect(legend.locator(".legend-item")).toHaveCount(before);
  });

  test("demo graph ring vs settled forces", async ({ page }) => {
    await page.goto("/");
    await waitConnected(page);
    await loadDemoNeighborhood(page);
    await expect(page.locator(".label-chip").first()).toBeVisible();
    const ring = await chipLayout(page);
    await page.screenshot({
      path: join(SDD, "task-4-before-settle.png"),
      fullPage: true,
    });

    await waitLayoutSettled(page);
    const settled = await chipLayout(page);
    expect(settled).not.toEqual(ring);
    await page.screenshot({
      path: join(SDD, "task-4-after-settle.png"),
      fullPage: true,
    });
  });
});

async function chipLayout(page: Page): Promise<string> {
  return page
    .locator(".label-chip")
    .evaluateAll((els) =>
      els
        .map((el) => {
          const node = el as HTMLElement;
          const x = Math.round(Number.parseFloat(node.style.left) / 4);
          const y = Math.round(Number.parseFloat(node.style.top) / 4);
          return `${x},${y}`;
        })
        .join("|"),
    );
}

async function waitLayoutSettled(page: Page): Promise<void> {
  let prev = "";
  let stable = 0;
  for (let i = 0; i < 50; i++) {
    const pos = await chipLayout(page);
    if (pos !== "" && pos === prev) {
      stable += 1;
      if (stable >= 4) {
        return;
      }
    } else {
      stable = 0;
      prev = pos;
    }
    await page.waitForTimeout(200);
  }
}

test.describe("bundle", () => {
  test("cosmos.gl is its own chunk and the entry stays under 250 kB", () => {
    const dist = join(dirname(fileURLToPath(import.meta.url)), "..", "dist");
    const assets = join(dist, "assets");
    const files = readdirSync(assets).filter((n) => n.endsWith(".js"));
    expect(files.length).toBeGreaterThan(1);

    const cosmos = files.filter((n) => n.startsWith("cosmos-"));
    expect(cosmos.length).toBeGreaterThan(0);

    let cosmosBytes = 0;
    for (const name of cosmos) {
      cosmosBytes += statSync(join(assets, name)).size;
    }
    expect(cosmosBytes).toBeGreaterThan(100_000);

    const entry = files.find((n) => n.startsWith("index-") || n.startsWith("main-"));
    expect(entry).toBeDefined();
    const entryBytes = statSync(join(assets, entry!)).size;
    expect(entryBytes).toBeLessThan(MAIN_BUDGET);

    const appSansCosmos = files
      .filter((n) => !n.startsWith("cosmos-"))
      .reduce((sum, name) => sum + statSync(join(assets, name)).size, 0);
    expect(appSansCosmos).toBeLessThan(MAIN_BUDGET);
  });
});
