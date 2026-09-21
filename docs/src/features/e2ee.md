# End-to-end encryption

Channels created with `"config": {"e2ee": true}` carry voice the node **cannot hear**. Every
participant encrypts its own Opus frames with a *sender key* that only the other members hold;
the node authenticates who is in the channel, relays the key exchange and forwards the sealed
frames — nothing more. Transport encryption (AURX session keys, DTLS-SRTP, the cascade relay
envelope) stays in place underneath; E2EE is an additional layer between the microphone and the
speaker.

Use it for content the operator must not be able to reconstruct even with full access to the
fleet: guild officer channels, private parties, anything under a "we cannot listen" promise.
Everything that needs the node to understand the audio is unavailable in such a channel (see
[What the server cannot do](#what-the-server-cannot-do)).

## Setup

```bash
curl -X POST http://localhost:8080/v1/channels -H "Authorization: Bearer $API_KEY" \
  -H 'content-type: application/json' \
  -d '{"name": "officers", "config": {"e2ee": true}}'
```

`e2ee` is refused together with `recording_enabled`, `transcription`, `safety_voice`,
`ambient`, `audience.mix_for_listeners` and the `echo` channel type (`VALIDATION_ERROR`), and
`POST /v1/recordings/start` on such a channel answers `400`. `ChannelJoinAck.audio.e2ee`
discloses the setting to clients; `ChannelAudioPolicy` follows edits.

Clients need nothing beyond an SDK with E2EE support, which is on by default:

| SDK | switch | identity persistence |
| --- | --- | --- |
| Web (`AurixClient`) | `e2ee: true \| false \| {identity, transform, workerUrl}` | `e2eeIdentitySecret` → `e2ee.identity` |
| Unity native (`AurixVoiceClient`) | `E2ee` | `ExportE2eeIdentity()` → `SetE2eeIdentity()` |
| Unity WebGL (`AurixWebGLVoiceClient`) | `E2ee` / `E2eeIdentity` in the inspector | `ExportE2eeIdentity()` |
| Native core (Rust / C ABI) | `ClientConfig::e2ee`, `AurixClientConfig.e2ee` | `e2ee_identity` / `has_e2ee_identity` |
| Unreal (`UAurixVoiceSubsystem`) | `FAurixClientConfig.bE2ee` | `E2eeIdentityHex` |
| Godot (`AurixVoiceClient`) | always on (core default); fingerprints / identity persistence not bound to GDScript yet | — |

On `connect()` an SDK that can encrypt announces the capability (`E2eeHello` without a
channel). A session that did not — an old SDK, a browser without the encoded-frame API,
`e2ee: false` — is refused at the door of every encrypted channel with **`E2EE_REQUIRED`**.
There is no plaintext fallback in either direction: the node drops plaintext frames sent into
an encrypted channel, and an SDK never plays plaintext arriving from one.

## How it works

Each client holds an **X25519 identity key** (random per session unless the application
persists the 32-byte secret; its SHA-256 **fingerprint** is what peers see) and a 32-byte
**sender key** it encrypts its own frames with. The sender key is per *sender*, not per
channel: a native client encodes and seals every frame once for all the encrypted channels it
transmits into.

```text
frame  = generation(1) | counter(4, BE) | AES-256-CTR(enc, IV, opus) | HMAC-SHA256(auth, header|ct)[..10]
IV     = (salt(12) XOR (generation | counter | 0*7)) | 0*4
enc    = HKDF-SHA256(secret, "aurix-e2ee-v1 enc", 32)
auth   = HKDF-SHA256(secret, "aurix-e2ee-v1 auth", 32)
salt   = HKDF-SHA256(secret, "aurix-e2ee-v1 salt", 12)

wrap   = nonce(12) | AES-256-CTR(wk_enc, nonce|0*4, secret) | HMAC-SHA256(wk_auth, generation|nonce|ct)[..16]
shared = X25519(sender_sk, recipient_pk)               -- low-order points are rejected
wk_*   = HKDF-SHA256(shared, salt = sender_pk|recipient_pk, "aurix-e2ee-v1 wrap enc" / "… wrap auth", 32)
```

Key distribution rides on the control WebSocket ([messages](../api/websocket.md#messages-by-purpose)):

1. `E2eeHello { public_key }` on connect announces the capability; `E2eeHello { channel_id,
   public_key }` after a join says "I am in this channel" and is relayed to every other member.
2. Members answer with `E2eeSenderKey { channel_id, to, public_key, generation, key }` — their
   current sender key wrapped for exactly that recipient. The node relays it to `to` only and
   stamps `from`; it checks that sender and recipient are members of the channel, that the
   channel belongs to the caller's application and is encrypted, that the key material is
   well-formed, and it rate-limits the exchange (`rate_limiting.e2ee_messages_per_minute`,
   3000 by default; a rotation costs one message per peer). Anything else is answered with
   `AUTH_DENIED` / `VALIDATION_ERROR` and relayed nowhere.
3. **Rotation.** A sender picks a fresh secret (generation + 1) whenever a peer joins — so the
   newcomer cannot read earlier frames — or leaves, so the leaver cannot read later ones;
   also when a peer shows up with a different identity key, after 2³¹ frames, and on
   `rotateE2eeKey()` / `RotateE2eeKey()`. Receivers keep the last four generations of every
   peer so frames in flight across a rotation still decrypt. The new key travels on the
   control plane while media takes UDP/QUIC, so a receiver may see a frame before its key:
   the native core parks such frames (up to 25 per sender, 500 ms) and decrypts them when the
   key lands; frames that wait longer count as lost.
4. Every frame is checked against a per-generation replay window; tampered, replayed, stale or
   unknown-generation frames are dropped (`e2ee_undecryptable` in the client statistics) and
   never reach the speaker.

Uplink frames carry the AURX `E2ee` flag (`SendAudioE2ee`); the node forwards them under the
sender's SSRC to receivers that hold the capability, flags them on the way out and never mixes
them, so native listeners keep per-speaker streams even in `mixed` downlink mode. Browsers only
hear members of an encrypted channel on their [per-participant tracks](channels.md#per-participant-tracks-for-browsers)
(the mixed track stays silent; a WebRTC session with `participantStreams = 0` is warned that
the channel is inaudible), and one WebRTC session cannot join encrypted and plaintext channels
at the same time (`E2EE_MIXED_CHANNELS`) because its transforms run on whole tracks.

Reconnects keep the state: a session resumed within the grace period (also on another node)
keeps its identity, sender key and peers, re-announces itself and only drops peers that left
meanwhile; a fresh session keeps the identity key but starts from an empty group.

### Trust model

Identity keys are authenticated by the platform's control plane: the node vouches that the key
announced under `user_id` came from that user's authenticated session. This protects against
anyone *other than the operator* — other players, the network, a leaked recording bucket, a
compromised STT provider — and against the operator's honest infrastructure (nothing on the node
can decode the audio, so a subpoena or a breach yields ciphertext). A **malicious node** could
still substitute identity keys during the exchange; applications that need protection against
their own operator show the fingerprints (`e2eeFingerprint`, `e2eePeerFingerprint`,
`OnE2eePeerKey` with the previous fingerprint on a change) and let players compare them out of
band, exactly like messenger safety numbers, and persist the identity secret so the fingerprint
stays stable across sessions.

## Browser support

The Web SDK encrypts inside WebRTC with WebCrypto and an encoded-frame transform; both must be
present or `AurixClient.e2eeSupport()` reports `ok: false` and `connect()` proceeds without the
capability (encrypted channels then fail with `E2EE_REQUIRED` — never a silent downgrade).

| browser | encoded-frame API | E2EE |
| --- | --- | --- |
| Chrome / Edge / Opera (Chromium ≥ 86) | `createEncodedStreams()` (`'streams'`), `RTCRtpScriptTransform` where shipped (`'script'`) | yes |
| Firefox ≥ 117 | `RTCRtpScriptTransform` | yes |
| Safari ≥ 15.4 / iOS 15.4 | `RTCRtpScriptTransform` | yes |
| Anything without either API | — | no: `E2EE_REQUIRED` |

`'script'` runs the cipher in a worker created from a `blob:` URL — pass `e2ee.workerUrl`
(serving `e2eeWorkerSource`) under a CSP without `worker-src blob:`. The matrix above is API
availability from vendor documentation; this repository's tests drive both transform kinds
against a fake WebRTC stack, and the `'streams'` path was verified live in Chrome against real
nodes (browser ↔ browser and browser ↔ native). Firefox and Safari were not run
([limitations](../limitations.md)).

## What the server cannot do

Because the node never sees plaintext, an encrypted channel has **no**:

* server mix (`downlink: mixed`, `audience.mix_for_listeners`, `ambient`) — listeners receive
  one stream per speaker; top-N stream caps and roles still apply, they rank by the
  sender-reported level byte, which stays outside the ciphertext;
* recording, mixdown, live audio streams, transcription, live translation, content safety on
  voice, and any server-synthesised audio addressed to the channel (`TtsSpeak` naming it,
  whatever the destination, `POST /v1/channels/{id}/tts` announcements, spoken translations —
  `VALIDATION_ERROR`);
* PCMU transcoding — PCMU sessions hear nothing in an encrypted channel and their μ-law uplink
  is dropped;
* server-side directional metadata (`Directional` bytes): positional attenuation is still
  applied to the level/gain byte, and browsers spatialise their per-participant tracks locally.

Presence, speaking/energy events (driven by the level byte), mute/kick moderation, chat and
chat filters, quotas and analytics work as in any other channel.

## Verifying it

`crates/aurix-common/src/e2ee.rs`, `sdk/web/src/e2ee.ts` and `sdk/unity/Runtime/Protocol/E2ee.cs`
are three implementations of the same format and share test vectors (frame, wrapped key,
HKDF/X25519/fingerprint); each one seals what the others open. The live end-to-end suites
(`crates/aurix-client/tests/e2e_live.rs`, the Unity `--scenario e2ee` demo, the Web SDK's
`e2ee-client` tests) cover join/leave rotation, reconnect/resume, a non-capable session being
refused, plaintext being dropped in both directions, undecryptable-frame accounting and the
relay's authorisation checks; `cargo run -p aurix-client --example e2ee_peer` is a headless
native peer for interoperability sessions with a browser or Unity.
