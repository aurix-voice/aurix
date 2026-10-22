# Aurix Voice — Godot 4 addon

GDExtension voice client for the [Aurix](https://github.com/aurix-voice/aurix) voice platform:
`AurixVoiceClient` node with Opus over the native AURX media path (QUIC/UDP with WebSocket tunnel
fallback), positional audio, per-participant mute/volume, push-to-talk/VAD, quality stats, audio
injection, region discovery and reconnect/recovery. Web exports use the bundled
`AurixWebVoiceClient` (WebRTC) instead.

Install: unzip the release addon (`aurix-godot-addon-<version>.zip`, prebuilt for Linux x86_64/arm64,
Windows x86_64, macOS universal, Android arm64) into your project's `addons/` and enable
**Aurix Voice** in *Project → Plugins*. Other targets: build with `sdk/godot/scripts/build_native.sh`.

Docs: https://github.com/aurix-voice/aurix/blob/main/docs/src/sdk/godot.md
