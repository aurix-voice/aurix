import { describe, expect, it } from "vitest";

import { asNumber, asRecord, asString, asStringArray, banState, isSafetyType, isUuid, SYSTEM_USER } from "@/pages/moderation/model";

describe("moderation model helpers", () => {
  it("isUuid accepts canonical ids (any case, trimmed) and rejects prefixes", () => {
    expect(isUuid(" 4F3B2C1D-0000-4000-8000-000000000001 ")).toBe(true);
    expect(isUuid(SYSTEM_USER)).toBe(true);
    expect(isUuid("4f3b2c1d")).toBe(false);
    expect(isUuid("player-42")).toBe(false);
  });

  it("banState: revoked wins over expiry, expiry is compared against now", () => {
    const now = Date.parse("2026-01-01T00:00:00Z");
    expect(banState({ revoked_at: null, expires_at: null }, now)).toBe("active");
    expect(banState({ revoked_at: null, expires_at: "2026-01-02T00:00:00Z" }, now)).toBe("active");
    expect(banState({ revoked_at: null, expires_at: "2025-12-31T23:59:59Z" }, now)).toBe("expired");
    expect(banState({ revoked_at: "2025-12-01T00:00:00Z", expires_at: "2025-12-31T23:59:59Z" }, now)).toBe("revoked");
  });

  it("isSafetyType only matches safety.* incidents", () => {
    expect(isSafetyType("safety.voice")).toBe(true);
    expect(isSafetyType("safety.text")).toBe(true);
    expect(isSafetyType("user_report")).toBe(false);
  });

  it("evidence narrowing never throws on foreign shapes", () => {
    expect(asRecord({ a: 1 })).toEqual({ a: 1 });
    expect(asRecord([1])).toBeNull();
    expect(asRecord(null)).toBeNull();
    expect(asString(3)).toBeNull();
    expect(asNumber(Number.NaN)).toBeNull();
    expect(asNumber(0.42)).toBe(0.42);
    expect(asStringArray(["hate", 3, "spam"])).toEqual(["hate", "spam"]);
    expect(asStringArray("hate")).toEqual([]);
  });
});
