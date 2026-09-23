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
the REST API, opens one WebSocket per session, performs an authenticated `SessionBind`, joins
channels and streams sealed AURX audio from the configured speakers while every member decrypts
its own downlink — nothing bypasses production validation.

```bash
cargo build --release -p aurix-server -p aurix-loadtest
AURIX_LOADTEST_API_KEY=aurx_... ./target/release/aurix-loadtest \
  --api http://127.0.0.1:8080 --ws ws://127.0.0.1:8081 --metrics http://127.0.0.1:4040/metrics \
  --sessions 1000 --channels 100 --speakers 2 --pps 50 --duration 30 \
  [--payload 80] [--setup-concurrency 64] [--json]
```

The report contains session setup success and latency, packets sent vs. delivered
(`expected = speakers × (members − 1) × frames`), bad-auth count, one-way latency percentiles
(a timestamp is embedded in every payload), control-plane message counts, and a before/after
diff of the server's Prometheus counters (packets, CPU seconds, RSS).

Practicalities: the load-generator host needs `ulimit -n ≥ 2 × sessions`; kernel-side drops show
in `/proc/net/snmp` (`Udp: RcvbufErrors`) — raise `net.core.rmem_max` and `media.rx_workers` on the
server side. Run the generator on a separate host for numbers you intend to publish.

Reference run (release build, 8 vCPU host shared with the load generator, PostgreSQL + Redis):

| scenario | in / out pps | delivered | one-way p50 / p99 | server CPU | RSS |
|---|---|---|---|---|---|
| 1000 sessions, 100 channels × 10, 2 speakers | 10k / 90k | 100 % | 2.5 / 4.8 ms | ~0.4 core | 48 MB |
| 2000 sessions, 200 channels × 10, 4 speakers | 40k / 360k | 99.8 % | 2.1 / 4.8 ms | ~1.4 cores | 70 MB |

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
  the first quarter (after warm-up) grow by less than `--max-rss-growth` (25 %), same for the
  bot process; `aurix_active_sessions` / `aurix_active_participants` return to zero once the
  bots leave.

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
