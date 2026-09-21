/**
 * Backend-only Aurix token server (Node ≥ 18, stdlib http + @aurix/server-sdk).
 *
 *   game client ──(game session)──▶ POST /voice/token ──▶ this server ──(API key)──▶ Aurix POST /v1/tokens
 *   game client ◀── { token, user_id, expires_at, endpoint } ◀──────────────────────┘
 *
 * The API key lives only in this process' environment. Clients never see it, never choose their
 * own player id (that comes from the game session) and never choose their grants (the backend
 * decides which channel a player may join).
 */

import { createHmac, timingSafeEqual } from "node:crypto";
import { readFileSync } from "node:fs";
import { createServer } from "node:http";

import { AurixClient, AurixError, AurixNetworkError } from "@aurix/server-sdk";

const REGIONS = new Set([
  "us_east", "us_west", "eu_west", "eu_central", "asia_pacific", "south_america", "australia", "middle_east", "africa",
]);
const MAX_BODY = 4096;

/** Reads configuration from the environment; throws on anything unsafe or missing. */
export function loadConfig(env = process.env) {
  const apiKey = env.AURIX_API_KEY_FILE ? readFileSync(env.AURIX_API_KEY_FILE, "utf8").trim() : env.AURIX_API_KEY?.trim();
  if (!apiKey) throw new Error("set AURIX_API_KEY or AURIX_API_KEY_FILE (backend environment only)");
  const sessionSecret = env.GAME_SESSION_SECRET;
  if (!sessionSecret || sessionSecret.length < 32) throw new Error("GAME_SESSION_SECRET must be ≥ 32 characters");
  const region = env.AURIX_REGION || undefined;
  if (region && !REGIONS.has(region)) throw new Error(`AURIX_REGION must be one of ${[...REGIONS].join(", ")}`);
  return {
    aurixUrl: env.AURIX_URL || "http://localhost:8080",
    apiKey,
    sessionSecret,
    region,
    port: Number(env.PORT || 3000),
    allowDevLogin: env.ALLOW_DEV_LOGIN === "1",
  };
}

// ---------------------------------------------------------------------------------------------
// Game session — stand-in for your real login. Replace `authenticatePlayer` with your own
// session/JWT validation; the important part is that the player id comes from *your* auth,
// not from the request body.
// ---------------------------------------------------------------------------------------------

const b64u = (buf) => Buffer.from(buf).toString("base64url");

export function mintDevSession(secret, playerId, displayName, ttlSec = 3600) {
  const payload = b64u(JSON.stringify({ pid: playerId, name: displayName, exp: Math.floor(Date.now() / 1000) + ttlSec }));
  return `${payload}.${b64u(createHmac("sha256", secret).update(payload).digest())}`;
}

export function authenticatePlayer(secret, authorization) {
  const m = /^Bearer\s+([A-Za-z0-9_-]+)\.([A-Za-z0-9_-]+)$/.exec(authorization ?? "");
  if (!m) return null;
  const expected = createHmac("sha256", secret).update(m[1]).digest();
  const given = Buffer.from(m[2], "base64url");
  if (given.length !== expected.length || !timingSafeEqual(given, expected)) return null;
  let claims;
  try {
    claims = JSON.parse(Buffer.from(m[1], "base64url").toString("utf8"));
  } catch {
    return null;
  }
  if (typeof claims.pid !== "string" || typeof claims.name !== "string" || typeof claims.exp !== "number") return null;
  if (claims.exp <= Math.floor(Date.now() / 1000)) return null;
  return { playerId: claims.pid, displayName: claims.name };
}

// ---------------------------------------------------------------------------------------------
// Token issuance
// ---------------------------------------------------------------------------------------------

/** Game-side authorisation: may this player join this match's voice? (stub: every match) */
export function playerMayJoin(_player, matchId) {
  return /^[A-Za-z0-9_-]{1,64}$/.test(matchId);
}

/**
 * Calls Aurix and returns ONLY what the game client needs. The response deliberately does not
 * spread `TokenResponse` — new server fields must be opted in here.
 */
export async function issueVoiceToken(aurix, player, matchId, region) {
  const res = await aurix.issueToken({
    external_id: player.playerId,
    display_name: player.displayName,
    channels: [{ ad_hoc: { name: `match-${matchId}`, channel_type: "team" }, join: true, speak: true, receive: true }],
    ...(region ? { region } : {}),
  });
  return {
    token: res.token,
    user_id: res.user_id,
    expires_at: res.expires_at,
    endpoint: res.endpoint ? { ws_url: res.endpoint.ws_url, region: res.endpoint.region } : null,
  };
}

// ---------------------------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------------------------

function readJson(req) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    let size = 0;
    req.on("data", (c) => {
      size += c.length;
      if (size > MAX_BODY) reject(Object.assign(new Error("body too large"), { status: 413 }));
      else chunks.push(c);
    });
    req.on("end", () => {
      try {
        resolve(chunks.length ? JSON.parse(Buffer.concat(chunks).toString("utf8")) : {});
      } catch {
        reject(Object.assign(new Error("invalid JSON"), { status: 400 }));
      }
    });
    req.on("error", reject);
  });
}

function send(res, status, body) {
  const data = JSON.stringify(body);
  res.writeHead(status, { "content-type": "application/json", "cache-control": "no-store", "content-length": Buffer.byteLength(data) });
  res.end(data);
}

export function createTokenServer(cfg, aurix = new AurixClient({ baseUrl: cfg.aurixUrl, apiKey: cfg.apiKey })) {
  return createServer(async (req, res) => {
    try {
      if (req.method === "GET" && req.url === "/healthz") return send(res, 200, { ok: true });

      if (req.method === "POST" && req.url === "/dev/login" && cfg.allowDevLogin) {
        const { player_id, display_name } = await readJson(req);
        if (typeof player_id !== "string" || typeof display_name !== "string") return send(res, 400, { error: "player_id and display_name required" });
        return send(res, 200, { session: mintDevSession(cfg.sessionSecret, player_id, display_name) });
      }

      if (req.method === "POST" && req.url === "/voice/token") {
        const player = authenticatePlayer(cfg.sessionSecret, req.headers.authorization);
        if (!player) return send(res, 401, { error: "not logged in" });
        const { match_id } = await readJson(req);
        if (typeof match_id !== "string" || !playerMayJoin(player, match_id)) return send(res, 403, { error: "not allowed to join this match" });
        return send(res, 200, await issueVoiceToken(aurix, player, match_id, cfg.region));
      }

      send(res, 404, { error: "not found" });
    } catch (err) {
      if (err instanceof AurixError) {
        // Aurix' message may describe our request; log it server-side, never forward it verbatim.
        console.error(`aurix ${err.status} ${err.code} (request ${err.requestId ?? "-"})`);
        return send(res, err.status === 429 ? 503 : 502, { error: "voice service unavailable" });
      }
      if (err instanceof AurixNetworkError) {
        console.error(`aurix unreachable: ${err.message}`);
        return send(res, 503, { error: "voice service unavailable" });
      }
      if (typeof err?.status === "number") return send(res, err.status, { error: err.message });
      console.error(err);
      send(res, 500, { error: "internal error" });
    }
  });
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const cfg = loadConfig();
  createTokenServer(cfg).listen(cfg.port, () => {
    console.log(`token server on :${cfg.port} → ${cfg.aurixUrl}${cfg.allowDevLogin ? " (dev login enabled)" : ""}`);
  });
}
