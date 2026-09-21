import { describe, expect, it } from "vitest";

import { shortId } from "@/ui/Primitives";

describe("shortId", () => {
  it("keeps the random tail of UUIDs so v7 ids minted together stay distinguishable", () => {
    expect(shortId("01a0c523-9426-755b-bdca-e70cc44eae81")).toBe("01a0c523…ae81");
    expect(shortId("01a0c523-9426-755b-bdca-e70cc44e0f00")).toBe("01a0c523…0f00");
  });
  it("returns the id untouched when it fits", () => {
    expect(shortId("01a0c523-9426-755b-bdca-e70cc44eae81", 36)).toBe("01a0c523-9426-755b-bdca-e70cc44eae81");
    expect(shortId("abc")).toBe("abc");
  });
  it("truncates non-UUID ids by prefix", () => {
    expect(shortId("session-key-0123456789")).toBe("session-");
  });
});
