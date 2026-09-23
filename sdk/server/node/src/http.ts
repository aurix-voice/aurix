/**
 * Hand-written transport under the generated `AurixClient`: authentication headers, JSON
 * encoding, query strings, timeouts, retries with `Retry-After`, and error mapping.
 */

import { AurixError, AurixNetworkError } from "./errors.js";
import { eventStream, type EventStreamOptions, type SseEvent } from "./sse.js";

/** Exactly one credential should be set; the API key is the normal server-side choice. */
export interface AurixCredentials {
  /** Application API key (`aurx_...`), sent as `X-API-Key`. Never ship it to game clients. */
  apiKey?: string;
  /** Admin JWT from `POST /admin/login` / OIDC, sent as `Authorization: Bearer`. */
  adminToken?: string;
  /** Player session JWT (for the `/v1/me/*` endpoints). */
  playerToken?: string;
  /** `auth.bootstrap_token`, only for `POST /admin/setup`. */
  bootstrapToken?: string;
}

export interface AurixClientOptions extends AurixCredentials {
  /** Control-plane base URL, e.g. `https://voice.example.com` (no trailing slash needed). */
  baseUrl: string;
  /** Per-request timeout in milliseconds (default 10 000). */
  timeoutMs?: number;
  /**
   * Retries for idempotent requests (GET/PUT/DELETE) on network errors, 502/503/504, and for
   * any method on 429 (the request was not processed). Default 2; `0` disables.
   */
  maxRetries?: number;
  /** Upper bound for a single backoff / `Retry-After` wait (default 5 000 ms). */
  maxBackoffMs?: number;
  /** `fetch` implementation (default: global `fetch`, Node ≥ 18). */
  fetch?: typeof fetch;
  /** Extra headers on every request. */
  headers?: Record<string, string>;
  userAgent?: string;
}

export interface RequestOptions {
  signal?: AbortSignal;
  headers?: Record<string, string>;
  timeoutMs?: number;
  /** Override the client credentials for this call only (e.g. a player token on `/v1/me/*`). */
  auth?: AurixCredentials;
}

export interface RawResponse {
  status: number;
  contentType: string;
  headers: Headers;
  body: Uint8Array;
  /** Decodes `body` as UTF-8. */
  text(): string;
}

interface CallOptions extends RequestOptions {
  body?: unknown;
  query?: object | undefined;
}

export const SDK_VERSION = "1.5.0";
const RETRY_METHODS = new Set(["GET", "PUT", "DELETE", "HEAD"]);
const RETRY_STATUSES = new Set([502, 503, 504]);

export class AurixHttp {
  readonly baseUrl: string;
  /** @internal */
  readonly opts: AurixClientOptions;
  /** @internal */
  readonly fetchImpl: typeof fetch;

  constructor(options: AurixClientOptions) {
    if (!options.baseUrl) throw new Error("AurixClient: baseUrl is required");
    this.baseUrl = options.baseUrl.replace(/\/+$/, "");
    this.opts = options;
    const f = options.fetch ?? globalThis.fetch;
    if (typeof f !== "function") throw new Error("AurixClient: no fetch available; pass options.fetch");
    // Browsers reject `fetch` called with a foreign `this` ("Illegal invocation").
    this.fetchImpl = options.fetch ? f : (input, init) => globalThis.fetch(input, init);
  }

  /** Builds the absolute URL for `path` + `query` (nullish values are skipped, arrays repeat). */
  url(path: string, query?: object): string {
    const u = new URL(this.baseUrl + path);
    if (query) {
      for (const [k, v] of Object.entries(query as Record<string, unknown>)) {
        if (v === undefined || v === null) continue;
        if (Array.isArray(v)) {
          for (const item of v) u.searchParams.append(k, String(item));
        } else {
          u.searchParams.set(k, String(v));
        }
      }
    }
    return u.toString();
  }

  /** @internal */
  authHeaders(auth: AurixCredentials): Record<string, string> {
    const h: Record<string, string> = {};
    if (auth.apiKey) h["X-API-Key"] = auth.apiKey;
    else if (auth.adminToken) h["Authorization"] = `Bearer ${auth.adminToken}`;
    else if (auth.playerToken) h["Authorization"] = `Bearer ${auth.playerToken}`;
    if (auth.bootstrapToken) h["X-Bootstrap-Token"] = auth.bootstrapToken;
    return h;
  }

  /**
   * `GET /v1/events` as an async iterator (auto-reconnect, `Last-Event-ID` resume). See `eventStream`.
   */
  events(options?: EventStreamOptions): AsyncGenerator<SseEvent, void, undefined> {
    return eventStream(this, options);
  }

  /** JSON request; resolves to `undefined` on an empty body. */
  async json<T>(method: string, path: string, call: CallOptions): Promise<T> {
    const res = await this.send(method, path, call);
    if (res.status === 204 || res.body.byteLength === 0) return undefined as T;
    if (!isJson(res.contentType)) {
      throw new AurixError({
        status: res.status,
        code: "unexpected_content_type",
        message: `expected application/json but got ${res.contentType || "no content type"}; use the Raw variant of this call`,
        method,
        path,
      });
    }
    return JSON.parse(res.text()) as T;
  }

  /** Raw request (binary / CSV / SRT / VTT bodies). */
  raw(method: string, path: string, call: CallOptions): Promise<RawResponse> {
    return this.send(method, path, call);
  }

  private async send(method: string, path: string, call: CallOptions): Promise<RawResponse> {
    const url = this.url(path, call.query);
    const headers: Record<string, string> = {
      Accept: "application/json, */*;q=0.5",
      "User-Agent": this.opts.userAgent ?? `aurix-server-sdk-node/${SDK_VERSION}`,
      ...this.opts.headers,
      ...this.authHeaders(call.auth ?? this.opts),
      ...call.headers,
    };
    let body: string | undefined;
    if (call.body !== undefined) {
      headers["Content-Type"] = "application/json";
      body = JSON.stringify(call.body);
    }
    const maxRetries = this.opts.maxRetries ?? 2;
    const timeoutMs = call.timeoutMs ?? this.opts.timeoutMs ?? 10_000;
    const maxBackoff = this.opts.maxBackoffMs ?? 5_000;
    const upper = method.toUpperCase();
    for (let attempt = 0; ; attempt++) {
      const controller = new AbortController();
      const onAbort = () => controller.abort(call.signal?.reason);
      if (call.signal) {
        if (call.signal.aborted) throw new AurixNetworkError("request aborted", method, path);
        call.signal.addEventListener("abort", onAbort, { once: true });
      }
      const timer = setTimeout(() => controller.abort(new Error(`timeout after ${timeoutMs} ms`)), timeoutMs);
      let res: Response;
      try {
        res = await this.fetchImpl(url, { method: upper, headers, body: body ?? null, signal: controller.signal });
      } catch (err) {
        clearTimeout(timer);
        call.signal?.removeEventListener("abort", onAbort);
        if (call.signal?.aborted) throw new AurixNetworkError("request aborted", method, path, err);
        if (attempt < maxRetries && RETRY_METHODS.has(upper)) {
          await sleep(backoff(attempt, maxBackoff));
          continue;
        }
        throw new AurixNetworkError(err instanceof Error ? err.message : String(err), method, path, err);
      }
      let bytes: Uint8Array;
      try {
        bytes = new Uint8Array(await res.arrayBuffer());
      } finally {
        clearTimeout(timer);
        call.signal?.removeEventListener("abort", onAbort);
      }
      const contentType = res.headers.get("content-type") ?? "";
      if (res.ok) {
        return { status: res.status, contentType, headers: res.headers, body: bytes, text: () => decode(bytes) };
      }
      const retryable = res.status === 429 || (RETRY_METHODS.has(upper) && RETRY_STATUSES.has(res.status));
      if (retryable && attempt < maxRetries) {
        await sleep(retryAfterMs(res.headers.get("retry-after")) ?? backoff(attempt, maxBackoff), maxBackoff);
        continue;
      }
      throw AurixError.fromResponse(res.status, contentType, decode(bytes), res.headers, method, path);
    }
  }
}

function isJson(contentType: string): boolean {
  const ct = contentType.toLowerCase();
  return ct.startsWith("application/json") || ct.includes("+json");
}

function decode(bytes: Uint8Array): string {
  return new TextDecoder().decode(bytes);
}

function backoff(attempt: number, max: number): number {
  const base = Math.min(max, 200 * 2 ** attempt);
  return Math.round(base / 2 + Math.random() * (base / 2));
}

export function retryAfterMs(header: string | null): number | undefined {
  if (!header) return undefined;
  const secs = Number(header);
  if (Number.isFinite(secs)) return Math.max(0, secs * 1000);
  const at = Date.parse(header);
  return Number.isNaN(at) ? undefined : Math.max(0, at - Date.now());
}

function sleep(ms: number, max = ms): Promise<void> {
  return new Promise((r) => setTimeout(r, Math.min(ms, max)));
}
