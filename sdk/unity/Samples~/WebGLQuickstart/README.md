# WebGL quick start

The browser flavour of the *Voice quick start*: a scene with `WebGLQuickstart` on top of
`AurixWebGLVoiceBehaviour`. In a Unity WebGL player the voice path is browser WebRTC through the
standalone Web SDK (`aurix-web-sdk.js`) — no UDP, no `ClientWebSocket`, no Unity microphone or
`AudioSource` playback; the browser owns capture and playback (see the package README, “Unity WebGL”).

## What it shows

* connect with a player token, join one or more channels;
* roster with speaking / muted flags and whether each participant plays on its own per-participant
  WebRTC track (HRTF-spatialized) or in the server mix;
* microphone mute, speaker mute and volume (browser `GainNode`);
* **Enable audio** button — browsers block remote playback until a user gesture
  (`AurixWebGLVoiceClient.ResumeAudioAsync`);
* network quality bars / R-factor / MOS from the node, WebRTC `getStats` (RTT, loss, jitter buffer,
  concealed samples) refreshed once a second;
* reconnect (`ReconnectNow`), chat line, dropped-events notice;
* connection fields pre-filled from the page URL:
  `index.html?ws=wss://voice.example.com/ws&api=https://voice.example.com&token=<jwt>&channel=<uuid>`.

## Install

1. Package Manager ▸ Aurix Voice SDK ▸ Samples ▸ **WebGL quick start** ▸ Import. The sample lands in
   `Assets/Samples/Aurix Voice SDK/<version>/WebGL quick start/`.
2. Build the Web SDK bundle once (`cd sdk/web && npm ci && npm run build`) and either
   * copy `sdk/web/dist/aurix-web-sdk.js` to `Assets/StreamingAssets/` (the behaviour's default
     `SdkUrl` is `StreamingAssets/aurix-web-sdk.js`; **Aurix Voice ▸ Copy Web SDK bundle to
     StreamingAssets…** in the Editor does this for you), or
   * use the bundled WebGL template: move `WebGLTemplates/Aurix` from the imported sample to
     `Assets/WebGLTemplates/Aurix` (Unity only lists templates from that folder), put
     `aurix-web-sdk.js` next to its `index.html`, and pick **Aurix** under Project Settings ▸ Player ▸
     WebGL ▸ Resolution and Presentation ▸ WebGL Template. The page then loads the SDK before the
     player starts and `SdkUrl` is ignored.
3. File ▸ Build Settings ▸ WebGL ▸ Build, serve the output over http(s) (`python3 -m http.server`
   for a local try; the microphone needs a secure origin outside `localhost`), open the page with the
   query parameters above and press **Connect**.

The node must allow the page origin (`AURIX__SERVER__CORS_ORIGINS`) and, for players behind
symmetric NAT, TURN (`UseTurn` on the behaviour). Compression: Unity's Brotli/Gzip output needs the
matching `Content-Encoding` headers from your web server, or disable compression for local tests.

## Outside WebGL

In the Editor and on desktop/mobile players the panel says so and **Connect** is disabled:
`NativeWebGLBridge.IsSupported` is false because the `.jslib` plugin only exists in WebGL builds.
Use the *Voice quick start* sample there. The class compiles on every platform, so a project can
ship both samples and pick the scene per build target.

## What is (not) verified in this repository

`sdk/unity/BrowserTests~/webgl_bridge_e2e.py` runs in Chromium (Playwright) and in CI: it boots this
sample's `WebGLTemplates/Aurix/index.html` with Unity's placeholders substituted and a stub loader
(SDK loaded before the loader, insecure-origin and missing-bundle reporting), then evaluates the real
`Runtime/Plugins/WebGL/AurixWebGL.jslib` against the real `aurix-web-sdk.js` under an Emscripten
stand-in (heap, UTF-8 marshalling, `mergeInto`) and issues exactly the calls `AurixWebGLVoiceClient`
makes — offline (status, error paths, protocol) and, with `AURIX_API_URL` / `AURIX_WS_URL` /
`AURIX_API_KEY`, against a live node (connect, join, roster, chat, WebRTC media with a second browser
peer, quality, stats, mute/volume/pin, leave, disconnect). `WebGLQuickstart.cs` compiles with the
`UNITY_WEBGL` define in `DotNet~/Aurix.Voice.UnityCheck`. The stand-in is not Unity's Emscripten
runtime and no Unity WebGL **player build** of this sample was run in this repository (no Unity
Editor in CI) — build it once in your project before relying on it.
