//! Fuzz harness bodies. Each `pub fn` takes the raw fuzzer input and must never panic on
//! any byte string: a panic, an abort or a sanitizer report is a finding. The binaries in
//! `fuzz_targets/` are one-liners over these functions so the same code runs (a) under
//! libFuzzer with sanitizers on nightly and (b) over the committed corpus on stable in
//! `tests/corpus.rs`.
//!
//! Harnesses only exercise code that is reachable from the network or from untrusted
//! files before any authentication succeeds — media datagrams, STUN/TURN, control-plane
//! JSON, E2EE frames and key wraps, recordings, webhook signatures, the client's downlink
//! decoder. Keys are fixed so every run is deterministic.

use aurix_common::crypto::MediaKeys;
use aurix_common::e2ee::{Group, IdentityKey, PeerKeys, SenderKey, SECRET_LEN};
use aurix_common::protocol::{
    is_aurix_packet, is_quic_packet, AurixPacket, ControlMessage, PacketFlags, RtpHeader,
    HEADER_SIZE, MAX_PACKET_SIZE, MAX_RELAY_PACKET_SIZE,
};
use aurix_common::types::{AudioCodec, ChannelId, Direction, UserId};
use aurix_turn::stun::{StunAttributeType, StunMessage};

/// Every target name, in `Cargo.toml` order. `tests/corpus.rs` checks this against the
/// manifest and runs each corpus through [`run`].
pub const TARGETS: &[&str] = &[
    "aurx_packet",
    "rtp_header",
    "stun_message",
    "control_message",
    "e2ee_frame",
    "opus_packet",
    "ogg_opus",
    "wav",
    "live_frame",
    "webhook_signature",
    "remote_mixer",
    "text_parsers",
];

/// Dispatch by target name (used by the stable corpus test and by `cargo run --bin`-less
/// tooling). Unknown names panic — that is a harness bug, not a finding.
pub fn run(target: &str, data: &[u8]) {
    match target {
        "aurx_packet" => aurx_packet(data),
        "rtp_header" => rtp_header(data),
        "stun_message" => stun_message(data),
        "control_message" => control_message(data),
        "e2ee_frame" => e2ee_frame(data),
        "opus_packet" => opus_packet(data),
        "ogg_opus" => ogg_opus(data),
        "wav" => wav(data),
        "live_frame" => live_frame(data),
        "webhook_signature" => webhook_signature(data),
        "remote_mixer" => remote_mixer(data),
        "text_parsers" => text_parsers(data),
        other => panic!("unknown fuzz target {other}"),
    }
}

const MASTER: &[u8; 32] = b"aurix-fuzz-master-key-0123456789";

pub fn media_keys() -> MediaKeys {
    MediaKeys::derive(MASTER)
}

/// Same bytes with the header checksum rewritten to match the payload, so the fuzzer also
/// reaches everything behind the CRC gate without having to learn CRC-32.
fn with_fixed_checksum(data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < HEADER_SIZE {
        return None;
    }
    let mut fixed = data.to_vec();
    let authenticated =
        u16::from_be_bytes([fixed[5], fixed[6]]) & PacketFlags::Authenticated as u16 != 0;
    let tag = if authenticated { 16 } else { 0 };
    let body_len = fixed.len().checked_sub(HEADER_SIZE + tag)?;
    let body_len = u16::try_from(body_len).ok()?;
    fixed[24..26].copy_from_slice(&body_len.to_be_bytes());
    let crc = crc32(&fixed[HEADER_SIZE..HEADER_SIZE + body_len as usize]);
    fixed[26..30].copy_from_slice(&crc.to_be_bytes());
    Some(fixed)
}

fn crc32(data: &[u8]) -> u32 {
    // Same polynomial/parameters as `crc32fast` (IEEE), inlined so the harness does not
    // depend on the crate under test to build its own oracle.
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn exercise_packet(data: &[u8], keys: &MediaKeys) {
    let _ = is_aurix_packet(data);
    let _ = is_quic_packet(data);
    let Ok(mut packet) = AurixPacket::decode(data) else {
        // The relay bound admits larger envelopes than the client bound.
        if let Ok(relay) = AurixPacket::decode_bounded(data, MAX_RELAY_PACKET_SIZE) {
            let _ = relay.relay_inner();
            let _ = relay.relay_inner_hop();
        }
        return;
    };
    // Whatever decodes must re-encode to something that decodes to the same packet.
    let again = packet.encode();
    let reparsed = AurixPacket::decode(&again).expect("re-encoded packet must decode");
    assert_eq!(reparsed.payload, packet.payload);
    assert_eq!(reparsed.header.sequence, packet.header.sequence);
    assert_eq!(reparsed.header.ssrc, packet.header.ssrc);
    let _ = packet.parse_session_bind();
    let _ = packet.relay_inner();
    let _ = packet.relay_inner_hop();
    let _ = packet.verify_auth(keys);
    if packet.open(keys) {
        // A forged tag must not verify: only packets we sealed ourselves open.
        let _ = packet.parse_session_bind();
    }
    let sealed = packet.seal(keys);
    assert!(sealed.len() <= MAX_RELAY_PACKET_SIZE + HEADER_SIZE);
    let mut roundtrip = AurixPacket::decode_bounded(&sealed, MAX_RELAY_PACKET_SIZE + HEADER_SIZE)
        .expect("sealed decodes");
    assert!(roundtrip.open(keys), "own seal must open");
    assert_eq!(roundtrip.payload, packet.payload);
}

/// AURX media datagrams: header/payload decoding, CRC, relay envelopes, `SessionBind`,
/// authentication and decryption with a fixed key, and seal/open round trips.
pub fn aurx_packet(data: &[u8]) {
    let keys = media_keys();
    exercise_packet(data, &keys);
    if let Some(fixed) = with_fixed_checksum(data) {
        exercise_packet(&fixed, &keys);
    }
    let _ = MAX_PACKET_SIZE;
}

/// RTP header parsing used for browser (WebRTC) media.
pub fn rtp_header(data: &[u8]) {
    if let Ok(h) = RtpHeader::parse(data) {
        assert!(h.header_size <= data.len());
        let _ = &data[h.header_size..];
    }
}

const STUN_KEY: &[u8] = b"fuzz-turn-key";

/// STUN/TURN messages: framing, attribute walk, XOR addresses, MESSAGE-INTEGRITY and
/// FINGERPRINT checks and encode/decode round trips.
pub fn stun_message(data: &[u8]) {
    let _ = StunMessage::is_stun(data);
    let _ = StunMessage::stun_frame_len(data);
    let _ = StunMessage::verify_fingerprint(data);
    let _ = StunMessage::verify_integrity(data, STUN_KEY);
    let _ = aurix_turn::stun::find_attribute_offset(data, 0x0008);
    let Ok(msg) = StunMessage::decode(data) else {
        return;
    };
    for attr in [
        StunAttributeType::XorMappedAddress,
        StunAttributeType::XorPeerAddress,
        StunAttributeType::XorRelayedAddress,
    ] {
        let _ = msg.get_xor_address(attr);
        let _ = msg.get_all_xor_addresses(attr);
    }
    for attr in &msg.attributes {
        let _ = msg.decode_xor_address(&attr.value);
    }
    let _ = msg.get_string(StunAttributeType::Username);
    let _ = msg.get_u32(StunAttributeType::Lifetime);
    let encoded = msg.encode();
    let again = StunMessage::decode(&encoded).expect("re-encoded STUN must decode");
    assert_eq!(again.transaction_id, msg.transaction_id);
    assert_eq!(again.msg_type, msg.msg_type);
    let signed = msg.encode_with_integrity(STUN_KEY);
    assert!(
        StunMessage::verify_integrity(&signed, STUN_KEY),
        "own integrity must verify"
    );
    assert!(
        StunMessage::verify_fingerprint(&signed),
        "own fingerprint must verify"
    );
}

/// Control-plane JSON (`ControlMessage`, both directions): deserialize, then the
/// serialize → deserialize → serialize round trip must reproduce the same JSON text.
/// (Text, not `serde_json::Value`: `Value` widens `f32` fields to `f64`, so `5.6` would
/// compare unequal to itself.) Inputs with a number outside the `f32` range are skipped:
/// serde widens them to `±inf`, which serializes as `null` and is not a round-trip property
/// of the message — handlers reject non-finite floats at validation.
pub fn control_message(data: &[u8]) {
    if has_f32_overflow(data) {
        return;
    }
    let Ok(msg) = serde_json::from_slice::<ControlMessage>(data) else {
        return;
    };
    let text = serde_json::to_string(&msg).expect("ControlMessage serializes");
    let again: ControlMessage = serde_json::from_str(&text).expect("own JSON must parse");
    let text_again = serde_json::to_string(&again).expect("ControlMessage re-serializes");
    assert_eq!(text, text_again, "ControlMessage round trip must be stable");
}

fn has_f32_overflow(data: &[u8]) -> bool {
    fn walk(v: &serde_json::Value) -> bool {
        match v {
            serde_json::Value::Number(n) => n
                .as_f64()
                .is_some_and(|f| !f.is_finite() || f.abs() > f32::MAX as f64),
            serde_json::Value::Array(a) => a.iter().any(walk),
            serde_json::Value::Object(o) => o.values().any(walk),
            _ => false,
        }
    }
    serde_json::from_slice::<serde_json::Value>(data).is_ok_and(|v| walk(&v))
}

fn fixed_identity(seed: u8) -> IdentityKey {
    IdentityKey::from_bytes([seed; 32])
}

/// Group E2EE: sender-key frames (`peek`/`open` with a known key and replay window), key
/// wraps addressed to us from a known and an unknown peer, base64 key strings, and the
/// group state machine driven by untrusted `hello`/`sender_key` inputs.
pub fn e2ee_frame(data: &[u8]) {
    let secret = [0x42u8; SECRET_LEN];
    let key = SenderKey::derive(3, &secret);
    let _ = SenderKey::peek(data);
    let _ = key.open(data);
    let mut peer = PeerKeys::default();
    peer.insert(key.clone());
    let _ = peer.open(data);
    // A frame we sealed ourselves must open exactly once through the replay window.
    if data.len() <= 400 {
        let sealed = key.seal(7, data);
        assert_eq!(key.open(&sealed).expect("own frame opens"), data);
        let mut fresh = PeerKeys::default();
        fresh.insert(key.clone());
        assert!(fresh.open(&sealed).is_ok());
        assert!(fresh.open(&sealed).is_err(), "replay must be rejected");
    }

    let me = fixed_identity(1);
    let them = fixed_identity(2);
    let _ = me.unwrap(them.public_key(), data.first().copied().unwrap_or(0), data);
    let _ = me.unwrap(&[0u8; 32], 0, data);
    if data.len() >= 32 {
        let mut pk = [0u8; 32];
        pk.copy_from_slice(&data[..32]);
        let _ = me.unwrap(&pk, 1, &data[32..]);
    }

    if let Ok(s) = std::str::from_utf8(data) {
        let _ = aurix_common::e2ee::parse_public_key(s);
        let _ = aurix_common::e2ee::decode_bytes(s);
    }

    let mut group = Group::new(fixed_identity(3));
    let channel = ChannelId::from_uuid(uuid::Uuid::from_u128(0x1234));
    let _ = group.joined(channel);
    let peer_id = UserId::from_uuid(uuid::Uuid::from_u128(0x5678));
    let mut pk = [0u8; 32];
    for (i, b) in data.iter().take(32).enumerate() {
        pk[i] = *b;
    }
    let _ = group.on_hello(channel, peer_id, pk);
    let _ = group.on_sender_key(channel, peer_id, pk, data.len() as u8, data);
    let _ = group.on_sender_key(channel, peer_id, *them.public_key(), 1, data);
    let _ = group.decrypt(&peer_id, data);
    let _ = group.rotate(true);
    let _ = group.encrypt(data);
}

/// Opus packets straight into libopus (mono and stereo decoders, FEC on/off, packet
/// inspection helpers, DRED parsing). This is the C code path.
pub fn opus_packet(data: &[u8]) {
    use aurix_opus::{packet, Channels, Decoder, Dred, DredDecoder};
    let _ = packet::get_nb_channels(data);
    let _ = packet::get_bandwidth(data);
    let _ = packet::get_nb_samples(data, 48_000);
    let _ = packet::has_lbrr(data);
    let mut out = vec![0i16; 5760 * 2];
    let mut outf = vec![0f32; 5760 * 2];
    for channels in [Channels::Mono, Channels::Stereo] {
        let Ok(mut dec) = Decoder::new(48_000, channels) else {
            continue;
        };
        let _ = dec.set_complexity((data.len() % 11) as i32);
        let _ = dec.get_nb_samples(data);
        let _ = dec.decode(data, &mut out, false);
        let _ = dec.decode(data, &mut out, true);
        let _ = dec.decode_float(data, &mut outf, false);
        let _ = dec.get_last_packet_duration();
        // Loss concealment after the packet.
        let _ = dec.decode(&[], &mut out[..960 * channels.count()], false);
        if let (Ok(mut dred), Ok(mut parser)) = (Dred::new(), DredDecoder::new()) {
            if let Ok(avail) = parser.parse(&mut dred, data, 48_000, 48_000) {
                if avail > 0 {
                    let frame = 960 * channels.count();
                    let _ = dec.dred_decode_float(&dred, frame.min(avail), &mut outf[..frame]);
                }
            }
        }
    }
}

/// Ogg/Opus recordings: page/CRC/lacing parser, then the first packets through libopus.
pub fn ogg_opus(data: &[u8]) {
    let Ok((head, packets)) = aurix_recording::mixdown::parse_ogg_opus(data) else {
        return;
    };
    assert!(head.channels == 1 || head.channels == 2 || packets.is_empty() || head.channels > 0);
    let channels = if head.channels == 2 {
        aurix_opus::Channels::Stereo
    } else {
        aurix_opus::Channels::Mono
    };
    let Ok(mut dec) = aurix_opus::Decoder::new(48_000, channels) else {
        return;
    };
    let mut out = vec![0i16; 5760 * 2];
    for p in packets.iter().take(64) {
        let _ = dec.decode(&p.data, &mut out, false);
    }
}

/// RIFF/WAVE decoding used for TTS provider responses.
pub fn wav(data: &[u8]) {
    if let Ok(pcm) = aurix_common::tts_stt::parse_wav(data) {
        assert!(pcm.channels > 0);
        assert!(pcm.sample_rate > 0);
    }
}

/// Live recording stream frames (`pull`/`push` connectors).
pub fn live_frame(data: &[u8]) {
    if let Some(frame) = aurix_recording::live::decode_frame(data) {
        assert!(frame.payload.len() <= data.len());
    }
}

const WEBHOOK_SECRET: &str = "whsec_fuzz_0123456789abcdef0123456789abcdef";

/// Webhook signature header parsing and constant-time verification. The input is split
/// at the first `\n` into header and body; a signature we produce must verify and a
/// signature over a different body must not.
pub fn webhook_signature(data: &[u8]) {
    use aurix_control::webhooks::{sign, verify_signature};
    let now = chrono::DateTime::from_timestamp(1_738_000_000, 0).unwrap();
    let tolerance = std::time::Duration::from_secs(300);
    let (header, body) = match data.iter().position(|&b| b == b'\n') {
        Some(i) => (&data[..i], &data[i + 1..]),
        None => (data, &[][..]),
    };
    if let Ok(h) = std::str::from_utf8(header) {
        let _ = verify_signature(WEBHOOK_SECRET, h, body, now, tolerance);
    }
    let sig = sign(WEBHOOK_SECRET, now.timestamp(), body);
    assert!(verify_signature(WEBHOOK_SECRET, &sig, body, now, tolerance));
    assert!(!verify_signature(
        "other-secret",
        &sig,
        body,
        now,
        tolerance
    ));
    let mut other = body.to_vec();
    other.push(0);
    assert!(!verify_signature(
        WEBHOOK_SECRET,
        &sig,
        &other,
        now,
        tolerance
    ));
}

/// The native client's downlink: a hostile node feeds arbitrary frames (SSRC, sequence,
/// codec, mixed/stereo flags, payload) into `RemoteMixer`, then the audio thread mixes.
pub fn remote_mixer(data: &[u8]) {
    use aurix_client::audio::RemoteMixer;
    let mut mixer = RemoteMixer::new(2, 8);
    mixer.set_visemes(data.first().is_some_and(|b| b & 1 == 1));
    let mut rest = data;
    let mut frames = 0;
    while rest.len() >= 4 && frames < 64 {
        let ctl = rest[0];
        let ssrc = (rest[1] & 0x0f) as u32;
        let seq = rest[2] as u32 | (((ctl >> 4) as u32) << 8);
        let len = rest[3] as usize;
        rest = &rest[4..];
        let take = len.min(rest.len());
        let payload = rest[..take].to_vec();
        rest = &rest[take..];
        let codec = if ctl & 0x01 != 0 {
            AudioCodec::Pcmu
        } else {
            AudioCodec::Opus
        };
        let mixed = ctl & 0x02 != 0;
        let direction = (ctl & 0x04 != 0).then_some(Direction {
            azimuth: (seq as f32 / 4096.0 - 0.5) * 6.0,
            elevation: 0.1,
        });
        let volume = (ctl & 0x08 != 0) as u8 as f32;
        let _ = mixer.push_wire_frame(ssrc, seq, volume, direction, codec, mixed, payload);
        frames += 1;
        if frames % 8 == 0 {
            let mut out = vec![0f32; 960 * 2];
            let _ = mixer.mix(&mut out, 2);
            let mut mono = vec![0f32; 480];
            let _ = mixer.mix(&mut mono, 1);
        }
    }
    let mut out = vec![0f32; 960 * 2];
    let _ = mixer.mix(&mut out, 2);
    for v in out {
        assert!(v.is_finite(), "mixer must never output NaN/inf");
    }
}

/// Short string parsers on untrusted text: chat cursors, TURN usernames, region/role
/// names, bind addresses, usage periods.
pub fn text_parsers(data: &[u8]) {
    let Ok(s) = std::str::from_utf8(data) else {
        return;
    };
    let _ = aurix_common::protocol::decode_chat_cursor(s);
    let _ = aurix_common::crypto::parse_turn_username(s);
    let _ = aurix_common::types::AdminRole::parse(s);
    let _ = aurix_common::types::AdminPermission::parse(s);
    let _ = aurix_common::types::AdminAuthSource::parse(s);
    let _ = aurix_common::types::Region::from_str_loose(s);
    let _ = aurix_common::net::parse_bind_addr(s, 8080);
    let _ = aurix_common::net::parse_trusted_proxies(&[s.to_string()]);
    let _ = aurix_common::usage::UsageMetric::parse(s);
    let _ = aurix_common::e2ee::parse_public_key(s);
    let _ = aurix_common::addr::host_port(s, 1);
    let _ = url_like(s);
}

fn url_like(s: &str) -> bool {
    aurix_common::net::validate_outbound_url(s, "https", "http", false, false, "fuzz").is_ok()
}
