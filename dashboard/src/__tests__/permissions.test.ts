import { describe, expect, it } from "vitest";

import type { T } from "@/api/client";
import { ALL_APP_PERMISSIONS, delegatedAppPermissions, hasAppPermission, KNOWN_KEY_PERMISSIONS } from "@/auth/store";

function admin(role: T.AdminRole): T.Admin {
  return { id: "a", email: `${role}@example.com`, display_name: role, role, permissions: [], active: true, auth_source: "password" };
}

describe("admin → tenant permission delegation (mirrors admin_api_permissions)", () => {
  it("viewer holds read-only permissions", () => {
    const v = admin("viewer");
    expect(hasAppPermission(v, "channels:read")).toBe(true);
    expect(hasAppPermission(v, "webhooks:read")).toBe(true);
    expect(hasAppPermission(v, "channels:write")).toBe(false);
    expect(hasAppPermission(v, "moderation:write")).toBe(false);
    expect(hasAppPermission(v, "keys:manage")).toBe(false);
  });

  it("moderator adds moderation/chat/audit/users:write but no channel/key writes", () => {
    const m = admin("moderator");
    expect(hasAppPermission(m, "moderation:write")).toBe(true);
    expect(hasAppPermission(m, "users:write")).toBe(true);
    expect(hasAppPermission(m, "keys:manage")).toBe(false);
    expect(hasAppPermission(m, "webhooks:write")).toBe(false);
  });

  it("admin holds every listed permission; superadmin is wildcard", () => {
    const a = admin("admin");
    for (const p of ALL_APP_PERMISSIONS) expect(hasAppPermission(a, p)).toBe(true);
    expect(delegatedAppPermissions(a)).toEqual(ALL_APP_PERMISSIONS);
    expect(delegatedAppPermissions(admin("superadmin"))).toBeNull();
    expect(delegatedAppPermissions(null)).toEqual([]);
  });

  it("users:erase is grantable to keys only by superadmin", () => {
    expect(KNOWN_KEY_PERMISSIONS).toContain("users:erase");
    expect(ALL_APP_PERMISSIONS).not.toContain("users:erase");
    expect(KNOWN_KEY_PERMISSIONS).not.toContain("*");
  });
});
