# The `aurix` CLI

`aurix` (crate `crates/aurix-cli`) is the command-line client for the control plane — for
operators (fleet, apps, admins, audit), for backend engineers (tokens, channels, users,
moderation, webhooks, analytics) and for troubleshooting (health, readiness, contract and
credential diagnostics). It embeds the same `api/openapi.json` the server ships, so **every**
operation is reachable even before it gets a curated command.

```sh
cargo install --path crates/aurix-cli      # or: cargo build -p aurix-cli → target/release/aurix
aurix --help
```

## Profiles and credentials

The CLI never stores a secret in its config file — it stores **where** the secret is.

```sh
aurix config init --name prod --server https://voice.example.com \
    --api-key-file ~/.config/aurix/prod.api-key --set-default
aurix config show            # effective server, timeout and the *source* of each credential
aurix config path            # $XDG_CONFIG_HOME/aurix/config.toml (or $AURIX_CONFIG)
```

Resolution order (first match wins):

| | 1 | 2 | 3 | 4 |
|---|---|---|---|---|
| server | `--server` | `AURIX_SERVER` | profile `server` | `http://localhost:8080` |
| API key | `--api-key-file` | `AURIX_API_KEY` | profile `api_key_file` | profile `api_key_env` |
| operator token | `--admin-token-file` | `AURIX_ADMIN_TOKEN` | profile `admin_token_file` | token saved by `aurix admin login --save` |
| profile | `--profile` | `AURIX_PROFILE` | `default_profile` in the config | `default` |

`--api-key` exists for scripts that cannot avoid it and prints a warning: a key on the command
line is visible to every process on the host. Files the CLI writes (saved operator tokens,
`--secret-out` webhook secrets) are created with mode `0600`; files it reads with wider
permissions produce a warning, and empty secret files are rejected. Credentials never appear in
output, error messages or `config show`.

Which credential a command sends is derived from the operation's security requirement in the
contract: API-key operations send `X-API-Key`, operator operations send the admin JWT,
`POST /admin/setup` sends the bootstrap token. `aurix api --auth` overrides that for one call.

## Everyday commands

```sh
# health / readiness / versions (no credentials)
aurix health; aurix ready; aurix version

# player token from your backend (the API key stays where the CLI runs)
aurix token issue --external-id acc_42 --display-name Alice \
    --channel 0193…:jsr --ad-hoc raid-7:team:40@jsrm --region eu_west --field token
aurix token action --action kick --user-id … --channel-id … --target-user-id …

# channels, users, moderation
aurix channel create --name lobby --type team
aurix channel participants <channel_id>
aurix user search alice; aurix user session <session_id>        # live MOS/RTT/loss/jitter
aurix moderate mute --channel-id … --user-id …; aurix moderate mute-all --channel-id … --off
aurix moderate ban --user-id … --reason "…" --duration-hours 24
aurix moderate priority --channel-id … --user-id …             # channel needs `ducking`

# webhooks: create (secret returned once → file), test, verify a signature offline
aurix webhook create --url https://game.example.com/aurix --event participant.joined,user.banned --secret-out ~/.config/aurix/wh.secret
aurix webhook test <webhook_id>
aurix webhook verify --secret-file ~/.config/aurix/wh.secret --signature "$SIG" --body-file body.json

# live events / analytics / recordings
aurix events tail --types participant.joined,quality.alert --count 20
aurix analytics summary --from 2026-09-01T00:00:00Z --to 2026-09-02T00:00:00Z --step 1h
aurix analytics sessions                             # worst sessions by MOS
aurix recording list; aurix recording download <id> --out call.ogg

# operators
aurix admin setup --email ops@example.com --display-name Ops --bootstrap-token-file ./bootstrap   # password from stdin
aurix admin login --email ops@example.com --save     # JWT saved for the profile (0600)
aurix app create --name MyGame --key-out ~/.config/aurix/mygame.api-key   # first API key, written once (0600)
aurix node list                                      # fleet with load and regions (operator token)
aurix node drain <node-id> --reason "kernel update"  # stop fresh admission; players already there stay
aurix node links                                     # measured cascade links: transport udp/tcp, RTT, age
aurix node undrain <node-id>
aurix node config                                    # effective configuration of the node you talk to, secrets masked
```

Every command prints JSON: `-o pretty` (default, indented) or `-o json` (one compact document
per line, errors as JSON on stderr too); `--field endpoint.ws_url` extracts one value for shell
scripts. Exit codes: `0` success, `3` authentication/authorisation, `4` not found, `5`
rate-limited (with `retry after Ns`), `6` network/TLS error, `1` anything else.

## Any operation: `aurix api`

```sh
aurix api --list                       # every operation of the embedded contract
aurix api --list analytics             # filtered by substring
aurix api --describe issueToken        # parameters, security, request schema

aurix api listChannels -q per_page=50 -q active_only=true
aurix api getChannel -p channel_id=0193…
aurix api /v1/channels/{channel_id}/config -X PUT -p channel_id=… -d @config.json
aurix api issueToken -d '{"external_id":"acc_42","display_name":"Alice"}'
cat body.json | aurix api createWebhook -d -
```

Operations are addressed by `operationId` (case-insensitive) or by path template (with `-X`
when the path has several methods). Path parameters are percent-encoded; unknown path
parameters, undeclared query parameters (unless `--allow-unknown-query`) and a body on a
bodiless operation are rejected before any request is sent. `--raw` prints non-JSON bodies
(CSV exports, recordings) verbatim.

Because the contract is embedded, `aurix api` on a node running a newer or older version can
disagree with it — `aurix diagnose` (below) reports exactly which operations differ.

## Diagnostics

```sh
aurix diagnose            # connectivity, /health, /ready, clock skew, contract match, credentials
aurix diagnose --no-auth  # without the credential probes
```

The report is one JSON document: per-check latency and result, the CLI vs node contract
version with `operations_missing_on_node` / `operations_unknown_to_cli`, the source each
credential resolved from (never the value), and `hints` — a rejected API key or expired
operator token, a node that is up but not ready, a contract mismatch, a clock skew large
enough to break token and webhook-signature tolerances.

## Preflight on the node host: `aurix doctor`

`diagnose` talks to a node over its API; `doctor` runs *on the node's host* against the node's
configuration and needs no profile or credentials. Run it before the first start, after every
configuration change and when a node refuses to come up:

```sh
cd /opt/aurix                       # the node's working directory (configs/default.toml is here)
aurix doctor                        # configs/default + AURIX__* env, like the node itself
aurix doctor -c /etc/aurix/node.toml
aurix doctor --skip-remote          # no Redis / PostgreSQL connections (air-gapped preflight)
aurix doctor --skip-probes          # no QUIC / TLS tunnel / WebTransport handshakes
aurix doctor --strict               # warnings fail too (CI, production promotion)
aurix -o json doctor --field checks # machine-readable report
```

What it checks, in order:

* **config** — the file loads, `AURIX__*` overrides apply and the node's own validation passes;
  **production** previews what `environment = "production"` would additionally reject
  (placeholder secrets, `*` CORS, …) while you are still in development; **endpoints** —
  `external_url` / `external_ws_url` / `media.external_ip` are set to something other machines
  can reach.
* **node** — `GET /health` on `server.api_port`. With no node the run is a *preflight*: every
  configured TCP/UDP listener (API, WS, metrics, media, cascade UDP/TCP, TLS tunnel,
  WebTransport, TURN) must be **free**, and a port that is already taken names the offender
  class (another process vs. a privileged port vs. a host that is not local). With a node
  running the same ports must all be **in use** — a free one means that listener failed to
  start, and the node log says why.
* **cert.\*** — the configured PEM pairs for the API/WS (`server.tls_*`), the media
  certificate shared by QUIC and the TLS tunnel (`media.quic_*`) and the optional WebTransport
  certificate: parseable chain, matching private key, key file permissions, validity window
  (warning inside 14 days of expiry), `quic_server_name` among the SAN/CN, and for
  WebTransport the ≤ 14-day validity browsers accept for `serverCertificateHashes`. The SHA-256
  fingerprint clients pin is in the details.
* **probe.\*** — with a node running: a real QUIC handshake on the media port, a TLS handshake
  on the tunnel port and an HTTP/3 handshake on the WebTransport port; the certificate each
  listener *serves* is compared with the configured file, so a node restarted with an old
  certificate (or a foreign process on the port) is caught. This proves the transport is
  reachable from this host, not that media flows — that needs a session.
* **redis** — `RedisSource` is opened exactly as the node does (direct, Sentinel with master
  discovery, or Cluster with `CLUSTER INFO`), then `PING`; the report shows the mode that
  actually came up and, for Cluster, whether sharded Pub/Sub is available.
* **postgres** / **migrations** — a plain connection (`SELECT version()`), then the
  `_sqlx_migrations` table is compared with the migrations embedded in this binary: applied,
  pending, checksum drift, rows left `success = false` by an interrupted run, and versions
  from a *newer* build (the database was already migrated by a later node). **Doctor never
  runs a migration**; with `database.run_migrations = true` a pending set is a warning ("the
  node will apply them"), otherwise a failure.

Exit code `0` when nothing failed (`2` with `--strict` if anything warned), `2` on any
failure. Secrets never appear in the output: URL passwords are replaced with `***`, and the
values of every secret configuration key are scrubbed from summaries, hints and error
messages before printing — including inside connection errors quoted from a driver.

Doctor reads the configuration the same way the node does, so run it from the node's working
directory (where `configs/default.toml` lives) with the same environment. In the Docker image
that is `docker exec <node> aurix doctor`.

## Limitations

* The CLI is a REST client. It does not join channels or send audio; use a client SDK or the
  live E2E tests for that.
* `events tail` follows SSE with reconnect but is a debugging tool, not a consumer with durable
  state — use webhooks or a server SDK for that.
* Curated commands cover the common paths; the long tail (live audio streams, safety evidence
  export, retention sweeps, …) is `aurix api`.
* No shell completion scripts are shipped; `clap` can generate them if you need
  (`clap_complete`), but this is not wired.
