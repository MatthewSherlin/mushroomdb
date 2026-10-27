import { randomUUID } from "node:crypto";
import { expect, inject, it } from "vitest";
import { MushroomClient } from "../src/index.js";

it("the graph quickstart returns exactly Alice and Bob", async () => {
  // A missing server is a failed quickstart, never a skipped success.
  expect(inject("serverReady")).toBe(true);
  const client = new MushroomClient(inject("baseUrl") as string);
  const suffix = randomUUID();
  const alice = `quickstart-alice-${suffix}`;
  const bob = `quickstart-bob-${suffix}`;
  await client.createNode("Person", alice, { name: "Alice" });
  await client.createNode("Person", bob, { name: "Bob" });
  await client.createEdge("KNOWS", alice, bob);
  const result = await client.query(
    "MATCH (a:Person)-[:KNOWS]->(b) WHERE key(a) = $alice AND key(b) = $bob RETURN a.name, b.name",
    { params: { alice, bob } },
  );
  expect(result.columns).toEqual(["a.name", "b.name"]);
  expect(result.rows).toEqual([["Alice", "Bob"]]);
});
