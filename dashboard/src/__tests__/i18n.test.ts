import { describe, expect, it } from "vitest";

import { en } from "@/i18n/en";
import { ru } from "@/i18n/ru";

describe("message catalogues", () => {
  it("ru has exactly the keys of en", () => {
    const enKeys = Object.keys(en).sort();
    const ruKeys = Object.keys(ru).sort();
    expect(ruKeys).toEqual(enKeys);
  });
  it("function-valued messages match in kind", () => {
    for (const k of Object.keys(en) as Array<keyof typeof en>) {
      expect(typeof ru[k], k).toBe(typeof en[k]);
    }
  });
});
