/**
 * Regression specs for two UI defects found during dogfood:
 *
 * F1: Rules slide-over (z-index 7) intercepted pointer events over the
 *     console's Run button (z-index 6) when both panels were open.
 *     Fix: console raised to z-index 8 (console drawer above rules panel).
 *
 * F2: addHarvestedToCanvas called expandNode for every added node at depth-1,
 *     which blocked the UI >5 s for dense hubs (400+ edges).
 *     Fix: degree > 50 → skip auto-expansion, mark dirty, show progress hint.
 */
import { expect, test } from "@playwright/test";
import { openConsole, waitConnected } from "./helpers";

// ── F1 ────────────────────────────────────────────────────────────────────────

test.describe("F1: rules + console stacking", () => {
  test("Run button stays clickable when rules panel and console are both open", async ({
    page,
  }) => {
    await page.goto("/");
    await waitConnected(page);

    // Open the rules side panel first.
    await page.getByRole("button", { name: "Rules" }).click();
    await expect(page.locator(".rules")).toHaveClass(/is-open/);

    // Open the console drawer — both panels are now visible simultaneously.
    await openConsole(page);
    await expect(page.locator(".console")).toBeVisible();

    // Type a query and click Run.
    // If the rules panel (z-index 7) intercepts pointer events over the console
    // bar, this click is swallowed and the table never appears — reproducing the
    // original 30 s playwright timeout from the dogfood report.
    const editor = page.locator(".console-input");
    await editor.fill("MATCH (n) RETURN n LIMIT 25");

    // force: false (default) → playwright checks that the element actually
    // receives the click; if the rules panel blocks it the check fails.
    await page.getByRole("button", { name: "Run" }).click();

    // A table (possibly 0 rows) must appear — proves the click landed.
    await expect(page.locator(".console-table-wrap table")).toBeVisible({
      timeout: 10_000,
    });
  });
});

// ── F2 ────────────────────────────────────────────────────────────────────────

test.describe("F2: dense hub add performance", () => {
  /**
   * Seed org-01 as a dense hub. The demo data's auto_fk_person_org_id rule
   * already exists. Ingesting 55 extra Person nodes pointing to org-01 pushes
   * its edge count well above DENSE_HUB_DEGREE_THRESHOLD (50).
   * Seeding is idempotent — safe on reused servers (reuseExistingServer).
   */
  async function seedDenseHub(page: Parameters<typeof waitConnected>[0]) {
    const persons = Array.from({ length: 55 }, (_, i) => ({
      id: `bulk-person-${i}`,
      org_id: "org-01",
    }));
    const resp = await page.request.post("/ingest", {
      data: {
        label: "Person",
        rows: persons,
        options: { auto_fk: { suffix: "_id" } },
      },
    });
    expect(resp.ok()).toBe(true);

    // Poll until org-01 has accumulated >50 edges (auto_fk rule fires).
    await expect
      .poll(
        async () => {
          try {
            const r = await page.request.get("/node/org-01/edges");
            const body = (await r.json()) as { edges: unknown[] };
            return body.edges.length;
          } catch {
            return 0;
          }
        },
        { timeout: 15_000, intervals: [500] },
      )
      .toBeGreaterThan(50);
  }

  test("Add dense hub returns in <1 s and shows expandable hint", async ({
    page,
  }) => {
    await page.goto("/");
    await waitConnected(page);
    await seedDenseHub(page);

    // Open console and query for org-01 (the dense hub).
    await openConsole(page);
    const editor = page.locator(".console-input");
    await editor.fill("MATCH (n:Org {id: 'org-01'}) RETURN n");
    await page.getByRole("button", { name: "Run" }).click();
    // Wait for result table — hub key must appear.
    await expect(page.locator(".console-table-wrap")).toContainText(
      "org-01",
      { timeout: 10_000 },
    );

    // Click Add to canvas and measure wall-clock time.
    const t0 = Date.now();
    await page.getByRole("button", { name: "Add to canvas" }).click();

    // The console-hint must show the dense-node skip message — confirming
    // auto-expansion was NOT attempted (which would have blocked >5 s).
    await expect(page.locator(".console-hint")).toContainText(
      "click to expand",
      { timeout: 5_000 },
    );
    const elapsed = Date.now() - t0;

    // Dense hub skipped expansion → Add must finish well under 1 s.
    expect(elapsed).toBeLessThan(1_000);

    await expect(page.locator(".console-hint")).toContainText("dense node");
  });
});
