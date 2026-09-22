# Licensing

Aurix is licensed along one boundary: **what an operator runs is AGPL-3.0-only; what a game
links or ships is Apache-2.0.** The machine-readable map is [`REUSE.toml`](REUSE.toml) (checked
by `reuse lint` in CI); the licence texts are in [`LICENSES/`](LICENSES/).

| Part | Paths | Licence |
|---|---|---|
| Server node (`aurix-server`), media, TURN, control plane, database, moderation, recording, metrics | `crates/aurix-{server,api,ws,control,media,turn,db,auth,moderation,recording,metrics}` | AGPL-3.0-only |
| Operator dashboard | `dashboard/` | AGPL-3.0-only |
| `aurix` CLI, load tester, fuzzing, chaos / netem harnesses, release tooling, CI, migrations, Dockerfiles | `crates/aurix-cli`, `crates/aurix-loadtest`, `fuzz/`, `tools/chaos`, `tools/netem`, `tools/release`, `.github/`, `migrations/` | AGPL-3.0-only |
| Native client core and C ABI | `crates/aurix-client` | Apache-2.0 |
| Protocol, configuration and audio crates shared by the client | `crates/aurix-common`, `crates/aurix-opus` | Apache-2.0 |
| Client SDKs: Web, Unity (native + WebGL), Unreal, Godot | `sdk/web`, `sdk/unity`, `sdk/unreal`, `sdk/godot` | Apache-2.0 |
| Server SDKs (Node, Python, Go, C#), token-server examples, test vectors, SDK generator | `sdk/server`, `tools/openapi-sdk` | Apache-2.0 |
| API contract | `api/openapi.json` | Apache-2.0 |
| Documentation, deployment examples (Compose, Helm, Terraform, Grafana, Caddy, Traefik), sample configs | `docs/`, `deploy/`, `configs/`, `*.md` | Apache-2.0 |

## What this means

**Shipping a game.** Your game — closed-source, commercial, on any store or console — links the
Apache-2.0 SDKs and talks to an Aurix node over the network. Nothing in the AGPL applies to the
game: the SDKs do not contain AGPL code, and using a network service does not make the client a
derivative of the server. Keep the Apache `LICENSE`/`NOTICE` with the SDK as usual.

**Running Aurix.** You may run Aurix — unmodified or modified — for your own studio, your players
or your customers, at any scale, without paying anyone. AGPL-3.0 section 13 adds one obligation:
if you **modify** the server (or dashboard) and let users interact with it over a network, you
must offer those users the corresponding source of your modified version. Running the unmodified
release, or private changes nobody else connects to, triggers nothing beyond the normal GPL terms.

**Forking.** Improvements to the server stay open: a fork must remain AGPL-3.0, and anyone who
deploys it publicly must publish their changes. Improvements to the SDKs may be kept proprietary
under Apache-2.0 — that is intentional, so engine and platform ports are never blocked.

**Combining.** The Apache-2.0 crates are compatible one way: the AGPL server may depend on them
(it does), the Apache-2.0 client never depends on an AGPL crate. `cargo deny` and `reuse lint`
keep that boundary in CI; a new crate is AGPL unless `REUSE.toml` says otherwise.

**Contributions** are accepted under the licence of the path they touch, with a
[Developer Certificate of Origin](https://developercertificate.org/) sign-off — see
[`CONTRIBUTING.md`](CONTRIBUTING.md). There is no CLA and no copyright assignment.

**Trademarks.** The licences cover the code, not the name. "Aurix" and the Aurix logo identify
this project; a fork or hosted offering must not present itself as Aurix or as endorsed by it.

## History

Releases up to and including `v1.3.0` were published entirely under Apache-2.0 and remain
available under that licence. The AGPL-3.0-only terms apply to the server-side parts from the
first commit after `v1.3.0` onwards.

This file explains the layout; it is not legal advice. The licence texts govern.
