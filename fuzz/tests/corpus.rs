//! Stable-toolchain half of the fuzzing setup: every file under `corpus/<target>/` (seeds
//! plus anything libFuzzer found interesting) runs through the same harness body the
//! fuzzer uses, so a regression on a previously found input fails `cargo test` without
//! nightly or sanitizers. Also keeps `Cargo.toml`, `fuzz_targets/`, `corpus/` and
//! `aurix_fuzz::TARGETS` in agreement.
//!
//! `cargo test --manifest-path fuzz/Cargo.toml -- --ignored seed_corpus` regenerates the
//! deterministic seed inputs (valid, well-formed samples the fuzzer mutates from).

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn manifest_bins() -> BTreeSet<String> {
    let manifest = fs::read_to_string(root().join("Cargo.toml")).unwrap();
    let mut names = BTreeSet::new();
    let mut in_bin = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_bin = line == "[[bin]]";
            continue;
        }
        if in_bin {
            if let Some(rest) = line.strip_prefix("name = ") {
                names.insert(rest.trim_matches('"').to_string());
            }
        }
    }
    names
}

fn dir_entries(dir: &Path, strip: &str) -> BTreeSet<String> {
    fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .filter(|n| !n.starts_with('.'))
        .map(|n| n.strip_suffix(strip).unwrap_or(&n).to_string())
        .collect()
}

#[test]
fn targets_agree_everywhere() {
    let expected: BTreeSet<String> = aurix_fuzz::TARGETS.iter().map(|s| s.to_string()).collect();
    assert_eq!(manifest_bins(), expected, "[[bin]] table vs TARGETS");
    assert_eq!(dir_entries(&root().join("fuzz_targets"), ".rs"), expected, "fuzz_targets/ vs TARGETS");
    assert_eq!(dir_entries(&root().join("corpus"), ""), expected, "corpus/ vs TARGETS");
    for target in aurix_fuzz::TARGETS {
        let src = fs::read_to_string(root().join("fuzz_targets").join(format!("{target}.rs"))).unwrap();
        assert!(
            src.contains(&format!("aurix_fuzz::{target}(data)")),
            "{target}.rs must call aurix_fuzz::{target}"
        );
    }
}

#[test]
fn corpus_replays_without_panics() {
    let mut files = 0;
    for target in aurix_fuzz::TARGETS {
        let dir = root().join("corpus").join(target);
        let mut entries: Vec<PathBuf> = fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
            .map(|e| e.unwrap().path())
            .filter(|p| p.is_file())
            .collect();
        entries.sort();
        assert!(!entries.is_empty(), "corpus/{target} has no seeds; run the seed_corpus test");
        for path in entries {
            let data = fs::read(&path).unwrap();
            let name = path.display().to_string();
            let result = std::panic::catch_unwind(|| aurix_fuzz::run(target, &data));
            assert!(result.is_ok(), "{name}: harness panicked");
            files += 1;
        }
    }
    eprintln!("replayed {files} corpus files across {} targets", aurix_fuzz::TARGETS.len());
}

/// Every harness must survive the classic edge inputs even without a corpus.
#[test]
fn edge_inputs() {
    let inputs: Vec<Vec<u8>> = vec![
        vec![],
        vec![0],
        vec![0xff],
        vec![0; 30],
        vec![0xff; 30],
        vec![0x41, 0x55, 0x52, 0x58],
        (0..=255u8).collect(),
        vec![0x80; 1500],
        b"{\"type\":\"Ping\"}".to_vec(),
        b"t=1,v1=zz\nbody".to_vec(),
    ];
    for target in aurix_fuzz::TARGETS {
        for input in &inputs {
            aurix_fuzz::run(target, input);
        }
    }
}

fn write_seed(target: &str, name: &str, data: &[u8]) {
    let dir = root().join("corpus").join(target);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(name), data).unwrap();
}

/// Deterministic, well-formed seeds. Ignored by default: it rewrites files in `corpus/`.
#[test]
#[ignore = "regenerates fuzz/corpus seeds"]
fn seed_corpus() {
    use aurix_common::protocol::{AurixPacket, ControlMessage, PacketHeader, PacketType};
    use aurix_common::types::{ChannelId, SessionId, UserId};
    use bytes::Bytes;

    let keys = aurix_fuzz::media_keys();
    let user = UserId::from_uuid(uuid::Uuid::from_u128(0xabcdef));
    let session = SessionId(uuid::Uuid::from_u128(0x1234_5678));
    let channel = ChannelId::from_uuid(uuid::Uuid::from_u128(0x77));

    // --- Opus frames shared by several targets ---------------------------------------
    let mut enc = aurix_opus::Encoder::new(48_000, aurix_opus::Channels::Mono, aurix_opus::Application::Voip).unwrap();
    enc.set_inband_fec(true).unwrap();
    enc.set_packet_loss_perc(20).unwrap();
    let pcm = aurix_opus::testing::speech_like_i16(4);
    let opus_frames: Vec<Vec<u8>> = pcm.chunks(960).map(|c| enc.encode_vec(c, 400).unwrap()).collect();
    let mut dred_enc = aurix_opus::Encoder::new(48_000, aurix_opus::Channels::Mono, aurix_opus::Application::Voip).unwrap();
    dred_enc.set_dred_duration(20).unwrap();
    let dred_frames: Vec<Vec<u8>> = pcm.chunks(960).map(|c| dred_enc.encode_vec(c, 1200).unwrap()).collect();
    let mut stereo_enc = aurix_opus::Encoder::new(48_000, aurix_opus::Channels::Stereo, aurix_opus::Application::Audio).unwrap();
    let stereo_pcm: Vec<i16> = pcm.iter().flat_map(|s| [*s, s / 2]).collect();
    let stereo_frames: Vec<Vec<u8>> = stereo_pcm.chunks(1920).map(|c| stereo_enc.encode_vec(c, 600).unwrap()).collect();

    // --- aurx_packet ---------------------------------------------------------------------
    let audio = AurixPacket::new(
        PacketHeader::new(PacketType::Audio, 17, 17 * 960, 0xdead_beef),
        Bytes::from(opus_frames[0].clone()),
    );
    write_seed("aurx_packet", "audio_plain", &audio.encode());
    write_seed("aurx_packet", "audio_sealed", &audio.seal(&keys));
    let bind = AurixPacket::session_bind(&session, 0xdead_beef, 1_738_000_000_000, 42);
    write_seed("aurx_packet", "session_bind", &bind.encode_authenticated(&keys));
    write_seed("aurx_packet", "session_bind_ack", &AurixPacket::session_bind_ack(0xdead_beef, 1, 1_738_000_000_000).encode());
    let relay = AurixPacket::relay_envelope(&audio, 0x0102_0304, 99, &user);
    write_seed("aurx_packet", "relay_envelope", &relay.seal(&keys));
    let relay_hop = AurixPacket::relay_envelope_hop(&audio, 0x0102_0304, 100, &user, 2);
    write_seed("aurx_packet", "relay_envelope_hop", &relay_hop.encode());
    let mut pcmu_header = PacketHeader::new(PacketType::Audio, 5, 5 * 160, 0x11);
    pcmu_header.flags |= aurix_common::protocol::PacketFlags::Pcmu as u16;
    write_seed("aurx_packet", "audio_pcmu", &AurixPacket::new(pcmu_header, Bytes::from(vec![0xffu8; 160])).encode());
    write_seed("aurx_packet", "heartbeat", &AurixPacket::new(PacketHeader::new(PacketType::Heartbeat, 1, 0, 0x11), Bytes::new()).seal(&keys));

    // --- rtp_header ----------------------------------------------------------------------
    let mut rtp = vec![0x80, 111, 0x00, 0x2a, 0, 0, 0x3a, 0x98, 0xde, 0xad, 0xbe, 0xef];
    rtp.extend_from_slice(&opus_frames[1]);
    write_seed("rtp_header", "opus_pt111", &rtp);
    let mut rtp_ext = vec![0x90, 0xe0, 0x00, 0x2b, 0, 0, 0x3a, 0x98, 0xde, 0xad, 0xbe, 0xef, 0xbe, 0xde, 0x00, 0x01, 0x10, 0x7f, 0, 0];
    rtp_ext.extend_from_slice(&opus_frames[2]);
    write_seed("rtp_header", "opus_with_extension", &rtp_ext);

    // --- stun_message --------------------------------------------------------------------
    use aurix_turn::stun::{StunAttributeType, StunMessage, StunMessageType};
    let tid = [7u8; 12];
    let mut binding = StunMessage::new(StunMessageType::BindingRequest, tid);
    binding.add_software("aurix-fuzz");
    write_seed("stun_message", "binding_request", &binding.encode());
    let mut alloc = StunMessage::new(StunMessageType::AllocateRequest, tid);
    alloc.add_attribute(StunAttributeType::RequestedTransport, vec![17, 0, 0, 0]);
    alloc.add_attribute(StunAttributeType::Username, b"1738000000:alice".to_vec());
    alloc.add_attribute(StunAttributeType::Realm, b"aurix".to_vec());
    alloc.add_attribute(StunAttributeType::Nonce, b"0123456789abcdef".to_vec());
    alloc.add_attribute(StunAttributeType::Lifetime, 600u32.to_be_bytes().to_vec());
    write_seed("stun_message", "allocate_with_integrity", &alloc.encode_with_integrity(b"fuzz-turn-key"));
    let mut resp = StunMessage::new(StunMessageType::BindingRequest.success_response(), tid);
    resp.add_xor_mapped_address("203.0.113.9:40000".parse().unwrap());
    resp.add_xor_address(StunAttributeType::XorRelayedAddress, "[2001:db8::1]:50000".parse().unwrap());
    write_seed("stun_message", "success_response_v4_v6", &resp.encode());
    let mut err = StunMessage::new(StunMessageType::AllocateRequest.error_response(), tid);
    err.add_error_code(401, "Unauthorized");
    write_seed("stun_message", "error_401", &err.encode());

    // --- control_message -----------------------------------------------------------------
    let msgs = [
        ("channel_join", ControlMessage::ChannelJoin { channel_id: channel, token: "join-token".into() }),
        ("channel_leave", ControlMessage::ChannelLeave { channel_id: channel }),
        ("set_participant_mute", ControlMessage::SetParticipantMute { user_id: user, channel_id: Some(channel), muted: true }),
        ("set_participant_volume", ControlMessage::SetParticipantVolume { user_id: user, volume: 0.5 }),
        ("set_user_block", ControlMessage::SetUserBlock { user_id: user, blocked: true }),
    ];
    for (name, msg) in msgs {
        write_seed("control_message", name, serde_json::to_string(&msg).unwrap().as_bytes());
    }
    write_seed("control_message", "ping", br#"{"type":"Ping"}"#);

    // --- e2ee_frame ----------------------------------------------------------------------
    use aurix_common::e2ee::{IdentityKey, SenderKey};
    let key = SenderKey::derive(3, &[0x42u8; 32]);
    write_seed("e2ee_frame", "sealed_frame", &key.seal(7, &opus_frames[0]));
    write_seed("e2ee_frame", "sealed_frame_gen3_counter9", &key.seal(9, &opus_frames[1]));
    let me = IdentityKey::from_bytes([1u8; 32]);
    let them = IdentityKey::from_bytes([2u8; 32]);
    // Wrapped for `me` by `them` (the harness unwraps with identity 1 from public key 2).
    let mut wrapped = them.public_key().to_vec();
    wrapped.extend(them.wrap(me.public_key(), 1, &[0x42u8; 32]).unwrap());
    write_seed("e2ee_frame", "wrapped_key_from_peer2", &wrapped);
    write_seed("e2ee_frame", "public_key_b64", aurix_common::e2ee::encode_bytes(them.public_key()).as_bytes());

    // --- opus_packet ---------------------------------------------------------------------
    for (i, f) in opus_frames.iter().enumerate() {
        write_seed("opus_packet", &format!("voip_fec_{i}"), f);
    }
    for (i, f) in dred_frames.iter().enumerate() {
        write_seed("opus_packet", &format!("voip_dred_{i}"), f);
    }
    for (i, f) in stereo_frames.iter().enumerate() {
        write_seed("opus_packet", &format!("stereo_{i}"), f);
    }
    write_seed("opus_packet", "celt_silence", &[0xfc, 0xff, 0xfe]);

    // --- ogg_opus ------------------------------------------------------------------------
    let mut ogg = Vec::new();
    {
        let mut w = aurix_recording::ogg::OggOpusWriter::new(&mut ogg, 0x0bad_cafe, 48_000, 1).unwrap();
        for f in &opus_frames {
            w.write_packet(f).unwrap();
        }
        w.finish().unwrap();
    }
    write_seed("ogg_opus", "mono_4_frames", &ogg);
    let mut ogg2 = Vec::new();
    {
        let mut w = aurix_recording::ogg::OggOpusWriter::new(&mut ogg2, 1, 48_000, 2).unwrap();
        for f in &stereo_frames {
            w.write_packet(f).unwrap();
        }
        w.finish().unwrap();
    }
    write_seed("ogg_opus", "stereo_4_frames", &ogg2);

    // --- wav -------------------------------------------------------------------------------
    for (name, bits, rate, ch) in [("pcm16_mono_16k", 16u16, 16_000u32, 1u16), ("pcm24_stereo_48k", 24, 48_000, 2), ("float32_mono_24k", 32, 24_000, 1)] {
        let frames = 96usize;
        let block = ch * bits / 8;
        let data_len = frames as u32 * block as u32;
        let fmt_tag: u16 = if name.starts_with("float") { 3 } else { 1 };
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_len).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&fmt_tag.to_le_bytes());
        wav.extend_from_slice(&ch.to_le_bytes());
        wav.extend_from_slice(&rate.to_le_bytes());
        wav.extend_from_slice(&(rate * block as u32).to_le_bytes());
        wav.extend_from_slice(&block.to_le_bytes());
        wav.extend_from_slice(&bits.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        for i in 0..frames * ch as usize {
            let v = ((i as f32 * 0.3).sin() * 0.5) as f32;
            match bits {
                16 => wav.extend_from_slice(&((v * 32767.0) as i16).to_le_bytes()),
                24 => wav.extend_from_slice(&((v * 8_388_607.0) as i32).to_le_bytes()[..3]),
                _ => wav.extend_from_slice(&v.to_le_bytes()),
            }
        }
        write_seed("wav", name, &wav);
    }

    // --- live_frame ----------------------------------------------------------------------
    write_seed("live_frame", "opus_frame", &aurix_recording::live::encode_frame(0, 0, 0xdead_beef, 960, 1_738_000_000_000, &user, &opus_frames[0]));
    write_seed("live_frame", "pcm_frame_flag1", &aurix_recording::live::encode_frame(1, 1, 0x11, 1920, 1_738_000_000_020, &user, &[0u8; 64]));

    // --- webhook_signature -----------------------------------------------------------------
    let body = br#"{"id":"evt_1","type":"channel.created","data":{}}"#;
    let sig = aurix_control::webhooks::sign("whsec_fuzz_0123456789abcdef0123456789abcdef", 1_738_000_000, body);
    let mut seed = sig.into_bytes();
    seed.push(b'\n');
    seed.extend_from_slice(body);
    write_seed("webhook_signature", "valid", &seed);
    write_seed("webhook_signature", "shared_vector", b"t=1738000000,v1=7bab837da083762d92200f3c5a54e98c65e36dc6610f3b1643bfe8be2d09eb66\n{\"id\":\"evt_1\"}");

    // --- remote_mixer ----------------------------------------------------------------------
    let mut stream = Vec::new();
    for (i, f) in opus_frames.iter().enumerate() {
        stream.push(((i as u8 & 0x0f) << 4) | 0x04); // ctl: seq high nibble, direction on
        stream.push(1); // ssrc 1
        stream.push(i as u8);
        stream.push(f.len() as u8);
        stream.extend_from_slice(f);
    }
    for (i, f) in stereo_frames.iter().enumerate() {
        stream.push(0x02 | 0x08); // mixed, volume 1
        stream.push(2);
        stream.push(i as u8);
        stream.push(f.len() as u8);
        stream.extend_from_slice(f);
    }
    stream.extend_from_slice(&[0x01 | 0x08, 3, 0, 160]);
    stream.extend_from_slice(&[0xffu8; 160]);
    write_seed("remote_mixer", "opus_stereo_pcmu_streams", &stream);

    // --- text_parsers ----------------------------------------------------------------------
    for (name, s) in [
        ("turn_username", "1738000000:alice"),
        ("region", "eu-west"),
        ("role", "moderator"),
        ("permission", "apps:read"),
        ("bind_addr", "[::]"),
        ("cidr", "10.0.0.0/8"),
        ("metric", "rtt_sum_ms"),
        ("url", "https://hooks.example.com/aurix"),
        ("chat_cursor", &aurix_common::protocol::encode_chat_cursor(chrono::DateTime::from_timestamp(1_738_000_000, 0).unwrap(), uuid::Uuid::from_u128(0x99))),
    ] {
        write_seed("text_parsers", name, s.as_bytes());
    }
}
