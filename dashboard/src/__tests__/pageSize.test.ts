import { describe, expect, it } from "vitest";

import { DEFAULT_PAGE_SIZE, isPageSize, PAGE_SIZES, parsePageSize } from "@/lib/usePageSize";

describe("page size preference", () => {
  it("offers sizes from 10 upwards with a 25-row default", () => {
    expect(PAGE_SIZES[0]).toBe(10);
    expect(PAGE_SIZES).toContain(DEFAULT_PAGE_SIZE);
    expect(DEFAULT_PAGE_SIZE).toBe(25);
  });
  it("accepts only the offered sizes", () => {
    for (const n of PAGE_SIZES) expect(isPageSize(n)).toBe(true);
    expect(isPageSize(0)).toBe(false);
    expect(isPageSize(15)).toBe(false);
    expect(isPageSize(1000)).toBe(false);
  });
  it("parses stored values and falls back on garbage", () => {
    expect(parsePageSize("50")).toBe(50);
    expect(parsePageSize("10")).toBe(10);
    expect(parsePageSize(null)).toBe(DEFAULT_PAGE_SIZE);
    expect(parsePageSize("")).toBe(DEFAULT_PAGE_SIZE);
    expect(parsePageSize("abc")).toBe(DEFAULT_PAGE_SIZE);
    expect(parsePageSize("999")).toBe(DEFAULT_PAGE_SIZE);
  });
});
