import { expect, test } from "@playwright/test";
import {
  addPerson01FitToCanvas,
  KNOWS_QUERY,
  loadDemoNeighborhood,
  openRule,
  person01Edges,
  runCypher,
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

    // Seeded user edge (HTTP /ingest is nodes-only). derived:false is the
    // canvas color contract (structure, not gold). Headless GPU link-pick is
    // not used; the table + /edges payload are the same merge input as paint.
    const edges = await person01Edges(page);
    expect(edges).toContainEqual({
      edge_type: "KNOWS",
      src_key: "person-01",
      dst_key: "person-02",
      derived: false,
    });
    expect(edges.some((e) => e.edge_type === "related")).toBe(false);
    await runCypher(page, KNOWS_QUERY);
    await expect(page.locator(".console-table-wrap")).toContainText("person-01");
    await expect(page.locator(".console-table-wrap")).toContainText("person-02");

    // FK-derived ORG still covers auto-inference (not the user-edge path).
    await openRule(page, "auto_fk_person_org_id");
    await expect(page.locator(".why-etype")).toHaveText("ORG");
    await expect(page.locator(".why")).not.toContainText("related");
    await expect(page.locator(".why-line")).toContainText("org_id");
  });
});
