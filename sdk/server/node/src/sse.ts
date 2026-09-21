/**
 * `GET /v1/events` server-sent events as an async iterator, with automatic reconnection and
 * `Last-Event-ID` resume.
 */

import { AurixError, AurixNetworkError } from "./errors.js";
import type { EventEnvelope } from "./generated/types.js";
import type { AurixHttp, RequestOptions } from "./http.js";

export interface SseEvent {
  /** `event:` field — an event type, `stream.open`, or `lagged`. */
  type: string;
  /** `id:` field (event id) when present. */
  id: string | undefined;
  /** Parsed `data:` (JSON) — an `EventEnvelope` for regular events; raw string when not JSON. */
  data: EventEnvelope | unknown;
  raw: string;
}

export interface EventStreamOptions extends RequestOptions {
  /** Comma-separated (or array of) event types; default: everything except high-frequency ones. */
  types?: string | string[];
  /** Reconnect on stream end / network error (default true). Non-retryable HTTP errors always throw. */
  reconnect?: boolean;
  /** Initial reconnect delay in ms (default 1000, doubles up to `maxReconnectDelayMs`). */
  reconnectDelayMs?: number;
  maxReconnectDelayMs?: number;
  /** Resume point sent as `Last-Event-ID` on the first connection. */
  lastEventId?: string;
}

/**
 * Iterates the application's event stream. Stop by aborting `options.signal` or breaking out of
 * the `for await` loop. On `lagged` fetch `/v1/events/snapshot` to resynchronise.
 */
export async function* eventStream(client: AurixHttp, options: EventStreamOptions = {}): AsyncGenerator<SseEvent, void, undefined> {
  const fetchImpl = client.fetchImpl;
  const types = Array.isArray(options.types) ? options.types.join(",") : options.types;
  const url = client.url("/v1/events", types ? { types } : undefined);
  let lastEventId = options.lastEventId;
  let delay = options.reconnectDelayMs ?? 1000;
  const maxDelay = options.maxReconnectDelayMs ?? 30_000;
  const reconnect = options.reconnect ?? true;
  for (;;) {
    if (options.signal?.aborted) return;
    const headers: Record<string, string> = {
      Accept: "text/event-stream",
      ...client.opts.headers,
      ...client.authHeaders(options.auth ?? client.opts),
      ...options.headers,
    };
    if (lastEventId) headers["Last-Event-ID"] = lastEventId;
    let res: Response;
    try {
      const init: RequestInit = { method: "GET", headers };
      if (options.signal) init.signal = options.signal;
      res = await fetchImpl(url, init);
    } catch (err) {
      if (options.signal?.aborted) return;
      if (!reconnect) throw new AurixNetworkError(err instanceof Error ? err.message : String(err), "GET", "/v1/events", err);
      await sleep(delay, options.signal);
      delay = Math.min(maxDelay, delay * 2);
      continue;
    }
    if (!res.ok) {
      const text = await res.text();
      const error = AurixError.fromResponse(res.status, res.headers.get("content-type") ?? "", text, res.headers, "GET", "/v1/events");
      if (reconnect && (res.status === 429 || res.status >= 500)) {
        await sleep(error.retryAfterMs ?? delay, options.signal);
        delay = Math.min(maxDelay, delay * 2);
        continue;
      }
      throw error;
    }
    if (!res.body) throw new AurixNetworkError("event stream has no body", "GET", "/v1/events");
    delay = options.reconnectDelayMs ?? 1000;
    try {
      for await (const ev of parseSse(res.body, options.signal)) {
        if (ev.id !== undefined) lastEventId = ev.id;
        if (ev.retry !== undefined) delay = ev.retry;
        if (ev.type === undefined && ev.data === "") continue;
        let data: unknown = ev.data;
        try {
          data = JSON.parse(ev.data);
        } catch {
          /* keep raw */
        }
        yield { type: ev.type ?? "message", id: ev.id, data, raw: ev.data };
      }
    } catch (err) {
      if (options.signal?.aborted) return;
      if (!reconnect) throw err;
    }
    if (options.signal?.aborted || !reconnect) return;
    await sleep(delay, options.signal);
    delay = Math.min(maxDelay, delay * 2);
  }
}

interface RawSse {
  type: string | undefined;
  id: string | undefined;
  data: string;
  retry: number | undefined;
}

/** Minimal SSE parser (comments, multi-line `data:`, `id:`, `event:`, `retry:`). */
export async function* parseSse(body: ReadableStream<Uint8Array>, signal?: AbortSignal): AsyncGenerator<RawSse, void, undefined> {
  const reader = body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";
  let cur: RawSse = { type: undefined, id: undefined, data: "", retry: undefined };
  let dataLines: string[] = [];
  const onAbort = () => void reader.cancel().catch(() => undefined);
  signal?.addEventListener("abort", onAbort, { once: true });
  try {
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true });
      let nl: number;
      while ((nl = buffer.search(/\r\n|\n|\r/)) >= 0) {
        const line = buffer.slice(0, nl);
        buffer = buffer.slice(nl + (buffer.startsWith("\r\n", nl) ? 2 : 1));
        if (line === "") {
          if (dataLines.length > 0 || cur.type !== undefined || cur.id !== undefined) {
            cur.data = dataLines.join("\n");
            yield cur;
          }
          cur = { type: undefined, id: undefined, data: "", retry: undefined };
          dataLines = [];
          continue;
        }
        if (line.startsWith(":")) continue;
        const colon = line.indexOf(":");
        const field = colon < 0 ? line : line.slice(0, colon);
        let value = colon < 0 ? "" : line.slice(colon + 1);
        if (value.startsWith(" ")) value = value.slice(1);
        switch (field) {
          case "event":
            cur.type = value;
            break;
          case "data":
            dataLines.push(value);
            break;
          case "id":
            if (!value.includes("\0")) cur.id = value;
            break;
          case "retry": {
            const n = Number(value);
            if (Number.isInteger(n) && n >= 0) cur.retry = n;
            break;
          }
          default:
            break;
        }
      }
    }
  } finally {
    signal?.removeEventListener("abort", onAbort);
    reader.releaseLock();
  }
}

function sleep(ms: number, signal?: AbortSignal): Promise<void> {
  return new Promise((resolve) => {
    if (signal?.aborted) return resolve();
    const t = setTimeout(() => {
      signal?.removeEventListener("abort", done);
      resolve();
    }, ms);
    const done = () => {
      clearTimeout(t);
      resolve();
    };
    signal?.addEventListener("abort", done, { once: true });
  });
}
