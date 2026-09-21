import type { T } from "@/api/client";

const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

export function isUuid(s: string): boolean {
  return UUID_RE.test(s.trim());
}

export const SYSTEM_USER = "00000000-0000-0000-0000-000000000000";

export function isSafetyType(type: string): boolean {
  return type.startsWith("safety.");
}

export type BanState = "active" | "expired" | "revoked";

export function banState(b: Pick<T.Ban, "revoked_at" | "expires_at">, now: number): BanState {
  if (b.revoked_at) return "revoked";
  if (b.expires_at && new Date(b.expires_at).getTime() <= now) return "expired";
  return "active";
}

/** Type-narrowing helpers for the untyped `evidence` JSON of moderation events. */
export function asRecord(v: unknown): Record<string, unknown> | null {
  return typeof v === "object" && v !== null && !Array.isArray(v) ? (v as Record<string, unknown>) : null;
}
export function asString(v: unknown): string | null {
  return typeof v === "string" ? v : null;
}
export function asNumber(v: unknown): number | null {
  return typeof v === "number" && Number.isFinite(v) ? v : null;
}
export function asStringArray(v: unknown): string[] {
  return Array.isArray(v) ? v.filter((x): x is string => typeof x === "string") : [];
}
