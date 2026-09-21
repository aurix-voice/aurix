import { describe, expect, it } from "vitest";

import type { T } from "@/api/client";
import {
  auditActions,
  chainLinks,
  chainSummary,
  detailsPreview,
  filterAdmins,
  filterAudit,
  otherActiveSuperadmins,
  parseAdminTab,
  passwordValid,
  roleAdds,
  rolePermissions,
  sweepTotal,
} from "@/pages/admin/model";
import { configFilename, configSections, countMasked, filterSections, formatLeaf } from "@/pages/config/model";

const admin = (p: Partial<T.Admin> & Pick<T.Admin, "id">): T.Admin => ({
  email: `${p.id}@example.com`,
  display_name: p.id.toUpperCase(),
  role: "admin",
  permissions: [],
  active: true,
  auth_source: "password",
  ...p,
});

const admins = [
  admin({ id: "root", role: "superadmin" }),
  admin({ id: "ops", role: "admin" }),
  admin({ id: "mod", role: "moderator", active: false }),
  admin({ id: "old", role: "superadmin", active: false }),
];

describe("admin tabs", () => {
  it("falls back to the first allowed tab", () => {
    expect(parseAdminTab("audit", ["admins", "audit", "profile"])).toBe("audit");
    expect(parseAdminTab("admins", ["audit", "profile"])).toBe("audit");
    expect(parseAdminTab("nope", ["profile"])).toBe("profile");
    expect(parseAdminTab(undefined, [])).toBe("profile");
  });
});

describe("roles", () => {
  it("mirrors the server minimum-role matrix", () => {
    expect(rolePermissions("viewer")).toEqual(["apps:read", "nodes:read", "analytics:read"]);
    expect(rolePermissions("moderator")).toContain("audit:read");
    expect(rolePermissions("moderator")).not.toContain("apps:write");
    expect(rolePermissions("admin")).toContain("config:read");
    expect(rolePermissions("superadmin")).toHaveLength(12);
    expect(roleAdds("superadmin")).toEqual(["apps:delete", "retention:run", "admins:manage"]);
  });

  it("validates passwords by code points", () => {
    expect(passwordValid("short")).toBe(false);
    expect(passwordValid("twelve-chars")).toBe(true);
    expect(passwordValid("😀".repeat(12))).toBe(true);
    expect(passwordValid("x".repeat(1025))).toBe(false);
  });
});

describe("admin list", () => {
  it("hides inactive unless asked and searches email/name/id", () => {
    expect(filterAdmins(admins, { search: "", showInactive: false }).map((a) => a.id)).toEqual(["root", "ops"]);
    expect(filterAdmins(admins, { search: "", showInactive: true })).toHaveLength(4);
    expect(filterAdmins(admins, { search: "MOD@", showInactive: true }).map((a) => a.id)).toEqual(["mod"]);
    expect(filterAdmins(admins, { search: "OPS", showInactive: false }).map((a) => a.id)).toEqual(["ops"]);
  });

  it("counts other active superadmins", () => {
    expect(otherActiveSuperadmins(admins, "root")).toBe(0);
    expect(otherActiveSuperadmins([...admins, admin({ id: "r2", role: "superadmin" })], "root")).toBe(1);
  });
});

const entry = (p: Partial<T.AuditLogEntry> & Pick<T.AuditLogEntry, "id" | "hash" | "previous_hash">): T.AuditLogEntry => ({
  actor_id: "a1",
  action: "app.created",
  target_type: "app",
  target_id: "t1",
  details: {},
  created_at: "2026-01-01T00:00:00Z",
  ...p,
});

describe("audit chain", () => {
  const entries = [
    entry({ id: "e3", hash: "h3", previous_hash: "h2", action: "admin.deleted", actor_id: "a2", target_id: "x" }),
    entry({ id: "e2", hash: "h2", previous_hash: "h1" }),
    entry({ id: "e1", hash: "h1", previous_hash: "genesis", details: { role: "admin" } }),
    entry({ id: "n2", hash: "n2", previous_hash: "n1", target_type: "node" }),
  ];

  it("classifies links conservatively", () => {
    const links = chainLinks(entries);
    expect(links.get("e1")).toBe("genesis");
    expect(links.get("e2")).toBe("linked");
    expect(links.get("e3")).toBe("linked");
    expect(links.get("n2")).toBe("unlinked");
    expect(chainSummary(links)).toEqual({ linked: 2, genesis: 1, unlinked: 1, total: 4 });
  });

  it("filters by action, actor prefix and target", () => {
    expect(filterAudit(entries, { action: "admin.deleted", actor: "", target: "" }).map((e) => e.id)).toEqual(["e3"]);
    expect(filterAudit(entries, { action: "", actor: "A2", target: "" }).map((e) => e.id)).toEqual(["e3"]);
    expect(filterAudit(entries, { action: "", actor: "", target: "node" }).map((e) => e.id)).toEqual(["n2"]);
    expect(auditActions(entries)).toEqual(["admin.deleted", "app.created"]);
  });

  it("previews details compactly", () => {
    expect(detailsPreview({})).toBe("");
    expect(detailsPreview(null)).toBe("");
    expect(detailsPreview({ role: "admin" })).toBe('{"role":"admin"}');
    expect(detailsPreview({ k: "x".repeat(200) }, 20)).toHaveLength(20);
  });
});

describe("retention", () => {
  it("sums the sweep report", () => {
    expect(sweepTotal({})).toBe(0);
    expect(sweepTotal({ sessions: 2, audit_log: 3, inactive_users: 1 })).toBe(6);
  });
});

describe("config viewer", () => {
  const cfg = {
    server: { bind: "0.0.0.0:8080", external_url: null, cors: { origins: ["https://a", "https://b"] } },
    redis: { url: "***", sentinel: { master: "m", hosts: [] } },
    media: { peers: [{ host: "h1", port: 1 }] },
    log_level: "info",
  };

  it("flattens nested sections and masks secrets", () => {
    const sections = configSections(cfg);
    expect(sections.map((s) => s.name)).toEqual(["general", "media", "redis", "server"]);
    const server = sections.find((s) => s.name === "server");
    expect(server?.leaves.map((l) => l.path)).toEqual(["bind", "cors.origins", "external_url"]);
    expect(sections.find((s) => s.name === "media")?.leaves.map((l) => l.path)).toEqual(["peers[0].host", "peers[0].port"]);
    expect(sections.find((s) => s.name === "redis")?.leaves.find((l) => l.path === "url")?.masked).toBe(true);
    expect(countMasked(sections)).toBe(1);
    expect(sections[0]?.leaves[0]).toEqual({ path: "log_level", value: "info", masked: false });
  });

  it("formats leaves", () => {
    expect(formatLeaf(["a", "b"])).toBe("a, b");
    expect(formatLeaf([])).toBe("[]");
    expect(formatLeaf(null)).toBe("null");
    expect(formatLeaf("")).toBe('""');
    expect(formatLeaf(false)).toBe("false");
  });

  it("searches section names, paths and values", () => {
    const sections = configSections(cfg);
    expect(filterSections(sections, "redis").map((s) => s.name)).toEqual(["redis"]);
    expect(filterSections(sections, "origins")[0]?.leaves).toHaveLength(1);
    expect(filterSections(sections, "https://b")[0]?.leaves.map((l) => l.path)).toEqual(["cors.origins"]);
    expect(filterSections(sections, "zzz")).toEqual([]);
    expect(filterSections(sections, "")).toHaveLength(4);
  });

  it("builds a safe filename", () => {
    expect(configFilename("node a/1", "1.2.0")).toBe("aurix-config-node_a_1-1.2.0.json");
  });
});
