import { expect, test } from "@playwright/test";
import {
  addPerson01FitToCanvas,
  loadDemoNeighborhood,
  openRule,
  waitConnected,
} from "./helpers";

test.describe("demo flow", () => {
  test("empty state, demo neighborhood, console add-to-canvas, typed edge", async ({
    page,
  }) => {
    await page.goto("/");
    await expect(page.locator(".empty")).toBeVisible();
    await expect(page.locator(".empty p")).toHaveText("Open a node to start");
    await expect(
      page.getByRole("button", { name: "Load demo neighborhood" }),
    ).toBeVisible();
    await expect(page.locator(".ticker-last")).toHaveText("no events");
    await waitConnected(page);

    await loadDemoNeighborhood(page);
    const chips = page.locator(".label-chip");
    await expect.poll(async () => chips.count()).toBeGreaterThan(1);
    const labels = await chips.allTextContents();
    expect(labels).toContain("Org");
    expect(labels.some((l) => l === "Person" || l === "Project")).toBe(true);

    await addPerson01FitToCanvas(page);
    await expect(page.locator(".label-chip").filter({ hasText: "Person" }).first()).toBeVisible();
    await expect(page.locator(".label-chip").filter({ hasText: "Project" }).first()).toBeVisible();

    // Demo has no hand-inserted edges; auto-FK ORG is a real etype (not
    // the legacy `related` synthesis). Inspector is the headless surface.
    await openRule(page, "auto_fk_person_org_id");
    await expect(page.locator(".why-etype")).toHaveText("ORG");
    await expect(page.locator(".why")).not.toContainText("related");
    await expect(page.locator(".why-line")).toContainText("org_id");
  });
});
