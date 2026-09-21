/**
 * Webhook receiver helpers: verify `X-Aurix-Signature` (`t=<unix>,v1=<hex HMAC-SHA256(secret,
 * "<t>.<raw body>")>`) in constant time with a replay window, and parse the `EventEnvelope`.
 */

import { createHmac, timingSafeEqual } from "node:crypto";

import type { EventEnvelope } from "./generated/types.js";

export const SIGNATURE_HEADER = "x-aurix-signature";
export const EVENT_HEADER = "x-aurix-event";
export const WEBHOOK_ID_HEADER = "x-aurix-webhook-id";
export const DELIVERY_ID_HEADER = "x-aurix-delivery-id";
export const ATTEMPT_HEADER = "x-aurix-attempt";

/** Default replay tolerance (seconds). */
export const DEFAULT_TOLERANCE_SEC = 300;

export interface VerifyOptions {
  /** Accepted |now - t| in seconds (default 300). */
  toleranceSec?: number;
  /** Clock override for tests (unix seconds). */
  nowSec?: number;
}

/** Computes the header value for `body` at `timestamp` (unix seconds) — useful for tests and for re-signing in proxies. */
export function signWebhook(secret: string, timestamp: number, body: Uint8Array | string): string {
  const mac = createHmac("sha256", secret);
  mac.update(`${timestamp}.`);
  mac.update(typeof body === "string" ? Buffer.from(body, "utf8") : body);
  return `t=${timestamp},v1=${mac.digest("hex")}`;
}

/** Returns true when `header` is a valid signature of the raw `body` within the replay window. */
export function verifyWebhookSignature(
  secret: string,
  header: string | null | undefined,
  body: Uint8Array | string,
  options: VerifyOptions = {},
): boolean {
  if (!header || !secret) return false;
  let t: number | undefined;
  let v1: string | undefined;
  for (const part of header.split(",")) {
    const eq = part.indexOf("=");
    if (eq < 0) continue;
    const k = part.slice(0, eq).trim();
    const v = part.slice(eq + 1).trim();
    if (k === "t") t = /^\d+$/.test(v) ? Number(v) : undefined;
    else if (k === "v1") v1 = v;
  }
  if (t === undefined || v1 === undefined) return false;
  const now = options.nowSec ?? Math.floor(Date.now() / 1000);
  if (Math.abs(now - t) > (options.toleranceSec ?? DEFAULT_TOLERANCE_SEC)) return false;
  const expected = signWebhook(secret, t, body).slice(`t=${t},v1=`.length);
  if (!/^[0-9a-fA-F]+$/.test(v1) || v1.length !== expected.length) return false;
  return timingSafeEqual(Buffer.from(expected, "hex"), Buffer.from(v1, "hex"));
}

export class WebhookVerificationError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "WebhookVerificationError";
  }
}

export interface IncomingWebhook {
  event: EventEnvelope;
  /** `X-Aurix-Webhook-Id` */
  webhookId: string | undefined;
  /** `X-Aurix-Delivery-Id` — stable across retries; use `event.id` or this for idempotency. */
  deliveryId: string | undefined;
  /** `X-Aurix-Attempt` (1-based). */
  attempt: number;
}

type HeaderLookup = Headers | Record<string, string | string[] | undefined> | ((name: string) => string | null | undefined);

function getHeader(headers: HeaderLookup, name: string): string | undefined {
  if (typeof headers === "function") return headers(name) ?? undefined;
  if (headers instanceof Headers) return headers.get(name) ?? undefined;
  const direct = headers[name] ?? headers[name.toLowerCase()];
  const v = direct ?? Object.entries(headers).find(([k]) => k.toLowerCase() === name)?.[1];
  return Array.isArray(v) ? v[0] : v;
}

/**
 * Verifies and parses one webhook POST. `rawBody` must be the exact bytes received (do not
 * re-serialise a parsed JSON object). Throws `WebhookVerificationError` on a bad signature.
 */
export function parseWebhook(secret: string, headers: HeaderLookup, rawBody: Uint8Array | string, options: VerifyOptions = {}): IncomingWebhook {
  const sig = getHeader(headers, SIGNATURE_HEADER);
  if (!verifyWebhookSignature(secret, sig, rawBody, options)) {
    throw new WebhookVerificationError("invalid or expired X-Aurix-Signature");
  }
  const text = typeof rawBody === "string" ? rawBody : new TextDecoder().decode(rawBody);
  const event = JSON.parse(text) as EventEnvelope;
  const headerType = getHeader(headers, EVENT_HEADER);
  if (headerType && headerType !== event.type) {
    throw new WebhookVerificationError(`X-Aurix-Event ${headerType} does not match body type ${event.type}`);
  }
  const attempt = Number(getHeader(headers, ATTEMPT_HEADER) ?? "1");
  return {
    event,
    webhookId: getHeader(headers, WEBHOOK_ID_HEADER),
    deliveryId: getHeader(headers, DELIVERY_ID_HEADER),
    attempt: Number.isFinite(attempt) ? attempt : 1,
  };
}
