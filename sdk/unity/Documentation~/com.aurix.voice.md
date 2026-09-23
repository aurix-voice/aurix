# Aurix Voice SDK (`com.aurix.voice`)

Game voice client for the self-hosted [Aurix Voice Platform](https://github.com/aurix-voice/aurix):
one `IAurixVoiceClient` API with two implementations.

| Platform | Class | Media path |
|---|---|---|
| Desktop, iOS, Android, consoles (via the native core), plain .NET | `Aurix.AurixVoiceClient` + `AurixVoiceBehaviour` | AURX v2 over UDP (sealed, replay-protected), WebSocket tunnel fallback; Opus through `IOpusCodec` |
| Unity WebGL players | `Aurix.WebGL.AurixWebGLVoiceClient` + `AurixWebGLVoiceBehaviour` | Browser WebRTC through the Web SDK (`aurix-web-sdk.js`) via `Plugins/WebGL/AurixWebGL.jslib` |

The full API reference with every feature (channels, moderation, positional audio, per-participant
playback, E2EE, chat, transcripts, TTS, translation, effects, lip-sync, ducking, quality, mobile)
is the package `README.md`; this file covers installation, the package layout and how it is
verified.

## Requirements

- Unity **2021.3 LTS** or newer (netstandard2.1, IL2CPP-safe: no reflection, no `unsafe`).
- An Opus codec for native players: import the **Concentus Opus codec** sample and add the
  [Concentus](https://www.nuget.org/packages/Concentus) 2.x `netstandard2.0` assembly to
  `Assets/Plugins/`, or ship the native core (`NativeOpusCodec`, libopus with DRED/OSCE). WebGL
  players need no codec — the browser negotiates Opus.
- *Project Settings ▸ Audio ▸ System Sample Rate* = **48000** (other rates go through the built-in
  resampler). `Aurix Voice ▸ Check project setup` verifies this and the platform-specific items.
- A running Aurix node. Tokens come from **your backend** (`POST /v1/tokens`); the API key never
  ships in a build.

## Installation

### From a git URL

*Window ▸ Package Manager ▸ + ▸ Add package from git URL…* and paste the repository URL with the
package subfolder and (optionally) a tag:

```
https://github.com/aurix-voice/aurix.git?path=sdk/unity#v1.6.0
```

or add it to `Packages/manifest.json`:

```json
{
  "dependencies": {
    "com.aurix.voice": "https://github.com/aurix-voice/aurix.git?path=sdk/unity#v1.6.0"
  }
}
```

Pin a release tag or a commit hash (`#<sha>`); `#main` follows the development branch. The
repository is versioned as one unit, so pick the tag/commit of the server you deploy. Unity needs
`git` on the `PATH` for git dependencies. Private forks work with SSH URLs
(`git@…:org/aurix.git?path=sdk/unity`).

### From disk / a tarball

*Add package from disk…* → `sdk/unity/package.json` of a checkout (a `file:` dependency in
`manifest.json`), or `npm pack` in `sdk/unity` and *Add package from tarball…*. Folders ending in
`~` (`Samples~`, `Documentation~`, `DotNet~`, `BrowserTests~`) are not imported by Unity — samples
arrive through the package page's **Import** buttons.

### Samples

| Sample | For | Notes |
|---|---|---|
| Concentus Opus codec | native players | `IOpusCodec` on Concentus (pure C#); add the Concentus DLL yourself |
| Voice quick start | desktop / mobile | IMGUI lobby on `AurixVoiceBehaviour`: connect, roster, mute / push-to-talk, quality bars, stats, reconnect, chat |
| WebGL quick start | Unity WebGL | the same lobby on `AurixWebGLVoiceBehaviour`, reads `ws`/`api`/`token`/`channel` from the page URL, autoplay unlock, HRTF indicators; ships a WebGL template that loads `aurix-web-sdk.js` |

Each sample has its own `README.md` after import
(`Assets/Samples/Aurix Voice SDK/<version>/<sample>/`).

## Package layout

```
Runtime/            Aurix.Voice assembly (all platforms). Native transport/audio is compiled out of WebGL players.
  Plugins/WebGL/    AurixWebGL.jslib — linked into WebGL players only
Editor/             Aurix.Voice.Editor (Editor only): project checks, StreamingAssets helper, docs link
Tests/Runtime/      Aurix.Voice.Tests (NUnit, Unity Test Runner): wire format, E2EE vectors, WebGL bridge contract
Samples~/           Concentus, VoiceQuickstart, WebGLQuickstart (+ WebGLTemplates/Aurix)
Documentation~/     this file
DotNet~/            development-only .NET solution: library build, xunit tests, Unity compile check, headless two-client demo
BrowserTests~/      development-only Chromium test of the real .jslib + Web SDK bundle
```

## Unity WebGL in two steps

1. Build the Web SDK bundle (`cd sdk/web && npm ci && npm run build`) and copy
   `dist/aurix-web-sdk.js` into `Assets/StreamingAssets/` — `Aurix Voice ▸ Copy Web SDK bundle to
   StreamingAssets…` does this and validates the file. Or import the WebGL quick start, select its
   `Aurix` WebGL template in *Player Settings ▸ Resolution and Presentation* and put the bundle
   next to `index.html`.
2. Add `AurixWebGLVoiceBehaviour` (API URL, `wss://` URL, token, channel). Serve the player over
   `https://` (or `localhost`) — `getUserMedia` refuses insecure origins — and allow the page origin in
   the node's `AURIX__SERVER__CORS_ORIGINS`. Keep `UseTurn` on unless your players are all on open
   networks. Remote audio starts after a user gesture: call `ResumeAudioAsync()` from a button.

Details, differences from the native client and the full option list: `README.md` → "Unity WebGL".

## Tests and verification

In the Editor: *Window ▸ General ▸ Test Runner ▸ PlayMode / EditMode* runs `Tests/Runtime`
(no server needed). In the repository:

```bash
cd sdk/unity/DotNet~
dotnet test Aurix.sln                                   # xunit: protocol, crypto, jitter buffer, client, WebGL bridge (scripted)
dotnet build Aurix.Voice.UnityCheck -p:UnityExtraDefines=UNITY_WEBGL   # Runtime + Editor + Tests against UnityEngine stubs, warnings as errors
cd ../../web && npm ci && npm test                      # includes AurixWebGL.jslib against the real bundle (Node, Emscripten stand-in)
python3 ../unity/BrowserTests~/webgl_bridge_e2e.py      # the same in Chromium; with AURIX_API_URL/AURIX_WS_URL/AURIX_API_KEY also a live join
```

What this does **not** cover: the Unity Editor itself, a Unity-built WebGL player, real devices,
headphones or microphones. Import the package into a throwaway project and build once before
relying on it — see the repository's `docs/src/limitations.md`.
