# Migrating from a hosted voice provider

These guides map the concepts and APIs of the three providers game teams most often come from
to their Aurix equivalents, list what you gain and what you give up, and give a migration order
that keeps players talking throughout. They are written for engineers who know their current
provider well; nothing here requires reading the rest of the book first.

| Coming from | Guide |
| --- | --- |
| Unity Vivox (Unity Gaming Services) / Vivox Core SDK | [Vivox → Aurix](vivox.md) |
| Agora Voice SDK (RTC) | [Agora → Aurix](agora.md) |
| Photon Voice (PUN 2 / Fusion / Quantum) | [Photon Voice → Aurix](photon-voice.md) |

> Vendor API names below are taken from the vendors' public documentation at the time of
> writing and are used only as landmarks; check your provider's current reference for exact
> signatures. Aurix names were checked against this repository's code and OpenAPI contract when
> the guides were written.

## What is the same everywhere

Every guide follows the same three moves, because the three providers share the same shape:

1. **Your backend issues credentials, the client only holds a short-lived token.** Vivox Access
   Tokens, Agora RTC tokens and Photon custom authentication all put a secret on your server
   and a token on the device. Aurix is the same: an **API key** (`aurx_…`) stays on your
   backend and mints **player JWTs** (`POST /v1/tokens`) that name the channels a player may
   join and what they may do there. [Server SDKs and token servers](../backend/server-sdks.md)
   ship a ready token endpoint in Node, Python, Go and C#; the [threat model](../concepts/threat-model.md)
   explains what the boundary protects.
2. **A channel is a channel.** Vivox channels, Agora channels/rooms and Photon interest groups
   all become Aurix **channels** with a `channel_type` (`team`, `positional`, `command`,
   `whisper`, `echo`) and a JSON `config` ([Channels and audio routing](../features/channels.md)).
   Ad-hoc channels (`{"ad_hoc": {"name": "match-8f3a", …}}` in a token grant) replace
   "join by name" APIs: the channel appears on first join and disappears with the last player.
3. **Moderation moves to your backend or to signed one-time tokens.** The providers either
   have a server-to-server API (Vivox, Agora) or leave it to game code (Photon). Aurix has
   `POST /v1/moderation/{mute,kick,ban,…}` for the backend and single-use **action tokens**
   (`kick`/`mute`/`unmute`) for in-game moderators, so a client can never redirect a moderation
   action ([Moderation](../features/moderation.md)).

## What changes for your operations team

You are now the provider. Concretely that means running (or paying someone to run):

* one or more `aurix-server` nodes per region, PostgreSQL and Redis
  ([Deployment](../operations/deployment.md), [Scaling out](../operations/scaling.md));
* TURN for players behind symmetric NAT — the built-in `[turn]` or your own
  ([Deployment → Network and firewall](../operations/deployment.md#network-and-firewall));
* HA for the control plane — Redis Sentinel or Cluster and PostgreSQL replicas
  ([High availability](../operations/high-availability.md));
* metrics and alerts ([Backups and observability](../operations/observability.md));
* upgrades ([Releases and versioning](../operations/releases.md)).

Aurix ships CI for a chaos suite (node kill, Redis Sentinel failover, PostgreSQL restart), fuzzed
parsers, signed releases and an SBOM, but it does **not** ship a global anycast network, a
status page or a support contract. Read [Limitations](../limitations.md) — it is honest about
what has not been verified outside this repository's CI (real consoles, WebGL players, WAN
scale).

## Feature comparison

Feature presence, from the vendors' public feature lists and this repository. "✓" for Aurix
means implemented and covered by tests here, not "benchmarked against the vendor".

| | Vivox | Agora Voice | Photon Voice | Aurix |
| --- | :-: | :-: | :-: | :-: |
| Self-hosted / source available | – | – | – (Photon Server for Voice is a separate licence) | ✓ AGPL-3.0 server, Apache-2.0 SDKs |
| Positional (3D) voice | ✓ | ✓ (spatial audio extension) | via engine `AudioSource` | ✓ server attenuation + client HRTF / engine spatialization |
| Per-participant streams for engine spatialization | – (channel mix) | ✓ | ✓ | ✓ native/Unity/Unreal/Godot PCM, browser tracks |
| Large channels (listeners, server mix, stream caps) | ✓ | ✓ (audience role) | interest groups | ✓ |
| Text chat | ✓ | separate product | separate product | ✓ (lite: channel, direct, history, read markers) |
| Server-side moderation API | ✓ | ✓ | – | ✓ REST + action tokens + webhooks |
| Recording | – (third party) | ✓ cloud recording | – | ✓ Ogg/Opus, consent-gated, mixdown, S3 |
| Speech-to-text / TTS / translation | STT ✓, TTS ✓ | via extensions | – | ✓ pluggable providers |
| End-to-end encryption | – | shared-key media encryption | transport encryption | ✓ group E2EE (sender keys) native + browser |
| Reconnect / resume / failover | ✓ | ✓ | ✓ | ✓ resume + cross-node takeover |
| Web / browser client | ✓ | ✓ | – | ✓ WebRTC SDK + Unity WebGL + Godot Web |
| Unity / Unreal / Godot | ✓ / ✓ / – | ✓ / ✓ / – | ✓ / – / – | ✓ / ✓ (not compiled in this CI) / ✓ |
| Consoles | ✓ | ✓ | ✓ (Unity) | porting guide over the C ABI, no shipped binaries |
| Global PoP network, SLA, support | ✓ | ✓ | ✓ | yours to provide |

## Migration order that keeps players talking

1. Stand up a staging node and run the [quick start](../getting-started/quick-start.md) to the
   end — two browser tabs talking. Budget an afternoon including PostgreSQL/Redis.
2. Add an Aurix token endpoint next to your existing one (the token servers in
   `sdk/server/*/examples` are a template). Identity comes from *your* session, never from the
   request body.
3. Port one channel type behind a feature flag — usually the party/squad channel — and ship it
   to an internal build. Compare: join latency, loss/jitter on your test networks, CPU on the
   lowest-end device you support.
4. Port positional channels and moderation; wire `participant.*` / `moderation.*` webhooks to
   whatever consumed the vendor's callbacks.
5. Canary by region or by cohort; keep the vendor SDK in the build until the canary has run
   through a peak. Aurix and the vendor can coexist — nothing in Aurix claims the audio device
   exclusively.
6. Remove the vendor SDK, rotate the vendor secrets out of your backend.
