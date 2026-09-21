import assert from "node:assert/strict";
import { createServer } from "node:http";
import { after, before, test } from "node:test";

import { authenticatePlayer, createTokenServer, loadConfig, mintDevSession } from "../token_server.mjs";

const API_KEY = "aurx_test_SECRET_KEY_never_in_client_payload";
const SECRET = "0123456789abcdef0123456789abcdef";

let fakeAurix, tokenServer, aurixSeen, tokenUrl;

before(async () => {
  aurixSeen = [];
  fakeAurix = createServer((req, res) => {
    let body = "";
    req.on("data", (c) => (body += c));
    req.on("end", () => {
      aurixSeen.push({ path: req.url, apiKey: req.headers["x-api-key"], body: JSON.parse(body || "null") });
      if (req.url !== "/v1/tokens") {
        res.writeHead(404);
        return res.end();
      }
      if (req.headers["x-api-key"] !== API_KEY) {
        res.writeHead(401, { "content-type": "application/json" });
        return res.end(JSON.stringify({ error: { code: "AUTH_FAILED", message: "bad key" } }));
      }
      res.writeHead(200, { "content-type": "application/json" });
      res.end(
        JSON.stringify({
          token: "player.jwt",
          user_id: "u-1",
          expires_at: "2030-01-01T00:00:00Z",
          channels: [{ channel_id: "c-1", channel_type: "team", join: true, speak: true, receive: true, moderate: false, priority: false }],
          endpoint: { region: "eu_west", node_id: "n1", ws_url: "wss://eu1.example/ws", nodes: 1, load_factor: 0.1 },
          api_key_echo: API_KEY,
        }),
      );
    });
  });
  await new Promise((r) => fakeAurix.listen(0, "127.0.0.1", r));
  const cfg = loadConfig({
    AURIX_URL: `http://127.0.0.1:${fakeAurix.address().port}`,
    AURIX_API_KEY: API_KEY,
    GAME_SESSION_SECRET: SECRET,
    AURIX_REGION: "eu_west",
    ALLOW_DEV_LOGIN: "1",
  });
  tokenServer = createTokenServer(cfg);
  await new Promise((r) => tokenServer.listen(0, "127.0.0.1", r));
  tokenUrl = `http://127.0.0.1:${tokenServer.address().port}`;
});

after(() => {
  tokenServer.close();
  fakeAurix.close();
});

const post = (path, body, headers = {}) =>
  fetch(tokenUrl + path, { method: "POST", headers: { "content-type": "application/json", ...headers }, body: JSON.stringify(body) });

test("config refuses to start without an API key or with a weak session secret", () => {
  assert.throws(() => loadConfig({ GAME_SESSION_SECRET: SECRET }), /AURIX_API_KEY/);
  assert.throws(() => loadConfig({ AURIX_API_KEY: API_KEY, GAME_SESSION_SECRET: "short" }), /GAME_SESSION_SECRET/);
  assert.throws(() => loadConfig({ AURIX_API_KEY: API_KEY, GAME_SESSION_SECRET: SECRET, AURIX_REGION: "eu" }), /AURIX_REGION/);
});

test("game session: forged or expired sessions are rejected", () => {
  const good = mintDevSession(SECRET, "p1", "Alice");
  assert.deepEqual(authenticatePlayer(SECRET, `Bearer ${good}`), { playerId: "p1", displayName: "Alice" });
  assert.equal(authenticatePlayer("x".repeat(32), `Bearer ${good}`), null);
  const [payload] = good.split(".");
  assert.equal(authenticatePlayer(SECRET, `Bearer ${payload}.AAAA`), null);
  assert.equal(authenticatePlayer(SECRET, `Bearer ${mintDevSession(SECRET, "p1", "Alice", -1)}`), null);
  assert.equal(authenticatePlayer(SECRET, undefined), null);
});

test("token endpoint requires a game session and never lets the client pick its id", async () => {
  const r = await post("/voice/token", { match_id: "m1", external_id: "admin" });
  assert.equal(r.status, 401);
  assert.equal(aurixSeen.length, 0, "no Aurix call without a session");
});

test("happy path: backend sends the API key, client gets only token + endpoint", async () => {
  const login = await post("/dev/login", { player_id: "p1", display_name: "Alice" });
  assert.equal(login.status, 200);
  const { session } = await login.json();

  const r = await post("/voice/token", { match_id: "m1", external_id: "spoof", channels: ["*"] }, { authorization: `Bearer ${session}` });
  assert.equal(r.status, 200);
  const body = await r.json();
  assert.deepEqual(body, {
    token: "player.jwt",
    user_id: "u-1",
    expires_at: "2030-01-01T00:00:00Z",
    endpoint: { ws_url: "wss://eu1.example/ws", region: "eu_west" },
  });
  assert.ok(!JSON.stringify(body).includes(API_KEY), "API key must never reach the client");

  const seen = aurixSeen.at(-1);
  assert.equal(seen.path, "/v1/tokens");
  assert.equal(seen.apiKey, API_KEY);
  assert.equal(seen.body.external_id, "p1", "player id comes from the session, not the body");
  assert.equal(seen.body.display_name, "Alice");
  assert.equal(seen.body.region, "eu_west");
  assert.deepEqual(seen.body.channels, [{ ad_hoc: { name: "match-m1", channel_type: "team" }, join: true, speak: true, receive: true }]);
});

test("invalid match id is refused before calling Aurix", async () => {
  const n = aurixSeen.length;
  const session = mintDevSession(SECRET, "p1", "Alice");
  const r = await post("/voice/token", { match_id: "../etc" }, { authorization: `Bearer ${session}` });
  assert.equal(r.status, 403);
  assert.equal(aurixSeen.length, n);
});

test("Aurix errors are logged server-side and mapped to a generic 502", async () => {
  const cfg = loadConfig({
    AURIX_URL: `http://127.0.0.1:${fakeAurix.address().port}`,
    AURIX_API_KEY: "aurx_wrong_key",
    GAME_SESSION_SECRET: SECRET,
  });
  const srv = createTokenServer(cfg);
  await new Promise((r) => srv.listen(0, "127.0.0.1", r));
  const logged = [];
  const orig = console.error;
  console.error = (m) => logged.push(String(m));
  try {
    const r = await fetch(`http://127.0.0.1:${srv.address().port}/voice/token`, {
      method: "POST",
      headers: { "content-type": "application/json", authorization: `Bearer ${mintDevSession(SECRET, "p1", "Alice")}` },
      body: JSON.stringify({ match_id: "m1" }),
    });
    assert.equal(r.status, 502);
    assert.deepEqual(await r.json(), { error: "voice service unavailable" });
    assert.match(logged.join("\n"), /aurix 401 AUTH_FAILED/);
    assert.ok(!logged.join("\n").includes("aurx_wrong_key"));
  } finally {
    console.error = orig;
    srv.close();
  }
});
