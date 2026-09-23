import assert from "node:assert/strict";
import { createServer } from "node:http";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, before, test } from "node:test";

import { RoomsService, createRoomsServer, loadConfig } from "../server.mjs";
import { PROFILES, SLUG_RE, randomSlug, slugFromChannelName } from "../rooms.mjs";

const API_KEY = "aurx_test_SECRET_KEY_never_in_client_payload";
const BOT_TOKEN = "bot-secret-0123456789abcdefghij";

/** Minimal Aurix control plane: channels + tokens, enough for the demo backend. */
function fakeAurix() {
  const channels = new Map();
  const tokens = [];
  let seq = 0;
  const server = createServer((req, res) => {
    let body = "";
    req.on("data", (c) => (body += c));
    req.on("end", () => {
      const json = (status, obj) => {
        res.writeHead(status, { "content-type": "application/json" });
        res.end(JSON.stringify(obj));
      };
      if (req.headers["x-api-key"] !== API_KEY) return json(401, { error: { code: "AUTH_FAILED", message: "bad key" } });
      const url = new URL(req.url, "http://x");
      const parsed = body ? JSON.parse(body) : undefined;
      if (url.pathname === "/v1/channels" && req.method === "GET") {
        const all = [...channels.values()].filter((c) => !c.deleted_at);
        return json(200, { data: all, total: all.length, page: 1, per_page: 100 });
      }
      if (url.pathname === "/v1/channels" && req.method === "POST") {
        seq += 1;
        const id = `00000000-0000-4000-8000-${String(seq).padStart(12, "0")}`;
        const now = new Date().toISOString();
        const ch = {
          id,
          app_id: "app",
          name: parsed.name,
          channel_type: parsed.config.channel_type,
          config: parsed.config,
          max_participants: parsed.config.max_participants,
          active_participants: 0,
          created_at: now,
          updated_at: now,
        };
        channels.set(id, ch);
        return json(201, ch);
      }
      const one = /^\/v1\/channels\/([^/]+)$/.exec(url.pathname);
      if (one) {
        const ch = channels.get(one[1]);
        if (!ch || ch.deleted_at) return json(404, { error: { code: "NOT_FOUND", message: "no channel" } });
        if (req.method === "GET") return json(200, ch);
        if (req.method === "DELETE") {
          ch.deleted_at = new Date().toISOString();
          return json(200, { deleted: true });
        }
      }
      if (url.pathname === "/v1/tokens" && req.method === "POST") {
        tokens.push(parsed);
        return json(200, {
          token: `jwt-${tokens.length}`,
          user_id: `u-${parsed.external_id}`,
          expires_at: "2030-01-01T00:00:00Z",
          channels: parsed.channels.map((g) => ({ channel_id: g.channel_id, join: true, speak: true, receive: true })),
          endpoint: null,
        });
      }
      json(404, { error: { code: "NOT_FOUND", message: url.pathname } });
    });
  });
  return { server, channels, tokens };
}

let aurix, service, http, base;

before(async () => {
  aurix = fakeAurix();
  await new Promise((r) => aurix.server.listen(0, "127.0.0.1", r));
  const cfg = loadConfig({
    AURIX_URL: `http://127.0.0.1:${aurix.server.address().port}`,
    AURIX_API_KEY: API_KEY,
    AURIX_WS_URL: "ws://127.0.0.1:8081/ws",
    ROOMS_BOT_TOKEN: BOT_TOKEN,
    ROOMS_STATE_FILE: join(mkdtempSync(join(tmpdir(), "rooms-")), "rooms.json"),
    ROOMS_IDLE_HOURS: "1",
  });
  service = new RoomsService(cfg);
  await service.start();
  http = createRoomsServer(service);
  await new Promise((r) => http.listen(0, "127.0.0.1", r));
  base = `http://127.0.0.1:${http.address().port}`;
});

after(() => {
  service.stop();
  http.close();
  aurix.server.close();
});

const api = (method, path, body, headers = {}) =>
  fetch(base + path, {
    method,
    headers: { "content-type": "application/json", ...headers },
    body: body === undefined ? undefined : JSON.stringify(body),
  });

test("slugs are typeable and map to channel names", () => {
  for (let i = 0; i < 200; i += 1) {
    const slug = randomSlug();
    assert.match(slug, SLUG_RE);
    assert.equal(slugFromChannelName(`rooms/${slug}`), slug);
  }
  assert.equal(slugFromChannelName("match-42"), undefined);
  assert.equal(slugFromChannelName("rooms/Bad Slug"), undefined);
});

test("start creates the pinned stage room with the music profile", async () => {
  const r = await api("GET", "/api/rooms/lounge");
  assert.equal(r.status, 200);
  const card = await r.json();
  assert.equal(card.title, "Lounge");
  assert.equal(card.profile, "music");
  assert.equal(card.pinned, true);
  assert.equal(card.stereo, true);
  assert.equal(card.bot, null);
  const ch = [...aurix.channels.values()].find((c) => c.name === "rooms/lounge");
  assert.deepEqual(ch.config, PROFILES.music);
});

test("a restart reconciles with the node and keeps the stage on the same channel", async () => {
  const before = service.requireRoom("lounge").channelId;
  const stray = { ...aurix.channels.get(before), id: "00000000-0000-4000-8000-00000000dead", created_at: "2000-01-01T00:00:00Z" };
  aurix.channels.set(stray.id, stray);
  const again = new RoomsService(service.cfg);
  await again.start();
  again.stop();
  assert.equal(again.requireRoom("lounge").channelId, before);
  aurix.channels.delete(stray.id);
  assert.equal([...aurix.channels.values()].filter((c) => c.name === "rooms/lounge").length, 1);
});

test("config derives public URLs from the proxy headers", async () => {
  const r = await api("GET", "/api/config", undefined, { "x-forwarded-host": "rooms.example", "x-forwarded-proto": "https" });
  const cfg = await r.json();
  assert.equal(cfg.apiUrl, "https://rooms.example");
  assert.equal(cfg.wsUrl, "wss://rooms.example/ws");
  assert.equal(cfg.stage, "lounge");
  assert.equal(cfg.transport, "auto");
});

test("a created room is a voice channel and a join issues a token for exactly that channel", async () => {
  const created = await api("POST", "/api/rooms", { title: "  Raid   night  " });
  assert.equal(created.status, 201);
  const room = await created.json();
  assert.match(room.slug, SLUG_RE);
  assert.equal(room.title, "Raid night");
  assert.equal(room.profile, "voice");
  assert.equal(room.pinned, false);

  const bad = await api("POST", `/api/rooms/${room.slug}/join`, { name: "", deviceId: "d".repeat(16) });
  assert.equal(bad.status, 400);
  const badDevice = await api("POST", `/api/rooms/${room.slug}/join`, { name: "Ann", deviceId: "short" });
  assert.equal(badDevice.status, 400);

  const joined = await api(
    "POST",
    `/api/rooms/${room.slug}/join`,
    { name: "Ann", deviceId: "6f1c2e2a-0d8e-4a6b-9c1d-2f3e4a5b6c7d" },
    { "x-forwarded-host": "rooms.example", "x-forwarded-proto": "https" },
  );
  assert.equal(joined.status, 200);
  const grant = await joined.json();
  assert.equal(grant.wsUrl, "wss://rooms.example/ws");
  assert.equal(grant.apiUrl, "https://rooms.example");
  assert.equal(grant.channelId, room.slug && [...aurix.channels.values()].find((c) => c.name === `rooms/${room.slug}`).id);
  assert.equal(grant.room.slug, room.slug);
  assert.match(grant.token, /^jwt-/);

  const issued = aurix.tokens.at(-1);
  assert.equal(issued.external_id, "rooms:6f1c2e2a-0d8e-4a6b-9c1d-2f3e4a5b6c7d");
  assert.equal(issued.display_name, "Ann");
  assert.equal(issued.channels.length, 1);
  assert.equal(issued.channels[0].channel_id, grant.channelId);
  assert.equal(issued.channels[0].moderate, false);
  assert.equal(JSON.stringify(issued).includes(API_KEY), false);
});

test("unknown and malformed rooms are 404", async () => {
  assert.equal((await api("GET", "/api/rooms/never-made-1")).status, 404);
  assert.equal((await api("GET", "/api/rooms/../../etc")).status, 404);
  assert.equal((await api("POST", "/api/rooms/never-made-1/join", { name: "x", deviceId: "d".repeat(16) })).status, 404);
});

test("bot endpoints need the shared secret; status is fanned out over SSE", async () => {
  assert.equal((await api("POST", "/api/bot/session", {})).status, 401);
  assert.equal((await api("POST", "/api/bot/session", {}, { authorization: "Bearer nope" })).status, 401);

  const auth = { authorization: `Bearer ${BOT_TOKEN}` };
  const session = await (await api("POST", "/api/bot/session", { name: "Aurix Bot" }, auth)).json();
  assert.equal(session.room, "lounge");
  assert.equal(session.wsUrl, "ws://127.0.0.1:8081/ws");
  const issued = aurix.tokens.at(-1);
  assert.equal(issued.external_id, "rooms-bot:lounge");
  assert.equal(issued.channels[0].moderate, true);

  const ctrl = new AbortController();
  const sse = await fetch(`${base}/api/bot/events?room=lounge`, { signal: ctrl.signal });
  assert.equal(sse.headers.get("content-type"), "text/event-stream");
  const reader = sse.body.getReader();
  const decoder = new TextDecoder();
  let buf = decoder.decode((await reader.read()).value);
  assert.match(buf, /event: bot\ndata: null/);

  const status = { room: "lounge", track: { title: "Sweep 20 Hz – 20 kHz", kind: "signal" }, position: 0 };
  const put = await api("PUT", "/api/bot/status", status, auth);
  assert.equal(put.status, 204);
  while (!buf.includes("Sweep")) buf += decoder.decode((await reader.read()).value);
  ctrl.abort();

  const card = await (await api("GET", "/api/rooms/lounge")).json();
  assert.equal(card.bot.track.title, "Sweep 20 Hz – 20 kHz");
  assert.equal(card.bot.room, "lounge");

  // A bot in another room neither replaces the stage status nor reaches its SSE listeners.
  const other = await (await api("POST", "/api/rooms", { title: "Meter", profile: "music" })).json();
  const otherStatus = { room: other.slug, track: { title: "Reference tone", kind: "signal" }, position: 0 };
  assert.equal((await api("PUT", "/api/bot/status", otherStatus, auth)).status, 204);
  assert.equal((await (await api("GET", "/api/rooms/lounge")).json()).bot.track.title, "Sweep 20 Hz – 20 kHz");
  assert.equal((await (await api("GET", `/api/rooms/${other.slug}`)).json()).bot.track.title, "Reference tone");

  assert.equal((await api("PUT", "/api/bot/status", { room: "nope-1" }, auth)).status, 400);
});

test("sweep deletes idle link-only rooms and keeps the stage", async () => {
  const room = await (await api("POST", "/api/rooms", { title: "Old" })).json();
  service.store.touch(room.slug, Date.now() - 2 * 3_600_000);
  const before = aurix.channels.size;
  await service.sweep();
  assert.equal((await api("GET", `/api/rooms/${room.slug}`)).status, 404);
  assert.equal([...aurix.channels.values()].filter((c) => c.deleted_at).length, 1);
  assert.equal(aurix.channels.size, before);
  assert.equal((await api("GET", "/api/rooms/lounge")).status, 200);
});

test("static requests without a frontend build answer 404, api key never leaks", async () => {
  const r = await fetch(`${base}/r/lounge`);
  assert.equal(r.status, 404);
  const health = await (await fetch(`${base}/healthz`)).json();
  assert.equal(health.status, "ok");
});
