import { describe, expect, it } from "vitest";

import { compactTick } from "@/components/charts";

describe("compactTick", () => {
  it("keeps small values verbatim", () => {
    expect(compactTick(0)).toBe("0");
    expect(compactTick(42)).toBe("42");
    expect(compactTick(0.225)).toBe("0.23");
    expect(compactTick(0.1)).toBe("0.1");
    expect(compactTick(2.5)).toBe("2.5");
  });

  it("abbreviates thousands and beyond", () => {
    expect(compactTick(1200)).toBe("1.2k");
    expect(compactTick(6000)).toBe("6k");
    expect(compactTick(150_000)).toBe("150k");
    expect(compactTick(3_500_000)).toBe("3.5M");
    expect(compactTick(2_000_000_000)).toBe("2G");
    expect(compactTick(-1500)).toBe("-1.5k");
  });
});
