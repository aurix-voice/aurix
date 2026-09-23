# Development and load testing

## Repository layout

| path | contents |
|---|---|
| `crates/aurix-common` | shared types, wire protocol (`protocol.rs`), AURX packet format and crypto, configuration; the `server` feature gates server-only code so clients build without it |
| `crates/aurix-auth`, `aurix-db`, `aurix-control` | credentials and tokens, PostgreSQL queries and migrations, session/channel bookkeeping, node registry, events, webhooks, retention |
| `crates/aurix-media`, `aurix-turn` | SFU, router, cascade, quality estimation, STT/TTS pipelines; embedded STUN/TURN |
| `crates/aurix-api`, `aurix-ws`, `aurix-moderation`, `aurix-recording`, `aurix-metrics` | REST handlers and router, WebSocket control plane, moderation, recordings and live streams, Prometheus |
| `crates/aurix-server`, `aurix-cli` | the binary (wiring, graceful shutdown, live E2E tests) and the operator CLI |
| `crates/aurix-client` | native client core with C ABI (used by Unreal and custom engines) |
| `crates/aurix-loadtest` | load generator |
| `sdk/web`, `sdk/unity`, `sdk/unreal` | client SDKs |
| `api/openapi.json`, `docs/` | REST contract and this book |
| `configs/`, `deploy/`, `docker-compose.yml`, `Dockerfile` | configuration and deployment |

## Toolchain

Rust stable (`rust-version = "1.88"` in `Cargo.toml`), PostgreSQL 15+, Redis 7+, and the native build
dependencies `pkg-config libssl-dev cmake` (Debian names — `libssl-dev` is only needed because the
WebRTC stack pulls OpenSSL transitively; everything else uses rustls; `cmake` builds the bundled
libopus 1.6 — no system `libopus` is used). For the SDKs:
Node 20 (`sdk/web`), .NET 8 SDK (`sdk/unity/DotNet~`), mdBook 0.5 for the book.

```bash
docker run -d --name aurix-pg -e POSTGRES_PASSWORD=aurix -e POSTGRES_USER=aurix -e POSTGRES_DB=aurix -p 5432:5432 postgres:16
docker run -d --name aurix-redis -p 6379:6379 redis:7
cargo run --bin aurix-server                     # configs/default.toml, migrations applied on start
```

## Checks that CI runs

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked                  # unit + in-process integration tests
cargo deny check                                 # licences, advisories, duplicate/banned crates

# live end-to-end against a running server (bootstrap steps: .github/workflows/ci.yml)
AURIX_E2E_API_KEY=aurx_... cargo test -p aurix-server --test e2e_live -- --ignored --nocapture
cargo test -p aurix-client --test e2e_live -- --nocapture
# lossy WAN + network migration (Linux, needs `sudo tc`; shapes the node's media port with netem)
AURIX_E2E_SUDO_TC=1 cargo test -p aurix-client --test netem_live -- --nocapture

# SDKs
(cd sdk/web && npm ci && npm run check && npm test && npm run build)
(cd sdk/unity/DotNet~ && dotnet test Aurix.sln -v q --nologo)

# contract + docs
cargo test -p aurix-api --test openapi_contract
npx @redocly/cli@1 lint api/openapi.json --skip-rule no-unused-components
mdbook build docs && test -f docs/book/api/openapi.json
```

The live E2E suite (`crates/aurix-server/tests/e2e_live.rs`) runs two nodes in CI and covers the
full player path over WebSocket + UDP — signed `SessionBind`, sealed audio, forged-packet
rejection, resume, action tokens (including a strict `require_action_tokens` run), receiver
preferences, transmission modes, positional/directional audio, echo channels, chat, webhooks and
SSE, ad-hoc channels, mute-all/kick-all, erasure/export, transcripts and TTS with the mock
provider (`cargo run -p aurix-server --example mock_speech`), live streams, quality alerts and
the stats endpoint, plus tenant isolation for each of them. The tests run in parallel: each one
that watches app-wide state — the SSE stream, webhooks, moderation and safety listings, quality
alerts, app quotas — creates its own application with `AURIX_E2E_ADMIN_TOKEN` (`isolated_env`),
so events of a neighbouring test never reach its observers; without the admin token those tests
fall back to the shared key and are best run with `--test-threads=1`. `AURIX_E2E_API_KEY2` is a
second, unrelated application used for the negative tenant checks.

The `native core` job builds `aurix-client` on Windows x64 (MSVC), macOS arm64 and macOS x64,
runs its unit tests and the C/C++ samples there and uploads the staged `lib/Win64` / `lib/Mac`
libraries as artifacts; Linux is covered by the workspace jobs.

### Lossy WAN and network migration

`crates/aurix-client/tests/netem_live.rs` puts Linux netem on the node's media port through
`tools/netem/shape.sh` (one `tc prio` qdisc with u32 filters on the UDP port — the WebSocket
control plane and everything else on the interface stay clean) and measures what two native
clients on that node go through, direction by direction:

* 20 % loss on the **downlink** only: the node reports the receiver's loss back to the talker
  as `receivers_loss_percent`, the talker moves to the `High` loss profile within a few seconds,
  and from then on at least half of what the listener loses is rebuilt from FEC/DRED rather than
  concealed; the listener's MOS drops below 3.5 and recovers to ≥ 4.0 once the link is cleared,
  the talker leaves `High` after the dwell.
* 12 % loss + 20 ± 10 ms delay with reordering on the **uplink** only: the node's own
  measurement (`uplink_loss_percent`) drives the same profile, reordered packets are put back.
* QUIC under 2 × (40 ± 15) ms and 3 % loss each way: the heartbeat RTT shows the added delay,
  `network_changed()` migrates the connection to a new local address without a re-bind or a
  new session, and audio keeps flowing.

```sh
sudo tools/netem/shape.sh apply 10000 --down "loss 20%" --up "delay 20ms 10ms reorder 25% 50%"
sudo tools/netem/shape.sh show
sudo tools/netem/shape.sh clear      # host-global; always clear
```

The shaper is the same one you can use by hand against a local node to listen to a lossy link
with a real SDK. It is a *simulation*: loopback with netem has no NAT, no radio and no real path
change, so the Wi-Fi ↔ cellular case is covered for the protocol logic (migration, profile,
repair), not for real networks ([Limitations](../limitations.md)).

## Drift guards

Several tests exist only to keep artefacts honest; when they fail, regenerate rather than
silence them:

* `openapi_contract` — every router route ↔ `api/openapi.json` (method, permissions, version).
* `committed_header_is_up_to_date` (`crates/aurix-client/tests/c_abi.rs`) —
  `include/aurix_client.h` matches the exported ABI (`cargo run -p aurix-client --example gen_header`).
* `unreal_plugin_uses_only_existing_abi` — the Unreal plugin references only symbols in the
  header.
* AURX wire vectors — `v2_wire_vectors_are_stable` in `protocol.rs` and
  `WireVectorsMatchServer` in `Aurix.Voice.Tests` pin the packet layout and key derivation with
  the same fixtures; the quality model (R-factor → bars) is pinned the same way in Rust, C# and
  TypeScript.
* `sdk/unity/DotNet~/Aurix.Voice.UnityCheck` — compiles the Unity-only code (`AurixVoiceBehaviour`,
  the samples) against a UnityEngine stub with the Unity/Android defines, because the Editor is
  not available in CI.

## Load testing

`aurix-loadtest` behaves like real native clients: it creates channels and player tokens through
the REST API, opens one WebSocket per session, performs an authenticated `SessionBind` over the
chosen media path, joins channels and streams sealed AURX audio from the configured speakers
while every member opens its own downlink — nothing bypasses production validation. UDP, QUIC and
the TLS tunnel go through the native client core (`aurix-client`, the same code every native SDK
ships), WebTransport through a WebTransport client the way a browser connects, and `tunnel` sends
AURX as binary frames on the control WebSocket.

```bash
cargo build --release -p aurix-server -p aurix-loadtest
AURIX_LOADTEST_API_KEY=aurx_... ./target/release/aurix-loadtest \
  --api http://127.0.0.1:8080 --ws ws://127.0.0.1:8081 --metrics http://127.0.0.1:4040/metrics \
  --transport udp|quic|tls|webtransport|tunnel \
  --sessions 1000 --channels 100 --speakers 2 --pps 50 --duration 60 \
  [--opus [--opus-bitrate 32000]] [--mix] [--noise-suppression] [--listen-only] \
  [--payload 80] [--setup-concurrency 64] [--warmup-ms 1000] [--json]
```

* `--opus` replaces the synthetic payload with a bank of real Opus frames (speech-like signal,
  encoded once at `--opus-bitrate`); `--mix` and `--noise-suppression` imply it because the node
  decodes those frames.
* `--mix` makes every session ask for the server-mixed downlink (`SetDownlinkMode`), and
  `--noise-suppression` makes every speaker ask for uplink denoising (`SetNoiseSuppression`);
  the report counts the acknowledgements and refusals (`NOISE_SUPPRESSION_UNAVAILABLE`).
* `--listen-only` gives the non-speaking members a listen-only grant. It changes nothing for the
  per-stream path, but a server mix then serves them from one shared mixer per channel (an
  audience) instead of one private mixer per receiver (a team where everyone may speak).
* Repeating `--ws` spreads sessions over several nodes so that every channel has members on
  every node — the nodes cascade. Pass one `--metrics` per node; `--warmup-ms` gives the cascade
  time to discover the channel before the speakers start.

The JSON report (`--json`, logs go to stderr) has a reproducible `run_id`, the configuration,
setup counts and timings, packets sent vs. delivered (`expected = speakers × (members − 1) ×
frames` per stream; with `--mix`, one mixed frame per hearing member per frame), one-way latency
percentiles (a timestamp is embedded in every synthetic payload — a mixed frame is a new encode, so
mixed runs report no latency), replay / bad-auth counts, the generator's own CPU and RSS, and per
node a before/after diff of the Prometheus counters (`aurix_packets_*`, transport, mixer, noise
suppression and cascade counters, gauges after the run, process CPU seconds, RSS, fds).

Practicalities: the load-generator host needs `ulimit -n ≥ 2 × sessions`; kernel-side drops show
in `/proc/net/snmp` (`Udp: RcvbufErrors`) — raise `net.core.rmem_max` / `wmem_max` to at least
the node's 4 MiB media socket buffers and `media.rx_workers` on the server side. Setting up 1000
sessions opens ~1000 short-lived DB transactions: size `database.max_connections` and
PostgreSQL's `max_connections` for that or lower `--setup-concurrency`. Run the generator on a
separate host for numbers you intend to publish.

### Reference runs (1.6.0)

All runs below: release build of `main` after v1.5.0 (Rust 1.98, libopus 1.6 bundled), one 8 vCPU
host (Intel Xeon Platinum 8559C) shared by the node(s), PostgreSQL 16 and Redis 7 in containers
and the load generator; `net.core.rmem_max = wmem_max = 16 MiB`, `ulimit -n 65536`,
`database.max_connections = 40` per node, otherwise `configs/default.toml` (`rx_workers = 0`, auto from the CPU count,
`mixer_decoder_complexity = 5`, noise suppression `level = "high"`, `max_sessions = 1024`).
Every run: **1000 sessions, 100 channels × 10 members, 2 speakers per channel, 50 pps per
speaker (20 ms frames), 60 s of streaming** — 10 000 packets/s uplink, 90 000 packets/s downlink
on the per-stream path. Server CPU is the node's `process_cpu_seconds_total` delta divided by the
streaming time; RSS is `process_resident_memory_bytes` after the run. Payload is an 80-byte
synthetic frame unless a row says Opus (real 32 kbit/s Opus frames, ~78 bytes). Every row is a
single completed run of the command shown (common flags: `--sessions 1000 --channels 100
--speakers 2 --duration 60 --setup-concurrency 64`), setup 1000/1000 in every one.

**Media paths, one node** (`--transport …`):

| transport | delivered | one-way p50 / p99 / max | server CPU | RSS |
|---|---|---|---|---|
| `udp` | 5 400 000 / 5 400 000 (100 %) | 1.55 / 2.85 / 5.1 ms | 0.43 core | 159 MiB |
| `quic` | 100 % | 2.05 / 3.85 / 23.7 ms | 0.80 core | 165 MiB |
| `tls` (TLS tunnel) | 100 % | 1.75 / 3.25 / 8.7 ms | 0.60 core | 165 MiB |
| `webtransport` | 100 % | 2.85 / 5.35 / 14.0 ms | 0.85 core | 171 MiB |
| `tunnel` (WebSocket) | 100 % | 2.05 / 20.65 / 48.8 ms | 0.53 core | 171 MiB |

QUIC setup took 3.6 s for the 1000 handshakes (max 3.06 s for one session) against ~0.7 s for the
others; the encrypted transports cost roughly 1.4–2× the UDP CPU for the same traffic. The
WebSocket tunnel's p99 is the head-of-line blocking of one TCP stream per session under 90k
frames/s on a shared host, not a steady offset.

**Processing, one node** (`--transport udp`, Opus payload):

| scenario | delivered | one-way p50 / p99 / max | server CPU | RSS |
|---|---|---|---|---|
| `--noise-suppression` (200 denoised uplinks, 600 000 frames `outcome="ok"`) | 100 % | 6.95 / 10.95 / 14.8 ms | 3.50 cores | 235 MiB |
| `--mix --listen-only` (100 shared mixers for 800 listeners + a private one per speaker; 1000 mixed downlinks at 50 pps) | 2 999 484 / 3 000 000 (99.98 %) | — | 4.97 cores | 230 MiB |
| `--mix --noise-suppression --listen-only` | 2 807 640 / 3 000 000 (93.6 %) | — | 7.73 cores | 307 MiB |

Noise suppression is ~15 ms of CPU per second of audio per uplink (RNNoise + Opus decode and
re-encode) and adds ~5 ms of one-way latency. A server mix costs one Opus decoder per speaker
per mixer plus one encoder per mixed downlink; the audience shape keeps that at
`channels + speakers` mixers, whereas the same 1000 sessions with everyone allowed to speak need
~1000 private mixers and did not fit this host (34 % delivered at 7.75 cores — the reason for
`--listen-only` and for keeping `speak: false` in the grants of members who only listen). The
combined row saturated the 8 vCPUs (7.73 cores) and is listed as the observed behaviour at the
limit, not as a supported configuration; size a node so that the sum stays well below the core
count, and watch `aurix_downlink_mix_frames_total{outcome!="sent"}` and
`aurix_noise_suppression_frames_total{outcome="skipped"}`.

**Cascade, two nodes on one host** (`--ws ws://node1 --ws ws://node2 --metrics … --metrics …
--warmup-ms 3000`; each channel has 5 members on each node, so every uplink frame crosses the
inter-node link once):

| scenario | delivered | one-way p50 / p99 / max | CPU per node | RSS node 1 / node 2 |
|---|---|---|---|---|
| cascade over UDP (`aurix_cascade_links{transport="udp"} = 1`) | 5 400 000 / 5 400 000 (100 %) | 1.05 / 2.05 / 4.1 ms | 0.25 core | 274 / 69 MiB |
| cascade over the TCP fallback (`cascade_tcp_fallback = true`, inter-node UDP dropped by the firewall, `aurix_cascade_links{transport="tcp"} = 1`) | 100 % | 1.25 / 2.45 / 4.6 ms | 0.25 core | 71 / 69 MiB |

Each node received 300 000 uplink frames and sent 2 700 000 to its own members; every uplink
frame was forwarded once (5 000 frames/s in each direction over the inter-node link,
`aurix_cascade_forwarded_total{role="origin"} = 300 000` per node). The
TCP fallback adds ~0.2 ms on loopback; on a WAN it adds the head-of-line blocking of one TCP
connection per peer under loss, which is why it is a fallback. The first TCP run of this series
dropped 168 of 5.4 M frames as `Replayed cascade packet`: the envelope counter is taken by
whichever receive worker seals the frame, so frames reach the wire slightly out of counter order
and the 64-packet anti-replay window rejected the stragglers — cascade links now use a 4096-packet
window, and the row above is the rerun after that fix.

## Soak testing

`tools/soak/run.sh` keeps a two-node fleet under real clients for hours while faults are
injected in rotation. It reuses the chaos topology (`tools/chaos/`: PostgreSQL and Redis
Sentinel or Cluster in host-network containers, two `aurix-server` processes on loopback) and
drives it with `aurix-soak`, a bot runner built on the native client core (`aurix-client`):
every bot performs the full `SessionBind`, joins a channel, the speakers stream Opus over the
chosen media path and the listeners pull the mixed output — reconnect, resume, cross-node
failover, path fallback, token refresh and the loss profile are the production code paths.

```bash
cargo build --release --bin aurix-server --bin aurix-soak
AURIX_CHAOS_BIN=target/release/aurix-server AURIX_SOAK_BIN=target/release/aurix-soak \
AURIX_SOAK_DURATION=24h AURIX_SOAK_CLIENTS=32 AURIX_SOAK_CHANNELS=8 tools/soak/run.sh

AURIX_SOAK_DURATION=30m AURIX_SOAK_HOOKS=node-kill-2,udp-blackhole-1 tools/soak/run.sh   # shorter, chosen faults
tools/soak/run.sh soak    # bots only, against a fleet started earlier with AURIX_SOAK_KEEP=1
tools/soak/run.sh down    # stop nodes and containers
```

Every `AURIX_SOAK_CHAOS_EVERY` (10 min) one hook from the rotation runs — `node-kill-2`
(SIGKILL of node 2, the clients on it resume on node 1, the node comes back after the reaper
window), `redis-failover` (Sentinel `failover` or a shard master kill), `node-kill-1`,
`postgres-restart` (`docker stop`/`start`), `udp-blackhole-1` (`iptables` drops UDP to node 1's
media port for `AURIX_SOAK_BLACKHOLE_SECS`, so its clients fall back to the TLS tunnel and
return to UDP/QUIC when the rule is removed), `netem-2` (`tools/netem/shape.sh` puts
`AURIX_SOAK_NETEM` — by default 8 % loss, 40 ± 15 ms delay, 25 % reordering, both directions —
on node 2's media port for the same length, so the loss profile, FEC/DRED and the jitter buffer
have to carry the audio without a path change). The two network hooks need passwordless `sudo`
for `iptables`/`tc` and are skipped without it. Nodes are started with a 30 s resume grace and
short player tokens (`AURIX_SOAK_TOKEN_TTL`, 15 min) so the bots refresh them several times
per hour.

What is asserted, and what fails the run (exit code 1, the reason in `violations`):

* **steady state** (intervals at least `--chaos-settle` after the last hook and after the
  warm-up): every bot `MediaBound` and joined, every listener hears its speakers in at least
  `--min-hearing` (97 %) of the 200 ms windows, at most `--max-loss` (2 %) on any downlink, no
  hard reconnects (a resume that had to become a fresh session), no `bad_auth`/replay counts,
  both `/metrics` endpoints answering;
* **each hook**: the command succeeds and every bot is healthy again within `--recover-within`
  (120 s) after it ends; per-bot `Recovering → Recovered` times are recorded;
* **resources**: median RSS and open-fd count of each node over the last quarter of the run vs.
  the first quarter grow by less than `--max-rss-growth` (25 %), same for the bot process. The
  baseline starts after the first hook has ended: the first failover allocates state that then
  stays resident (TLS/QUIC client, failover endpoint, the second node's tables) — a one-off
  step, not a leak — so a run needs at least 8 steady intervals after that point to be judged;
  `aurix_active_sessions` / `aurix_active_participants` return to zero once the bots leave.

`target/soak/report.jsonl` has one row per report interval (`kind: "interval"`: fleet
aggregates, per-node samples from Prometheus — RSS, fds, CPU seconds, sessions, cascade link
transports — and one `bots[]` entry per client with state, media path, hearing ratio, loss,
RTT, jitter, MOS, recovery counters) and one per hook (`kind: "chaos"`: exit status, output
tail, recovery time, bots still unhealthy). `summary.json` is the verdict. The `Soak`
workflow (`.github/workflows/soak.yml`) runs a 90 min rotation nightly on a hosted runner and
can be dispatched with a longer duration; 24–72 h runs belong on a dedicated machine, where
the time budget of hosted runners (6 h) does not apply. Like the chaos harness it is one host
and one loopback: real partitions, cross-region RTT and NAT are still out of scope
([Limitations](../limitations.md)).

## Building the book

```bash
cargo install mdbook --version 0.5.4     # or a release binary
mdbook serve docs                        # live reload on http://localhost:3000
mdbook build docs                        # static site in docs/book
```

`docs/src/api/openapi.json` is a symlink to `api/openapi.json`, so the browsable reference always
shows the committed specification; `create-missing = false` in `docs/book.toml` makes a broken
`SUMMARY.md` link fail the build instead of creating an empty page.
