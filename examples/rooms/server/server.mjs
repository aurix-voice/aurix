/**
 * Aurix Rooms — demo backend (Node ≥ 20, stdlib `http` + `@aurix/server-sdk`).
 *
 *   browser ──▶ GET  /api/rooms/:slug          room card (title, headcount, what the bot plays)
 *   browser ──▶ POST /api/rooms                create a room  = POST /v1/channels (`rooms/<slug>`)
 *   browser ──▶ POST /api/rooms/:slug/join     player token   = POST /v1/tokens with one channel grant
 *   bot     ──▶ POST /api/bot/session          bot token for the stage room (shared secret)
 *   bot     ──▶ PUT  /api/bot/status           "now playing"; fanned out on GET /api/bot/events (SSE)
 *
 * The Aurix API key lives only here. Browsers get a session JWT scoped to exactly one channel;
 * the room slug is the invitation. Everything else — presence, speaking, chat, quality — comes
 * straight from the Aurix node over the Web SDK; this process is not in the media or signalling
 * path (a reverse proxy puts `/ws` and `/v1/me/*` of the node next to this app on one origin).
 */

import { timingSafeEqual } from "node:crypto";
import { createReadStream, readFileSync, statSync } from "node:fs";
import { createServer } from "node:http";
import { extname, join, normalize, resolve } from "node:path";
import { pathToFileURL } from "node:url";

import { AurixClient, AurixError, AurixNetworkError } from "@aurix/server-sdk";

import {
  MAX_TITLE,
  PROFILES,
  RoomStore,
  SLUG_RE,
  channelNameFor,
  normalizeTitle,
  randomSlug,
  slugFromChannelName,
} from "./rooms.mjs";

const MAX_BODY = 8 * 1024;
const MAX_NAME = 32;
const BOT_STALE_MS = 30_000;
const SWEEP_INTERVAL_MS = 15 * 60_000;
const ID_RE = /^[A-Za-z0-9._~-]{8,128}$/;
const TRANSPORTS = new Set(["auto", "webrtc", "webtransport", "websocket"]);

/** Reads configuration from the environment; throws on anything unsafe or missing. */
export function loadConfig(env = process.env) {
  const apiKey = env.AURIX_API_KEY_FILE ? readFileSync(env.AURIX_API_KEY_FILE, "utf8").trim() : env.AURIX_API_KEY?.trim();
  if (!apiKey) throw new Error("set AURIX_API_KEY or AURIX_API_KEY_FILE (backend environment only)");
  const botToken = env.ROOMS_BOT_TOKEN?.trim();
  if (botToken !== undefined && botToken.length < 24) throw new Error("ROOMS_BOT_TOKEN must be ≥ 24 characters");
  const transport = env.ROOMS_TRANSPORT ?? "auto";
  if (!TRANSPORTS.has(transport)) throw new Error(`ROOMS_TRANSPORT must be one of ${[...TRANSPORTS].join(", ")}`);
  const idleHours = Number(env.ROOMS_IDLE_HOURS ?? 24);
  if (!Number.isFinite(idleHours) || idleHours < 1) throw new Error("ROOMS_IDLE_HOURS must be ≥ 1");
  return {
    aurixUrl: (env.AURIX_URL || "http://localhost:8080").replace(/\/+$/, ""),
    apiKey,
    /** Node WebSocket URL handed to the bot (same host, no proxy). */
    aurixWsUrl: env.AURIX_WS_URL || "ws://localhost:8081/ws",
    /** Public URLs the browser uses; derived from the request when unset or empty (reverse proxy in front). */
    publicApiUrl: env.ROOMS_PUBLIC_API_URL?.replace(/\/+$/, "") || undefined,
    publicWsUrl: env.ROOMS_PUBLIC_WS_URL || undefined,
    /**
     * Media transport the browser should use. `auto` tries WebTransport → WebRTC → WebSocket;
     * behind an HTTP-only tunnel (ngrok, cloudflared) set `websocket` so nobody waits on ICE.
     */
    transport,
    port: Number(env.PORT || 3000),
    host: env.HOST || "0.0.0.0",
    stateFile: env.ROOMS_STATE_FILE ?? "./data/rooms.json",
    staticDir: env.ROOMS_STATIC_DIR,
    botToken,
    stageSlug: env.ROOMS_STAGE_SLUG ?? "lounge",
    stageTitle: env.ROOMS_STAGE_TITLE ?? "Lounge",
    idleHours,
    region: env.AURIX_REGION || undefined,
    trustProxy: env.TRUST_PROXY !== "0",
  };
}

// ---------------------------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------------------------

class HttpError extends Error {
  constructor(status, code, message) {
    super(message);
    this.status = status;
    this.code = code;
  }
}

const SECURITY_HEADERS = {
  "x-content-type-options": "nosniff",
  "referrer-policy": "same-origin",
  "permissions-policy": "microphone=(self), camera=(), geolocation=()",
  "x-frame-options": "DENY",
};

function sendJson(res, status, body, extra = {}) {
  const data = JSON.stringify(body);
  res.writeHead(status, {
    "content-type": "application/json; charset=utf-8",
    "content-length": Buffer.byteLength(data),
    "cache-control": "no-store",
    ...SECURITY_HEADERS,
    ...extra,
  });
  res.end(data);
}

function sendError(res, err) {
  if (err instanceof HttpError) return sendJson(res, err.status, { error: { code: err.code, message: err.message } });
  if (err instanceof AurixError) {
    const status = err.status === 429 ? 429 : err.status === 404 ? 404 : 502;
    return sendJson(res, status, { error: { code: `AURIX_${err.code}`, message: err.message } });
  }
  if (err instanceof AurixNetworkError) {
    return sendJson(res, 503, { error: { code: "AURIX_UNREACHABLE", message: "voice node is not reachable" } });
  }
  console.error("unhandled", err);
  sendJson(res, 500, { error: { code: "INTERNAL", message: "internal error" } });
}

function readJson(req) {
  return new Promise((resolvePromise, reject) => {
    let size = 0;
    const chunks = [];
    req.on("data", (c) => {
      size += c.length;
      if (size > MAX_BODY) {
        reject(new HttpError(413, "BODY_TOO_LARGE", "request body too large"));
        req.destroy();
        return;
      }
      chunks.push(c);
    });
    req.on("end", () => {
      if (chunks.length === 0) return resolvePromise({});
      try {
        const parsed = JSON.parse(Buffer.concat(chunks).toString("utf8"));
        if (parsed === null || typeof parsed !== "object" || Array.isArray(parsed)) throw new Error("not an object");
        resolvePromise(parsed);
      } catch {
        reject(new HttpError(400, "BAD_JSON", "body must be a JSON object"));
      }
    });
    req.on("error", reject);
  });
}

/** Fixed-window per-key limiter; enough to keep a demo from being a token faucet. */
class RateLimiter {
  #hits = new Map();
  constructor(limit, windowMs) {
    this.limit = limit;
    this.windowMs = windowMs;
  }
  check(key, now = Date.now()) {
    const slot = Math.floor(now / this.windowMs);
    const entry = this.#hits.get(key);
    if (!entry || entry.slot !== slot) {
      this.#hits.set(key, { slot, count: 1 });
      if (this.#hits.size > 10_000) this.#hits.clear();
      return true;
    }
    entry.count += 1;
    return entry.count <= this.limit;
  }
}

function clientIp(req, trustProxy) {
  if (trustProxy) {
    const fwd = req.headers["x-forwarded-for"];
    if (typeof fwd === "string" && fwd.length > 0) return fwd.split(",")[0].trim();
  }
  return req.socket.remoteAddress ?? "unknown";
}

function requestOrigin(req, cfg) {
  const host = (cfg.trustProxy && req.headers["x-forwarded-host"]) || req.headers.host || `localhost:${cfg.port}`;
  const proto = (cfg.trustProxy && req.headers["x-forwarded-proto"]) || "http";
  return { host: String(host).split(",")[0].trim(), secure: String(proto).split(",")[0].trim() === "https" };
}

function publicUrls(req, cfg) {
  const { host, secure } = requestOrigin(req, cfg);
  return {
    apiUrl: cfg.publicApiUrl ?? `${secure ? "https" : "http"}://${host}`,
    wsUrl: cfg.publicWsUrl ?? `${secure ? "wss" : "ws"}://${host}/ws`,
    transport: cfg.transport,
  };
}

function bearerMatches(req, expected) {
  if (!expected) return false;
  const header = req.headers.authorization;
  if (typeof header !== "string" || !header.startsWith("Bearer ")) return false;
  const given = Buffer.from(header.slice(7));
  const want = Buffer.from(expected);
  return given.length === want.length && timingSafeEqual(given, want);
}

// ---------------------------------------------------------------------------------------------
// Static frontend (built Vite bundle) with SPA fallback
// ---------------------------------------------------------------------------------------------

const MIME = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".mjs": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".json": "application/json; charset=utf-8",
  ".svg": "image/svg+xml",
  ".png": "image/png",
  ".ico": "image/x-icon",
  ".webmanifest": "application/manifest+json",
  ".woff2": "font/woff2",
  ".txt": "text/plain; charset=utf-8",
};

const CSP = [
  "default-src 'self'",
  "script-src 'self' blob:", // the SDK's AudioWorklet modules (playback, effects, visemes) are blob: URLs
  "style-src 'self'",
  "img-src 'self' data:",
  "font-src 'self'",
  "connect-src 'self' https: wss:",
  "worker-src 'self' blob:",
  "media-src 'self' blob:",
  "base-uri 'none'",
  "form-action 'none'",
  "frame-ancestors 'none'",
].join("; ");

function serveStatic(root, req, res) {
  const url = new URL(req.url, "http://x");
  let path = normalize(decodeURIComponent(url.pathname)).replace(/^(\.\.[/\\])+/, "");
  if (path.endsWith("/")) path += "index.html";
  let file = resolve(root, `.${path}`);
  if (!file.startsWith(resolve(root))) {
    res.writeHead(403);
    return res.end();
  }
  let stat;
  try {
    stat = statSync(file);
    if (stat.isDirectory()) throw new Error("dir");
  } catch {
    file = join(root, "index.html");
    try {
      stat = statSync(file);
    } catch {
      res.writeHead(404, { "content-type": "text/plain" });
      return res.end("frontend not built");
    }
  }
  const ext = extname(file);
  const immutable = /\/assets\//.test(file) && /-[A-Za-z0-9_-]{6,}\.[a-z0-9]+$/.test(file);
  const headers = {
    "content-type": MIME[ext] ?? "application/octet-stream",
    "content-length": stat.size,
    "cache-control": immutable ? "public, max-age=31536000, immutable" : "no-cache",
    ...SECURITY_HEADERS,
  };
  if (ext === ".html") headers["content-security-policy"] = CSP;
  res.writeHead(200, headers);
  if (req.method === "HEAD") return res.end();
  createReadStream(file).pipe(res);
}

// ---------------------------------------------------------------------------------------------
// Rooms service
// ---------------------------------------------------------------------------------------------

export class RoomsService {
  constructor(cfg, aurix = new AurixClient({ baseUrl: cfg.aurixUrl, apiKey: cfg.apiKey, userAgent: "aurix-rooms/1.6" })) {
    this.cfg = cfg;
    this.aurix = aurix;
    this.store = new RoomStore(cfg.stateFile);
    this.bots = new Map();
    this.sseClients = new Set();
    this.joinLimiter = new RateLimiter(30, 60_000);
    this.createLimiter = new RateLimiter(5, 60_000);
  }

  /**
   * Reconciles the registry with the node's channel list and makes sure the stage room exists.
   * Several channels may carry the same `rooms/<slug>` name (the API does not enforce uniqueness);
   * the one already on record wins, otherwise the oldest.
   */
  async start() {
    const bySlug = new Map();
    for (let page = 1; page <= 50; page += 1) {
      const list = await this.aurix.listChannels({ page, per_page: 100 });
      for (const ch of list.data ?? []) {
        const slug = slugFromChannelName(ch.name);
        if (!slug || ch.deleted_at) continue;
        const known = this.store.get(slug);
        const current = bySlug.get(slug);
        const keep =
          !current ||
          (known?.channelId === ch.id) ||
          (known?.channelId !== current.id && String(ch.created_at) < String(current.created_at));
        if (keep) bySlug.set(slug, ch);
      }
      if ((list.data?.length ?? 0) < 100) break;
    }
    for (const [slug, ch] of bySlug) {
      const known = this.store.get(slug);
      if (known?.channelId === ch.id) continue;
      this.store.put({
        slug,
        channelId: ch.id,
        title: known?.title ?? slug,
        profile: ch.config?.audio_profile === "music" ? "music" : "voice",
        pinned: known?.pinned ?? slug === this.cfg.stageSlug,
        createdAt: ch.created_at,
        lastActiveAt: known?.lastActiveAt ?? ch.updated_at,
      });
    }
    for (const room of this.store.list()) if (!bySlug.has(room.slug)) this.store.delete(room.slug);
    if (!this.store.get(this.cfg.stageSlug)) {
      await this.createRoom({ slug: this.cfg.stageSlug, title: this.cfg.stageTitle, profile: "music", pinned: true });
    }
    this.sweepTimer = setInterval(() => this.sweep().catch((e) => console.error("sweep", e)), SWEEP_INTERVAL_MS);
    this.sweepTimer.unref();
  }

  stop() {
    clearInterval(this.sweepTimer);
    for (const res of this.sseClients) res.end();
  }

  async createRoom({ slug = randomSlug(), title, profile = "voice", pinned = false }) {
    const config = PROFILES[profile];
    if (!config) throw new HttpError(400, "BAD_PROFILE", "profile must be voice or music");
    const channel = await this.aurix.createChannel({ name: channelNameFor(slug), config: { ...config } });
    const now = new Date().toISOString();
    return this.store.put({
      slug,
      channelId: channel.id,
      title: title ?? slug,
      profile,
      pinned,
      createdAt: channel.created_at ?? now,
      lastActiveAt: now,
    });
  }

  /** Removes link-only rooms nobody used for `idleHours`; the stage and pinned rooms stay. */
  async sweep(now = Date.now()) {
    const cutoff = now - this.cfg.idleHours * 3_600_000;
    for (const room of this.store.list()) {
      if (room.pinned || Date.parse(room.lastActiveAt) > cutoff) continue;
      let channel;
      try {
        channel = await this.aurix.getChannel(room.channelId);
      } catch (err) {
        if (err instanceof AurixError && err.status === 404) {
          this.store.delete(room.slug);
          continue;
        }
        throw err;
      }
      if ((channel.active_participants ?? 0) > 0) {
        this.store.touch(room.slug, now);
        continue;
      }
      await this.aurix.deleteChannel(room.channelId);
      this.store.delete(room.slug);
    }
  }

  botStatusFor(slug) {
    const entry = this.bots.get(slug);
    if (!entry || Date.now() - entry.updatedAt > BOT_STALE_MS) return null;
    return entry.status;
  }

  async roomCard(room) {
    const channel = await this.aurix.getChannel(room.channelId);
    return {
      slug: room.slug,
      title: room.title,
      profile: room.profile,
      pinned: room.pinned,
      participants: channel.active_participants ?? 0,
      maxParticipants: channel.max_participants,
      stereo: channel.config?.stereo === true,
      createdAt: room.createdAt,
      bot: this.botStatusFor(room.slug),
    };
  }

  requireRoom(slug) {
    if (!SLUG_RE.test(slug)) throw new HttpError(404, "ROOM_NOT_FOUND", "no such room");
    const room = this.store.get(slug);
    if (!room) throw new HttpError(404, "ROOM_NOT_FOUND", "no such room");
    return room;
  }

  async issueToken(room, { externalId, displayName, moderate = false }) {
    const res = await this.aurix.issueToken({
      external_id: externalId,
      display_name: displayName,
      channels: [{ channel_id: room.channelId, join: true, speak: true, receive: true, moderate }],
      metadata: { app: "aurix-rooms", room: room.slug },
      ...(this.cfg.region ? { region: this.cfg.region } : {}),
    });
    this.store.touch(room.slug);
    return res;
  }

  setBotStatus(status) {
    this.bots.set(status.room, { status, updatedAt: Date.now() });
    const frame = `event: bot\ndata: ${JSON.stringify(status)}\n\n`;
    for (const res of this.sseClients) {
      if (res.botRoom === status.room) res.write(frame);
    }
  }

  // -- request handling ------------------------------------------------------------------------

  async handleApi(req, res, url) {
    const path = url.pathname;
    const ip = clientIp(req, this.cfg.trustProxy);

    if (path === "/api/config" && req.method === "GET") {
      const urls = publicUrls(req, this.cfg);
      return sendJson(res, 200, { ...urls, stage: this.cfg.stageSlug, maxNameLength: MAX_NAME, maxTitleLength: MAX_TITLE });
    }

    if (path === "/api/rooms" && req.method === "POST") {
      if (!this.createLimiter.check(ip)) throw new HttpError(429, "RATE_LIMITED", "too many rooms created; try later");
      const body = await readJson(req);
      const title = normalizeTitle(body.title);
      const profile = body.profile === "music" ? "music" : "voice";
      let room;
      for (let attempt = 0; attempt < 5 && !room; attempt += 1) {
        const slug = randomSlug();
        if (!this.store.get(slug)) room = await this.createRoom({ slug, title, profile });
      }
      if (!room) throw new HttpError(503, "NO_SLUG", "could not allocate a room code");
      return sendJson(res, 201, await this.roomCard(room));
    }

    const roomMatch = /^\/api\/rooms\/([^/]+)(?:\/(join))?$/.exec(path);
    if (roomMatch) {
      const room = this.requireRoom(roomMatch[1]);
      if (!roomMatch[2] && req.method === "GET") return sendJson(res, 200, await this.roomCard(room));
      if (roomMatch[2] === "join" && req.method === "POST") {
        if (!this.joinLimiter.check(ip)) throw new HttpError(429, "RATE_LIMITED", "too many joins; try later");
        const body = await readJson(req);
        const displayName = normalizeTitle(body.name);
        if (!displayName || displayName.length > MAX_NAME) throw new HttpError(400, "BAD_NAME", `name must be 1..${MAX_NAME} characters`);
        if (typeof body.deviceId !== "string" || !ID_RE.test(body.deviceId)) throw new HttpError(400, "BAD_DEVICE", "deviceId is missing or malformed");
        const token = await this.issueToken(room, { externalId: `rooms:${body.deviceId}`, displayName });
        const urls = publicUrls(req, this.cfg);
        return sendJson(res, 200, {
          token: token.token,
          userId: token.user_id,
          expiresAt: token.expires_at,
          channelId: room.channelId,
          room: await this.roomCard(room),
          ...urls,
        });
      }
      throw new HttpError(405, "METHOD_NOT_ALLOWED", "method not allowed");
    }

    if (path === "/api/bot/events" && req.method === "GET") {
      res.writeHead(200, {
        "content-type": "text/event-stream",
        "cache-control": "no-store",
        connection: "keep-alive",
        "x-accel-buffering": "no",
        ...SECURITY_HEADERS,
      });
      res.botRoom = url.searchParams.get("room") ?? this.cfg.stageSlug;
      res.write(`retry: 3000\nevent: bot\ndata: ${JSON.stringify(this.botStatusFor(res.botRoom))}\n\n`);
      this.sseClients.add(res);
      const ping = setInterval(() => res.write(": ping\n\n"), 20_000);
      req.on("close", () => {
        clearInterval(ping);
        this.sseClients.delete(res);
      });
      return undefined;
    }

    if (path.startsWith("/api/bot/")) {
      if (!bearerMatches(req, this.cfg.botToken)) throw new HttpError(401, "BOT_AUTH", "bot token required");
      if (path === "/api/bot/session" && req.method === "POST") {
        const body = await readJson(req);
        const room = this.requireRoom(typeof body.room === "string" ? body.room : this.cfg.stageSlug);
        const displayName = normalizeTitle(body.name) ?? "Aurix Bot";
        const token = await this.issueToken(room, { externalId: `rooms-bot:${room.slug}`, displayName, moderate: true });
        return sendJson(res, 200, {
          token: token.token,
          userId: token.user_id,
          expiresAt: token.expires_at,
          channelId: room.channelId,
          room: room.slug,
          wsUrl: this.cfg.aurixWsUrl,
        });
      }
      if (path === "/api/bot/status" && req.method === "PUT") {
        const body = await readJson(req);
        if (typeof body.room !== "string" || !this.store.get(body.room)) throw new HttpError(400, "BAD_ROOM", "unknown room");
        this.setBotStatus({ ...body, updatedAt: new Date().toISOString() });
        res.writeHead(204, SECURITY_HEADERS);
        return res.end();
      }
      throw new HttpError(404, "NOT_FOUND", "no such endpoint");
    }

    throw new HttpError(404, "NOT_FOUND", "no such endpoint");
  }
}

export function createRoomsServer(service) {
  const { cfg } = service;
  return createServer((req, res) => {
    const url = new URL(req.url ?? "/", "http://x");
    if (url.pathname === "/healthz") return sendJson(res, 200, { status: "ok", rooms: service.store.list().length });
    if (url.pathname.startsWith("/api/")) {
      return service.handleApi(req, res, url).catch((err) => sendError(res, err));
    }
    if (req.method !== "GET" && req.method !== "HEAD") {
      res.writeHead(405);
      return res.end();
    }
    if (!cfg.staticDir) {
      res.writeHead(404, { "content-type": "text/plain" });
      return res.end("frontend not configured (ROOMS_STATIC_DIR)");
    }
    return serveStatic(cfg.staticDir, req, res);
  });
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const cfg = loadConfig();
  const service = new RoomsService(cfg);
  try {
    await service.start();
  } catch (err) {
    console.error(`cannot reach Aurix at ${cfg.aurixUrl}:`, err.message);
    process.exit(1);
  }
  const server = createRoomsServer(service);
  server.listen(cfg.port, cfg.host, () => {
    console.log(`aurix-rooms listening on http://${cfg.host}:${cfg.port} → ${cfg.aurixUrl}, stage "${cfg.stageSlug}", bot ${cfg.botToken ? "enabled" : "disabled"}`);
  });
  const shutdown = () => {
    service.stop();
    server.close(() => process.exit(0));
    setTimeout(() => process.exit(0), 2000).unref();
  };
  process.on("SIGINT", shutdown);
  process.on("SIGTERM", shutdown);
}
