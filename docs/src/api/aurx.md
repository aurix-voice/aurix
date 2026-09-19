# Native AURX media (protocol v2)

AURX is the UDP media protocol spoken by the Unity SDK, the native `aurix-client` core (and thus
Unreal) and the load-test tool. It carries 20 ms Opus frames with a 30-byte header, encrypted and
authenticated per packet with keys derived from the session's `media_key`. WebRTC clients share
the same UDP port; the server tells the two apart by the first bytes.

Normative source: `crates/aurix-common/src/protocol.rs` (`PacketHeader`, `PacketFlags`,
`AurixPacket::seal/open`) and `crates/aurix-common/src/crypto.rs` (`MediaKeys`). The Rust and C#
implementations are pinned to the same wire test vectors.

## Packet layout

```
 0      4     5     6      8         12        16        20            24        26        30
 ┌──────┬─────┬─────┬──────┬─────────┬─────────┬─────────┬─────────────┬─────────┬─────────┐
 │ AURX │ ver │type │flags │sequence │timestamp│  ssrc   │channel hash │ len     │ crc32   │
 │magic │ =2  │     │ u16  │  u32    │  u32    │  u32    │    u32      │ u16     │  u32    │
 └──────┴─────┴─────┴──────┴─────────┴─────────┴─────────┴─────────────┴─────────┴─────────┘
 header (30 bytes, big-endian) | payload (ciphertext) | HMAC tag (16 bytes)
```

* `MAGIC_BYTES = "AURX"`, `PROTOCOL_VERSION = 2`, `MAX_PACKET_SIZE = 1400`.
* `crc32` covers the (cipher-text) payload so a plain decode can validate integrity before keys
  are touched; `len` is the payload length.
* `channel hash` selects the channel for `Audio` and `PositionUpdate` packets (`channel_hash`
  in `ChannelJoinAck`/SDK helpers); the server maps it back to the channel id.

### Packet types

| Type | Value | Direction | Payload |
| --- | --- | --- | --- |
| `Audio` | `0x01` | both | [`level`] Opus frame (uplink) / [`gain`] [`direction`] Opus frame (downlink) |
| `AudioFec` | `0x02` | both | reserved |
| `Control` | `0x10` | both | reserved |
| `Heartbeat` / `HeartbeatAck` | `0x20` / `0x21` | client → server / server → client | empty; the header timestamp is echoed in the ack (RTT) |
| `SessionBind` / `SessionBindAck` | `0x33` / `0x34` | client → server / server → client | `session_id(16) | unix_ms(8) | nonce(8)` / `unix_ms(8)` |
| `MuteState` | `0x60` | both | mute flag |
| `SpeakingState` | `0x61` | server → client | speaking flag (WebSocket `SpeakingStateChanged` is the primary channel) |
| `QualityReport` / `BitrateCommand` | `0x70` / `0x71` | client → server / server → client | see [Network quality](../features/quality.md) |
| `Relay` | `0x80` | node ↔ node | cascade envelope, never accepted from clients |
| `Error` | `0xFF` | server → client | code + message |

`SessionInit`/`SessionInitAck`/`SessionClose`/`ChannelJoin`/`ChannelJoinAck`/`ChannelLeave` also
have packet-type values but the WebSocket control plane is used for them.

### Flags

| Flag | Bit | Meaning |
| --- | --- | --- |
| `Encrypted` | `0x0001` | payload is AES-256-CTR encrypted (always set by `seal`) |
| `Dtx` | `0x0004` | discontinuous transmission (comfort-noise / silence) |
| `Relay` | `0x0040` | packet crossed a cascade hop |
| `VolumeAttenuated` | `0x0080` | downlink payload starts with a gain byte (`128` = unity, `255` ≈ 2.0) |
| `E2ee` | `0x0100` | client end-to-end encrypted frame — forwarded untouched, never decoded |
| `Rtp` | `0x0200` | payload is an RTP packet (server-side bridging) |
| `Authenticated` | `0x0400` | 16-byte HMAC tag follows the payload |
| `Energy` | `0x0800` | uplink payload starts with an RFC 6464 `-dBov` level byte (`127` = silence); stripped before fan-out |
| `Directional` | `0x1000` | downlink payload carries 2 signed bytes (azimuth in π/127, elevation in π/254 units) after the gain byte |
| `Pcmu` | `0x2000` | the audio frame is G.711 μ-law, not Opus — only on sessions that negotiated `SetAudioCodec {codec: "pcmu"}` ([codecs](../features/channels.md#codecs-opus-and-the-pcmu-fallback)); the server sets it on the downlink copies sent to such sessions |

## Keys and sealing

```
master = base64-decode(SessionInitAck.media_key)              # 32 bytes
auth   = HMAC-SHA256(master, "AURXv2 auth")
enc    = HMAC-SHA256(master, "AURXv2 enc")                    # AES-256 key
salt   = HMAC-SHA256(master, "AURXv2 salt")[0..16]

iv     = salt XOR ( type(1) | 0(1) | ssrc(4) | seq(4) | ts(4) | 0(2) )
wire   = header(30) | AES-256-CTR(enc, iv, payload) | HMAC-SHA256(auth, header | ciphertext)[0..16]
```

The trailing 16 zero bits of the IV are the CTR block counter, so a sender must never reuse
`(type, ssrc, sequence, timestamp)` under one key — a single monotonic sequence counter per
session satisfies this, and the server's anti-replay window requires it anyway. Downlink packets
are sealed per receiver with *that receiver's* keys; the server never forwards a packet it
cannot authenticate, and heartbeat/control packets use a separate sequence space from audio so
jitter buffers do not see phantom losses.

## Lifecycle

1. Obtain `session_id`, `ssrc`, `media_addr`, `media_key` from `SessionInitAck` over the
   WebSocket.
2. Send `SessionBind` (`AurixPacket::session_bind(session_id, ssrc, unix_ms, nonce)`) encoded with
   `encode_authenticated(keys)` — signed but **not** encrypted, because the server needs the
   session id to find the key. The timestamp must be within `SESSION_BIND_MAX_SKEW_MS` (30 s) of
   server time; the nonce is replay-protected. Retry with backoff until `SessionBindAck` arrives
   (the WebSocket additionally delivers `MediaBound`).
3. Join channels over the WebSocket, then send `Audio` packets (`seal`) with the channel hash,
   incrementing `sequence` per packet and `timestamp` by 960 per 20 ms frame (48 kHz). Set
   `Energy` and prefix the level byte if you measured the microphone level; set `Dtx` for
   silence frames.
4. Send `Heartbeat` every `media.heartbeat_interval_ms` (5 s); missing acks mean the path is
   dead — trigger a reconnect (the SDKs do this).
5. On receive: `open(keys)` (verify tag, decrypt), check the replay window per sender SSRC, strip
   the gain byte (`VolumeAttenuated`) and direction bytes (`Directional`), apply the gain, hand the
   Opus frame to the per-sender jitter buffer, mix with panning. The sender is identified by
   `ssrc` → `ParticipantJoined.ssrc`; the top bit marks server-synthesised streams (TTS).
6. After a resume, re-send `SessionBind` from the current UDP socket (the address may have
   changed) and continue the sequence counter — do not restart it.

## What the server does with your packets

* Drops anything from an unbound address, with a bad tag, wrong SSRC, protocol version ≠ 2,
  or outside the replay window (counted in `aurix_packets_dropped_total`; clients count their
  own `bad_auth` / `replayed` in the [statistics](../features/quality.md)).
* Applies moderator mute, transmission mode, channel membership, then per receiver: local mute,
  block, per-participant volume, channel focus, positional attenuation/direction — and re-seals.
* Feeds the decoded frame to recording, live streams and transcription **only** when the
  channel/consent rules allow and the frame is not `E2ee`.
* Estimates uplink quality from sequence gaps/jitter for [network quality](../features/quality.md).
* On a session that negotiated PCMU: decodes the μ-law uplink and re-encodes it as narrowband
  Opus before any of the above, so the rest of the channel is unaffected; encodes the Opus it
  would have sent to that session as μ-law after the per-receiver step (gain/direction bytes and
  the seal are the same as for Opus). PCMU frames from a session that did not negotiate and
  `Pcmu | E2ee` frames are dropped (`aurix_packets_dropped_total`); frames of a length other
  than 80/160/320/480 bytes fail the transcode (`aurix_pcmu_frames_total{outcome="error"}`).

## Reference implementations

* Rust: `crates/aurix-client/src/media.rs` (client), `crates/aurix-media/src/router.rs` (server).
* C#: `sdk/unity/Runtime/Protocol/*.cs` — same test vectors as Rust in
  `sdk/unity/DotNet/Aurix.Voice.Tests`.
