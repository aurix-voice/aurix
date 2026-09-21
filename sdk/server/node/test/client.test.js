import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createServer } from "node:http";
import { test } from "node:test";

import {
  AurixClient,
  AurixError,
  AurixNetworkError,
  WebhookVerificationError,
  parseWebhook,
  parseSse,
  signWebhook,
  verifyWebhookSignature,
} from "../dist/index.js";

const vector = JSON.parse(readFileSync(new URL("../../vectors/webhook_signature.json", import.meta.url), "utf8"));

/** Minimal fake Aurix node: records requests, answers from a scripted handler. */
async function withServer(handler, fn) {
  const calls = [];
  const server = createServer((req, res) => {
    const chunks = [];
    req.on("data", (c) => chunks.push(c));
    req.on("end", () => {
      const body = Buffer.concat(chunks).toString("utf8");
      const url = new URL(req.url, "http://x");
      calls.push({ method: req.method, path: url.pathname, query: url.searchParams, headers: req.headers, body: body ? JSON.parse(body) : undefined });
      handler(calls[calls.length - 1], res, calls.length);
    });
  });
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  const baseUrl = `http://127.0.0.1:${server.address().port}`;
  try {
    await fn(baseUrl, calls);
  } finally {
    server.close();
  }
}

function json(res, status, body, headers = {}) {
  res.writeHead(status, { "content-type": "application/json", ...headers });
  res.end(JSON.stringify(body));
}

test("issueToken posts the request body with the API key header and decodes TokenResponse", async () => {
  await withServer(
    (call, res) => json(res, 200, {"token": "jwt", "user_id": "11111111-0000-4000-8000-000000000001", "expires_at": "2030-01-01T00:00:00Z", "channels": [], "endpoint": {"region": "eu_west", "node_id": "22222222-0000-4000-8000-000000000002", "ws_url": "wss://node/ws", "probe_url": null, "location": null, "distance_km": null}}),
    async (baseUrl, calls) => {
      const client = new AurixClient({ baseUrl: `${baseUrl}/`, apiKey: "ak_test" });
      const tok = await client.issueToken({ external_id: "player-1", display_name: "Player", region: "eu_west" });
      assert.equal(tok.token, "jwt");
      assert.equal(tok.endpoint?.ws_url, "wss://node/ws");
      assert.equal(calls.length, 1);
      assert.equal(calls[0].method, "POST");
      assert.equal(calls[0].path, "/v1/tokens");
      assert.equal(calls[0].headers["x-api-key"], "ak_test");
      assert.equal(calls[0].headers.authorization, undefined);
      assert.deepEqual(calls[0].body, { external_id: "player-1", display_name: "Player", region: "eu_west" });
      assert.match(calls[0].headers["user-agent"], /aurix-server-sdk-node/);
    },
  );
});

test("path and query parameters are encoded; arrays repeat; undefined is skipped", async () => {
  await withServer(
    (call, res) => json(res, 200, { items: [], next_cursor: null }),
    async (baseUrl, calls) => {
      const client = new AurixClient({ baseUrl, apiKey: "k" });
      await client.listChannelMessages("ch/1 x", { limit: 5, before: undefined });
      assert.equal(calls[0].path, "/v1/channels/ch%2F1%20x/messages");
      assert.equal(calls[0].query.get("limit"), "5");
      assert.equal(calls[0].query.has("before"), false);
    },
  );
});

test("admin and player tokens go to Authorization: Bearer; per-call auth overrides client auth", async () => {
  await withServer(
    (call, res) => json(res, 200, { items: [], total: 0 }),
    async (baseUrl, calls) => {
      const client = new AurixClient({ baseUrl, adminToken: "admin-jwt" });
      await client.listApps();
      assert.equal(calls[0].headers.authorization, "Bearer admin-jwt");
      await client.listApps(undefined, { auth: { apiKey: "other" } });
      assert.equal(calls[1].headers.authorization, undefined);
      assert.equal(calls[1].headers["x-api-key"], "other");
    },
  );
});

test("non-2xx becomes AurixError with the API error envelope", async () => {
  await withServer(
    (call, res) => json(res, 403, { error: { code: "FORBIDDEN", message: "app mismatch" } }, { "x-request-id": "req-1" }),
    async (baseUrl) => {
      const client = new AurixClient({ baseUrl, apiKey: "k", maxRetries: 0 });
      await assert.rejects(client.getChannel("c1"), (err) => {
        assert.ok(err instanceof AurixError);
        assert.equal(err.status, 403);
        assert.equal(err.code, "FORBIDDEN");
        assert.equal(err.message, "GET /v1/channels/c1 → 403 FORBIDDEN: app mismatch");
        assert.equal(err.requestId, "req-1");
        assert.ok(err.isAuth);
        return true;
      });
    },
  );
});

test("retries idempotent requests on 503 and honours Retry-After on 429 for POST", async () => {
  await withServer(
    (call, res, n) => {
      if (call.path === "/health") return n === 1 ? json(res, 503, { error: { code: "UNAVAILABLE", message: "warming up" } }) : json(res, 200, { status: "ok" });
      if (n === 3) return json(res, 429, { error: { code: "RATE_LIMITED", message: "slow down" } }, { "retry-after": "0" });
      return json(res, 200, { token: "t", user_id: "u", expires_at: "x", channels: [], endpoint: null });
    },
    async (baseUrl, calls) => {
      const client = new AurixClient({ baseUrl, apiKey: "k", maxRetries: 2, maxBackoffMs: 10 });
      assert.equal((await client.health()).status, "ok");
      assert.equal(calls.length, 2);
      const tok = await client.issueToken({ external_id: "u", display_name: "U" });
      assert.equal(tok.token, "t");
      assert.equal(calls.length, 4);
    },
  );
});

test("POST is not retried on 503 (not idempotent)", async () => {
  await withServer(
    (call, res) => json(res, 503, { error: { code: "UNAVAILABLE", message: "x" } }),
    async (baseUrl, calls) => {
      const client = new AurixClient({ baseUrl, apiKey: "k", maxRetries: 3, maxBackoffMs: 5 });
      await assert.rejects(client.issueToken({ external_id: "u", display_name: "U" }), (e) => e instanceof AurixError && e.status === 503);
      assert.equal(calls.length, 1);
    },
  );
});

test("timeouts surface as AurixNetworkError", async () => {
  await withServer(
    () => {
      /* never answers */
    },
    async (baseUrl) => {
      const client = new AurixClient({ baseUrl, apiKey: "k", timeoutMs: 50, maxRetries: 0 });
      await assert.rejects(client.health(), (e) => e instanceof AurixNetworkError && /timeout after 50 ms/.test(e.message));
    },
  );
});

test("raw variants return bytes and content type for non-JSON responses; typed variant refuses them", async () => {
  await withServer(
    (call, res) => {
      res.writeHead(200, { "content-type": "text/csv" });
      res.end("bucket_start,minutes\n2025-01-01T00:00:00Z,12\n");
    },
    async (baseUrl) => {
      const client = new AurixClient({ baseUrl, apiKey: "k" });
      const raw = await client.exportUsageRaw({ format: "csv" });
      assert.equal(raw.status, 200);
      assert.equal(raw.contentType, "text/csv");
      assert.match(raw.text(), /^bucket_start,minutes/);
      await assert.rejects(client.exportUsage({ format: "csv" }), (e) => e instanceof AurixError && e.code === "unexpected_content_type");
    },
  );
});

test("204 / empty body resolves to undefined", async () => {
  await withServer(
    (call, res) => {
      res.writeHead(204);
      res.end();
    },
    async (baseUrl) => {
      const client = new AurixClient({ baseUrl, apiKey: "k" });
      assert.equal(await client.deleteChannel("c1"), undefined);
    },
  );
});

test("webhook signature: shared vector verifies, tampering / stale / wrong secret fail", () => {
  const { secret, header, body, timestamp } = vector;
  assert.equal(signWebhook(secret, timestamp, body), header);
  assert.ok(verifyWebhookSignature(secret, header, body, { nowSec: timestamp + 10 }));
  assert.ok(verifyWebhookSignature(secret, header, Buffer.from(body, "utf8"), { nowSec: timestamp - 10 }));
  assert.equal(verifyWebhookSignature(secret, header, body + " ", { nowSec: timestamp }), false);
  assert.equal(verifyWebhookSignature("whsec_other", header, body, { nowSec: timestamp }), false);
  assert.equal(verifyWebhookSignature(secret, header, body, { nowSec: timestamp + 301 }), false);
  assert.equal(verifyWebhookSignature(secret, header.replace("v1=", "v1=0"), body, { nowSec: timestamp }), false);
  assert.equal(verifyWebhookSignature(secret, "garbage", body, { nowSec: timestamp }), false);
  assert.equal(verifyWebhookSignature(secret, null, body), false);

  const delivery = parseWebhook(
    secret,
    { "x-aurix-signature": header, "X-Aurix-Event": "participant.joined", "x-aurix-delivery-id": "d1", "x-aurix-attempt": "2" },
    body,
    { nowSec: timestamp },
  );
  assert.equal(delivery.event.type, "participant.joined");
  assert.equal(delivery.event.data.user_id, "u1");
  assert.equal(delivery.deliveryId, "d1");
  assert.equal(delivery.attempt, 2);
  assert.throws(() => parseWebhook(secret, { "x-aurix-signature": header, "x-aurix-event": "participant.left" }, body, { nowSec: timestamp }), WebhookVerificationError);
  assert.throws(() => parseWebhook("nope", { "x-aurix-signature": header }, body, { nowSec: timestamp }), WebhookVerificationError);
});

test("SSE parser handles comments, multi-line data, ids and CRLF", async () => {
  const chunks = [
    ": keepalive\n\nevent: stream.open\ndata: {\"ok\":true}\n\n",
    "id: 42\r\nevent: participant.joined\r\ndata: {\"id\":\"e1\",\r\ndata: \"type\":\"participant.joined\"}\r\n\r\n",
    "retry: 250\ndata: tail\n\n",
  ];
  const stream = new ReadableStream({
    start(controller) {
      for (const c of chunks) controller.enqueue(new TextEncoder().encode(c));
      controller.close();
    },
  });
  const events = [];
  for await (const ev of parseSse(stream)) events.push(ev);
  assert.deepEqual(
    events.map((e) => [e.type, e.id, e.data, e.retry]),
    [
      ["stream.open", undefined, '{"ok":true}', undefined],
      ["participant.joined", "42", '{"id":"e1",\n"type":"participant.joined"}', undefined],
      [undefined, undefined, "tail", 250],
    ],
  );
});

test("events() streams SSE with auth and resumes with Last-Event-ID after the server closes", async () => {
  await withServer(
    (call, res, n) => {
      res.writeHead(200, { "content-type": "text/event-stream" });
      if (n === 1) {
        res.write("event: stream.open\ndata: {}\n\n");
        res.write('id: ev-1\nevent: participant.joined\ndata: {"id":"ev-1","type":"participant.joined","app_id":"a","created_at":"c","data":{}}\n\n');
        res.end();
      } else {
        res.write('id: ev-2\nevent: participant.left\ndata: {"id":"ev-2","type":"participant.left","app_id":"a","created_at":"c","data":{}}\n\n');
        res.end();
      }
    },
    async (baseUrl, calls) => {
      const client = new AurixClient({ baseUrl, apiKey: "k" });
      const ac = new AbortController();
      const seen = [];
      for await (const ev of client.events({ types: ["participant.joined", "participant.left"], signal: ac.signal, reconnectDelayMs: 5 })) {
        seen.push(ev.type);
        if (seen.length === 3) ac.abort();
      }
      assert.deepEqual(seen, ["stream.open", "participant.joined", "participant.left"]);
      assert.equal(calls[0].path, "/v1/events");
      assert.equal(calls[0].query.get("types"), "participant.joined,participant.left");
      assert.equal(calls[0].headers["x-api-key"], "k");
      assert.equal(calls[0].headers.accept, "text/event-stream");
      assert.equal(calls[1].headers["last-event-id"], "ev-1");
    },
  );
});
