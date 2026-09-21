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
| `Mixed` | `0x4000` | downlink only: the frame is the server's stereo mix of a whole channel for this receiver (`SetDownlinkMode {mode: "mixed"}`, [server mix](../features/channels.md#server-mix-for-native-clients)), under the channel's synthetic mix SSRC with its own sequence; per-receiver gains are baked in — never combined with `E2ee` or `Directional`, combined with `Pcmu` for PCMU sessions |
| `RelayHop` | `0x8000` | `Relay` envelopes only: a hop byte (`0x80 \| hops`) follows the 16-byte sender id, so a [relay-tree](../operations/scaling.md#cascade-sfu-to-sfu-relay) hub can re-forward the envelope; hops are capped at 3 and a receiver never re-forwards an envelope without it |

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
   `ssrc` → `ParticipantJoined.ssrc`; the top bit marks server-synthesised streams (TTS,
   channel mixes). A `Mixed` frame is stereo Opus (decode with a 2-channel decoder, downmix if
   you play mono) and must not be panned again.
6. After a resume, re-send `SessionBind` from the current UDP socket (the address may have
   changed) and continue the sequence counter — do not restart it.

## Tunnel: AURX over the control WebSocket

When UDP is blocked (corporate NAT, hotel Wi-Fi, some carriers) a native session can carry the
same packets over its authenticated control WebSocket instead. Nothing about the packets
changes: **one sealed AURX packet per binary WebSocket frame**, both directions; text frames
remain the JSON control plane. The node advertises support with `SessionInitAck.media_tunnel`
(`media.media_tunnel`, on by default) and reports the active link in `MediaBound.transport`
(`udp` | `tunnel`) and in `GET /v1/sessions/{id}/stats`.

* **Bind.** Send the signed `SessionBind` as a binary frame; the ack comes back as a binary
  frame (and `MediaBound` over text). The session id inside the bind must be the one this
  WebSocket authenticated — a tunnel is owned by exactly one connection and one session, so the
  "source address" the server attributes uplink packets to is the connection itself. A bind with
  a timestamp newer than the current UDP binding moves the session onto the tunnel; a later UDP
  `SessionBind` moves it back (the SDKs use this to return to UDP once it answers again).
* **Same checks.** Tag, SSRC, protocol version, replay window and sequence continuity are
  verified exactly as for UDP, so keep the **same sequence counter** across path changes; a
  frame larger than `MAX_PACKET_SIZE` is rejected before decoding.
* **Downlink.** Packets for a tunnelled receiver are sealed per receiver as usual and queued
  behind that session's WebSocket (`media.tunnel_queue_packets`, default 128 ≈ 2.5 s of one
  speaker); when the queue is full *that* receiver's packets are dropped
  (`aurix_tunnel_packets_total{direction="downlink",outcome="dropped"}`) — a stalled TCP
  connection never blocks the SFU or other participants.
* **Lifecycle.** A reconnected/resumed WebSocket must bind again (the old tunnel dies with the
  socket); a second bind on the same socket replaces the first; the tunnel is released when the
  session ends. Heartbeats work over the tunnel too, and their RTT includes TCP.
* **Cost.** TCP retransmission means head-of-line blocking under loss: the jitter buffer sees
  bursts instead of gaps, latency rises. Treat the tunnel as a fallback — every SDK tries UDP
  first, falls back after a failed bind or `udp_fallback_lost_heartbeats` unanswered heartbeats,
  and re-probes UDP every `udp_reprobe_interval` while tunnelled. WebRTC clients are unaffected
  (they have ICE/TURN for the same problem).

Metrics: `aurix_tunnel_sessions`, `aurix_tunnel_packets_total{direction,outcome}`.

## QUIC: AURX datagrams with 0-RTT resume and connection migration

Native clients may also carry AURX over **QUIC** (RFC 9000, DATAGRAM extension RFC 9221) to
the very same media address: the node speaks QUIC, raw AURX and WebRTC on one UDP socket and
classifies packets by their first byte (server-chosen connection ids start with a byte ≥ 0x80,
so a QUIC short-header packet can never spell the AURX magic). Nothing about the packets
changes — **one sealed AURX packet per QUIC DATAGRAM frame**, both directions, no streams
(`max_concurrent_*_streams = 0`), so a lost datagram never holds up the next one. The node
advertises the path in `SessionInitAck.quic { cert_sha256, server_name }` (`media.quic`, on by
default) and reports it as `MediaBound.transport = "quic"` / `GET /v1/sessions/{id}/stats`.

* **Handshake.** TLS 1.3, ALPN `aurix-media/1`, SNI `server_name` (informational). Clients **pin**
  `cert_sha256` (SHA-256 of the DER certificate) that arrived over the authenticated control
  WebSocket and ignore the CA chain — the node's self-signed certificate (generated at start
  unless `media.quic_cert_path`/`quic_key_path` point at a PEM pair) is exactly as secure as an
  issued one, and no CA can impersonate a node. Session tickets let a client that already
  talked to the node resume with **0-RTT**: the `SessionBind` travels as early data and its
  ack comes back with the handshake, so a reconnect costs one round trip in total.
* **Bind and ownership.** TLS identity is *not* trusted for session ownership. A connection
  speaks for a session only after an authenticated `SessionBind` (same signature, same
  `SESSION_BIND_MAX_SKEW_MS` window, timestamp strictly newer than the session's last bind)
  arrived on it; a connection is owned by exactly one session for its lifetime and a bind for
  another session on the same connection is refused. Later datagrams are attributed to the
  connection the session is bound to — never to the source address, which migration changes.
  The newest bind wins across links: a bind over QUIC moves the session off UDP/tunnel and a
  later UDP/tunnel bind moves it back, closing the superseded connection; a stale connection
  (one the session already left) can neither deliver media nor reclaim the session without a
  fresh, newer bind.
* **Same checks.** Tag, SSRC, protocol version, replay window and sequence continuity are
  verified exactly as for UDP, so keep the **same sequence counter** across path changes; the
  E2EE payload stays opaque to the node; PCMU and mixed downlinks work unchanged.
* **0-RTT is replayable** at the transport layer, which is why nothing new is derived from
  early data: a replayed early `SessionBind` fails the strictly-newer-timestamp rule, replayed
  media/heartbeats fall into the per-session anti-replay window. `media.quic_zero_rtt = false`
  forces a 1-RTT handshake before any datagram is read.
* **Migration.** When the client's address changes (Wi-Fi ↔ cellular, NAT rebinding) QUIC path
  validation moves the connection and the session keeps its id, key, sequence counter, replay
  and E2EE state — no re-bind, no audible gap beyond the path's own RTT
  (`aurix_quic_migrations_total`). `media.quic_migration = false` drops migrating connections
  instead (the client then re-binds like a UDP client).
* **Downlink.** Sealed per receiver as usual and queued per connection
  (`media.quic_queue_packets`, 128); a congested path drops *its own* packets
  (`aurix_quic_packets_total{direction="downlink",outcome="dropped"}`), never anyone else's.
* **Lifecycle.** Idle timeout `media.quic_idle_timeout_ms` (20 s, ≥ 3 heartbeats); connections
  above `media.quic_max_connections` (default 2 × `max_participants_per_node`) are refused
  during the handshake (`aurix_quic_handshakes_total{outcome="refused"}`); the connection is
  closed when its session ends or is superseded.
* **Cost.** One TLS handshake per fresh connection (none on 0-RTT resume) and QUIC framing
  (≈ 1 % of the audio bitrate). Every SDK tries QUIC first, then raw UDP, then the tunnel
  ([native SDK](../sdk/native.md#when-udp-is-blocked-the-websocket-tunnel)); nodes without
  `quic` in their `SessionInitAck` and clients that never learned QUIC interoperate as before.
  WebRTC clients are unaffected.

Metrics: `aurix_quic_connections`, `aurix_quic_sessions`,
`aurix_quic_handshakes_total{outcome="accepted"|"accepted_0rtt"|"refused"|"failed"}`,
`aurix_quic_packets_total{direction,outcome}`, `aurix_quic_migrations_total`.

## What the server does with your packets

* Drops anything from an unbound address, with a bad tag, wrong SSRC, protocol version ≠ 2,
  or outside the replay window (counted in `aurix_packets_dropped_total`; clients count their
  own `bad_auth` / `replayed` in the [statistics](../features/quality.md)).
* Applies moderator mute, transmission mode, channel membership, then per receiver: local mute,
  block, per-participant volume, channel focus, positional attenuation/direction — and re-seals.
* Feeds the decoded frame to recording, live streams and transcription **only** when the
  channel/consent rules allow and the frame is not `E2ee`.
* Estimates uplink quality from sequence gaps/jitter for [network quality](../features/quality.md).
* Renumbers audio per sender: the uplink sequence is shared with heartbeats and reports, so
  receivers get a per-sender audio-only sequence. A short run of frames missing on the uplink
  (packet sequence and the 20 ms timestamp clock jumped by the same count, up to 50 frames)
  stays a **gap** in that numbering, so receivers' jitter buffers see the loss and rebuild it
  from the next packet's in-band FEC / DRED or conceal it; a pause (DTX, VAD gate, heartbeats
  only) or a longer jump does not. A frame arriving late for such a slot keeps that slot.
  Timestamps are forwarded unchanged. The server mixers do the same repair on their own
  decoders (`media.mixer_decoder_complexity`, `aurix_mixer_lost_frames_total{method}`) —
  see [Packet loss](../sdk/native.md#packet-loss-fec-dred-and-the-neural-plc).
* On a session that negotiated PCMU: decodes the μ-law uplink and re-encodes it as narrowband
  Opus before any of the above, so the rest of the channel is unaffected; encodes the Opus it
  would have sent to that session as μ-law after the per-receiver step (gain/direction bytes and
  the seal are the same as for Opus). PCMU frames from a session that did not negotiate and
  `Pcmu | E2ee` frames are dropped (`aurix_packets_dropped_total`); frames of a length other
  than 80/160/320/480 bytes fail the transcode (`aurix_pcmu_frames_total{outcome="error"}`).
* Drops uplink audio from members whose channel role is `listener`, withholds per-speaker frames
  over a receiver's `audience.max_streams` cap, and for receivers in `mixed` downlink mode feeds
  the frame to their channel mixer instead of forwarding it (E2EE frames are still forwarded
  as-is) — see [large channels](../features/channels.md#large-channels-and-audiences).

## Reference implementations

* Rust: `crates/aurix-client/src/media.rs` (client), `crates/aurix-media/src/router.rs` (server).
* C#: `sdk/unity/Runtime/Protocol/*.cs` — same test vectors as Rust in
  `sdk/unity/DotNet~/Aurix.Voice.Tests`.
