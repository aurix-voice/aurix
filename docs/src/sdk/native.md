# Native core and Unreal SDK

`aurix-client` is the reusable native client: the AURX v2 media path and the WebSocket control
plane written once in Rust and exposed as a Rust crate, a stable **C ABI**
(`crates/aurix-client/include/aurix_client.h`, generated with cbindgen and drift-checked in
tests) and a header-only **C++11 RAII wrapper** (`aurix_client.hpp`). The Unreal plugin is a
thin layer over that wrapper; mobile wrappers or other engines can follow the same pattern.
Full reference: `crates/aurix-client/README.md` and `sdk/unreal/README.md`.

```
 your engine / app              aurix-client
 ┌──────────────────┐   PCM     ┌──────────┐  Opus + level  ┌───────┐  AURX v2 (AES-CTR+HMAC)
 │ mic callback     ├──────────►│ capture  ├───────────────►│ media ├────────────────────────► UDP
 │ speaker callback │◄──────────┤ mixer    │◄───────────────┤ (udp) │◄──────────────────────── UDP
 │ game thread      │ poll      ├──────────┤                └───────┘
 │  (events, joins) │◄─────────►│ client   ├─── typed ControlMessage ─── WebSocket (wss, bearer)
 └──────────────────┘           └──────────┘
```

## What the core does

* **Audio:** 48 kHz / 20 ms Opus with input gain, VAD gating and RFC 6464 level metadata,
  resampling from any device rate, per-sender jitter buffers with PLC, stereo mixer with
  per-participant gain and directional panning, output volume/mute. Opus is linked statically —
  no system `libopus` in the shipped game.
* **Capture DSP:** pure-Rust microphone processing — 80 Hz high-pass, acoustic echo
  cancellation, RNNoise-derived neural noise suppression, speech-gated AGC — between resampling
  and the input gain / VAD / encoder ([below](#capture-dsp-echo-cancellation-noise-suppression-agc)).
* **Media:** signed `SessionBind`, AES-256-CTR + HMAC on every packet, replay windows,
  heartbeats with RTT, quality reports; QUIC datagrams first (0-RTT resume, connection
  migration — [below](#quic-0-rtt-resume-and-connection-migration)), raw UDP next, the same
  packets over the control WebSocket when UDP is blocked
  ([below](#when-udp-is-blocked-the-websocket-tunnel)).
* **Control:** `Authorization: Bearer` WebSocket, one-time resume tokens, typed
  `ControlMessage`s.
* **Client:** one voice session on a small private Tokio runtime; reconnect with backoff and
  session resume (same session / SSRC / media key / channels), automatic re-join after a fresh
  session (`REJOIN_FAILED` per channel that needs a new join token), receiver preferences that
  survive resume, failover to the nodes the server advertised (`aurix_client_endpoint`,
  `aurix_client_failover_endpoint*`, `AURIX_EVENT_ENDPOINT_CHANGED`, `migrated` on
  `RECOVERED` — same session/SSRC, new media key/endpoint, see
  [High availability](../operations/high-availability.md#cross-node-session-failover)), chat
  (including stored history pages, read markers, the offline inbox, edits / deletions,
  reactions and search — `aurix_client_chat_history`, `aurix_client_mark_chat_read`,
  `aurix_client_chat_read_markers`, `aurix_client_edit_chat` / `aurix_client_delete_chat`,
  `aurix_client_react_chat`, `aurix_client_search_chat`, `AURIX_EVENT_CHAT_HISTORY` /
  `CHAT_READ_MARKER(S)` / `CHAT_INBOX_SYNCED` / `CHAT_MESSAGE_UPDATED` / `CHAT_REACTION_CHANGED` /
  `CHAT_SEARCH_RESULT`; `AurixClientConfig.device_id` for the exactly-once per-device queue with
  automatic `ChatAck`, see
  [Text chat](../features/chat.md#stored-chat-history-offline-delivery-read-markers)),
  moderation with action tokens, transcripts, TTS, recording consent, positions, statistics —
  behind a synchronous, thread-safe API with a poll-based event queue.

## Rust

```rust
use aurix_client::{Client, ClientConfig, Event};
use std::time::Duration;

let client = Client::new(ClientConfig::new("wss://voice.example.com/ws", session_jwt))?;
client.connect()?;
loop {
    match client.wait_event(Duration::from_millis(100)) {
        Some(Event::SessionReady(_)) => client.join_channel(channel_id, None)?,
        Some(Event::ChannelJoined { participants, .. }) => println!("{} others", participants.len()),
        Some(Event::Disconnected { .. }) => break,
        _ => {}
    }
    // audio thread: client.push_capture_f32(mic_pcm, device_rate, channels);
    //               client.mix_output_f32(speaker_pcm, 2);
}
```

`push_capture_*` / `mix_output_*` are lock-light and meant for the audio callback; everything
else is game-thread API. `set_wake_hook` lets an engine signal its main loop instead of polling.

## C / C++

```bash
cargo build -p aurix-client --release              # libaurix_client.{so,dylib,dll,a}
cargo run -p aurix-client --example gen_header     # regenerate include/aurix_client.h
cc  -std=c99   -Icrates/aurix-client/include crates/aurix-client/examples/c/voice_loop.c     -Ltarget/release -laurix_client -lm -o voice_loop
c++ -std=c++11 -Icrates/aurix-client/include crates/aurix-client/examples/cpp/voice_loop.cpp -Ltarget/release -laurix_client -lm -o voice_loop_cpp
```

Both samples are complete integrations (create → connect → `AURIX_EVENT_SESSION_READY` → join →
push a tone / mix output → statistics → disconnect) and are compiled, linked and run by
`cargo test -p aurix-client --test c_abi`.

Rules that matter:

* `aurix_client_create` / `aurix_client_destroy` own the handle; never destroy from the wake
  callback. Events from `aurix_client_poll_event` / `wait_event` are freed with
  `aurix_event_free`; accessor pointers live until then.
* Input strings/arrays are borrowed for the call and copied when retained.
* `aurix_last_error` is thread-local.
* Audio functions may be called from a real-time thread concurrently with control calls.
* `aurix_client_disconnect` blocks up to ~3 s for the server acknowledgement; `connect` is
  asynchronous (`STATE_CHANGED` / `SESSION_READY` / `DISCONNECTED`).
* Every Rust enum has a `#[repr(C)]` mirror; `aurix_event_json` exposes the full serialised
  event for fields without a dedicated accessor.

Per-participant playout (engine spatialization): instead of `aurix_client_mix_output_*`
(everyone, directional panning already applied), pull each talker separately with
`aurix_client_pull_participant_f32/i16(client, &user_id, out, samples, channels)` — the user's
microphone and TTS streams only, **overwriting** `out`, no local panning (a stereo sender keeps
L/R on a stereo output, is downmixed for mono), per-participant volume / server gain / master
volume still applied; the return value is the number of frames that carried audio.
`aurix_client_set_participant_claimed(client, &user_id, true)` takes that user out of
`mix_output_*` so a spatialized emitter per talker and the aggregate mix for everyone else run
side by side without double-playing (each frame is consumed by whoever renders it; the claim
follows the user across SSRC changes). `aurix_client_participant_streams` lists the buffered
streams (`AurixParticipantStream`: ssrc, owner, synthesized, mixed, stereo, active,
buffered_frames) for hosts that spawn an emitter per talker. Pulled audio is not fed to the
echo canceller — `aurix_client_push_render_*` the engine's final output. A server-mixed
downlink (`DownlinkMode::Mixed`) carries no per-participant streams. C++: `pull_participant`,
`set_participant_claimed`, `participant_streams`; Rust: `Client::pull_participant_f32/i16`,
`set_participant_claimed`, `participant_streams`.

Statistics: `aurix_client_stats` (`AurixStats`: media counters, `bad_auth`, `replayed`,
`heartbeats_lost`, RTT last/min/avg/max, jitter, `loss_percent`, `r_factor`, `mos`, `bars`),
`aurix_client_network_quality` and the `NETWORK_QUALITY` event — see
[Network quality](../features/quality.md).

## Opus encoder controls

The core owns a libopus encoder (statically linked) and exposes every control of it:

```rust
use aurix_client::{EncoderSettings, OpusBandwidth, OpusSignal};

let mut cfg = ClientConfig::new(ws_url, jwt);
cfg.encoder = EncoderSettings {
    bitrate_bps: 32_000,                 // 6_000..=300_000 mono, ..=510_000 stereo
    complexity: 9,                       // 0..=10
    max_bandwidth: OpusBandwidth::Fullband,
    signal: OpusSignal::Voice,           // Auto | Voice | Music → application + signal hint
    vbr: true, constrained_vbr: true,
    fec: true, expected_loss_percent: 5,
    dtx: false,
    channels: 1,                         // 2 = stereo uplink (music / broadcast sources)
};
cfg.follow_channel_policy = true;        // default

client.set_encoder_settings(settings)?;  // replace the baseline at runtime
client.set_complexity(Some(5))?;         // pin the CPU budget regardless of policy hints
client.audio_policy();                   // merged policy of the joined channels
// Event::AudioPolicyChanged(policy) / Event::BitrateChanged { .. }
```

C: `AurixEncoderSettings`, `aurix_client_set_encoder_settings` / `aurix_client_encoder_settings`,
`aurix_client_set_complexity` (−1 un-pins), `aurix_client_audio_policy` (`AurixAudioPolicy`),
`AURIX_EVENT_AUDIO_POLICY_CHANGED` + `aurix_event_audio_policy`. Three layers are applied in
order: the baseline, the merged **channel audio policy** (`ChannelJoinAck.audio`, live
`ChannelAudioPolicy`; max bitrate, widest bandwidth, FEC if any channel wants it, DTX only if
all allow it, Music > Voice > Auto, complexity hint unless pinned) and the server's transient
`BitrateCommand` (clamped to the policy's floor/target; its `expected_loss_percent` also raises
the FEC tuning). Same semantics in Unity and — where WebRTC permits — the Web SDK
([Channels](../features/channels.md#configuration), [Network quality](../features/quality.md)).

**Stereo.** `channels: 2` encodes the first two capture channels as L/R (a mono device is
duplicated); it is honoured only while the merged policy has `stereo: true`
([Stereo and music uplinks](../features/channels.md#stereo-and-music-uplinks)) — a voice
channel forces the encoder back to mono, PCMU is always mono. The capture DSP is bypassed for
stereo frames (gain, VAD and energy still run on the L/R average), so pair it with
`OpusSignal::Music` and a music source rather than a microphone. On the receive side nothing is
configured: the mixer switches a stream to a stereo decoder on its first stereo packet, keeps the
image for non-positional senders, downmixes before panning directional ones, and averages L/R
for `mix_output_*(…, 1)`.

The codec is also available **standalone**, for hosts that run their own transport or want
libopus without the client: `aurix_opus_encoder_create/apply/settings/encode_f32/encode_i16`,
`aurix_opus_decoder_create/decode_f32/decode_i16` (`packet == NULL` = PLC, `fec = true` =
recover the previous lost frame from this packet's FEC data). None of these is variadic, so they
are safe P/Invoke targets — this is how the Unity SDK's `NativeOpusCodec` gets libopus. C++:
`aurix::OpusEncoder` / `aurix::OpusDecoder` in `aurix_client.hpp`.

## Packet loss: FEC, DRED and the neural PLC

The core links **libopus 1.6** (bundled sources, static; `aurix_dred_supported()` reports whether
the build has the neural DRED decoder — the shipped one does) and uses its three loss tools:

* **In-band FEC (LBRR)** — every packet may carry a low-rate copy of the *previous* frame;
  `fec` / `expected_loss_percent` tune how much bitrate goes to it.
* **DRED (Deep REDundancy)** — a neural, very low-rate copy of up to `dred_duration_ms`
  (0 … 1040 ms, 10 ms steps) of *history* in every packet, so a burst of lost frames is
  rebuilt from the first packet that arrives after it. libopus codes as much of the requested
  history as the bitrate affords (nothing below ≈ 28 kbit/s with FEC on, about 100–150 ms at
  28–40 kbit/s, the full span only well above); the wire coverage, not the request, decides
  how far back a receiver can rebuild — older frames fall through to PLC.
* **PLC** — what remains is concealed by the decoder; at decoder `complexity >= 5` libopus uses
  its neural (deep) PLC, `>= 6` adds OSCE (LACE) speech enhancement, `>= 7` the larger NoLACE
  model. `osce_bwe` (bandwidth extension of narrowband/wideband speech) is honoured only by a
  libopus built with it and otherwise silently ignored.

**Receive side.** Every jitter buffer in `RemoteMixer` hands a missing frame to the codec in
that order — FEC when exactly the previous frame is missing and the next packet carries LBRR,
DRED for any frame within the next packet's coverage, PLC for the rest — before decoding the
packet itself, so no extra buffering latency is added: recovery happens when the packet that
ends the gap is played, from bytes that already arrived. Reordered packets that still fit the
buffer are played in order; packets for a slot already played are dropped (`frames_late`). A
stream that ran dry mid-spurt keeps its sequence instead of restarting when the packet that
ends the gap can rebuild it. PCMU streams have none of this (μ-law has no redundancy).
`ClientStats.frames_lost` / `frames_fec_recovered` / `frames_dred_recovered` tell the three
apart; `ClientConfig.decoder` (`DecoderSettings { complexity: 5, osce_bwe: false }`) and
`set_decoder_settings` / `decoder_settings` tune the decoders of every stream at once.

**Send side — the loss profile.** The server measures the loss on *our* packets
(`NetworkQuality.uplink_loss_percent`) and relays the worst downlink loss any receiver of our
audio on the node reported (`receivers_loss_percent` — only we can add redundancy for them).
A `LossController` maps the higher of the two (`NetworkQuality::protect_loss_percent`) to a
tier and re-shapes the effective encoder settings on top of baseline, channel policy and
`BitrateCommand`:

| Profile | Enter | Leave (after 6 s dwell) | Effect on the encoder |
|---|---|---|---|
| `Low` | — | — | baseline / policy as is |
| `Moderate` | loss ≥ 3 % | < 1 % | FEC on, `expected_loss_percent` ≥ 10 (or the measured loss) |
| `High` | ≥ 10 % | < 5 % | FEC on, `expected_loss_percent` ≥ 20, DRED ≥ 400 ms, bitrate raised to 28 kbit/s where the policy/command ceiling allows |

Escalation is immediate, relaxation waits for the dwell (hysteresis keeps a flapping link from
toggling redundancy every report). `LossAdaptation::Auto` is the default;
`Fixed(LossProfile)` pins a tier (a LAN game may want `Low`, a mobile title may start `High`);
`set_loss_adaptation` / `loss_adaptation` / `loss_profile` and `Event::LossProfileChanged
{ profile, uplink_loss_percent }` (the loss the tier protects against; also when a session
ends and the tier resets) expose it;
`ClientStats.loss_profile` mirrors the current tier. A `dred_duration_ms` in the baseline is
kept as a floor — the profile only ever raises redundancy. Everything applies equally to E2EE
frames (redundancy is inside the Opus payload the core seals).

C: `AurixEncoderSettings.dred_duration_ms`, `AurixDecoderSettings` +
`aurix_client_set_decoder_settings` / `aurix_client_decoder_settings`, `AurixLossAdaptation` +
`aurix_client_set_loss_adaptation` / `aurix_client_loss_adaptation` /
`aurix_client_loss_profile`, `AURIX_EVENT_LOSS_PROFILE_CHANGED` + `aurix_event_loss_profile`,
`AurixStats.frames_fec_recovered` / `frames_dred_recovered` / `loss_profile`,
`aurix_dred_supported`. Standalone codec: `aurix_opus_decoder_apply` / `_settings`,
`aurix_opus_decoder_dred_decode_f32/_i16(decoder, later_packet, len, frames_before, pcm,
frame_samples)` (returns samples per channel, `0` when the packet's DRED does not reach that
frame or the build has none) and `aurix_opus_packet_has_fec`. C++: `aurix::OpusDecoder::apply`
/ `decode_dred` / `packet_has_fec`, `aurix::Client::set_loss_adaptation` / `loss_profile` /
`set_decoder_settings` / `dred_supported`.

On the node the same recovery runs in the server mixers (`media.mixer_decoder_complexity`,
`aurix_mixer_lost_frames_total{method}`) and the forwarded per-sender sequence keeps short
uplink losses as gaps so receivers can see and repair them
([AURX](../api/aurx.md#what-the-server-does-with-your-packets)). Cost: DRED adds encoder CPU
and ≈ 1 MB of model weights to the library; the neural PLC/OSCE cost decoder CPU per concealed
or enhanced frame — lower `complexity` on constrained devices.

## Capture DSP: echo cancellation, noise suppression, AGC

The core cleans the microphone before anything else sees it, on 48 kHz mono after
downmix/resampling and before input gain, VAD and the encoder:

```text
device PCM → downmix → resample → high-pass → AEC → noise suppression → AGC → gain → VAD → Opus/PCMU
```

* **High-pass** — second-order Butterworth at 80 Hz: rumble, desk thumps, DC offset.
* **Acoustic echo cancellation** — frequency-domain adaptive filter over a 40–500 ms tail
  (`echo_tail_ms`, 10 ms partitions) with a render→capture delay estimator (up to 500 ms plus
  `stream_delay_ms`), double-talk protection and residual echo suppression. The far-end
  reference is everything `mix_output_f32/i16` renders, so the default "core plays the remote
  voices" setup needs no extra wiring; a game that plays the voice mix or other audio through
  its own engine mixer feeds that speaker signal to `push_render_f32/i16` in playout order.
* **Noise suppression** — RNNoise-derived recurrent network (`nnnoiseless`, pure Rust) at
  `Low` / `Moderate` / `High` (dry/wet blend); keyboard, fans, traffic. Also yields the
  `speech_probability` diagnostic.
* **AGC** — speech-gated (silence is not pumped up), `agc_target_dbfs` −30…−6, at most
  `agc_max_gain_db` (0–40) of boost, soft limiter so a shout never clips.

```rust
use aurix_client::{DspConfig, NoiseSuppression};

cfg.dsp = DspConfig::default();            // everything on, tail 200 ms, NS High, AGC −18 dBFS / +24 dB
cfg.dsp = DspConfig::BYPASS;               // nothing — bring your own processing
client.set_dsp(DspConfig { noise_suppression: NoiseSuppression::Moderate, ..client.dsp() });
let s = client.dsp_stats();                // erle_db, echo_delay_ms, echo_converged, far_end_active,
                                           // speech_probability, agc_gain_db, far_end_underruns
```

C: `AurixConfig.dsp` (`AurixDspConfig`; `aurix_dsp_config_default` / `aurix_dsp_config_bypass`),
`aurix_client_set_dsp` / `aurix_client_dsp` / `aurix_client_dsp_stats`,
`aurix_client_push_render_f32/i16`. Standalone, for hosts with their own transport:
`aurix_dsp_create/destroy/set_config/config/stats`, `aurix_dsp_process_f32` (mono 48 kHz in
place, whole 480-sample blocks) and `aurix_dsp_push_render_f32` — the Unity SDK's
`NativeCaptureDsp` is built on these ([Unity SDK](unity.md#capture-processing-echo-cancellation-noise-suppression-agc)).
Out-of-range values are clamped rather than rejected. Everything is enabled by default; the
browser SDK relies on the browser's own AEC/NS/AGC instead (`getUserMedia` constraints).

## Voice effects

After the DSP and the input gain — and before the VAD meter and the encoder, so peers, level
bars and transcripts all get the effected voice — the capture path runs an
`EffectChain` (`aurix_client::effects`) of `VoiceEffect` stages on each 20 ms 48 kHz frame
(mono or interleaved stereo, output clamped to ±1). The library — `VoiceEffectParams` builds
the chain in the fixed order *filters → formant → pitch → ring modulation → distortion →
tremolo → static → reverb* — is documented with its ranges and presets in
[Voice effects](../features/speech.md#voice-effects); the individual stages (`Biquad`,
`FormantShift`, `PitchShift`, `RingModulator`, `Distortion`, `Tremolo`, `Static`, `Reverb`) are
public too, and `CallbackEffect::new(|frame, channels| …)` wraps the host's own processing.
Effects touch only the microphone uplink — injected audio, TTS and the downlink are untouched —
and the mono, stereo and PCMU encoders all see the processed frame. The chain runs on the
capture thread: no blocking, no allocation.

```rust
use aurix_client::effects::{CallbackEffect, EffectChain, EffectPreset, VoiceEffectParams};
client.set_voice_effects(EffectPreset::Radio.chain());
let mut p = EffectPreset::Monster.params();           // tweak a preset …
p.pitch_semitones = -10.0;
p.reverb_mix = 0.3;
client.set_voice_effects(p.chain());                   // … (`sanitized()` clamps to the limits)
let mut chain = VoiceEffectParams { ring_mod_hz: 60.0, ..VoiceEffectParams::BYPASS }.chain();
chain.push(Box::new(CallbackEffect::new(|frame: &mut [f32], channels: u8| my_dsp(frame, channels))));
client.set_voice_effects(chain);
client.set_voice_effects(EffectChain::default()); // bypass
```

C: `aurix_client_set_voice_effects(&AurixVoiceEffects { highpass_hz, lowpass_hz, formant_semitones,
pitch_semitones, ring_mod_hz, distortion_drive, tremolo_hz, tremolo_depth, static_level,
reverb_mix, reverb_size, reverb_damping })` (zero = stage off, clamped to the `AURIX_MAX_*` /
`AURIX_MIN_FILTER_HZ` limits, `NULL` = bypass), `aurix_voice_effects_preset(AURIX_VOICE_PRESET_*)`
/ `aurix_client_set_voice_preset`, `aurix_client_voice_effects` (read back clamped), and
`aurix_client_set_voice_effect_callback(client, fn, user_data)` for a host stage
`void fn(void* user_data, float* frame, uint32_t samples_per_channel, uint8_t channels)` run after
the built-ins (`NULL` removes it). The same chain is also usable standalone on any 48 kHz PCM —
`aurix_voice_effects_create/set/get/process_f32/reset/destroy` — which is how the Unity native
client runs it on its own capture. Unreal: `SetVoiceEffects` / `GetVoiceEffects` /
`MakeVoicePreset` / `SetVoiceEffectCallback`; Godot: `set_voice_effects(Dictionary)`.

## Visemes (lip-sync)

`client.set_visemes(true)` analyses every decoded participant stream and the outgoing voice
(after DSP, gain and effects) for lip-sync — one small FFT per stream per 20 ms — and
`client.participant_visemes(user_id)` / `client.local_visemes()` return the latest
`VisemeFrame { weights: [f32; 9], dominant, mouth_open, energy, confidence, sequence }`
(buckets `sil PP FF SS aa E ih oh ou`, see [Visemes](../features/speech.md#visemes-lip-sync)).
Participant frames are taken right after decoding (and, in E2EE channels, decrypting), before
the per-participant volume, mute and panning, so a quiet or distant speaker still moves their
mouth; a participant heard only through the server mix has none (`None`). Everything is local
— no audio or mouth data leaves the machine — and the analysers go with their streams (leave,
reconnect, `set_visemes(false)`). `aurix_client::visemes::VisemeAnalyzer` (`push`, `frame`,
`reset`) is public for hosts that decode elsewhere. C: `aurix_client_set_visemes`,
`aurix_client_visemes_enabled`, `aurix_client_participant_visemes(client, &user, &out)` /
`aurix_client_local_visemes` (return `false` when unknown), `AurixVisemeFrame`,
`AURIX_VISEME_COUNT`; standalone `aurix_viseme_analyzer_create/push_f32/frame/reset/destroy`.
Unreal: `SetVisemesEnabled` / `AreVisemesEnabled` / `GetParticipantVisemes` / `GetLocalVisemes`;
Godot: `set_visemes_enabled`, `get_participant_visemes`, `get_local_visemes`.

## Priority speakers and ducking

In a channel with `ducking` the node attenuates the other voices you receive while a priority
speaker talks ([Priority speakers and ducking](../features/channels.md#priority-speakers-and-ducking))
— nothing to do on the client. `client.set_priority(channel, Some(user) | None, bool)` promotes
or demotes (others need a moderator role; yourself a `priority` grant; `VALIDATION_ERROR` without
`ducking`), `client.is_priority(channel)` / `Participant::priority` / `channel_ducking(channel)`
read the state, and `Event::ParticipantPriorityChanged { channel_id, user_id, priority }` /
`Event::DuckingChanged { channel_id, active, config }` report transitions — the latter is the hook
for game audio: engage your own envelope on `active`, release on `!active`
(`client.ducking_active(channel)` for polling). C: `aurix_client_set_priority`,
`aurix_client_ducking_active`, `AURIX_EVENT_PARTICIPANT_PRIORITY_CHANGED` /
`AURIX_EVENT_DUCKING_CHANGED` with `aurix_event_ducking(event)` → `AurixDucking`; Unreal:
`SetPriority`, `IsDuckingActive`, `OnParticipantPriorityChanged`, `OnDuckingChanged`; Godot:
`set_priority`, `is_ducking_active` and the matching signals.

## Live translation

`SessionInfo.translation` (`Some { speech, languages }` on nodes with `[translation]`) says
whether the node translates transcripts; `client.set_translation(Some("de"), Some("en"), speech)`
asks for the captions this session receives in German while declaring English as the language
it speaks, and the server confirms with `Event::TranslationChanged` (normalised tags) or
`ServerError` (`VALIDATION_ERROR` unknown / unoffered tag, `TRANSLATION_DISABLED`). Translated
`Transcript`s carry `original: Some { text, language }`; segments already in the target language
or that the provider could not translate arrive as the original. With `speech` the translation
is also spoken privately to this session on the channel's translator SSRC (synthetic, no
participant behind it). The preference is replayed after reconnect and failover. C:
`AurixSessionInfo.translation` / `translation_speech`, `aurix_client_set_translation(client,
language, spoken_language, speech)`, `AURIX_EVENT_TRANSLATION_CHANGED` + `aurix_event_translation`,
`AurixTranscript.original_text` / `original_language` (`NULL` when untranslated). Unreal:
`SetTranslation`, `OnTranslationChanged`, `FAurixSessionInfo.bTranslation` /
`bTranslationSpeech`, `FAurixTranscript.OriginalText` / `OriginalLanguage`. See
[live translation](../features/speech.md#live-translation).

## PCMU / PCMA (G.711) fallback

`client.set_audio_codec(AudioCodec::Pcmu)` (or `AudioCodec::Pcma`) asks the server to run the
session on G.711 μ-law / A-law (8 kHz, 64 kbit/s, no Opus CPU); `Event::AudioCodecChanged(codec)`
confirms it and `client.audio_codec()` reports the acknowledged codec. The core does the rest:
capture pushed with `push_capture_*` is decimated to 8 kHz and companded in the negotiated law,
G.711 downlink frames (flagged `Pcmu` / `Pcma`) are decoded in the law the flag names and
upsampled into the same mixer as Opus streams, and after a fresh session the preferred codec is
negotiated again. In plaintext channels the node transcodes at the edge, so other participants
are unaffected; in an E2EE channel the core seals the G.711 frame like an Opus one and the node
relays it untouched with its codec flag — every member decodes it after opening
([codecs](../features/channels.md#codecs-opus-and-the-pcmu-fallback)).

## QUIC: 0-RTT resume and connection migration

When the node advertises it (`SessionInfo::media_quic`, `SessionInitAck.quic`) the core carries
the same sealed AURX packets as **QUIC datagrams** to the same media address
([protocol](../api/aurx.md#quic-aurx-datagrams-with-0-rtt-resume-and-connection-migration)).
What it buys a player over raw UDP:

* **Connection migration.** Call `client.network_changed()` when the OS reports a network
  change (Wi-Fi ↔ cellular, VPN up/down, laptop lid) — the core rebinds its socket and QUIC
  path validation moves the connection; the session, SSRC, media key, sequence counter, replay
  and E2EE state stay, nothing is re-negotiated and no `SessionBind` is sent
  (`Event::MediaPathChanged { path: Quic, reason: "network changed: QUIC migrated to …" }`).
  On raw UDP the same call re-sends `SessionBind` from the new address; on the tunnel it is a
  no-op. Migration also happens implicitly when a NAT rebinds the address mid-call.
* **0-RTT reconnect.** The TLS session ticket of a node is kept per node for the lifetime of
  the `Client` (`QuicClientState`), so after a control-channel reconnect the `SessionBind`
  travels as early data and media resumes one round trip after the socket is up
  (`reason: "QUIC bound (0-RTT)"`, `aurix_quic_handshakes_total{outcome="accepted_0rtt"}` on the
  node); a first connection to a node is a normal 1-RTT handshake (`"QUIC bound"`).
* **No head-of-line blocking** under loss, unlike the tunnel: each datagram is independent, the
  jitter buffer sees gaps rather than bursts and the FEC/DRED/PLC path
  ([packet loss](#packet-loss-fec-dred-and-the-neural-plc)) works exactly as on UDP.

Security is the AURX layer's, not TLS's: the core pins the node certificate hash received over
the authenticated control WebSocket (no CA, no system trust store — a self-signed node
certificate is fine), and the node attributes datagrams to a session only after the signed
`SessionBind` arrived on that very connection. `ClientConfig::quic = false` keeps `Auto` on the
pre-QUIC order (UDP → tunnel); `MediaPathPolicy::QuicOnly` refuses nodes that do not offer QUIC
and treats a blocked media port as a connection failure. A failed QUIC bind is remembered for
`udp_reprobe_interval` so the next attempts go straight to UDP, and `udp_fallback_lost_heartbeats`
unanswered heartbeats (or the connection closing) move a QUIC session to the tunnel exactly like
a UDP one; the tunnel re-probe tries QUIC first again. C: `AurixClientConfig.quic`,
`AURIX_MEDIA_PATH_QUIC_ONLY`, `AURIX_MEDIA_QUIC`, `AurixSessionInfo.media_quic`,
`aurix_client_network_changed`. Nodes without QUIC and clients built before it interoperate
unchanged.

## When UDP is blocked: the WebSocket tunnel

`ClientConfig::media_path` picks the link ([protocol](../api/aurx.md#tunnel-aurx-over-the-control-websocket)):

* `MediaPathPolicy::Auto` (default) — bind over [QUIC](#quic-0-rtt-resume-and-connection-migration)
  when offered, then raw UDP; if neither bind gets an answer, try the node's
  [dedicated TLS tunnel](#when-everything-but-443-is-blocked-the-tls-tunnel) when offered, and
  finally carry the media over the already-authenticated control WebSocket. While on a native link,
  `udp_fallback_lost_heartbeats` (3) unanswered heartbeats in a row move the session onto the
  tunnel mid-call; while tunnelled, every `udp_reprobe_interval` (30 s, `0` = never) the core
  binds natively again and moves back as soon as it answers. The node must advertise the tunnel
  (`SessionInfo::media_tunnel`); otherwise `Auto` behaves like `UdpOnly`.
* `MediaPathPolicy::UdpOnly` — never tunnel (a dead UDP path is a reconnect, as before).
* `MediaPathPolicy::TunnelOnly` — never open a UDP socket (tests, environments that forbid it).
* `MediaPathPolicy::QuicOnly` — QUIC or nothing.
* `MediaPathPolicy::TlsOnly` — the dedicated TLS tunnel or nothing.

The uplink sequence counter is shared by all links, so the server's replay window and the
receivers' jitter buffers see one continuous stream across a switch; a resume re-binds on the
link the policy selects. `Event::MediaPathChanged { path, reason }` fires after every bind and
switch (`"QUIC bound (0-RTT)"`, `"native bind failed: …"`, `"3 QUIC heartbeats unanswered"`,
`"UDP re-probe answered"`),
`client.media_path()` returns the current link and `ClientStats` adds `media_path`,
`heartbeats_lost_consecutive` and `uplink_dropped` (frames the tunnel's bounded send queue
refused — always 0 on UDP). C: `AurixClientConfig.media_path` (`AURIX_MEDIA_PATH_*`),
`udp_fallback_lost_heartbeats`, `udp_reprobe_interval_ms`, `aurix_client_media_path`,
`aurix_event_media_path`, `AurixSessionInfo.media_tunnel`.

Expect more latency on the tunnel (TCP retransmits stall everything behind a lost segment);
it is a way to stay in the call, not a replacement for UDP.

## When everything but 443 is blocked: the TLS tunnel

Some networks block UDP *and* the node's WebSocket port, leaving only 443/TCP. A node with
`media.tls_tunnel_port` set (`SessionInfo::media_tls`, `SessionInitAck.tls_tunnel`) offers a
[TLS 1.3 listener](../api/aurx.md#tls-tunnel-aurx-frames-on-a-dedicated-443-port) — normally on
443, or behind a TLS-passthrough Caddy/Traefik — that carries the same sealed AURX packets as
length-prefixed frames. The core pins the advertised certificate hash (the QUIC one; no CA, no
system trust store), requires ALPN `aurix-tunnel/1`, sends the signed `SessionBind` as the first
frame and keeps the shared sequence counter, so the switch is invisible to receivers.
`ClientConfig::tls_tunnel = false` removes it from `Auto`; `MediaPathPolicy::TlsOnly` refuses
nodes without it. A TLS connection the node closes falls to the WebSocket tunnel, a TLS session
re-probes UDP every `udp_reprobe_interval` and moves back, and a resumed session re-binds over
TLS. `Event::MediaPathChanged { path: Tls, .. }`, `ClientStats::media_path = "tls"`. C:
`AurixClientConfig.tls_tunnel`, `AURIX_MEDIA_PATH_TLS_ONLY`, `AURIX_MEDIA_TLS`,
`AurixSessionInfo.media_tls`. Same latency caveat as the WebSocket tunnel: TCP.

## Large channels: roles and the server mix

`Event::ChannelJoined { role, participant_count, hidden_listeners, .. }` tells the host what
kind of member it is ([large channels](../features/channels.md#large-channels-and-audiences)):
`client.channel_role(id)` / `can_speak_in(id)` (a `Listener` may keep pushing capture — the
node drops it), `participant_count(id)` (whole-fleet headcount, including listeners hidden from
the roster) and `channel_hidden_listeners(id)`.

`client.set_downlink_mode(DownlinkMode::Mixed)` asks the node for one server-mixed stereo
stream per channel instead of a stream per speaker; `Event::DownlinkModeChanged(mode)` confirms
it and `client.downlink_mode()` reports the acknowledged mode. Mixed frames (`PacketFlags::Mixed`,
the channel's synthetic SSRC) are decoded as stereo and played without panning, so the same
mixer output works in both modes; speakers with E2EE still arrive as separate streams. The
request is refused with `VALIDATION_ERROR` when the node has `media.downlink_mix = false`
(`SessionInfo::downlink_mix`); a fresh session reports `Streams` and re-applies the preferred
mode. C: `AurixChannelInfo` (`role`, `participant_count`, `hidden_listeners`, `transcription`,
`safety_voice`) via `aurix_client_channel_info` / `aurix_event_channel_info`,
`aurix_event_participant_count`, `aurix_client_set_downlink_mode` /
`aurix_client_downlink_mode`, `AURIX_EVENT_DOWNLINK_MODE_CHANGED` + `aurix_event_downlink_mode`,
`AurixSessionInfo.downlink_mix`; C++ `channel_info`, `can_speak_in`, `set_downlink_mode`,
`downlink_mode`; Unreal `GetChannelInfo`, `CanSpeakIn`, `SetDownlinkMode`, `GetDownlinkMode`,
`OnDownlinkModeChanged`.

## Server-side noise suppression

The capture DSP above is the default place for noise suppression; a client that cannot run it
(PCMU device, a build with `noise_suppression = false`, a bot feeding raw audio) can ask the
node to do it: `client.set_server_noise_suppression(true)` sends `SetNoiseSuppression`,
`Event::NoiseSuppressionChanged(enabled)` confirms what the node holds and
`client.server_noise_suppression()` reports it. `SessionInfo::noise_suppression` says whether
the node offers it (`media.noise_suppression.enabled`); the request fails with
`NOISE_SUPPRESSION_UNAVAILABLE` when it does not or all of its `max_sessions` are busy. The
preference is kept across reconnects — replayed after a resume/failover, re-sent for a fresh
session (the active state resets to `false` and the event fires again when the node confirms).
The node never cleans E2EE frames or frames into stereo channels; both DSPs on at once is
harmless but pointless ([server-side noise suppression](../features/channels.md#server-side-noise-suppression)).
C: `aurix_client_set_server_noise_suppression` / `aurix_client_server_noise_suppression`,
`AURIX_EVENT_SERVER_NOISE_SUPPRESSION_CHANGED` + `aurix_event_flag`,
`AurixSessionInfo.noise_suppression`; C++ `set_server_noise_suppression` /
`server_noise_suppression`; Unreal `SetServerNoiseSuppression` / `IsServerNoiseSuppressionEnabled`
/ `OnServerNoiseSuppressionChanged` / `FAurixSessionInfo.bNoiseSuppression`.

## End-to-end encryption

`ClientConfig::e2ee` (default `true`) announces the E2EE capability on connect and makes the
core seal every Opus frame it sends into an [`e2ee` channel](../features/e2ee.md) with its own
sender key, exchange wrapped keys with the members over the control WebSocket and open their
frames by SSRC before decoding — one encode and one seal per frame whatever the number of
channels. Rotation is automatic (peer joined / left / re-keyed, `2³¹` frames), reconnect and
failover keep identity, keys and peers; a session without the capability (`e2ee: false`, an old
build) is refused at `join` with `E2EE_REQUIRED`, and the core never sends plaintext into or
plays plaintext out of an encrypted channel.

```rust
let mut cfg = ClientConfig::new(ws_url, token);
cfg.e2ee_identity = load_identity();               // Option<[u8; 32]>: stable fingerprint across runs
let client = Client::new(cfg)?;
println!("my fingerprint: {}", client.e2ee_fingerprint());
match event {
    Event::E2eePeerKey { user_id, fingerprint, previous_fingerprint } => { /* show / compare */ }
    Event::E2eePeerDecryptable { user_id, decryptable } => {}
    Event::E2eeKeyRotated { generation } => {}
    _ => {}
}
client.e2ee_peer_fingerprint(user_id); client.e2ee_peer_decryptable(user_id);
let s = client.stats(); (s.frames_e2ee, s.e2ee_undecryptable);
```

C: `AurixClientConfig.e2ee`, `has_e2ee_identity` + `e2ee_identity[32]`,
`aurix_client_e2ee_fingerprint`, `aurix_client_e2ee_peer_fingerprint`,
`aurix_client_e2ee_peer_decryptable`, events `AURIX_EVENT_E2EE_PEER_KEY` /
`_PEER_DECRYPTABLE` / `_KEY_ROTATED`, `AurixClientStats.frames_e2ee` / `e2ee_undecryptable`;
the C++ wrapper and the Unreal subsystem (`FAurixClientConfig.bE2ee`, `E2eeIdentityHex`,
`GetE2eeFingerprint`, `OnE2eePeerKey`, …) mirror them. The mixed downlink and per-participant
pull work unchanged — encrypted speakers simply always arrive as separate streams.
`cargo run -p aurix-client --example e2ee_peer -- <ws-url> <jwt> <channel> [secs] [tone-hz]
[identity-hex]` is a headless peer that prints fingerprints and decrypted RMS for
interoperability tests with browsers and Unity.

## Presence and text range

`Event::ChannelJoined { scope, .. }` and `client.channel_scope(channel_id)` expose the
positional channel's `roster_radius` / `text_radius` (`ChannelScope`, `None` = the whole
channel). In a scoped channel `Event::ParticipantJoined` / `ParticipantLeft` fire as players
move in and out of the roster radius, not only on join/leave, and audio may arrive from an SSRC
the client has not been introduced to (the mixer plays it; the host names it when the roster
entry arrives). C: `AurixChannelScope` (a `0` radius means unscoped) via
`aurix_client_channel_scope` / `aurix_event_channel_scope`, C++ `Client::channel_scope`,
Unreal `GetChannelScope` ([radius-scoped presence](../features/channels.md#radius-scoped-presence-and-text)).
C: `aurix_client_set_audio_codec(client, AURIX_CODEC_PCMU | AURIX_CODEC_PCMA)`,
`aurix_client_audio_codec`, `AURIX_EVENT_AUDIO_CODEC_CHANGED` + `aurix_event_audio_codec`; C++
`set_audio_codec` / `audio_codec`; Unreal `SetAudioCodec(EAurixAudioCodec::Pcmu | Pcma)`, `GetAudioCodec`,
`OnAudioCodecChanged`. Fails with `CODEC_NOT_AVAILABLE` when the node sets
`media.pcmu_fallback = false`.

## Region selection

The core has no HTTP client, so region discovery is split: the host performs the HTTP requests,
the core parses, stores probe results and ranks — identically to the Web and Unity SDKs.

1. `aurix_regions_discovery_url(api_url, preferred_region | NULL, has_location, lat, lon, buf, cap)`
   → `GET` it with `Authorization: Bearer <player JWT>` (`/v1/me/regions`).
2. `aurix_regions_parse(json)` → opaque `AurixRegionList` (`aurix_regions_len` / `aurix_regions_get`
   fill `AurixRegionEndpoint`: `region`, `node_id`, `ws_url`, `probe_url`, coordinates,
   `distance_km`, `nodes`, `load_factor`).
3. Probe each `probe_url` (a few GETs, discard the first, keep the minimum) and record it with
   `aurix_regions_set_rtt(list, i, rtt_ms)` — `-1.0` marks an unreachable node.
4. `aurix_regions_rank(list, preferred_region | NULL, tolerance_ms)`: preferred region first
   unless its probe failed → RTT in `tolerance_ms` buckets (ties keep the server's distance/load
   order) → unprobed → failed (`probe_failed`).
5. Use entry 0's `ws_url` as the WebSocket URL; `aurix_regions_free` when done.

Rust: `aurix_client::regions::{discovery_url, parse_regions, rank_regions, ProbedRegion}`;
C++: `aurix::Regions` (RAII: `discovery_url`, `parse`, `set_rtt`, `rank`, `all`). Server side:
[Regions](../operations/scaling.md#regions).

## Unreal plugin

`sdk/unreal/AurixVoice` is a runtime plugin for UE 5.3+ (Win64 / Linux / Mac). It contains no
protocol, crypto or codec code: `UAurixVoiceSubsystem` (a `UGameInstanceSubsystem`) pumps native
events into Blueprint delegates on the game thread, feeds the engine `AudioCapture` microphone
into `aurix_client_push_capture_f32` and plays the remote mix through a procedural 48 kHz stereo
`USoundWave`.

### Build and install

```bash
sdk/unreal/scripts/build_native.sh                              # Linux/macOS host → lib/Linux|Mac + include/
sdk/unreal/scripts/build_native.sh --target aarch64-apple-darwin
sdk/unreal/scripts/build_native.sh /path/to/MyGame/Plugins/AurixVoice
```

```powershell
sdk\unreal\scripts\build_native.ps1                             # Windows → lib/Win64 + include/
sdk\unreal\scripts\build_native.ps1 -PluginDir C:\MyGame\Plugins\AurixVoice
```

A Windows DLL can also be cross-built from Linux with MinGW-w64
(`rustup target add x86_64-pc-windows-gnu`, `apt install gcc-mingw-w64-x86-64`, then
`cargo build --release -p aurix-client --target x86_64-pc-windows-gnu`); the repository's
`.cargo/config.toml` adds the `-lssp` link flag the bundled libopus needs there. The
`windows-gnu` DLL suits Unity and plain C hosts; for Unreal prefer the MSVC build above, which
also emits the `.lib` import library UBT links against.

Then copy or symlink `sdk/unreal/AurixVoice` to `<Project>/Plugins/AurixVoice`, regenerate
project files and build. The `AurixClientLibrary` ThirdParty module fails at UBT time with a
message naming the missing header/library, so an unstaged plugin never fails silently at
runtime; the library is copied next to the game binaries through `RuntimeDependencies`.

The plugin is laid out for `RunUAT BuildPlugin` / Fab: `Config/FilterPlugin.ini`,
`Resources/Icon128.png`, `Docs/`, docs/support URLs and `SupportedTargetPlatforms` in the
descriptor, and two runtime modules — `AurixVoice` (the API) and the optional
`AurixVoiceSamples` (Blueprint components on top of the public API only). Packaging steps and
the Fab checklist: `sdk/unreal/AurixVoice/Docs/Packaging.md`.

### Usage

Blueprint, sample components (`sdk/unreal/AurixVoice/Docs/QuickStart.md` has the node-by-node
walk-through): **Aurix Voice Lobby** on the PlayerController — `Web Socket Url`, optional
`Channel Id` / `Push To Talk`, `Connect With Token` with the backend-minted token; it joins on
session ready and emits a UI-ready roster (`On Roster Changed`), status (`On Status Changed`:
state, media path, endpoint, quality bars, MOS, RTT), chat lines and errors; input goes to
`Set Push To Talk Pressed` / `Toggle Microphone Muted`, listener-side preferences to
`Set Participant Muted Locally` / `Set Participant Volume Locally`. **Aurix Proximity Voice**
on the local pawn — `Set Channel` after the join; it reports the pose when the pawn moved, and
`Attach Participant Voice(UserId, AvatarHead)` puts a talker's voice on their avatar through
Unreal attenuation/spatialization (the talker leaves the 2D mix while attached). **Aurix Voice
Blueprint Library** — `Get Aurix Voice`, `Make Voice Settings`, state/media-path/bars/MOS to
text, `Guid To Uuid`.

Blueprint, subsystem directly: *Get Game Instance Subsystem → Aurix Voice Subsystem*, build *Aurix Voice Settings*
(WebSocket URL, token from your backend), **Connect**; on *On Session Ready* call **Join
Channel** (`Parse Uuid`, join token empty unless `require_action_tokens`); bind *On Channel
Joined*, *On Participant Joined/Left*, *On Participant Speaking*, *On Channel Energy*, *On Chat
Message*, *On Recovering/Recovered/Failed To Recover*; for positional channels call **Update Own
Position** from tick (`WorldToMeters` converts Unreal units).

```cpp
UAurixVoiceSubsystem* Voice = GetGameInstance()->GetSubsystem<UAurixVoiceSubsystem>();
Voice->OnSessionReady.AddDynamic(this, &AMyPC::OnVoiceReady);
Voice->OnParticipantSpeaking.AddDynamic(this, &AMyPC::OnSpeaking);

FAurixVoiceSettings Settings;
Settings.WebSocketUrl = TEXT("wss://voice.example.com/ws");
Settings.Token = Jwt;          // minted by your backend; deliberately not an editable property
Settings.bVadGate = true;
Voice->Connect(Settings);
```

Audio options: default engine capture + 2D `UAudioComponent` (`PlaybackSoundClass` routes it
through your mixer/ducking); or `PushCaptureAudio` from your own capture path and
`MixOutputAudio` from your own procedural sound/submix — both audio-thread safe until
`Disconnect()`. For engine spatialization, `CreateParticipantSound(UserId)` /
`SpawnParticipantAudioComponent(UserId, HeadComponent, Attenuation)` give each talker its own
mono `UAurixParticipantSoundWave` (no local panning; the user is claimed so the 2D mix skips
it — unclaimed talkers stay 2D), spatialized by Unreal's attenuation / spatializer plugin /
occlusion / reverb like any in-world sound; `ReleaseParticipantSound`, `GetParticipantSound`,
`GetParticipantStreams`, and `SetParticipantClaimed` + `PullParticipantAudio` for a custom
graph. Details in `sdk/unreal/README.md` ("Audio integration options").

Capture processing: `FAurixVoiceSettings.Dsp` (`FAurixDspSettings`: high-pass, echo
cancellation + tail, noise suppression strength, AGC target/max gain) is applied by the core;
`SetDspSettings` / `GetDspSettings` change it live and `GetDspStats` (`FAurixDspStats`) feeds a
diagnostics overlay. The echo canceller already hears what `MixOutputAudio` and the plugin's
sound wave play; call `PushRenderAudio` with any other speaker audio (game mix, music) so it
can cancel that too.

Opus: `FAurixVoiceSettings.Encoder` (`FAurixEncoderSettings`: bitrate, complexity, max
bandwidth, signal, VBR/constrained VBR, FEC, expected loss, DTX, `bStereo`, `DredDurationMs`),
`Decoder` (`FAurixDecoderSettings`: `Complexity`, `bOsceBwe`), `LossAdaptation`
(`EAurixLossAdaptation`) and `bFollowChannelPolicy`;
at runtime `SetEncoderSettings`, `GetEncoderSettings`, `SetComplexity` (pin, `-1` un-pins),
`GetAudioPolicy` (`FAurixAudioPolicy`, with `bStereo`), `SetLossAdaptation` / `GetLossAdaptation`
/ `GetLossProfile` (`EAurixLossProfile`), `SetDecoderSettings` / `GetDecoderSettings`,
`IsDredSupported` and the `OnAudioPolicyChanged` / `OnBitrateChanged` / `OnLossProfileChanged`
delegates — the same layering as the Rust API above.

`GetStats(FAurixStats&)`, `GetNetworkQuality(FAurixNetworkQuality&)` and `OnNetworkQuality`
expose the shared quality model; `OnRawEvent` delivers every event as JSON for anything without
a typed delegate. Events are dispatched from the subsystem tick, up to 256 per tick, nothing
dropped.

Media links: `FAurixVoiceSettings.MediaPath` (`EAurixMediaPathPolicy` Auto / UdpOnly /
TunnelOnly / QuicOnly), `bQuic`, `UdpFallbackLostHeartbeats`, `UdpReprobeIntervalMs`;
`GetMediaPath()` (`EAurixMediaPath`, includes `Quic`), `OnMediaPathChanged(Path, Reason)`,
`NetworkChanged()` (Blueprint-callable; wire it to your platform's connectivity notification
for [QUIC migration](#quic-0-rtt-resume-and-connection-migration)), `FAurixSessionInfo.bMediaTunnel`
/ `bMediaQuic` and `FAurixStats.MediaPath` — the core's
[tunnel behaviour](#when-udp-is-blocked-the-websocket-tunnel) unchanged.

**Regions.** `DiscoverRegions(FAurixRegionDiscoveryRequest, OnComplete)` runs the whole flow
above with the engine's `HTTP` module (bearer `GET /v1/me/regions`, optional probes with
`ProbeSamples` / `ProbeTimeoutSeconds`, native ranking) and delivers a best-first
`TArray<FAurixRegionEndpoint>` to a Blueprint delegate; put `Regions[0].WsUrl` into
`FAurixVoiceSettings.WebSocketUrl`. `CancelRegionDiscovery()` drops an in-flight request; a new
`DiscoverRegions` cancels the previous one.

### Verification status

Verified in CI: the native library builds with statically linked Opus on Linux x64, Windows x64
(MSVC), macOS arm64 and macOS x64 — the `native core` job runs the crate's unit tests, the
header-drift check and the C/C++ samples against the freshly built library on each host, stages
`lib/Win64` / `lib/Mac` with `build_native.ps1` / `build_native.sh` and uploads them as the
`aurix-client-<target>` artifacts — and
`unreal_plugin_uses_only_existing_abi` parses the plugin sources and checks that every
`aurix_*` function, `AURIX_*` constant and `aurix::Client` method they use is declared in the
committed headers (and that `AurixVoiceSamples` never reaches the C ABI). The `unreal` job runs
`sdk/unreal/scripts/check_plugin.py` — descriptor schema/URLs/version parity, packaging files,
`.generated.h` / `GENERATED_BODY` / `*_API` / `UFUNCTION`-handler conventions, module dependency
direction — and, only when the repository has `UE_GHCR_TOKEN` + `UE_GHCR_USER` from an
Epic-linked GitHub account, `RunUAT BuildPlugin -TargetPlatforms=Linux -Rocket -StrictIncludes`
inside Epic's `ghcr.io/epicgames/unreal-engine:dev-slim-5.3.2` image with the packaged plugin
as an artifact; without the secrets it prints a notice and does **not** claim a compile.
**Not** verified here: Unreal Header Tool and the module compile against a live
UE 5.3+ install (`AudioCaptureCore` callback signature, `USoundWaveProcedural::GeneratePCMData`)
, `BuildPlugin` itself and packaging of a game on Windows/macOS — treat the first build in your project as a required
step; any
mismatch surfaces as a compile error in `AurixAudioCapture.cpp`, `AurixVoiceSoundWave.cpp` or
the sample components.
