# Threat model

This chapter states who Aurix defends against, what it guarantees them, and what it
explicitly does not. It complements the [security model](security.md), which describes the
mechanisms; this page is about the assumptions behind them. Reports that break one of the
"defended" rows below are security vulnerabilities — see [`SECURITY.md`](https://github.com/aurix-voice/aurix/blob/main/SECURITY.md).

## Assets

| Asset | Where it lives | Owner |
|---|---|---|
| Voice (Opus/PCMU frames, mixes), positions, speaking state | Node memory, AURX/QUIC/WebRTC datagrams, cascade relays, recordings | Player |
| Text chat, transcripts, translations | Node, Postgres (optional history), STT/MT providers | Player |
| Identity: user id, session id, channel membership, grants | Player JWT, node memory, Redis mirror, Postgres | Game backend |
| Tenant secrets: API keys (hash), webhook secrets, `media.cascade_secret`, JWT signing key, TURN shared secret | Postgres / config / environment | Operator |
| Recordings, evidence clips, exports | Node disk or S3-compatible storage | Operator (on behalf of the game) |
| Admin accounts, audit log, analytics | Postgres | Operator |
| E2EE identity and sender keys | Client only | Player |

## Actors and trust

| Actor | Trusted with | Not trusted with |
|---|---|---|
| **Operator** (runs the nodes, PG, Redis, TURN) | Everything a node can see: plaintext media in non-E2EE channels, recordings, metadata | Content of `e2ee: true` channels (ciphertext only, but can see who talks when and for how long) |
| **Game backend** (holds an API key) | Its own tenant: minting player tokens, channels, moderation, webhooks, exports | Other tenants (every resource is tenant-scoped, cross-tenant lookups answer `404`) |
| **Player** (holds a player JWT) | Speaking/receiving in the channels the token grants, its own preferences, chat where allowed | Other users' sessions, channels not granted, moderation without a grant, server-side mixing decisions |
| **Anonymous network peer** | Nothing | Everything: cannot open a session, cannot inject media into any session, cannot relay through TURN, cannot make the node do unbounded work |
| **Other node in the fleet** | Cascade relays authenticated with `media.cascade_secret`; registry entries in PG/Redis | Impersonating a client (it never has a session key), reading E2EE content |
| **Third-party providers** (STT, TTS, MT, moderation classifiers, webhook receivers, S3) | Only the data the operator configures to send them | Calling back into the node beyond the documented endpoints; being on private networks (SSRF guard) |

## Defended

| Threat | Defence | Verified by |
|---|---|---|
| Forged or replayed AURX/QUIC media (spoofed SSRC, replay, bit flips) | AES-256-CTR + HMAC-SHA256 with per-session keys, per-session replay window, signed `SessionBind` before any media is accepted, QUIC connection bound to the session by that bind | `protocol`/`quic` unit tests, `fuzz/aurx_packet`, live E2E |
| Session hijack from another address / connection migration abuse | Media only accepted from the bound address/connection; a fresh signed bind wins, stale connections are closed | `quic` tests: stale connection, replayed early data |
| Cross-tenant data access through the REST/WS API | Tenant from validated credential only; every query scoped by `app_id` | `e2e_live` isolation tests, `api` unit tests |
| Token theft and replay | Short-lived JWTs, one-time action tokens (`jti` claimed atomically), one-time rotated resume tokens, erasure tombstones | `auth` tests, E2E `TOKEN_REUSED` |
| Malicious or buggy client crashing a node with malformed input | Bounded, checked parsers for AURX, RTP, STUN/TURN, control JSON, E2EE frames, Opus, Ogg, WAV, live frames; corpus replayed on stable, libFuzzer with ASan on nightly | `fuzz/` (12 targets), CI `fuzz` job |
| Resource exhaustion by one tenant or IP | Per-IP and per-key rate limits (fleet-wide with Redis), per-node participant caps, per-session channel limits, bounded queues for webhooks/live streams/E2EE key relays, max packet sizes | `control` rate-limit tests, load tests |
| Open TURN relay | MESSAGE-INTEGRITY with time-limited credentials, nonce, allocation ownership, permissions | `turn` tests, `fuzz/stun_message` |
| Operator infrastructure used to attack the operator's own network (SSRF) | Outbound URLs validated, private/link-local refused, DNS pinned per delivery, redirects not followed | `net` tests |
| Webhook receivers accepting forged events | HMAC-SHA256 signature with timestamp, constant-time compare, shared test vector across all SDKs | `webhooks` tests, `fuzz/webhook_signature`, server-SDK tests |
| Downgrade of an E2EE channel to plaintext | Node drops plaintext in `e2ee: true` channels and refuses incapable sessions | `router` tests, live E2E |
| Another node in the fleet injecting media | Relay envelopes authenticated with the cascade secret, unknown source addresses dropped, hop cap | `cascade` tests, 4-node live E2E |
| Privilege escalation between admin roles | Permission matrix checked on every admin route; SSO claims mapped to roles server-side | `admin` E2E with mock IdP |
| Supply-chain: vulnerable or unlicensed dependencies, tampered release artifacts | `cargo audit` + `cargo deny` in CI, `--locked` builds, bundled libopus pinned by crate version, SBOM + SHA-256 checksums (+ Sigstore signatures when release signing is configured) | CI `audit`, release workflow |

## Not defended (by design)

* **A malicious operator in non-E2EE channels.** The node mixes, records and transcribes
  plaintext; that is the product. Use `e2ee: true` where the operator must not hear.
* **A malicious operator against E2EE identity binding.** Nodes vouch for identity public
  keys; an operator could substitute their own. Applications that need this guarantee compare
  fingerprints out of band. Traffic analysis (who speaks, when, how much) is always visible.
* **A compromised game backend.** Whoever holds the API key is the tenant: they can mint
  tokens for any user of that tenant and read its recordings. Keep the key server-side
  ([token servers](../backend/server-sdks.md)), rotate it, scope keys to the permissions they need.
* **Client-side attacks.** A cheating client can mute itself, send silence, send garbage
  audio inside a valid session or run a modified SDK. Aurix authenticates *who* sends, not
  that the audio is honest; voice moderation is a policy layer ([content safety](../features/safety.md)).
* **Denial of service beyond a node's capacity.** Rate limits and caps protect one node from
  one abuser; volumetric UDP floods are a network-layer problem (place nodes behind your
  provider's DDoS protection, restrict `media.bind` ranges).
* **Side channels in libopus, AES or the OS.** We use constant-time primitives from RustCrypto
  and the upstream libopus; we do not claim resistance to physical or micro-architectural
  attacks on shared hosts.
* **Correctness of third-party providers.** STT/MT/classifier output is what the provider
  returns; treat moderation verdicts as signals, not proofs.

## Attack surface inventory

Byte-level parsers reachable before authentication, each with a fuzz target in `fuzz/`:

| Surface | Entry point | Target |
|---|---|---|
| AURX datagram (UDP, QUIC datagram, WS tunnel) | `AurixPacket::decode`/`decode_bounded`, `open`, `relay_inner*`, `parse_session_bind` | `aurx_packet` |
| WebRTC RTP | `RtpHeader::parse` | `rtp_header` |
| STUN/TURN | `StunMessage::decode`, `verify_integrity`, `verify_fingerprint`, XOR addresses | `stun_message` |
| Control WebSocket (JSON) | `ControlMessage` deserialization round trip | `control_message` |
| E2EE key relay and frames | `SenderKey::open`, `PeerKeys::open`, `IdentityKey::unwrap`, `Group` state machine | `e2ee_frame` |
| Opus payloads (C library) | `aurix_opus::Decoder`, DRED parser, packet inspection | `opus_packet` |
| Client downlink | `RemoteMixer::push_wire_frame` + `mix` with hostile frames | `remote_mixer` |
| Recording uploads / mixdown input | `parse_ogg_opus` | `ogg_opus` |
| TTS provider responses | `parse_wav` | `wav` |
| Live-stream pull connectors | `live::decode_frame` | `live_frame` |
| Webhook signature header | `verify_signature` | `webhook_signature` |
| Short strings (TURN usernames, cursors, roles, addresses, URLs) | assorted `parse` helpers | `text_parsers` |

Authenticated surfaces (REST bodies, WS commands after `SessionInit`) are covered by the
integration and E2E suites rather than fuzzing; serde rejects unknown shapes before handlers run.

## Running the fuzzers

```bash
# Stable: replay the committed corpus + edge inputs (also runs in CI on every push).
cargo test --manifest-path fuzz/Cargo.toml --release

# Nightly + libFuzzer/ASan (Linux/macOS):
cargo install cargo-fuzz
cargo +nightly fuzz list
cargo +nightly fuzz run aurx_packet fuzz/corpus/aurx_packet -- -max_total_time=600 -max_len=4096

# Regenerate the deterministic seeds after changing a wire format:
cargo test --manifest-path fuzz/Cargo.toml --release -- --ignored seed_corpus
```

Crashes land in `fuzz/artifacts/<target>/`. Minimise with `cargo fuzz tmin`, add the reduced
input to `fuzz/corpus/<target>/` under a descriptive name (`regress_<what>`) with the fix so the
stable replay test guards the regression. Only named seeds and `regress_*` files are committed;
the SHA-1-named inputs libFuzzer accumulates in the same directory are ignored by git.
