import { expect, test } from "@playwright/test";
import {
  addPerson01FitToCanvas,
  loadDemoNeighborhood,
  openRule,
  waitConnected,
} from "./helpers";

test.describe("why panel", () => {
  test.beforeEach(async ({ page }) => {
    await page.goto("/");
    await waitConnected(page);
    await loadDemoNeighborhood(page);
    await addPerson01FitToCanvas(page);
  });

  test("founded_within arithmetic on org-01 → org-02", async ({ page }) => {
    await openRule(page, "founded_within");
    await expect(page.locator(".why-rule")).toHaveText("founded_within");
    await expect(page.locator(".why-line")).toHaveText(
      "numeric_within(founded_year) = |2010 − 2011| = 1 ≤ 2",
    );
  });

  test("skill_fit overlap arithmetic on person-01 → proj-01", async ({
    page,
  }) => {
    await openRule(page, "skill_fit");
    await expect(page.locator(".why-rule")).toHaveText("skill_fit");
    await expect(page.locator(".why-line")).toHaveText(
      "overlap(skills) = |{s01, s02, s03}| / |{s01, s02, s03}| = 1",
    );
  });

  test("nearby_office geo arithmetic on org-01 → org-07", async ({ page }) => {
    await openRule(page, "nearby_office");
    await expect(page.locator(".why-rule")).toHaveText("nearby_office");
    await expect(page.locator(".why-line")).toHaveText(
      "geo_radius(office) = 3.2 km ≤ 50 km",
    );
  });
});
