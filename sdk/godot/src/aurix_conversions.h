// Conversions between the C ABI structs of `aurix_client.h` and Godot Variants (Strings for
// UUIDs, Dictionaries for records). Everything is a value copy; no Variant keeps a pointer into
// an `AurixEvent`.
#pragma once

#include "aurix_client.hpp"

#include <godot_cpp/variant/array.hpp>
#include <godot_cpp/variant/dictionary.hpp>
#include <godot_cpp/variant/packed_float32_array.hpp>
#include <godot_cpp/variant/string.hpp>

#include <algorithm>

namespace aurix_godot {

using godot::Array;
using godot::Dictionary;
using godot::PackedFloat32Array;
using godot::String;

inline String uuid_to_string(const AurixUuid& id) {
    aurix::Uuid u(id);
    if (u.is_nil()) {
        return String();
    }
    return String::utf8(u.str().c_str());
}

/// Empty / invalid text → nil UUID and `false`.
inline bool uuid_from_string(const String& text, AurixUuid& out) {
    std::memset(out.bytes, 0, sizeof out.bytes);
    if (text.is_empty()) {
        return false;
    }
    return aurix_uuid_parse(text.utf8().get_data(), &out) == AURIX_OK;
}

inline String cstr(const char* s) { return s ? String::utf8(s) : String(); }

inline Dictionary session_to_dict(const AurixSessionInfo& s) {
    Dictionary d;
    d["session_id"] = uuid_to_string(s.session_id);
    d["user_id"] = uuid_to_string(s.user_id);
    d["ssrc"] = static_cast<int64_t>(s.ssrc);
    d["resume_grace_ms"] = static_cast<int64_t>(s.resume_grace_ms);
    d["resumed"] = s.resumed;
    d["migrated"] = s.migrated;
    d["media_tunnel"] = s.media_tunnel;
    d["downlink_mix"] = s.downlink_mix;
    d["translation"] = s.translation;
    d["translation_speech"] = s.translation_speech;
    return d;
}

inline Dictionary participant_to_dict(const AurixParticipant& p) {
    Dictionary d;
    d["user_id"] = uuid_to_string(p.user_id);
    d["ssrc"] = static_cast<int64_t>(p.ssrc);
    d["role"] = static_cast<int>(p.role);
    d["muted"] = p.muted;
    d["server_muted"] = p.server_muted;
    d["speaking"] = p.speaking;
    d["energy"] = p.energy;
    d["priority"] = p.priority;
    d["display_name"] = cstr(p.display_name);
    return d;
}

inline Dictionary ducking_to_dict(const AurixDucking& k) {
    Dictionary d;
    d["enabled"] = k.enabled;
    d["gain"] = k.gain;
    d["attack_ms"] = static_cast<int64_t>(k.attack_ms);
    d["release_ms"] = static_cast<int64_t>(k.release_ms);
    d["hold_ms"] = static_cast<int64_t>(k.hold_ms);
    d["moderators"] = k.moderators;
    return d;
}

inline Dictionary channel_info_to_dict(const AurixChannelInfo& c) {
    Dictionary d;
    d["role"] = static_cast<int>(c.role);
    d["participant_count"] = static_cast<int64_t>(c.participant_count);
    d["hidden_listeners"] = c.hidden_listeners;
    d["transcription"] = c.transcription;
    d["safety_voice"] = c.safety_voice;
    d["priority"] = c.priority;
    d["ducking"] = ducking_to_dict(c.ducking);
    return d;
}

inline Dictionary voice_effects_to_dict(const AurixVoiceEffects& fx) {
    Dictionary d;
    d["highpass_hz"] = fx.highpass_hz;
    d["lowpass_hz"] = fx.lowpass_hz;
    d["formant_semitones"] = fx.formant_semitones;
    d["pitch_semitones"] = fx.pitch_semitones;
    d["ring_mod_hz"] = fx.ring_mod_hz;
    d["distortion_drive"] = fx.distortion_drive;
    d["tremolo_hz"] = fx.tremolo_hz;
    d["tremolo_depth"] = fx.tremolo_depth;
    d["static_level"] = fx.static_level;
    d["reverb_mix"] = fx.reverb_mix;
    d["reverb_size"] = fx.reverb_size;
    d["reverb_damping"] = fx.reverb_damping;
    return d;
}

inline void voice_effects_from_dict(const Dictionary& d, AurixVoiceEffects& fx) {
    auto f = [&](const char* key, float& field) {
        if (d.has(key)) field = static_cast<float>(static_cast<double>(d[key]));
    };
    f("highpass_hz", fx.highpass_hz);
    f("lowpass_hz", fx.lowpass_hz);
    f("formant_semitones", fx.formant_semitones);
    f("pitch_semitones", fx.pitch_semitones);
    f("ring_mod_hz", fx.ring_mod_hz);
    f("distortion_drive", fx.distortion_drive);
    f("tremolo_hz", fx.tremolo_hz);
    f("tremolo_depth", fx.tremolo_depth);
    f("static_level", fx.static_level);
    f("reverb_mix", fx.reverb_mix);
    f("reverb_size", fx.reverb_size);
    f("reverb_damping", fx.reverb_damping);
}

inline Dictionary viseme_frame_to_dict(const AurixVisemeFrame& f) {
    Dictionary d;
    PackedFloat32Array weights;
    weights.resize(AURIX_VISEME_COUNT);
    for (int i = 0; i < AURIX_VISEME_COUNT; ++i) weights[i] = f.weights[i];
    d["weights"] = weights;
    d["dominant"] = static_cast<int>(f.dominant);
    d["mouth_open"] = f.mouth_open;
    d["energy"] = f.energy;
    d["confidence"] = f.confidence;
    d["sequence"] = static_cast<int64_t>(f.sequence);
    return d;
}

inline Dictionary chat_to_dict(const AurixChatMessage& m) {
    Dictionary d;
    d["message_id"] = uuid_to_string(m.message_id);
    d["channel_id"] = uuid_to_string(m.channel_id);
    d["sender_id"] = uuid_to_string(m.sender_id);
    d["recipient_id"] = uuid_to_string(m.recipient_id);
    d["sender_name"] = cstr(m.sender_name);
    d["text"] = cstr(m.text);
    d["metadata_json"] = cstr(m.metadata_json);
    d["sent_at_ms"] = static_cast<int64_t>(m.sent_at_ms);
    d["request_id"] = static_cast<int64_t>(m.request_id);
    d["offline"] = m.offline;
    d["cursor"] = cstr(m.cursor);
    return d;
}

inline Dictionary read_marker_to_dict(const AurixReadMarker& r) {
    Dictionary d;
    d["user_id"] = uuid_to_string(r.user_id);
    d["channel_id"] = uuid_to_string(r.channel_id);
    d["peer_user_id"] = uuid_to_string(r.peer_user_id);
    d["message_id"] = uuid_to_string(r.message_id);
    d["message_sent_at_ms"] = static_cast<int64_t>(r.message_sent_at_ms);
    d["read_at_ms"] = static_cast<int64_t>(r.read_at_ms);
    return d;
}

inline Dictionary transcript_to_dict(const AurixTranscript& t) {
    Dictionary d;
    d["channel_id"] = uuid_to_string(t.channel_id);
    d["user_id"] = uuid_to_string(t.user_id);
    d["text"] = cstr(t.text);
    d["language"] = cstr(t.language);
    d["started_at_ms"] = static_cast<int64_t>(t.started_at_ms);
    d["duration_ms"] = static_cast<int64_t>(t.duration_ms);
    d["word_count"] = static_cast<int64_t>(t.word_count);
    d["original_text"] = cstr(t.original_text);
    d["original_language"] = cstr(t.original_language);
    return d;
}

inline Dictionary translation_to_dict(const AurixTranslation& t) {
    Dictionary d;
    d["language"] = cstr(t.language);
    d["spoken_language"] = cstr(t.spoken_language);
    d["speech"] = t.speech;
    return d;
}

inline Dictionary tts_to_dict(const AurixTtsStatus& t) {
    Dictionary d;
    d["request_id"] = static_cast<int64_t>(t.request_id);
    d["server_request_id"] = uuid_to_string(t.server_request_id);
    d["state"] = static_cast<int>(t.state);
    d["duration_ms"] = static_cast<int64_t>(t.duration_ms);
    d["message"] = cstr(t.message);
    return d;
}

inline Dictionary audio_policy_to_dict(const AurixAudioPolicy& p) {
    Dictionary d;
    d["bitrate_bps"] = static_cast<int64_t>(p.bitrate_bps);
    d["min_bitrate_bps"] = static_cast<int64_t>(p.min_bitrate_bps);
    d["fec"] = p.fec;
    d["dtx"] = p.dtx;
    d["max_bandwidth"] = static_cast<int>(p.max_bandwidth);
    d["complexity"] = static_cast<int>(p.complexity);
    d["signal"] = static_cast<int>(p.signal);
    d["stereo"] = p.stereo;
    return d;
}

inline Dictionary encoder_to_dict(const AurixEncoderSettings& e) {
    Dictionary d;
    d["bitrate_bps"] = static_cast<int64_t>(e.bitrate_bps);
    d["complexity"] = static_cast<int>(e.complexity);
    d["max_bandwidth"] = static_cast<int>(e.max_bandwidth);
    d["signal"] = static_cast<int>(e.signal);
    d["vbr"] = e.vbr;
    d["constrained_vbr"] = e.constrained_vbr;
    d["fec"] = e.fec;
    d["expected_loss_percent"] = static_cast<int>(e.expected_loss_percent);
    d["dtx"] = e.dtx;
    d["channels"] = static_cast<int>(e.channels);
    d["dred_duration_ms"] = static_cast<int>(e.dred_duration_ms);
    return d;
}

/// Missing keys keep the value already in `e` (callers pre-fill it with the current settings).
inline void encoder_from_dict(const Dictionary& d, AurixEncoderSettings& e) {
    if (d.has("bitrate_bps")) e.bitrate_bps = static_cast<uint32_t>(static_cast<int64_t>(d["bitrate_bps"]));
    if (d.has("complexity")) e.complexity = static_cast<uint8_t>(static_cast<int64_t>(d["complexity"]));
    if (d.has("max_bandwidth")) e.max_bandwidth = static_cast<AurixOpusBandwidth>(static_cast<int64_t>(d["max_bandwidth"]));
    if (d.has("signal")) e.signal = static_cast<AurixOpusSignal>(static_cast<int64_t>(d["signal"]));
    if (d.has("vbr")) e.vbr = static_cast<bool>(d["vbr"]);
    if (d.has("constrained_vbr")) e.constrained_vbr = static_cast<bool>(d["constrained_vbr"]);
    if (d.has("fec")) e.fec = static_cast<bool>(d["fec"]);
    if (d.has("expected_loss_percent")) e.expected_loss_percent = static_cast<uint8_t>(static_cast<int64_t>(d["expected_loss_percent"]));
    if (d.has("dtx")) e.dtx = static_cast<bool>(d["dtx"]);
    if (d.has("channels")) e.channels = static_cast<uint8_t>(static_cast<int64_t>(d["channels"]));
    if (d.has("dred_duration_ms")) e.dred_duration_ms = static_cast<uint16_t>(std::clamp<int64_t>(static_cast<int64_t>(d["dred_duration_ms"]), 0, 1040));
}

inline Dictionary decoder_to_dict(const AurixDecoderSettings& s) {
    Dictionary d;
    d["complexity"] = static_cast<int>(s.complexity);
    d["osce_bwe"] = s.osce_bwe;
    return d;
}

inline void decoder_from_dict(const Dictionary& d, AurixDecoderSettings& s) {
    if (d.has("complexity")) s.complexity = static_cast<uint8_t>(std::clamp<int64_t>(static_cast<int64_t>(d["complexity"]), 0, 10));
    if (d.has("osce_bwe")) s.osce_bwe = static_cast<bool>(d["osce_bwe"]);
}

inline Dictionary dsp_to_dict(const AurixDspConfig& c) {
    Dictionary d;
    d["high_pass"] = c.high_pass;
    d["echo_cancellation"] = c.echo_cancellation;
    d["echo_tail_ms"] = static_cast<int64_t>(c.echo_tail_ms);
    d["stream_delay_ms"] = static_cast<int64_t>(c.stream_delay_ms);
    d["noise_suppression"] = static_cast<int>(c.noise_suppression);
    d["agc"] = c.agc;
    d["agc_target_dbfs"] = c.agc_target_dbfs;
    d["agc_max_gain_db"] = c.agc_max_gain_db;
    return d;
}

inline void dsp_from_dict(const Dictionary& d, AurixDspConfig& c) {
    if (d.has("high_pass")) c.high_pass = static_cast<bool>(d["high_pass"]);
    if (d.has("echo_cancellation")) c.echo_cancellation = static_cast<bool>(d["echo_cancellation"]);
    if (d.has("echo_tail_ms")) c.echo_tail_ms = static_cast<uint32_t>(static_cast<int64_t>(d["echo_tail_ms"]));
    if (d.has("stream_delay_ms")) c.stream_delay_ms = static_cast<uint32_t>(static_cast<int64_t>(d["stream_delay_ms"]));
    if (d.has("noise_suppression")) c.noise_suppression = static_cast<AurixNoiseSuppression>(static_cast<int64_t>(d["noise_suppression"]));
    if (d.has("agc")) c.agc = static_cast<bool>(d["agc"]);
    if (d.has("agc_target_dbfs")) c.agc_target_dbfs = static_cast<float>(static_cast<double>(d["agc_target_dbfs"]));
    if (d.has("agc_max_gain_db")) c.agc_max_gain_db = static_cast<float>(static_cast<double>(d["agc_max_gain_db"]));
}

inline Dictionary dsp_stats_to_dict(const AurixDspStats& s) {
    Dictionary d;
    d["erle_db"] = s.erle_db;
    d["echo_delay_ms"] = static_cast<int64_t>(s.echo_delay_ms);
    d["echo_converged"] = s.echo_converged;
    d["far_end_active"] = s.far_end_active;
    d["speech_probability"] = s.speech_probability;
    d["agc_gain_db"] = s.agc_gain_db;
    d["far_end_underruns"] = static_cast<int64_t>(s.far_end_underruns);
    return d;
}

inline Dictionary network_quality_to_dict(const AurixNetworkQuality& q) {
    Dictionary d;
    d["bars"] = static_cast<int>(q.bars);
    d["r_factor"] = q.r_factor;
    d["mos"] = q.mos;
    d["rtt_ms"] = q.rtt_ms;
    d["downlink_jitter_ms"] = q.downlink_jitter_ms;
    d["downlink_loss_percent"] = q.downlink_loss_percent;
    d["uplink_jitter_ms"] = q.uplink_jitter_ms;
    d["uplink_loss_percent"] = q.uplink_loss_percent;
    d["uplink_bitrate_kbps"] = static_cast<int64_t>(q.uplink_bitrate_kbps);
    d["uplink_packets_received"] = static_cast<int64_t>(q.uplink_packets_received);
    d["uplink_packets_lost"] = static_cast<int64_t>(q.uplink_packets_lost);
    return d;
}

inline Dictionary stats_to_dict(const AurixStats& s) {
    Dictionary d;
    d["packets_sent"] = static_cast<int64_t>(s.packets_sent);
    d["bytes_sent"] = static_cast<int64_t>(s.bytes_sent);
    d["packets_received"] = static_cast<int64_t>(s.packets_received);
    d["bytes_received"] = static_cast<int64_t>(s.bytes_received);
    d["audio_frames_received"] = static_cast<int64_t>(s.audio_frames_received);
    d["bad_auth"] = static_cast<int64_t>(s.bad_auth);
    d["replayed"] = static_cast<int64_t>(s.replayed);
    d["heartbeats_lost"] = static_cast<int64_t>(s.heartbeats_lost);
    d["frames_lost"] = static_cast<int64_t>(s.frames_lost);
    d["frames_late"] = static_cast<int64_t>(s.frames_late);
    d["underruns"] = static_cast<int64_t>(s.underruns);
    d["frames_encoded"] = static_cast<int64_t>(s.frames_encoded);
    d["frames_sent"] = static_cast<int64_t>(s.frames_sent);
    d["frames_gated"] = static_cast<int64_t>(s.frames_gated);
    d["rtt_ms"] = s.rtt_ms;
    d["rtt_min_ms"] = s.rtt_min_ms;
    d["rtt_avg_ms"] = s.rtt_avg_ms;
    d["rtt_max_ms"] = s.rtt_max_ms;
    d["jitter_ms"] = s.jitter_ms;
    d["loss_percent"] = s.loss_percent;
    d["r_factor"] = s.r_factor;
    d["mos"] = s.mos;
    d["bars"] = static_cast<int>(s.bars);
    d["active_streams"] = static_cast<int64_t>(s.active_streams);
    d["media_path"] = static_cast<int>(s.media_path);
    d["uplink_dropped"] = static_cast<int64_t>(s.uplink_dropped);
    d["heartbeats_lost_consecutive"] = static_cast<int64_t>(s.heartbeats_lost_consecutive);
    if (s.has_server) {
        d["server"] = network_quality_to_dict(s.server);
    }
    return d;
}

inline Dictionary stream_to_dict(const AurixParticipantStream& s) {
    Dictionary d;
    d["ssrc"] = static_cast<int64_t>(s.ssrc);
    d["user_id"] = uuid_to_string(s.user_id);
    d["synthesized"] = s.synthesized;
    d["mixed"] = s.mixed;
    d["stereo"] = s.stereo;
    d["active"] = s.active;
    d["buffered_frames"] = static_cast<int64_t>(s.buffered_frames);
    return d;
}

inline Dictionary region_to_dict(const AurixRegionEndpoint& r) {
    Dictionary d;
    d["region"] = cstr(r.region);
    d["node_id"] = uuid_to_string(r.node_id);
    d["ws_url"] = cstr(r.ws_url);
    d["probe_url"] = cstr(r.probe_url);
    if (r.has_location) {
        d["latitude"] = r.latitude;
        d["longitude"] = r.longitude;
    }
    if (r.has_distance) {
        d["distance_km"] = r.distance_km;
    }
    d["nodes"] = static_cast<int64_t>(r.nodes);
    d["load_factor"] = r.load_factor;
    if (r.has_rtt) {
        d["rtt_ms"] = r.rtt_ms;
    }
    d["probe_failed"] = r.probe_failed;
    return d;
}

}  // namespace aurix_godot
