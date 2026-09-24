# Aurix Rooms — voice conference demo

A small, complete product on top of one Aurix node: named voice rooms you join from a link, a
pinned **Lounge** where a bot plays music, readings, field recordings and test signals, chat with
typing indicators, speaking rings, per-participant mute, live quality (RTT / loss / jitter /
transport), RU/EN, and a layout that works on a phone. Nothing is mocked: every browser tab is a
real Aurix session, the bot is a native `aurix-client`, and the backend is a plain Node process
on `@aurix/server-sdk`.

```
browser ──HTTPS/WSS──▶ Caddy (one origin) ──▶ /            rooms backend (static frontend + /api)
                                           ──▶ /ws         Aurix node: control WebSocket + AURX media tunnel
                                           ──▶ /v1/* /health  Aurix node REST (only what the Web SDK needs)
bot (native aurix-client, UDP/QUIC) ─────────▶ Aurix node
```

| Part | Path | What it does |
|---|---|---|
| Backend | [`server/`](server) | rooms ⇄ Aurix channels `rooms/<slug>`, join → one-channel session token, bot session/status, SSE "now playing", static frontend, strict CSP. The API key never leaves this process. |
| Frontend | [`web/`](web) | Vite + React + TypeScript on `@aurix/web-sdk`: lobby with mic check, room with tiles, chat, quality, autoplay unlock, reconnect states. |
| Bot | [`bot/`](bot) | Rust binary on `aurix-client`: 48 kHz playlist (stereo music, mono speech/ambience, generated tone/sweep/stereo check/pink noise), `!np` `!next` `!list` chat commands, token refresh, resume/failover. |
| Deploy | [`deploy/`](deploy) | `run.sh up|down|status|logs` — PostgreSQL + Redis (Docker), node, backend, bot, Caddy, optional HTTP tunnel (ngrok, localhost.run, cloudflared). |

## Run it

Requirements: Docker, Rust toolchain, Node ≥ 20.19 (Vite), `ffmpeg` + `curl` for the playlist.

```sh
examples/rooms/bot/fetch-assets.sh              # public-domain / CC BY tracks → WAV + playlist.json
examples/rooms/deploy/run.sh up                 # http://127.0.0.1:8000
examples/rooms/deploy/run.sh status
```

Public address from a host without inbound connectivity (an authenticated ngrok agent and a
reserved domain, or an account-less quick tunnel with a random host that changes on every start):

```sh
NGROK_DOMAIN=example.ngrok-free.dev examples/rooms/deploy/run.sh up
QUICK_TUNNEL=localhost.run examples/rooms/deploy/run.sh up    # ssh -R; prints the host at the end
QUICK_TUNNEL=cloudflared examples/rooms/deploy/run.sh up      # cloudflared quick tunnel
```

localhost.run passes the bot's `text/event-stream` status through unbuffered; with a cloudflared
quick tunnel the same stream reached the browser only once the response ended, so the
now-playing panel stays empty behind it while voice and chat work.

HTTP tunnels carry no UDP, so behind one the frontend is configured with
`transport: 'websocket'` (AURX over the control WebSocket, see the Web SDK docs). On a host with a
public IP leave `ROOMS_TRANSPORT` at `auto` and browsers take WebTransport / WebRTC directly; the
bot uses QUIC or UDP either way.

State (generated secrets, API key, pid files, logs) lives under `~/.aurix-rooms`; nothing is
written into the repository.

## Develop

```sh
(cd sdk/server/node && npm ci && npm run build)      # the backend links @aurix/server-sdk from the tree
(cd examples/rooms/server && npm ci && npm test)
(cd sdk/web && npm ci && npm run build)              # the frontend links @aurix/web-sdk from the tree
(cd examples/rooms/web && npm ci && npm run check && npm run build)
cargo test -p aurix-rooms-bot
```

Backend environment: `AURIX_URL`, `AURIX_WS_URL`, `AURIX_API_KEY` (or `AURIX_API_KEY_FILE`),
`ROOMS_BOT_TOKEN` (shared with the bot), `ROOMS_TRANSPORT`, `ROOMS_PUBLIC_API_URL` /
`ROOMS_PUBLIC_WS_URL` (what browsers should dial when a proxy is in front), `ROOMS_STATE_FILE`,
`ROOMS_STATIC_DIR`, `ROOMS_STAGE_SLUG` / `ROOMS_STAGE_TITLE`, `ROOMS_IDLE_HOURS`.

## What to listen for

The Lounge channel is a stereo `music` profile (128 kbit/s Opus, fullband, no DTX). Music tracks
are stereo, readings and the field recording mono; the generated signals check the path end to
end — a 1 kHz reference tone at −20 dBFS, a 20 Hz → 20 kHz sweep, a left/right/centre stereo
check and pink noise. Type `!np` in the chat for the current track, `!next` to skip, `!list` for
the playlist.
