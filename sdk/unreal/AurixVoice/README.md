# Aurix Voice — Unreal Engine plugin

Voice client plugin for the [Aurix](https://github.com/aurix-voice/aurix) voice platform (UE 5.3+):
`UAurixVoiceSubsystem` with Opus over the native AURX media path (QUIC/UDP with WebSocket tunnel
fallback), positional audio, per-participant controls, push-to-talk/VAD, capture hot-swap, audio
injection, quality stats, region discovery, background/foreground handling and reconnect/recovery.
Platforms: Win64, Linux, Mac, Android (the Rust core ships prebuilt in `Source/ThirdParty`).

Install: unzip `AurixVoice-unreal-<version>.zip` into `<Project>/Plugins/` (or the engine-compiled
`-packaged-Linux.zip` when available) and enable the plugin. The `AurixVoiceSamples` module has a
lobby sample.

Docs: `Docs/` in this folder (quick start, packaging, platform notes) and
https://github.com/aurix-voice/aurix/tree/main/sdk/unreal
