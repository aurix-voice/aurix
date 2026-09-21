# Voice quick start (Unity sample)

A single scene that exercises the whole client flow against a running Aurix node: connect with a
player token, join a channel, talk (open mic or push-to-talk), see who is speaking, watch network
quality and client statistics, force a reconnect, send a chat line.

Everything is driven by `VoiceQuickstart.cs`, a small `MonoBehaviour` that wires up the SDK's
`AurixVoiceBehaviour` and draws an IMGUI panel — no prefabs, UI Toolkit or extra packages.

## Install

1. Add the package (`sdk/unity`) to your project via *Package Manager → Add package from disk/git*.
2. Import **both** samples from the package page: *Concentus Opus codec* (the codec the SDK needs;
   requires the [Concentus](https://www.nuget.org/packages/Concentus) assembly in your project, see
   the package README) and *Voice quick start*.
3. Open `Assets/Samples/Aurix Voice SDK/<version>/Voice quick start/VoiceQuickstart.unity`.

## Run

1. Start a server (see the *Quick start* chapter of the docs) and create a channel with your API key:
   `POST /v1/channels`.
2. Mint a player token for that channel: `POST /v1/tokens` with `user_id`, `display_name` and
   `channels: [<channel_id>]`. In production this call is made by **your game backend** — the API
   key must never be shipped in a Unity build.
3. Press Play. Fill in *WebSocket URL* (`ws://<host>:8081/ws`), paste the token and channel id,
   press **Connect**. Run a second instance (another editor / build, or the .NET demo
   `dotnet run --project DotNet~/Aurix.Demo`) with a token for a different `user_id` to hear each other.

Panel controls:

| Control | SDK call |
| --- | --- |
| Connect / Disconnect | `AurixVoiceBehaviour.Connect()` / `Disconnect()` |
| Reconnect | `AurixVoiceClient.ForceReconnect()` — session resume, same SSRC |
| Mute microphone / push-to-talk | `AurixVoiceBehaviour.SetMuted(bool)` |
| Mute speakers / volume | `SetOutputMuted(bool)` / `SetOutputVolume(float)` |
| Quality bars, R-factor, MOS, RTT, loss | `OnNetworkQuality`, `OnStats`, `Client.GetStats()` |
| Participants (speaking, energy, mutes) | `OnChannelJoined`, `OnParticipantJoined/Left/Updated` |
| Chat | `Client.SendMessageAsync(channelId, text)`, `OnChatMessage` |
| Log | state changes, recovering/recovered/failed-to-recover, kicks, recording notices, server errors |

Inspector fields: `PushToTalkKey` (default `None` = open microphone), `ConnectOnStart`,
`ShowPanel`, `PanelWidth`, `LogLines`.

## Adapting

* Replace the IMGUI panel with your own UI — the component only uses public SDK API, so the
  `Subscribe()` method is a checklist of the events a real HUD should handle.
* Positional audio: set `channel_type: positional` on the channel and call
  `Voice.Client.UpdatePositionAsync(...)` from your player controller each tick.
* Mobile: the behaviour already handles the microphone permission dialog, background/foreground
  and network changes; see *Mobile (iOS / Android)* in the package README.

The sample compiles under the same rules as the runtime (`netstandard2.1`, no `unsafe`, no
reflection); CI type-checks it together with the Unity-only runtime code via
`DotNet~/Aurix.Voice.UnityCheck`.
