import { describe, expect, it } from "vitest";

import { parseLimit } from "@/pages/apps/AppForm";

describe("parseLimit", () => {
  it("empty → undefined (leave unchanged)", () => {
    expect(parseLimit("")).toBeUndefined();
    expect(parseLimit("   ")).toBeUndefined();
  });
  it("non-negative integers pass, honouring the minimum", () => {
    expect(parseLimit("0")).toBe(0);
    expect(parseLimit("250")).toBe(250);
    expect(parseLimit("0", 1)).toBeNull();
    expect(parseLimit("1", 1)).toBe(1);
  });
  it("rejects negatives, decimals, text and unsafe integers", () => {
    expect(parseLimit("-1")).toBeNull();
    expect(parseLimit("1.5")).toBeNull();
    expect(parseLimit("abc")).toBeNull();
    expect(parseLimit("9007199254740993")).toBeNull();
  });
});
