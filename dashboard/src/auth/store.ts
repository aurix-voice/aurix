import { useSyncExternalStore } from "react";

import type { T } from "@/api/client";

export interface AuthSession {
  token: string;
  /** Unix ms. */
  expiresAt: number;
  admin: T.Admin | null;
}

const STORAGE_KEY = "aurix.auth";
let current: AuthSession | null = load();
const listeners = new Set<() => void>();

function load(): AuthSession | null {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return null;
    const parsed = JSON.parse(raw) as Partial<AuthSession>;
    if (typeof parsed.token !== "string" || typeof parsed.expiresAt !== "number") return null;
    if (parsed.expiresAt <= Date.now()) return null;
    return { token: parsed.token, expiresAt: parsed.expiresAt, admin: parsed.admin ?? null };
  } catch {
    return null;
  }
}

function persist(): void {
  try {
    if (current) localStorage.setItem(STORAGE_KEY, JSON.stringify(current));
    else localStorage.removeItem(STORAGE_KEY);
  } catch {
    /* ignore */
  }
}

function emit(): void {
  for (const l of listeners) l();
}

export const authStore = {
  get: (): AuthSession | null => current,
  set: (session: AuthSession | null): void => {
    current = session;
    persist();
    emit();
  },
  setAdmin: (admin: T.Admin): void => {
    if (!current) return;
    current = { ...current, admin };
    persist();
    emit();
  },
  clear: (): void => {
    authStore.set(null);
  },
  subscribe: (fn: () => void): (() => void) => {
    listeners.add(fn);
    return () => {
      listeners.delete(fn);
    };
  },
};

if (typeof window !== "undefined") {
  window.addEventListener("storage", (e) => {
    if (e.key === STORAGE_KEY) {
      current = load();
      emit();
    }
  });
}

export function useAuthSession(): AuthSession | null {
  return useSyncExternalStore(authStore.subscribe, authStore.get, authStore.get);
}

export function hasPermission(admin: T.Admin | null | undefined, perm: T.AdminPermission): boolean {
  return !!admin && admin.permissions.includes(perm);
}

/** Tenant-scoped API permission strings (`scope:action`) used on `/v1/*` routes. */
export type AppPermission =
  | "channels:read"
  | "channels:write"
  | "users:read"
  | "users:write"
  | "users:export"
  | "analytics:read"
  | "events:read"
  | "webhooks:read"
  | "webhooks:write"
  | "recordings:read"
  | "recordings:write"
  | "audio_streams:read"
  | "audio_streams:write"
  | "moderation:read"
  | "moderation:write"
  | "chat:read"
  | "chat:write"
  | "audit:read"
  | "keys:manage"
  | "tokens:issue"
  | "turn:issue"
  | "tts:write";

const VIEWER_APP_PERMS: readonly AppPermission[] = ["channels:read", "users:read", "analytics:read", "events:read", "webhooks:read", "recordings:read", "audio_streams:read"];
const MODERATOR_APP_PERMS: readonly AppPermission[] = ["moderation:read", "moderation:write", "chat:read", "chat:write", "audit:read", "users:write"];
const ADMIN_APP_PERMS: readonly AppPermission[] = ["channels:write", "webhooks:write", "recordings:write", "audio_streams:write", "keys:manage", "tokens:issue", "turn:issue", "tts:write", "users:export"];

export const ALL_APP_PERMISSIONS: readonly AppPermission[] = [...VIEWER_APP_PERMS, ...MODERATOR_APP_PERMS, ...ADMIN_APP_PERMS].sort();

/** Every permission an API key can carry (server `KNOWN_PERMISSIONS` minus `*`); `users:erase` is not delegated to any role below superadmin. */
export const KNOWN_KEY_PERMISSIONS: readonly string[] = [...ALL_APP_PERMISSIONS, "users:erase"].sort();

const ROLE_RANK: Record<T.AdminRole, number> = { viewer: 0, moderator: 1, admin: 2, superadmin: 3 };

/** Mirrors the server's role → tenant-permission mapping for admin JWT + `X-Aurix-App` delegation. */
export function hasAppPermission(admin: T.Admin | null | undefined, perm: AppPermission): boolean {
  if (!admin) return false;
  const rank = ROLE_RANK[admin.role];
  if (rank >= ROLE_RANK.superadmin) return true;
  if (VIEWER_APP_PERMS.includes(perm)) return true;
  if (rank >= ROLE_RANK.moderator && MODERATOR_APP_PERMS.includes(perm)) return true;
  return rank >= ROLE_RANK.admin && ADMIN_APP_PERMS.includes(perm);
}

/** Tenant permissions the admin's delegation holds; `null` = wildcard (`*`). */
export function delegatedAppPermissions(admin: T.Admin | null | undefined): readonly AppPermission[] | null {
  if (!admin) return [];
  if (ROLE_RANK[admin.role] >= ROLE_RANK.superadmin) return null;
  return ALL_APP_PERMISSIONS.filter((p) => hasAppPermission(admin, p));
}
