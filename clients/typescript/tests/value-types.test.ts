// The five format-V11 value types as the server's JSON tags carry them.
//
// Pure unit tests: no server. The shapes are the ones crates/server/src/json.rs
// emits (`tagged_json`) and accepts (`json_to_value`). A write that carries one
// is refused by the engine until TY-12 — see integration.test.ts.

import { describe, expect, it } from "vitest";
import {
  asBinary,
  asDate,
  asDateTime,
  asDuration,
  asLocalDateTime,
  binaryBytes,
} from "../src/index";
import type {
  BinaryValue,
  CellValue,
  DateTimeValue,
  DateValue,
  DurationValue,
  LocalDateTimeValue,
} from "../src/index";

const DATE: CellValue = { $date: "2024-01-15" };
const LDT: CellValue = { $ldt: "2024-01-15T12:34:56.789012" };
const DT: CellValue = { $dt: "2024-01-15T12:34:56.789012+05:30" };
const DUR: CellValue = { $dur: "P1Y2M3DT4H5M6.789S" };
const BIN: CellValue = { $binary: "3q2+7w==" };

describe("CellValue accepts the tagged forms", () => {
  it("each tagged object is a CellValue and keeps its text", () => {
    const date: CellValue = { $date: "2024-01-15" };
    expect((date as DateValue).$date).toBe("2024-01-15");
    expect((LDT as LocalDateTimeValue).$ldt).toBe("2024-01-15T12:34:56.789012");
    expect((DT as DateTimeValue).$dt).toBe("2024-01-15T12:34:56.789012+05:30");
    expect((DUR as DurationValue).$dur).toBe("P1Y2M3DT4H5M6.789S");
    expect((BIN as BinaryValue).$binary).toBe("3q2+7w==");
  });

  it("tagged values nest in a list cell", () => {
    const list: CellValue = [DATE, BIN, 1, null];
    expect(asDate((list as CellValue[])[0]!)).toBe("2024-01-15");
  });

  it("survive a JSON round trip, as they do through the server", () => {
    const wire = JSON.stringify([DATE, LDT, DT, DUR, BIN]);
    const [d, l, t, u, b] = JSON.parse(wire) as CellValue[];
    expect(asDate(d!)).toBe("2024-01-15");
    expect(asLocalDateTime(l!)).toBe("2024-01-15T12:34:56.789012");
    expect(asDateTime(t!)).toBe("2024-01-15T12:34:56.789012+05:30");
    expect(asDuration(u!)).toBe("P1Y2M3DT4H5M6.789S");
    expect(asBinary(b!)).toBe("3q2+7w==");
  });
});

describe("narrowing helpers", () => {
  it("asDate extracts tagged date string", () => {
    expect(asDate({ $date: "2024-01-15" })).toBe("2024-01-15");
    expect(asDate(42)).toBeUndefined();
    expect(asDate("2024-01-15")).toBeUndefined();
  });

  it("each helper answers only for its own tag", () => {
    const all = [DATE, LDT, DT, DUR, BIN];
    const helpers = [asDate, asLocalDateTime, asDateTime, asDuration, asBinary];
    helpers.forEach((helper, i) => {
      all.forEach((cell, j) => {
        expect(helper(cell) !== undefined).toBe(i === j);
      });
    });
  });

  it("answer undefined for every other kind of cell", () => {
    const others: CellValue[] = [0, "text", true, false, null, [], [DATE]];
    for (const cell of others) {
      expect(asDate(cell)).toBeUndefined();
      expect(asLocalDateTime(cell)).toBeUndefined();
      expect(asDateTime(cell)).toBeUndefined();
      expect(asDuration(cell)).toBeUndefined();
      expect(asBinary(cell)).toBeUndefined();
      expect(binaryBytes(cell)).toBeUndefined();
    }
  });

  it("refuse an object whose tag is not a string or that has other keys", () => {
    const loose = JSON.parse('{"$date": 5}') as CellValue;
    expect(asDate(loose)).toBeUndefined();
    const extra = JSON.parse('{"$date": "2024-01-15", "x": 1}') as CellValue;
    expect(asDate(extra)).toBeUndefined();
    const empty = JSON.parse("{}") as CellValue;
    expect(asDate(empty)).toBeUndefined();
  });

  it("an empty text is a value, not an absence", () => {
    expect(asBinary({ $binary: "" })).toBe("");
    expect(binaryBytes({ $binary: "" })).toEqual(new Uint8Array());
  });

  it("binaryBytes decodes the base64", () => {
    expect(binaryBytes(BIN)).toEqual(Uint8Array.from([0xde, 0xad, 0xbe, 0xef]));
    expect(binaryBytes({ $binary: "AA==" })).toEqual(Uint8Array.from([0]));
  });
});

describe("a null list element", () => {
  it("is a CellValue and narrows to no tagged type", () => {
    const row: CellValue = [1, null];
    expect(row).toEqual([1, null]);
    expect(asDate(null)).toBeUndefined();
    expect(asBinary(null)).toBeUndefined();
  });
});
