# Changelog

All notable changes to `com.aurix.voice` are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the package version follows the Aurix
server version it ships with (the repository is versioned as one unit).

## [Unreleased]

Nothing yet.

## [1.3.0] — 2026-09-19

### Added

- Stored chat: `EditMessageAsync`, `DeleteMessageAsync`, `ReactAsync`, `SearchAsync` /
  `SearchDirectAsync`, `OnChatMessageUpdated` / `OnChatReactionChanged` on both the native and
  the WebGL client (`ChatMessage.Reactions`, tombstones for deleted messages).
- `NetworkQuality.ReceiversLossPercent` / `ProtectLossPercent`: the loss profile (FEC / DRED)
  now protects the uplink against the higher of the uplink loss and the worst downlink loss a
  receiver of this client reports, so listeners on lossy links get redundancy from the talker.

## [1.2.0] — 2026-09-19

First tagged release of the package (versioned together with the server). Everything below
shipped under this number.

### Added

- UPM package layout: `Documentation~/com.aurix.voice.md`, this changelog, `Editor/` assembly
  (`Aurix Voice ▸ Check project setup`, `Copy Web SDK bundle to StreamingAssets…`,
  `Documentation` menu items) and `Tests/Runtime/` NUnit tests (protocol wire format, E2EE
  vectors shared with the Rust and TypeScript SDKs, WebGL bridge contract) that run in the Unity
  Test Runner and are compiled by the .NET Unity compile check.
- **WebGL quick start** sample: `AurixWebGLVoiceBehaviour` lobby that reads `ws`/`api`/`token`/
  `channel` from the page URL, autoplay unlock button, roster with speaking/spatialization
  indicators, chat, stats, and a WebGL template (`WebGLTemplates/Aurix`) that ships
  `aurix-web-sdk.js` next to the Unity loader.
- Browser test of the real `AurixWebGL.jslib` + real Web SDK bundle under an Emscripten stand-in
  in Chromium (`BrowserTests~/webgl_bridge_e2e.py`): template boot, plugin protocol, and — with a
  live node — connect/join/roster/chat/media/quality/mute/volume/pin/leave. Wired into CI; a Unity
  Editor / Unity-built WebGL player is still not part of CI.

### Changed

- `WebGLClientOptions.UseTurn` and `AurixWebGLVoiceBehaviour.UseTurn` now default to `true`, matching
  the Web SDK: the node's TURN credentials are fetched and offered as an ICE server (needed behind
  symmetric NAT; failures stay non-fatal). The previous documentation of the flag as "force relay"
  was wrong — it never set `iceTransportPolicy`.
- The development-only .NET solution moved from `DotNet/` to `DotNet~/` so the Unity importer skips
  it; `package.json` gained `type`, `license`, `documentationUrl`/`changelogUrl` and per-sample
  descriptions.

### Fixed

- Web SDK (which the WebGL client runs on): a rejected initial WebSocket handshake left the client in
  `connecting`; it now settles to `failed` and `ConnectAsync` rejects.

### Baseline

Newest first, what the SDK contained before the sections above were added:

- QUIC media path for the native core (`NativeOpusCodec` users) — the C# native transport stays
  on AURX/UDP with the WebSocket tunnel fallback.
- Adaptive packet-loss profile (`LossController`: FEC / expected-loss / DRED tiers from server
  uplink loss reports), decoder complexity controls, FEC/DRED/PLC recovery in `RemoteMixer`.
- Per-session MOS / R-factor in `OnStats`, `OnNetworkQuality`.
- Lip-sync visemes (`AurixLipSync`), priority speakers + game-audio ducking
  (`AurixGameAudioDucker`), voice-effects library and presets.
- Group E2EE (X25519 key wrap, sender keys with rotation, AES-256-CTR + HMAC frames) with
  fingerprints and events; plaintext downgrade is refused.
- Unity WebGL: `AurixWebGLVoiceClient` / `AurixWebGLVoiceBehaviour` — the same `IAurixVoiceClient`
  over the browser Web SDK, per-participant WebRTC tracks spatialized with HRTF, pinning.
- Per-participant PCM (`AurixParticipantAudioSource`, `AurixListenerTap`) for engine
  spatialization; stereo / music uplink.
- Cross-node failover, WebSocket tunnel when UDP is blocked, region discovery with RTT ranking,
  IPv6 candidates, PCMU fallback, downlink modes for large channels.
- Chat with history / offline delivery / read markers, transcripts, live translation, TTS,
  one-time action tokens, safety events.
- iOS / Android: runtime microphone permission, background handling, network-change reconnect,
  output-rate resampling.

## [1.0.0] — 2026-09-17

- Initial release: AURX v2 over UDP (AES-256-CTR + HMAC-SHA256, replay window), WebSocket control
  plane, `IOpusCodec` abstraction with the Concentus sample, `JitterBuffer` / `RemoteMixer`,
  `AurixVoiceBehaviour`, session resume + auto-reconnect, local mute / volume / block, xunit
  tests and the headless two-client demo.
