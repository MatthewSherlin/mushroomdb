import { describe, expect, it } from "vitest";
import { ApiError } from "./api";
import { runRuleClick } from "./inspector";

describe("runRuleClick", () => {
  it("surfaces a thrown ApiError as verbatim strip text", async () => {
    const error = await runRuleClick(async () => {
      throw new ApiError(400, { error: "parse: unexpected end of input" });
    });
    expect(error).toBe("parse: unexpected end of input");
  });

  it("returns undefined when openWhy succeeds", async () => {
    const error = await runRuleClick(async () => undefined);
    expect(error).toBeUndefined();
  });
});
