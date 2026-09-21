import type { T } from "@/api/client";

export const ADMIN_TABS = ["admins", "audit", "retention", "profile"] as const;
export type AdminTab = (typeof ADMIN_TABS)[number];

export function parseAdminTab(v: string | undefined, allowed: readonly AdminTab[]): AdminTab {
  const found = ADMIN_TABS.find((t) => t === v);
  if (found && allowed.includes(found)) return found;
  return allowed[0] ?? "profile";
}

export const ADMIN_ROLES: readonly T.AdminRole[] = ["viewer", "moderator", "admin", "superadmin"];

export const ROLE_RANK: Record<T.AdminRole, number> = { viewer: 0, moderator: 1, admin: 2, superadmin: 3 };

/** Minimum role per permission, mirrored from `AdminPermission::minimum_role` for the role picker. */
const PERMISSION_MIN_ROLE: Record<T.AdminPermission, T.AdminRole> = {
  "apps:read": "viewer",
  "nodes:read": "viewer",
  "analytics:read": "viewer",
  "audit:read": "moderator",
  "moderation:read": "moderator",
  "apps:write": "admin",
  "keys:rotate": "admin",
  "nodes:drain": "admin",
  "config:read": "admin",
  "apps:delete": "superadmin",
  "retention:run": "superadmin",
  "admins:manage": "superadmin",
};

export const ALL_ADMIN_PERMISSIONS = Object.keys(PERMISSION_MIN_ROLE) as T.AdminPermission[];

export function rolePermissions(role: T.AdminRole): T.AdminPermission[] {
  return ALL_ADMIN_PERMISSIONS.filter((p) => ROLE_RANK[PERMISSION_MIN_ROLE[p]] <= ROLE_RANK[role]);
}

/** Permissions `role` adds on top of the role directly below it. */
export function roleAdds(role: T.AdminRole): T.AdminPermission[] {
  return ALL_ADMIN_PERMISSIONS.filter((p) => PERMISSION_MIN_ROLE[p] === role);
}

/** Server rule mirrored for the form (`aurix-auth`): passwords are 12–1024 characters. */
export const MIN_PASSWORD_LEN = 12;

export function passwordValid(p: string): boolean {
  const n = [...p].length;
  return n >= MIN_PASSWORD_LEN && p.length <= 1024;
}

export interface AdminFilter {
  search: string;
  showInactive: boolean;
}

export function filterAdmins(list: readonly T.Admin[], f: AdminFilter): T.Admin[] {
  const q = f.search.trim().toLowerCase();
  return list.filter(
    (a) =>
      (f.showInactive || a.active) &&
      (!q || a.email.toLowerCase().includes(q) || a.display_name.toLowerCase().includes(q) || a.id.startsWith(q)),
  );
}

/** Active superadmins other than `exceptId`; the server refuses to remove the last one. */
export function otherActiveSuperadmins(list: readonly T.Admin[], exceptId: string): number {
  return list.filter((a) => a.active && a.role === "superadmin" && a.id !== exceptId).length;
}

export type ChainLink = "genesis" | "linked" | "unlinked";

/**
 * Each node keeps its own hash chain (starting at `genesis` on process start), so entries from
 * different nodes interleave in the global log. An entry is `linked` when its predecessor is on
 * this page; `unlinked` means the predecessor is on another page or node, not that the chain is broken.
 */
export function chainLinks(entries: readonly T.AuditLogEntry[]): Map<string, ChainLink> {
  const hashes = new Set(entries.map((e) => e.hash));
  const out = new Map<string, ChainLink>();
  for (const e of entries) {
    out.set(e.id, e.previous_hash === "genesis" ? "genesis" : hashes.has(e.previous_hash) ? "linked" : "unlinked");
  }
  return out;
}

export function chainSummary(links: ReadonlyMap<string, ChainLink>): { linked: number; genesis: number; unlinked: number; total: number } {
  let linked = 0;
  let genesis = 0;
  let unlinked = 0;
  for (const v of links.values()) {
    if (v === "linked") linked++;
    else if (v === "genesis") genesis++;
    else unlinked++;
  }
  return { linked, genesis, unlinked, total: links.size };
}

export interface AuditFilter {
  action: string;
  actor: string;
  target: string;
}

export function filterAudit(entries: readonly T.AuditLogEntry[], f: AuditFilter): T.AuditLogEntry[] {
  const actor = f.actor.trim().toLowerCase();
  const target = f.target.trim().toLowerCase();
  return entries.filter(
    (e) =>
      (!f.action || e.action === f.action) &&
      (!actor || e.actor_id.toLowerCase().startsWith(actor)) &&
      (!target || e.target_id.toLowerCase().includes(target) || e.target_type.toLowerCase() === target),
  );
}

export function auditActions(entries: readonly T.AuditLogEntry[]): string[] {
  return [...new Set(entries.map((e) => e.action))].sort();
}

export function detailsPreview(details: unknown, max = 80): string {
  if (details === null || details === undefined) return "";
  if (typeof details === "object" && !Array.isArray(details) && Object.keys(details).length === 0) return "";
  const s = JSON.stringify(details);
  return s.length > max ? `${s.slice(0, max - 1)}…` : s;
}

export function sweepTotal(r: T.SweepReport): number {
  return (r.sessions ?? 0) + (r.moderation_events ?? 0) + (r.audit_log ?? 0) + (r.analytics ?? 0) + (r.tombstones ?? 0) + (r.inactive_users ?? 0);
}
