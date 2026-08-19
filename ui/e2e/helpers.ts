import { expect, type Page } from "@playwright/test";

const FIT_QUERY = `MATCH (p:Person {id: 'person-01'})-[r:FIT]->(proj:Project)
RETURN p, proj, r.score AS score
ORDER BY score DESC, proj`;

export async function waitConnected(page: Page): Promise<void> {
  await expect(page.locator(".status-dot")).toHaveAttribute(
    "data-watch",
    "connected",
    { timeout: 20_000 },
  );
}

export async function loadDemoNeighborhood(page: Page): Promise<void> {
  await page.getByRole("button", { name: "Load demo neighborhood" }).click();
  await expect(page.locator(".empty")).toBeHidden();
  await expect(page.locator(".label-chip").first()).toBeVisible();
}

export async function openConsole(page: Page): Promise<void> {
  await page.getByRole("button", { name: "Console" }).click();
  await expect(page.locator(".console")).toBeVisible();
}

export async function runCypher(page: Page, cypher: string): Promise<void> {
  const editor = page.locator(".console-input");
  await editor.fill(cypher);
  await page.getByRole("button", { name: "Run" }).click();
}

export async function addPerson01FitToCanvas(page: Page): Promise<void> {
  await openConsole(page);
  await runCypher(page, FIT_QUERY);
  await expect(page.locator(".console-table-wrap")).toContainText("person-01");
  await expect(page.locator(".console-table-wrap")).toContainText("proj-01");
  await page.getByRole("button", { name: "Add to canvas" }).click();
  await expect(page.locator(".label-chip").filter({ hasText: "Person" }).first()).toBeVisible();
}

export async function openRule(page: Page, name: string): Promise<void> {
  const rules = page.locator(".rules");
  if (!(await rules.evaluate((el) => el.classList.contains("is-open")))) {
    await page.getByRole("button", { name: "Rules" }).click();
  }
  await expect(rules).toHaveClass(/is-open/);
  await page.locator(".rules-item", { hasText: name }).click();
  await expect(page.locator(".why.is-open")).toBeVisible();
}

export async function person01Edges(
  page: Page,
): Promise<{ edge_type: string; src_key: string; dst_key: string; derived: boolean }[]> {
  const res = await page.request.get("/node/person-01/edges");
  expect(res.ok()).toBe(true);
  const body = (await res.json()) as {
    edges: {
      edge_type: string;
      src_key: string;
      dst_key: string;
      derived: boolean;
    }[];
  };
  return body.edges;
}

const KNOWS_QUERY = `MATCH (a:Person {id: 'person-01'})-[r:KNOWS]->(b:Person)
RETURN a, b`;

export { FIT_QUERY, KNOWS_QUERY };
