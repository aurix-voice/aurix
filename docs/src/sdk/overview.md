# Client SDKs

Four clients ship in the repository. They share one security model (player JWT or one-time
action token from **your** backend, API keys never leave the server side), one control plane
(the [WebSocket protocol](../api/websocket.md)) and one feature vocabulary — the table shows
where they differ.

| | [Web](web.md) | [Unity / .NET](unity.md) | [Native core](native.md) | [Unreal](native.md#unreal-plugin) |
|---|---|---|---|---|
| Language | TypeScript (ES2020, no deps) | C# (netstandard2.1) | Rust + C ABI (`aurix_client.h`, C++11 RAII header) | C++ / Blueprint over the C ABI |
| Media transport | WebRTC (Opus, single peer connection) | AURX v2 over UDP (WS tunnel fallback); Unity WebGL: browser WebRTC through the Web SDK | AURX v2 over UDP (WS tunnel fallback) | AURX v2 over UDP (WS tunnel fallback) |
| Codec | browser Opus | `IOpusCodec` — Concentus sample (pure C#) or `NativeOpusCodec` (libopus from the native core) | bundled libopus (static) | bundled libopus (static) |
| PCMU (G.711) fallback | — (WebRTC negotiates Opus) | `SetAudioCodecAsync` / `PreferredCodec`, `PcmuCodec` | `set_audio_codec` / `aurix_client_set_audio_codec` | `SetAudioCodec` |
| Platforms | Chromium, Firefox, Safari | Unity 2021.3+ incl. WebGL (`AurixWebGLVoiceClient`), iOS/Android, plain .NET | Linux, macOS, Windows | UE 5.3+ Win64/Linux/Mac |
| Downlink | server-mixed stereo track | per-participant streams, client mixer | per-participant streams, client mixer | client mixer → procedural `USoundWave` |
| Reconnect / resume | yes | yes | yes | yes |
| Local mute / volume / block | yes | yes | yes | yes |
| Transmission mode / focus | yes | yes | yes | yes |
| Positional + directional | yes (stereo mix) | yes (client pan) | yes | yes |
| Echo channel / audio injection | yes | yes | capture push API | capture push API |
| Text chat lite | yes | yes | yes | yes |
| Transcripts / TTS | yes | yes | yes | yes |
| Stats / quality bars | `getStats()` | `GetStats()` | `aurix_client_stats` | `GetStats` |
| Devices / input gain / speaker mute | yes | yes | host-provided capture | engine `AudioCapture` |
| Action tokens (`refreshToken`/`joinToken`) | yes | yes | yes | yes |
| Opus controls (bitrate, bandwidth, complexity, signal, VBR/CVBR, FEC, loss %, DTX) | bitrate, bandwidth, FEC, DTX, CBR via WebRTC `fmtp`/`setParameters` | all (`OpusEncoderSettings`) | all (`EncoderSettings` / `AurixEncoderSettings`) | all (`FAurixEncoderSettings`) |
| Channel audio policy (`ChannelJoinAck.audio`, `ChannelAudioPolicy`) | merged, applied where WebRTC allows | merged, applied | merged, applied | merged, applied |

Where a browser is not involved the native path is preferred: it avoids ICE/DTLS, costs ~30
bytes of header per 20 ms frame and lets the client mix and pan per participant. Browsers cannot
send raw UDP, so the Web SDK is the only WebRTC client — Unity WebGL players reuse it through a
JavaScript bridge (`AurixWebSdk.AurixBridge` + `AurixWebGL.jslib`) behind the same C# interface as
the native Unity client ([Unity WebGL](unity.md#unity-webgl)); the server bridges both transports
inside the same channel.

## Common lifecycle

1. Backend mints a token (`POST /v1/tokens` or `POST /v1/tokens/action`) and hands it to the
   client.
2. Client opens the WebSocket → `SessionInitAck` (session id, SSRC, media address, media key).
3. Native clients bind UDP with a signed `SessionBind`; the Web SDK negotiates WebRTC.
4. `ChannelJoin` per channel → roster; microphone frames flow; events drive the HUD.
5. On a drop the SDK resumes the same session within the grace window
   (`recovering` → `recovered {resumed: true}`), otherwise it opens a fresh session and
   re-joins.
6. `disconnect()` closes the session; a server `SessionClose` (kick, ban, erasure, shutdown) is
   final and never triggers a reconnect.

The full event list and payloads are in the [WebSocket](../api/websocket.md) chapter; each SDK
chapter maps its methods and events onto those messages.

## Choosing the codec in Unity

The Unity SDK does not bundle Opus. The **Concentus Opus codec** sample (`Samples~/Concentus`)
is a pure-C# implementation that works everywhere Unity runs; **`NativeOpusCodec`** binds
libopus from the native core (`aurix_client`) through P/Invoke for a fraction of the CPU — drop
the binary into `Plugins/` and the quick-start scene picks it automatically
(`NativeOpusCodec.IsAvailable`). Both implement every encoder control and FEC recovery; a custom
`IOpusCodec` needs only `Encode`, `Decode`, `DecodeLost`, `SetBitrate`
([Unity SDK](unity.md#opus-codec-and-controls)).

## Versioning

SDKs are versioned with the server (`Cargo.toml` workspace version, `sdk/web/package.json`,
`sdk/unity/package.json`, `AurixVoice.uplugin`). The wire protocol carries `PROTOCOL_VERSION`
(2) in every AURX packet; control messages are JSON with `type` + `data`, unknown message types
are ignored by all SDKs, so a newer server can talk to an older client as long as the AURX
version matches.
