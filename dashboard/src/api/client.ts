import { AurixError, AurixNetworkError } from "@aurix/server-sdk-src/errors";
import { AurixClient } from "@aurix/server-sdk-src/generated/client";
import type * as T from "@aurix/server-sdk-src/generated/types";
import type { RawResponse } from "@aurix/server-sdk-src/http";

import { translate, type Locale } from "@/i18n";

export { AurixClient, AurixError, AurixNetworkError };
export type { T, RawResponse };

/** Tenant routes accept an admin JWT when this header names the application to act on. */
export const APP_HEADER = "X-Aurix-App";

export function baseUrl(): string {
  return window.location.origin;
}

export function makeClient(token: string | null, appId: string | null): AurixClient {
  const headers: Record<string, string> = {};
  if (appId) headers[APP_HEADER] = appId;
  return new AurixClient({
    baseUrl: baseUrl(),
    adminToken: token ?? undefined,
    headers,
    timeoutMs: 15_000,
    maxRetries: 1,
  });
}

export type ErrorKind =
  | "network"
  | "unauthorized"
  | "forbidden"
  | "notFound"
  | "conflict"
  | "rateLimited"
  | "validation"
  | "server"
  | "unknown";

export function errorKind(err: unknown): ErrorKind {
  if (err instanceof AurixNetworkError) return "network";
  if (err instanceof AurixError) {
    if (err.status === 401) return "unauthorized";
    if (err.status === 403) return "forbidden";
    if (err.status === 404) return "notFound";
    if (err.status === 409) return "conflict";
    if (err.status === 429) return "rateLimited";
    if (err.status === 400 || err.status === 422) return "validation";
    if (err.status >= 500) return "server";
  }
  return "unknown";
}

/** Human message: the server's own text where it has one, a localized fallback otherwise. */
export function errorMessage(err: unknown, locale: Locale = "en"): string {
  if (err instanceof AurixNetworkError) return translate(locale, "error.network");
  if (err instanceof AurixError) {
    const server = err.message.replace(/^[A-Z]+ \S+ → \d+ \S+: /, "").trim();
    if (server && server !== err.code) return server;
    const kind = errorKind(err);
    const key = kind === "unknown" ? "error.unknown" : (`error.${kind}` as const);
    return err.code ? translate(locale, "error.withCode", { message: translate(locale, key), code: err.code }) : translate(locale, key);
  }
  if (err instanceof Error) return err.message;
  return String(err);
}

export function errorCode(err: unknown): string | undefined {
  return err instanceof AurixError ? err.code : undefined;
}

export function isStatus(err: unknown, status: number): boolean {
  return err instanceof AurixError && err.status === status;
}

/** Triggers a browser download of a raw API response. */
export function downloadRaw(res: RawResponse, filename: string): void {
  const blob = new Blob([res.body as BlobPart], { type: res.contentType || "application/octet-stream" });
  downloadBlob(blob, filename);
}

export function downloadBlob(blob: Blob, filename: string): void {
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = filename;
  document.body.appendChild(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}

export function downloadJson(value: unknown, filename: string): void {
  downloadBlob(new Blob([JSON.stringify(value, null, 2)], { type: "application/json" }), filename);
}
