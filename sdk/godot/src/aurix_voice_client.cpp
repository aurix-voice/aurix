#include "aurix_voice_client.h"

#include "aurix_conversions.h"

#include <godot_cpp/classes/audio_server.hpp>
#include <godot_cpp/classes/audio_stream_generator.hpp>
#include <godot_cpp/classes/audio_stream_microphone.hpp>
#include <godot_cpp/classes/engine.hpp>
#include <godot_cpp/core/class_db.hpp>
#include <godot_cpp/variant/utility_functions.hpp>

#include <algorithm>
#include <cstring>
#include <string>

using namespace aurix_godot;

namespace godot {

namespace {

constexpr const char* kCaptureBusName = "AurixCapture";
constexpr uint32_t kMixRate = 48000;

bool parse_uuid_arg(const String& text, AurixUuid& out, const char* what) {
    if (!uuid_from_string(text, out)) {
        UtilityFunctions::push_error(String("AurixVoiceClient: invalid ") + what + " id '" + text + "'");
        return false;
    }
    return true;
}

/// Optional id argument: empty string → `nullptr`, invalid → `false`.
bool parse_optional_uuid(const String& text, AurixUuid& storage, const AurixUuid*& ptr, const char* what) {
    ptr = nullptr;
    if (text.is_empty()) {
        return true;
    }
    if (!parse_uuid_arg(text, storage, what)) {
        return false;
    }
    ptr = &storage;
    return true;
}

const char* opt_cstr(const CharString& s) { return s.length() == 0 ? nullptr : s.get_data(); }

String gstr(const std::string& s) { return String::utf8(s.c_str()); }

}  // namespace

AurixVoiceClient::AurixVoiceClient() {
    aurix_client_config_default(&config_);
    set_process(false);
}

AurixVoiceClient::~AurixVoiceClient() { client_.reset(); }

// ---- configuration ---------------------------------------------------------------------------

void AurixVoiceClient::set_auto_reconnect(bool enabled) { config_.auto_reconnect = enabled; }
bool AurixVoiceClient::get_auto_reconnect() const { return config_.auto_reconnect; }
void AurixVoiceClient::set_reconnect_max_attempts(int attempts) { config_.reconnect_max_attempts = std::max(0, attempts); }
int AurixVoiceClient::get_reconnect_max_attempts() const { return static_cast<int>(config_.reconnect_max_attempts); }
void AurixVoiceClient::set_request_timeout_ms(int ms) { config_.request_timeout_ms = std::max(0, ms); }
int AurixVoiceClient::get_request_timeout_ms() const { return static_cast<int>(config_.request_timeout_ms); }
void AurixVoiceClient::set_jitter_target_frames(int frames) { config_.jitter_target_frames = std::max(0, frames); }
int AurixVoiceClient::get_jitter_target_frames() const { return static_cast<int>(config_.jitter_target_frames); }
void AurixVoiceClient::set_vad_gate_enabled(bool enabled) {
    config_.vad_gate = enabled;
    if (client_) client_.set_vad_gate(enabled);
}
bool AurixVoiceClient::get_vad_gate_enabled() const { return config_.vad_gate; }
void AurixVoiceClient::set_follow_channel_policy(bool enabled) { config_.follow_channel_policy = enabled; }
bool AurixVoiceClient::get_follow_channel_policy() const { return config_.follow_channel_policy; }
void AurixVoiceClient::set_media_path_policy(MediaPathPolicy policy) { config_.media_path = static_cast<AurixMediaPathPolicy>(policy); }
AurixVoiceClient::MediaPathPolicy AurixVoiceClient::get_media_path_policy() const { return static_cast<MediaPathPolicy>(config_.media_path); }
void AurixVoiceClient::set_quic_enabled(bool enabled) { config_.quic = enabled; }
bool AurixVoiceClient::get_quic_enabled() const { return config_.quic; }
void AurixVoiceClient::set_auto_capture(bool enabled) { auto_capture_ = enabled; }
bool AurixVoiceClient::get_auto_capture() const { return auto_capture_; }
void AurixVoiceClient::set_auto_playback(bool enabled) { auto_playback_ = enabled; }
bool AurixVoiceClient::get_auto_playback() const { return auto_playback_; }
void AurixVoiceClient::set_playback_buffer_seconds(double seconds) { playback_buffer_seconds_ = std::clamp(seconds, 0.02, 1.0); }
double AurixVoiceClient::get_playback_buffer_seconds() const { return playback_buffer_seconds_; }
void AurixVoiceClient::set_playback_bus(const StringName& bus) {
    playback_bus_ = bus;
    if (out_player_) out_player_->set_bus(bus);
}
StringName AurixVoiceClient::get_playback_bus() const { return playback_bus_; }
void AurixVoiceClient::set_playback_mode(PlaybackMode mode) {
    playback_mode_ = mode;
    if (mode == PLAYBACK_PER_PARTICIPANT_ONLY && out_player_) {
        stop_playback();
        playback_requested_ = auto_playback_;
    }
}
AurixVoiceClient::PlaybackMode AurixVoiceClient::get_playback_mode() const { return playback_mode_; }
void AurixVoiceClient::set_max_events_per_frame(int count) { max_events_per_frame_ = std::max(1, count); }
int AurixVoiceClient::get_max_events_per_frame() const { return max_events_per_frame_; }
void AurixVoiceClient::set_dsp_bypass(bool bypass) {
    dsp_bypass_ = bypass;
    if (bypass) {
        aurix_dsp_config_bypass(&config_.dsp);
    } else {
        aurix_dsp_config_default(&config_.dsp);
    }
    if (client_) client_.set_dsp(config_.dsp);
}
bool AurixVoiceClient::get_dsp_bypass() const { return dsp_bypass_; }

// ---- lifecycle -------------------------------------------------------------------------------

int AurixVoiceClient::connect_to_server(const String& ws_url, const String& token) {
    if (ws_url.is_empty() || token.is_empty()) {
        UtilityFunctions::push_error("AurixVoiceClient: ws_url and token are required");
        return AURIX_INVALID_ARGUMENT;
    }
    disconnect_from_server();
    aurix::Config cfg(ws_url.utf8().get_data(), token.utf8().get_data());
    cfg.raw = config_;
    client_ = aurix::Client::create(cfg);
    ++client_generation_;
    if (!client_) {
        UtilityFunctions::push_error(String("AurixVoiceClient: create failed: ") + String::utf8(aurix::last_error().c_str()));
        return AURIX_INVALID_ARGUMENT;
    }
    if (visemes_enabled_) client_.set_visemes(true);
    const AurixResult r = client_.connect();
    if (r != AURIX_OK) {
        UtilityFunctions::push_error(String("AurixVoiceClient: connect failed: ") + String::utf8(aurix::last_error().c_str()));
        client_.reset();
        return r;
    }
    playback_requested_ = auto_playback_;
    set_process(true);
    return AURIX_OK;
}

void AurixVoiceClient::disconnect_from_server() {
    stop_capture();
    stop_playback();
    if (client_) {
        client_.disconnect();
        // Deliver what the shutdown produced (Disconnected / StateChanged) before dropping it.
        pump_events();
        client_.reset();
        emit_signal("state_changed", static_cast<int>(STATE_DISCONNECTED));
    }
    set_process(false);
}

int AurixVoiceClient::set_token(const String& token) {
    if (!client_) return AURIX_NOT_CONNECTED;
    return client_.set_token(token.utf8().get_data());
}

bool AurixVoiceClient::is_client_created() const { return static_cast<bool>(client_); }
int64_t AurixVoiceClient::get_client_generation() const { return client_generation_; }

AurixVoiceClient::ConnectionState AurixVoiceClient::get_connection_state() const {
    return client_ ? static_cast<ConnectionState>(client_.state()) : STATE_DISCONNECTED;
}

Dictionary AurixVoiceClient::get_session() const {
    AurixSessionInfo info;
    if (!client_ || !client_.session(info)) return Dictionary();
    return session_to_dict(info);
}

String AurixVoiceClient::get_endpoint() const {
    return client_ ? String::utf8(client_.endpoint().c_str()) : String();
}

PackedStringArray AurixVoiceClient::get_failover_endpoints() const {
    PackedStringArray out;
    if (!client_) return out;
    for (const std::string& e : client_.failover_endpoints()) {
        out.push_back(String::utf8(e.c_str()));
    }
    return out;
}

AurixVoiceClient::MediaPath AurixVoiceClient::get_media_path() const {
    return client_ ? static_cast<MediaPath>(client_.media_path()) : MEDIA_NONE;
}

bool AurixVoiceClient::network_changed() { return client_ && client_.network_changed(); }

String AurixVoiceClient::get_last_error() const { return String::utf8(aurix::last_error().c_str()); }

String AurixVoiceClient::get_native_version() { return String::utf8(aurix::version().c_str()); }

// ---- channels --------------------------------------------------------------------------------

int64_t AurixVoiceClient::join_channel(const String& channel_id, const String& join_token) {
    AurixUuid channel;
    if (!client_ || !parse_uuid_arg(channel_id, channel, "channel")) return 0;
    uint64_t request_id = 0;
    const CharString tok = join_token.utf8();
    if (client_.join_channel(aurix::Uuid(channel), opt_cstr(tok), &request_id) != AURIX_OK) {
        UtilityFunctions::push_error(String("AurixVoiceClient: join failed: ") + get_last_error());
        return 0;
    }
    return static_cast<int64_t>(request_id);
}

int AurixVoiceClient::leave_channel(const String& channel_id) {
    AurixUuid channel;
    if (!client_) return AURIX_NOT_CONNECTED;
    if (!parse_uuid_arg(channel_id, channel, "channel")) return AURIX_INVALID_ARGUMENT;
    return client_.leave_channel(aurix::Uuid(channel));
}

PackedStringArray AurixVoiceClient::get_joined_channels() const {
    PackedStringArray out;
    if (!client_) return out;
    for (const aurix::Uuid& c : client_.joined_channels()) {
        out.push_back(uuid_to_string(c.raw));
    }
    return out;
}

Array AurixVoiceClient::get_participants(const String& channel_id) const {
    Array out;
    AurixUuid channel;
    if (!client_ || !uuid_from_string(channel_id, channel)) return out;
    for (const AurixParticipant& p : client_.participants(aurix::Uuid(channel))) {
        out.push_back(participant_to_dict(p));
    }
    return out;
}

Dictionary AurixVoiceClient::get_channel_info(const String& channel_id) const {
    AurixUuid channel;
    AurixChannelInfo info;
    if (!client_ || !uuid_from_string(channel_id, channel) || !client_.channel_info(aurix::Uuid(channel), info)) {
        return Dictionary();
    }
    return channel_info_to_dict(info);
}

bool AurixVoiceClient::can_speak_in(const String& channel_id) const {
    AurixUuid channel;
    return client_ && uuid_from_string(channel_id, channel) && client_.can_speak_in(aurix::Uuid(channel));
}

bool AurixVoiceClient::channel_transcribes(const String& channel_id) const {
    AurixUuid channel;
    return client_ && uuid_from_string(channel_id, channel) && client_.channel_transcribes(aurix::Uuid(channel));
}

bool AurixVoiceClient::channel_monitored(const String& channel_id) const {
    AurixUuid channel;
    return client_ && uuid_from_string(channel_id, channel) && client_.channel_monitored(aurix::Uuid(channel));
}

Dictionary AurixVoiceClient::get_channel_scope(const String& channel_id) const {
    AurixUuid channel;
    AurixChannelScope scope;
    if (!client_ || !uuid_from_string(channel_id, channel) || !client_.channel_scope(aurix::Uuid(channel), scope)) {
        return Dictionary();
    }
    Dictionary d;
    d["roster_radius"] = scope.roster_radius;
    d["text_radius"] = scope.text_radius;
    return d;
}

String AurixVoiceClient::user_for_ssrc(int64_t ssrc) const {
    aurix::Uuid user;
    if (!client_ || !client_.user_for_ssrc(static_cast<uint32_t>(ssrc), user)) return String();
    return uuid_to_string(user.raw);
}

// ---- microphone ------------------------------------------------------------------------------

bool AurixVoiceClient::start_capture() {
    if (mic_player_) return true;
    AudioServer* server = AudioServer::get_singleton();
    if (!server) return false;

    int bus = server->get_bus_index(kCaptureBusName);
    if (bus < 0) {
        bus = server->get_bus_count();
        server->add_bus(bus);
        server->set_bus_name(bus, kCaptureBusName);
        // Only the effect sees the microphone; nothing of it reaches the speakers.
        server->set_bus_mute(bus, true);
        capture_effect_.instantiate();
        capture_effect_->set_buffer_length(0.2);
        server->add_bus_effect(bus, capture_effect_);
    } else {
        capture_effect_ = server->get_bus_effect(bus, 0);
        if (capture_effect_.is_null()) {
            UtilityFunctions::push_error("AurixVoiceClient: bus 'AurixCapture' exists without an AudioEffectCapture");
            return false;
        }
    }
    capture_bus_index_ = bus;

    Ref<AudioStreamMicrophone> mic;
    mic.instantiate();
    mic_player_ = memnew(AudioStreamPlayer);
    mic_player_->set_name("AurixMicrophone");
    mic_player_->set_stream(mic);
    mic_player_->set_bus(StringName(kCaptureBusName));
    add_child(mic_player_);
    mic_player_->play();
    capture_effect_->clear_buffer();
    return true;
}

void AurixVoiceClient::stop_capture() {
    if (mic_player_) {
        mic_player_->stop();
        remove_child(mic_player_);
        memdelete(mic_player_);
        mic_player_ = nullptr;
    }
    if (client_) client_.reset_capture();
}

void AurixVoiceClient::destroy_capture_bus() {
    AudioServer* server = AudioServer::get_singleton();
    if (server && capture_bus_index_ >= 0) {
        const int idx = server->get_bus_index(kCaptureBusName);
        if (idx >= 0) server->remove_bus(idx);
    }
    capture_bus_index_ = -1;
    capture_effect_.unref();
}

bool AurixVoiceClient::is_capturing() const { return mic_player_ != nullptr; }

void AurixVoiceClient::feed_capture() {
    if (!client_ || capture_effect_.is_null()) return;
    const int available = capture_effect_->get_frames_available();
    if (available <= 0) return;
    const PackedVector2Array frames = capture_effect_->get_buffer(available);
    if (frames.is_empty()) return;
    const uint32_t rate = static_cast<uint32_t>(AudioServer::get_singleton()->get_mix_rate());
    capture_scratch_.resize(static_cast<size_t>(frames.size()) * 2);
    const Vector2* in = frames.ptr();
    for (int64_t i = 0; i < frames.size(); ++i) {
        capture_scratch_[static_cast<size_t>(i) * 2] = in[i].x;
        capture_scratch_[static_cast<size_t>(i) * 2 + 1] = in[i].y;
    }
    client_.push_capture(capture_scratch_.data(), capture_scratch_.size(), rate, 2);
}

void AurixVoiceClient::push_capture(const PackedVector2Array& frames, int sample_rate_hz) {
    if (!client_ || frames.is_empty() || sample_rate_hz <= 0) return;
    capture_scratch_.resize(static_cast<size_t>(frames.size()) * 2);
    const Vector2* in = frames.ptr();
    for (int64_t i = 0; i < frames.size(); ++i) {
        capture_scratch_[static_cast<size_t>(i) * 2] = in[i].x;
        capture_scratch_[static_cast<size_t>(i) * 2 + 1] = in[i].y;
    }
    client_.push_capture(capture_scratch_.data(), capture_scratch_.size(), static_cast<uint32_t>(sample_rate_hz), 2);
}

void AurixVoiceClient::push_capture_mono(const PackedFloat32Array& samples, int sample_rate_hz) {
    if (!client_ || samples.is_empty() || sample_rate_hz <= 0) return;
    client_.push_capture(samples.ptr(), static_cast<size_t>(samples.size()), static_cast<uint32_t>(sample_rate_hz), 1);
}

void AurixVoiceClient::set_muted(bool muted) { if (client_) client_.set_muted(muted); }
bool AurixVoiceClient::is_muted() const { return client_ && client_.is_muted(); }
bool AurixVoiceClient::is_speaking() const { return client_ && client_.is_speaking(); }
void AurixVoiceClient::set_input_gain(double gain) { if (client_) client_.set_input_gain(static_cast<float>(gain)); }
double AurixVoiceClient::get_input_energy() const { return client_ ? client_.input_energy() : 0.0; }
void AurixVoiceClient::set_vad(double threshold, int hangover_frames) {
    if (client_) client_.set_vad(static_cast<float>(threshold), static_cast<uint32_t>(std::max(0, hangover_frames)));
}
void AurixVoiceClient::set_vad_gate(bool enabled) { set_vad_gate_enabled(enabled); }
int AurixVoiceClient::set_bitrate(int bps) { return client_ ? client_.set_bitrate(static_cast<uint32_t>(std::max(0, bps))) : AURIX_NOT_CONNECTED; }
int AurixVoiceClient::set_complexity(int complexity) {
    return client_ ? client_.set_complexity(static_cast<int8_t>(std::clamp(complexity, -1, 10))) : AURIX_NOT_CONNECTED;
}

int AurixVoiceClient::set_encoder_settings(const Dictionary& settings) {
    AurixEncoderSettings e = config_.encoder;
    if (client_) client_.encoder_settings(e);
    encoder_from_dict(settings, e);
    config_.encoder = e;
    return client_ ? client_.set_encoder_settings(e) : AURIX_OK;
}

Dictionary AurixVoiceClient::get_encoder_settings() const {
    AurixEncoderSettings e = config_.encoder;
    if (client_) client_.encoder_settings(e);
    return encoder_to_dict(e);
}

Dictionary AurixVoiceClient::get_audio_policy() const {
    AurixAudioPolicy p;
    if (!client_ || !client_.audio_policy(p)) return Dictionary();
    return audio_policy_to_dict(p);
}

int AurixVoiceClient::set_loss_adaptation(LossAdaptation adaptation) {
    config_.loss_adaptation = static_cast<AurixLossAdaptation>(adaptation);
    return client_ ? client_.set_loss_adaptation(config_.loss_adaptation) : AURIX_OK;
}

AurixVoiceClient::LossAdaptation AurixVoiceClient::get_loss_adaptation() const {
    return static_cast<LossAdaptation>(client_ ? client_.loss_adaptation() : config_.loss_adaptation);
}

AurixVoiceClient::LossProfile AurixVoiceClient::get_loss_profile() const {
    return static_cast<LossProfile>(client_ ? client_.loss_profile() : AURIX_LOSS_PROFILE_LOW);
}

int AurixVoiceClient::set_decoder_settings(const Dictionary& settings) {
    AurixDecoderSettings d = config_.decoder;
    if (client_) client_.decoder_settings(d);
    decoder_from_dict(settings, d);
    config_.decoder = d;
    return client_ ? client_.set_decoder_settings(d) : AURIX_OK;
}

Dictionary AurixVoiceClient::get_decoder_settings() const {
    AurixDecoderSettings d = config_.decoder;
    if (client_) client_.decoder_settings(d);
    return decoder_to_dict(d);
}

bool AurixVoiceClient::is_dred_supported() { return aurix::Client::dred_supported(); }

int AurixVoiceClient::set_dsp(const Dictionary& config) {
    AurixDspConfig c = config_.dsp;
    if (client_) client_.dsp(c);
    dsp_from_dict(config, c);
    config_.dsp = c;
    return client_ ? client_.set_dsp(c) : AURIX_OK;
}

Dictionary AurixVoiceClient::get_dsp() const {
    AurixDspConfig c = config_.dsp;
    if (client_) client_.dsp(c);
    return dsp_to_dict(c);
}

Dictionary AurixVoiceClient::get_dsp_stats() const {
    AurixDspStats s;
    if (!client_ || !client_.dsp_stats(s)) return Dictionary();
    return dsp_stats_to_dict(s);
}

int AurixVoiceClient::set_voice_effects(const Dictionary& effects) {
    if (!client_) return AURIX_NOT_CONNECTED;
    AurixVoiceEffects fx{};
    voice_effects_from_dict(effects, fx);
    return client_.set_voice_effects(fx);
}

Dictionary AurixVoiceClient::get_voice_effects() const {
    AurixVoiceEffects fx{};
    if (client_) client_.voice_effects(fx);
    return voice_effects_to_dict(fx);
}

Dictionary AurixVoiceClient::get_voice_preset(VoicePreset preset) const {
    return voice_effects_to_dict(aurix::Client::voice_preset(static_cast<AurixVoicePreset>(preset)));
}

int AurixVoiceClient::set_voice_preset(VoicePreset preset) {
    if (!client_) return AURIX_NOT_CONNECTED;
    return client_.set_voice_preset(static_cast<AurixVoicePreset>(preset));
}

void AurixVoiceClient::set_visemes_enabled(bool enabled) {
    visemes_enabled_ = enabled;
    if (client_) client_.set_visemes(enabled);
}

bool AurixVoiceClient::get_visemes_enabled() const { return visemes_enabled_; }

Dictionary AurixVoiceClient::get_participant_visemes(const String& user_id) const {
    AurixUuid user;
    AurixVisemeFrame f;
    if (!client_ || !uuid_from_string(user_id, user) || !client_.participant_visemes(aurix::Uuid(user), f)) {
        return Dictionary();
    }
    return viseme_frame_to_dict(f);
}

Dictionary AurixVoiceClient::get_local_visemes() const {
    AurixVisemeFrame f;
    if (!client_ || !client_.local_visemes(f)) return Dictionary();
    return viseme_frame_to_dict(f);
}

void AurixVoiceClient::reset_capture() { if (client_) client_.reset_capture(); }

// ---- playback --------------------------------------------------------------------------------

bool AurixVoiceClient::start_playback() {
    playback_requested_ = true;
    if (out_player_) return true;
    if (playback_mode_ == PLAYBACK_PER_PARTICIPANT_ONLY) return false;

    Ref<AudioStreamGenerator> gen;
    gen.instantiate();
    gen->set_mix_rate(static_cast<float>(kMixRate));
    gen->set_buffer_length(static_cast<float>(playback_buffer_seconds_));
    out_player_ = memnew(AudioStreamPlayer);
    out_player_->set_name("AurixPlayback");
    out_player_->set_stream(gen);
    out_player_->set_bus(playback_bus_);
    add_child(out_player_);
    out_player_->play();
    out_playback_ = Ref<AudioStreamGeneratorPlayback>(Object::cast_to<AudioStreamGeneratorPlayback>(out_player_->get_stream_playback().ptr()));
    if (out_playback_.is_null()) {
        UtilityFunctions::push_error("AurixVoiceClient: AudioStreamGenerator produced no playback");
        stop_playback();
        return false;
    }
    return true;
}

void AurixVoiceClient::stop_playback() {
    playback_requested_ = false;
    out_playback_.unref();
    if (out_player_) {
        out_player_->stop();
        remove_child(out_player_);
        memdelete(out_player_);
        out_player_ = nullptr;
    }
}

bool AurixVoiceClient::is_playing() const { return out_player_ != nullptr && out_player_->is_playing(); }

void AurixVoiceClient::feed_playback() {
    if (!client_ || out_playback_.is_null()) return;
    if (out_player_ && !out_player_->is_playing()) {
        // The engine stops a generator that ran dry; resume and refill.
        out_player_->play();
        out_playback_ = Ref<AudioStreamGeneratorPlayback>(Object::cast_to<AudioStreamGeneratorPlayback>(out_player_->get_stream_playback().ptr()));
        if (out_playback_.is_null()) return;
    }
    const int frames = out_playback_->get_frames_available();
    if (frames <= 0) return;
    out_playback_->push_buffer(mix_output(frames));
}

PackedVector2Array AurixVoiceClient::mix_output(int frames) {
    PackedVector2Array out;
    if (frames <= 0) return out;
    out.resize(frames);
    Vector2* dst = out.ptrw();
    if (!client_) {
        std::fill(dst, dst + frames, Vector2());
        return out;
    }
    mix_scratch_.assign(static_cast<size_t>(frames) * 2, 0.0f);
    client_.mix_output(mix_scratch_.data(), mix_scratch_.size(), 2);
    for (int i = 0; i < frames; ++i) {
        dst[i] = Vector2(mix_scratch_[static_cast<size_t>(i) * 2], mix_scratch_[static_cast<size_t>(i) * 2 + 1]);
    }
    return out;
}

PackedVector2Array AurixVoiceClient::pull_participant(const String& user_id, int frames) {
    PackedVector2Array out;
    if (frames <= 0) return out;
    out.resize(frames);
    Vector2* dst = out.ptrw();
    std::fill(dst, dst + frames, Vector2());
    AurixUuid user;
    if (!client_ || !uuid_from_string(user_id, user)) return out;
    mix_scratch_.assign(static_cast<size_t>(frames) * 2, 0.0f);
    client_.pull_participant(aurix::Uuid(user), mix_scratch_.data(), mix_scratch_.size(), 2);
    for (int i = 0; i < frames; ++i) {
        dst[i] = Vector2(mix_scratch_[static_cast<size_t>(i) * 2], mix_scratch_[static_cast<size_t>(i) * 2 + 1]);
    }
    return out;
}

int AurixVoiceClient::set_participant_claimed(const String& user_id, bool claimed) {
    AurixUuid user;
    if (!client_) return AURIX_NOT_CONNECTED;
    if (!parse_uuid_arg(user_id, user, "user")) return AURIX_INVALID_ARGUMENT;
    return client_.set_participant_claimed(aurix::Uuid(user), claimed);
}

Array AurixVoiceClient::get_participant_streams() const {
    Array out;
    if (!client_) return out;
    for (const AurixParticipantStream& s : client_.participant_streams()) {
        out.push_back(stream_to_dict(s));
    }
    return out;
}

void AurixVoiceClient::push_render(const PackedVector2Array& frames) {
    if (!client_ || frames.is_empty()) return;
    mix_scratch_.resize(static_cast<size_t>(frames.size()) * 2);
    const Vector2* in = frames.ptr();
    for (int64_t i = 0; i < frames.size(); ++i) {
        mix_scratch_[static_cast<size_t>(i) * 2] = in[i].x;
        mix_scratch_[static_cast<size_t>(i) * 2 + 1] = in[i].y;
    }
    client_.push_render(mix_scratch_.data(), mix_scratch_.size(), 2);
}

void AurixVoiceClient::set_output_volume(double volume) { if (client_) client_.set_output_volume(static_cast<float>(volume)); }
void AurixVoiceClient::set_output_muted(bool muted) { if (client_) client_.set_output_muted(muted); }

// ---- receiver preferences --------------------------------------------------------------------

int AurixVoiceClient::set_participant_mute(const String& user_id, const String& channel_id, bool muted) {
    AurixUuid user, channel_storage;
    const AurixUuid* channel = nullptr;
    if (!client_) return AURIX_NOT_CONNECTED;
    if (!parse_uuid_arg(user_id, user, "user") || !parse_optional_uuid(channel_id, channel_storage, channel, "channel")) {
        return AURIX_INVALID_ARGUMENT;
    }
    aurix::Uuid ch;
    if (channel) ch = aurix::Uuid(*channel);
    return client_.set_participant_mute(aurix::Uuid(user), channel ? &ch : nullptr, muted);
}

int AurixVoiceClient::set_participant_volume(const String& user_id, double volume) {
    AurixUuid user;
    if (!client_) return AURIX_NOT_CONNECTED;
    if (!parse_uuid_arg(user_id, user, "user")) return AURIX_INVALID_ARGUMENT;
    return client_.set_participant_volume(aurix::Uuid(user), static_cast<float>(volume));
}

int AurixVoiceClient::set_user_block(const String& user_id, bool blocked) {
    AurixUuid user;
    if (!client_) return AURIX_NOT_CONNECTED;
    if (!parse_uuid_arg(user_id, user, "user")) return AURIX_INVALID_ARGUMENT;
    return client_.set_user_block(aurix::Uuid(user), blocked);
}

int AurixVoiceClient::set_priority(const String& channel_id, const String& user_id, bool priority) {
    AurixUuid channel;
    AurixUuid user;
    if (!client_) return AURIX_NOT_CONNECTED;
    if (!parse_uuid_arg(channel_id, channel, "channel")) return AURIX_INVALID_ARGUMENT;
    if (!user_id.is_empty() && !parse_uuid_arg(user_id, user, "user")) return AURIX_INVALID_ARGUMENT;
    const aurix::Uuid self(user);
    return client_.set_priority(aurix::Uuid(channel), user_id.is_empty() ? nullptr : &self, priority);
}

bool AurixVoiceClient::is_ducking_active(const String& channel_id) const {
    AurixUuid channel;
    return client_ && uuid_from_string(channel_id, channel) && client_.ducking_active(aurix::Uuid(channel));
}

int AurixVoiceClient::set_transmission(TransmissionMode mode, const String& channel_id) {
    AurixUuid storage;
    const AurixUuid* channel = nullptr;
    if (!client_) return AURIX_NOT_CONNECTED;
    if (!parse_optional_uuid(channel_id, storage, channel, "channel")) return AURIX_INVALID_ARGUMENT;
    aurix::Uuid ch;
    if (channel) ch = aurix::Uuid(*channel);
    return client_.set_transmission(static_cast<AurixTransmissionMode>(mode), channel ? &ch : nullptr);
}

int AurixVoiceClient::set_channel_focus(const String& channel_id) {
    AurixUuid storage;
    const AurixUuid* channel = nullptr;
    if (!client_) return AURIX_NOT_CONNECTED;
    if (!parse_optional_uuid(channel_id, storage, channel, "channel")) return AURIX_INVALID_ARGUMENT;
    aurix::Uuid ch;
    if (channel) ch = aurix::Uuid(*channel);
    return client_.set_channel_focus(channel ? &ch : nullptr);
}

int AurixVoiceClient::set_audio_codec(AudioCodec codec) {
    return client_ ? client_.set_audio_codec(static_cast<AurixAudioCodec>(codec)) : AURIX_NOT_CONNECTED;
}
AurixVoiceClient::AudioCodec AurixVoiceClient::get_audio_codec() const {
    return client_ ? static_cast<AudioCodec>(client_.audio_codec()) : CODEC_OPUS;
}
int AurixVoiceClient::set_downlink_mode(DownlinkMode mode) {
    return client_ ? client_.set_downlink_mode(static_cast<AurixDownlinkMode>(mode)) : AURIX_NOT_CONNECTED;
}
AurixVoiceClient::DownlinkMode AurixVoiceClient::get_downlink_mode() const {
    return client_ ? static_cast<DownlinkMode>(client_.downlink_mode()) : DOWNLINK_STREAMS;
}
int AurixVoiceClient::set_transcripts(bool enabled) { return client_ ? client_.set_transcripts(enabled) : AURIX_NOT_CONNECTED; }

int AurixVoiceClient::set_translation(const String& language, const String& spoken_language, bool speech) {
    if (!client_) return AURIX_NOT_CONNECTED;
    const CharString lang = language.utf8();
    const CharString spoken = spoken_language.utf8();
    return client_.set_translation(opt_cstr(lang), opt_cstr(spoken), speech);
}

int AurixVoiceClient::update_positions(const String& channel_id, const Array& positions) {
    AurixUuid channel;
    if (!client_) return AURIX_NOT_CONNECTED;
    if (!parse_uuid_arg(channel_id, channel, "channel")) return AURIX_INVALID_ARGUMENT;
    std::vector<AurixPosition> out;
    out.reserve(static_cast<size_t>(positions.size()));
    for (int64_t i = 0; i < positions.size(); ++i) {
        const Dictionary d = positions[i];
        AurixPosition p{};
        if (!uuid_from_string(d.get("user_id", String()), p.user_id)) {
            UtilityFunctions::push_error("AurixVoiceClient: update_positions entry without a valid user_id");
            return AURIX_INVALID_ARGUMENT;
        }
        const Vector3 pos = d.get("position", Vector3());
        const Vector3 fwd = d.get("forward", Vector3(0, 0, -1));
        const Vector3 up = d.get("up", Vector3(0, 1, 0));
        p.x = pos.x; p.y = pos.y; p.z = pos.z;
        p.forward_x = fwd.x; p.forward_y = fwd.y; p.forward_z = fwd.z;
        p.up_x = up.x; p.up_y = up.y; p.up_z = up.z;
        out.push_back(p);
    }
    return client_.update_positions(aurix::Uuid(channel), out.data(), out.size());
}

int AurixVoiceClient::update_transforms(const String& channel_id, const Dictionary& transforms) {
    Array positions;
    const Array keys = transforms.keys();
    for (int64_t i = 0; i < keys.size(); ++i) {
        const Transform3D t = transforms[keys[i]];
        Dictionary d;
        d["user_id"] = keys[i];
        d["position"] = t.origin;
        // Godot looks down -Z; the channel's `right_handed` coordinate setting mirrors it server-side.
        d["forward"] = -t.basis.get_column(2).normalized();
        d["up"] = t.basis.get_column(1).normalized();
        positions.push_back(d);
    }
    return update_positions(channel_id, positions);
}

int AurixVoiceClient::respond_recording_consent(const String& recording_id, bool accepted) {
    AurixUuid rec;
    if (!client_) return AURIX_NOT_CONNECTED;
    if (!parse_uuid_arg(recording_id, rec, "recording")) return AURIX_INVALID_ARGUMENT;
    return client_.respond_recording_consent(aurix::Uuid(rec), accepted ? AURIX_CONSENT_ACCEPTED : AURIX_CONSENT_DECLINED);
}

int AurixVoiceClient::send_control_json(const String& json) {
    return client_ ? client_.send_control_json(json.utf8().get_data()) : AURIX_NOT_CONNECTED;
}

// ---- chat / moderation / speech --------------------------------------------------------------

int64_t AurixVoiceClient::send_chat(const String& channel_id, const String& text, const String& metadata_json) {
    AurixUuid channel;
    if (!client_ || !parse_uuid_arg(channel_id, channel, "channel")) return 0;
    uint64_t request_id = 0;
    const CharString meta = metadata_json.utf8();
    if (client_.send_chat(aurix::Uuid(channel), text.utf8().get_data(), opt_cstr(meta), &request_id) != AURIX_OK) return 0;
    return static_cast<int64_t>(request_id);
}

int64_t AurixVoiceClient::send_direct_chat(const String& user_id, const String& text, const String& metadata_json) {
    AurixUuid user;
    if (!client_ || !parse_uuid_arg(user_id, user, "user")) return 0;
    uint64_t request_id = 0;
    const CharString meta = metadata_json.utf8();
    if (client_.send_direct_chat(aurix::Uuid(user), text.utf8().get_data(), opt_cstr(meta), &request_id) != AURIX_OK) return 0;
    return static_cast<int64_t>(request_id);
}

int AurixVoiceClient::set_typing(const String& channel_id, bool typing) {
    AurixUuid channel;
    if (!client_) return AURIX_NOT_CONNECTED;
    if (!parse_uuid_arg(channel_id, channel, "channel")) return AURIX_INVALID_ARGUMENT;
    return client_.set_typing(aurix::Uuid(channel), typing);
}

int64_t AurixVoiceClient::channel_history(const String& channel_id, const String& before, const String& after, int limit) {
    AurixUuid channel;
    if (!client_ || !parse_uuid_arg(channel_id, channel, "channel")) return 0;
    uint64_t request_id = 0;
    const CharString b = before.utf8(), a = after.utf8();
    if (client_.channel_history(aurix::Uuid(channel), opt_cstr(b), opt_cstr(a), static_cast<uint32_t>(std::max(0, limit)), &request_id) != AURIX_OK) return 0;
    return static_cast<int64_t>(request_id);
}

int64_t AurixVoiceClient::direct_history(const String& user_id, const String& before, const String& after, int limit) {
    AurixUuid user;
    if (!client_ || !parse_uuid_arg(user_id, user, "user")) return 0;
    uint64_t request_id = 0;
    const CharString b = before.utf8(), a = after.utf8();
    if (client_.direct_history(aurix::Uuid(user), opt_cstr(b), opt_cstr(a), static_cast<uint32_t>(std::max(0, limit)), &request_id) != AURIX_OK) return 0;
    return static_cast<int64_t>(request_id);
}

int AurixVoiceClient::mark_channel_read(const String& channel_id, const String& message_id) {
    AurixUuid channel, message;
    if (!client_) return AURIX_NOT_CONNECTED;
    if (!parse_uuid_arg(channel_id, channel, "channel") || !parse_uuid_arg(message_id, message, "message")) return AURIX_INVALID_ARGUMENT;
    return client_.mark_channel_read(aurix::Uuid(channel), aurix::Uuid(message));
}

int AurixVoiceClient::mark_direct_read(const String& user_id, const String& message_id) {
    AurixUuid user, message;
    if (!client_) return AURIX_NOT_CONNECTED;
    if (!parse_uuid_arg(user_id, user, "user") || !parse_uuid_arg(message_id, message, "message")) return AURIX_INVALID_ARGUMENT;
    return client_.mark_direct_read(aurix::Uuid(user), aurix::Uuid(message));
}

int AurixVoiceClient::channel_read_markers(const String& channel_id) {
    AurixUuid channel;
    if (!client_) return AURIX_NOT_CONNECTED;
    if (!parse_uuid_arg(channel_id, channel, "channel")) return AURIX_INVALID_ARGUMENT;
    return client_.channel_read_markers(aurix::Uuid(channel));
}

int AurixVoiceClient::direct_read_markers(const String& user_id) {
    AurixUuid user;
    if (!client_) return AURIX_NOT_CONNECTED;
    if (!parse_uuid_arg(user_id, user, "user")) return AURIX_INVALID_ARGUMENT;
    return client_.direct_read_markers(aurix::Uuid(user));
}

int64_t AurixVoiceClient::moderate(const String& channel_id, const String& user_id, ModerationAction action,
                                   const String& action_token, const String& reason) {
    AurixUuid channel, user;
    if (!client_ || !parse_uuid_arg(channel_id, channel, "channel") || !parse_uuid_arg(user_id, user, "user")) return 0;
    uint64_t request_id = 0;
    const CharString why = reason.utf8();
    if (client_.moderate(aurix::Uuid(channel), aurix::Uuid(user), static_cast<AurixModerationAction>(action),
                         action_token.utf8().get_data(), opt_cstr(why), &request_id) != AURIX_OK) {
        return 0;
    }
    return static_cast<int64_t>(request_id);
}

int64_t AurixVoiceClient::speak(const String& text, const String& channel_id, TtsDestination destination, const String& voice) {
    AurixUuid storage;
    const AurixUuid* channel = nullptr;
    if (!client_ || !parse_optional_uuid(channel_id, storage, channel, "channel")) return 0;
    aurix::Uuid ch;
    if (channel) ch = aurix::Uuid(*channel);
    uint64_t request_id = 0;
    const CharString v = voice.utf8();
    if (client_.speak(text.utf8().get_data(), channel ? &ch : nullptr, static_cast<AurixTtsDestination>(destination), opt_cstr(v), &request_id) != AURIX_OK) {
        return 0;
    }
    return static_cast<int64_t>(request_id);
}

int AurixVoiceClient::cancel_speech() { return client_ ? client_.cancel_speech() : AURIX_NOT_CONNECTED; }

// ---- diagnostics -----------------------------------------------------------------------------

Dictionary AurixVoiceClient::get_stats() const {
    AurixStats s;
    if (!client_ || !client_.stats(s)) return Dictionary();
    return stats_to_dict(s);
}

Dictionary AurixVoiceClient::get_network_quality() const {
    AurixNetworkQuality q;
    if (!client_ || !client_.network_quality(q)) return Dictionary();
    return network_quality_to_dict(q);
}

// ---- frame loop ------------------------------------------------------------------------------

void AurixVoiceClient::_process(double) {
    if (Engine::get_singleton()->is_editor_hint()) return;
    feed_capture();
    pump_events();
    feed_playback();
}

void AurixVoiceClient::_exit_tree() {
    disconnect_from_server();
    destroy_capture_bus();
}

void AurixVoiceClient::pump_events() {
    for (int i = 0; i < max_events_per_frame_ && client_; ++i) {
        aurix::Event ev = client_.poll_event();
        if (!ev) break;
        dispatch(ev);
    }
}

void AurixVoiceClient::dispatch(const aurix::Event& ev) {
    const String channel = uuid_to_string(ev.channel_id().raw);
    const String user = uuid_to_string(ev.user_id().raw);
    emit_signal("raw_event", static_cast<int>(ev.type()), gstr(ev.json()));

    switch (ev.type()) {
    case AURIX_EVENT_STATE_CHANGED:
        emit_signal("state_changed", static_cast<int>(ev.state()));
        break;
    case AURIX_EVENT_SESSION_READY: {
        AurixSessionInfo info;
        if (ev.session(info)) {
            if (auto_capture_ && !is_capturing()) start_capture();
            if (playback_requested_ && !out_player_) start_playback();
            // The event payload carries no user id; the client snapshot does.
            AurixSessionInfo snapshot;
            if (client_.session(snapshot)) info = snapshot;
            emit_signal("session_ready", session_to_dict(info));
        }
        break;
    }
    case AURIX_EVENT_MEDIA_BOUND:
        emit_signal("media_bound");
        break;
    case AURIX_EVENT_CHANNEL_JOINED: {
        Array participants;
        for (const AurixParticipant& p : ev.participants()) participants.push_back(participant_to_dict(p));
        emit_signal("channel_joined", static_cast<int64_t>(ev.request_id()), channel, participants, channel_info_to_dict(ev.channel_info()));
        break;
    }
    case AURIX_EVENT_CHANNEL_LEFT:
        emit_signal("channel_left", channel);
        break;
    case AURIX_EVENT_PARTICIPANT_JOINED: {
        AurixParticipant p;
        if (aurix_event_participant(ev.raw(), 0, &p)) emit_signal("participant_joined", channel, participant_to_dict(p));
        break;
    }
    case AURIX_EVENT_PARTICIPANT_LEFT:
        emit_signal("participant_left", channel, user);
        break;
    case AURIX_EVENT_PARTICIPANT_MUTE_CHANGED:
        emit_signal("participant_mute_changed", channel, user, ev.flag(), ev.flag2());
        break;
    case AURIX_EVENT_PARTICIPANT_SPEAKING:
        emit_signal("participant_speaking", channel, user, ev.flag());
        break;
    case AURIX_EVENT_PARTICIPANT_PRIORITY_CHANGED:
        emit_signal("participant_priority_changed", channel, user, ev.flag());
        break;
    case AURIX_EVENT_DUCKING_CHANGED:
        emit_signal("ducking_changed", channel, ev.flag(), ducking_to_dict(ev.ducking()));
        break;
    case AURIX_EVENT_CHANNEL_ENERGY: {
        Dictionary levels;
        for (const AurixParticipant& p : ev.participants()) levels[uuid_to_string(p.user_id)] = p.energy;
        emit_signal("channel_energy", channel, levels);
        break;
    }
    case AURIX_EVENT_LOCAL_SPEAKING:
        emit_signal("local_speaking", ev.flag());
        break;
    case AURIX_EVENT_TRANSMISSION_CHANGED:
        emit_signal("transmission_changed", static_cast<int>(ev.transmission()), channel);
        break;
    case AURIX_EVENT_CHANNEL_FOCUS_CHANGED:
        emit_signal("channel_focus_changed", channel);
        break;
    case AURIX_EVENT_USER_BLOCK_CHANGED:
        emit_signal("user_block_changed", user, ev.flag());
        break;
    case AURIX_EVENT_RECORDING:
        emit_signal("recording", channel, uuid_to_string(ev.object_id().raw), ev.flag(), ev.flag2(), user);
        break;
    case AURIX_EVENT_BITRATE_CHANGED:
        emit_signal("bitrate_changed", static_cast<int64_t>(ev.number()), gstr(ev.message()));
        break;
    case AURIX_EVENT_KICKED:
        emit_signal("kicked", channel, gstr(ev.message()));
        break;
    case AURIX_EVENT_MODERATION_APPLIED:
        emit_signal("moderation_applied", static_cast<int64_t>(ev.request_id()), channel, user, static_cast<int>(ev.moderation_action()));
        break;
    case AURIX_EVENT_CHAT_MESSAGE: {
        AurixChatMessage m;
        if (ev.chat(m)) emit_signal("chat_message", chat_to_dict(m));
        break;
    }
    case AURIX_EVENT_PARTICIPANT_TYPING:
        emit_signal("participant_typing", channel, user, ev.flag());
        break;
    case AURIX_EVENT_TRANSCRIPT: {
        AurixTranscript t;
        if (ev.transcript(t)) emit_signal("transcript", transcript_to_dict(t));
        break;
    }
    case AURIX_EVENT_TTS_STATUS: {
        AurixTtsStatus t;
        if (ev.tts(t)) emit_signal("tts_status", tts_to_dict(t));
        break;
    }
    case AURIX_EVENT_POSITIONS:
        emit_signal("positions", channel, gstr(ev.json()));
        break;
    case AURIX_EVENT_REJOIN_FAILED:
        emit_signal("rejoin_failed", channel, gstr(ev.code()), gstr(ev.message()));
        break;
    case AURIX_EVENT_REQUEST_FAILED:
        emit_signal("request_failed", static_cast<int64_t>(ev.request_id()), gstr(ev.code()), gstr(ev.message()));
        break;
    case AURIX_EVENT_SERVER_ERROR:
        emit_signal("server_error", gstr(ev.code()), gstr(ev.message()));
        break;
    case AURIX_EVENT_RECOVERING:
        emit_signal("recovering", static_cast<int64_t>(ev.number()), static_cast<int64_t>(ev.number2()), gstr(ev.message()));
        break;
    case AURIX_EVENT_RECOVERED:
        emit_signal("recovered", ev.flag(), ev.flag2());
        break;
    case AURIX_EVENT_FAILED_TO_RECOVER:
        emit_signal("failed_to_recover", gstr(ev.message()));
        break;
    case AURIX_EVENT_DISCONNECTED:
        emit_signal("disconnected", gstr(ev.message()));
        break;
    case AURIX_EVENT_NETWORK_QUALITY: {
        AurixNetworkQuality q;
        if (client_ && client_.network_quality(q)) emit_signal("network_quality", network_quality_to_dict(q));
        break;
    }
    case AURIX_EVENT_AUDIO_POLICY_CHANGED: {
        AurixAudioPolicy p;
        if (ev.audio_policy(p)) emit_signal("audio_policy_changed", audio_policy_to_dict(p));
        break;
    }
    case AURIX_EVENT_AUDIO_CODEC_CHANGED:
        emit_signal("audio_codec_changed", static_cast<int>(ev.audio_codec()));
        break;
    case AURIX_EVENT_LOSS_PROFILE_CHANGED:
        emit_signal("loss_profile_changed", static_cast<int>(ev.loss_profile()), static_cast<int>(ev.number2()));
        break;
    case AURIX_EVENT_MEDIA_PATH_CHANGED:
        emit_signal("media_path_changed", static_cast<int>(ev.media_path()), gstr(ev.message()));
        break;
    case AURIX_EVENT_DOWNLINK_MODE_CHANGED:
        emit_signal("downlink_mode_changed", static_cast<int>(ev.downlink_mode()));
        break;
    case AURIX_EVENT_ENDPOINT_CHANGED:
        emit_signal("endpoint_changed", gstr(ev.message()));
        break;
    case AURIX_EVENT_CHAT_HISTORY: {
        AurixChatHistory page{};
        if (ev.chat_history(page)) {
            Array messages;
            for (const AurixChatMessage& m : ev.chat_history_messages()) messages.push_back(chat_to_dict(m));
            emit_signal("chat_history", static_cast<int64_t>(ev.request_id()), channel, user, messages, cstr(page.next_before), cstr(page.next_after));
        }
        break;
    }
    case AURIX_EVENT_CHAT_READ_MARKER: {
        AurixReadMarker r;
        if (ev.read_marker(r)) emit_signal("chat_read_marker", read_marker_to_dict(r));
        break;
    }
    case AURIX_EVENT_CHAT_READ_MARKERS: {
        Array markers;
        for (const AurixReadMarker& r : ev.read_markers()) markers.push_back(read_marker_to_dict(r));
        emit_signal("chat_read_markers", channel, user, static_cast<int64_t>(ev.number()), markers);
        break;
    }
    case AURIX_EVENT_CHAT_INBOX_SYNCED:
        emit_signal("chat_inbox_synced", static_cast<int64_t>(ev.number()), ev.flag());
        break;
    case AURIX_EVENT_TRANSLATION_CHANGED: {
        AurixTranslation t;
        if (ev.translation(t)) emit_signal("translation_changed", translation_to_dict(t));
        break;
    }
    }
}

// ---- bindings --------------------------------------------------------------------------------

void AurixVoiceClient::_bind_methods() {
    // configuration
    ClassDB::bind_method(D_METHOD("set_auto_reconnect", "enabled"), &AurixVoiceClient::set_auto_reconnect);
    ClassDB::bind_method(D_METHOD("get_auto_reconnect"), &AurixVoiceClient::get_auto_reconnect);
    ClassDB::bind_method(D_METHOD("set_reconnect_max_attempts", "attempts"), &AurixVoiceClient::set_reconnect_max_attempts);
    ClassDB::bind_method(D_METHOD("get_reconnect_max_attempts"), &AurixVoiceClient::get_reconnect_max_attempts);
    ClassDB::bind_method(D_METHOD("set_request_timeout_ms", "ms"), &AurixVoiceClient::set_request_timeout_ms);
    ClassDB::bind_method(D_METHOD("get_request_timeout_ms"), &AurixVoiceClient::get_request_timeout_ms);
    ClassDB::bind_method(D_METHOD("set_jitter_target_frames", "frames"), &AurixVoiceClient::set_jitter_target_frames);
    ClassDB::bind_method(D_METHOD("get_jitter_target_frames"), &AurixVoiceClient::get_jitter_target_frames);
    ClassDB::bind_method(D_METHOD("set_vad_gate_enabled", "enabled"), &AurixVoiceClient::set_vad_gate_enabled);
    ClassDB::bind_method(D_METHOD("get_vad_gate_enabled"), &AurixVoiceClient::get_vad_gate_enabled);
    ClassDB::bind_method(D_METHOD("set_follow_channel_policy", "enabled"), &AurixVoiceClient::set_follow_channel_policy);
    ClassDB::bind_method(D_METHOD("get_follow_channel_policy"), &AurixVoiceClient::get_follow_channel_policy);
    ClassDB::bind_method(D_METHOD("set_media_path_policy", "policy"), &AurixVoiceClient::set_media_path_policy);
    ClassDB::bind_method(D_METHOD("get_media_path_policy"), &AurixVoiceClient::get_media_path_policy);
    ClassDB::bind_method(D_METHOD("set_quic_enabled", "enabled"), &AurixVoiceClient::set_quic_enabled);
    ClassDB::bind_method(D_METHOD("get_quic_enabled"), &AurixVoiceClient::get_quic_enabled);
    ClassDB::bind_method(D_METHOD("set_auto_capture", "enabled"), &AurixVoiceClient::set_auto_capture);
    ClassDB::bind_method(D_METHOD("get_auto_capture"), &AurixVoiceClient::get_auto_capture);
    ClassDB::bind_method(D_METHOD("set_auto_playback", "enabled"), &AurixVoiceClient::set_auto_playback);
    ClassDB::bind_method(D_METHOD("get_auto_playback"), &AurixVoiceClient::get_auto_playback);
    ClassDB::bind_method(D_METHOD("set_playback_buffer_seconds", "seconds"), &AurixVoiceClient::set_playback_buffer_seconds);
    ClassDB::bind_method(D_METHOD("get_playback_buffer_seconds"), &AurixVoiceClient::get_playback_buffer_seconds);
    ClassDB::bind_method(D_METHOD("set_playback_bus", "bus"), &AurixVoiceClient::set_playback_bus);
    ClassDB::bind_method(D_METHOD("get_playback_bus"), &AurixVoiceClient::get_playback_bus);
    ClassDB::bind_method(D_METHOD("set_playback_mode", "mode"), &AurixVoiceClient::set_playback_mode);
    ClassDB::bind_method(D_METHOD("get_playback_mode"), &AurixVoiceClient::get_playback_mode);
    ClassDB::bind_method(D_METHOD("set_max_events_per_frame", "count"), &AurixVoiceClient::set_max_events_per_frame);
    ClassDB::bind_method(D_METHOD("get_max_events_per_frame"), &AurixVoiceClient::get_max_events_per_frame);
    ClassDB::bind_method(D_METHOD("set_dsp_bypass", "bypass"), &AurixVoiceClient::set_dsp_bypass);
    ClassDB::bind_method(D_METHOD("get_dsp_bypass"), &AurixVoiceClient::get_dsp_bypass);
    ClassDB::bind_method(D_METHOD("set_visemes_enabled", "enabled"), &AurixVoiceClient::set_visemes_enabled);
    ClassDB::bind_method(D_METHOD("get_visemes_enabled"), &AurixVoiceClient::get_visemes_enabled);

    ADD_PROPERTY(PropertyInfo(Variant::BOOL, "auto_reconnect"), "set_auto_reconnect", "get_auto_reconnect");
    ADD_PROPERTY(PropertyInfo(Variant::INT, "reconnect_max_attempts"), "set_reconnect_max_attempts", "get_reconnect_max_attempts");
    ADD_PROPERTY(PropertyInfo(Variant::INT, "request_timeout_ms"), "set_request_timeout_ms", "get_request_timeout_ms");
    ADD_PROPERTY(PropertyInfo(Variant::INT, "jitter_target_frames"), "set_jitter_target_frames", "get_jitter_target_frames");
    ADD_PROPERTY(PropertyInfo(Variant::BOOL, "vad_gate"), "set_vad_gate_enabled", "get_vad_gate_enabled");
    ADD_PROPERTY(PropertyInfo(Variant::BOOL, "follow_channel_policy"), "set_follow_channel_policy", "get_follow_channel_policy");
    ADD_PROPERTY(PropertyInfo(Variant::INT, "media_path_policy", PROPERTY_HINT_ENUM, "Auto,UDP Only,Tunnel Only,QUIC Only"), "set_media_path_policy", "get_media_path_policy");
    ADD_PROPERTY(PropertyInfo(Variant::BOOL, "quic"), "set_quic_enabled", "get_quic_enabled");
    ADD_PROPERTY(PropertyInfo(Variant::BOOL, "auto_capture"), "set_auto_capture", "get_auto_capture");
    ADD_PROPERTY(PropertyInfo(Variant::BOOL, "auto_playback"), "set_auto_playback", "get_auto_playback");
    ADD_PROPERTY(PropertyInfo(Variant::FLOAT, "playback_buffer_seconds", PROPERTY_HINT_RANGE, "0.02,1.0,0.01"), "set_playback_buffer_seconds", "get_playback_buffer_seconds");
    ADD_PROPERTY(PropertyInfo(Variant::STRING_NAME, "playback_bus"), "set_playback_bus", "get_playback_bus");
    ADD_PROPERTY(PropertyInfo(Variant::INT, "playback_mode", PROPERTY_HINT_ENUM, "Mixed,Per Participant,Per Participant Only"), "set_playback_mode", "get_playback_mode");
    ADD_PROPERTY(PropertyInfo(Variant::INT, "max_events_per_frame"), "set_max_events_per_frame", "get_max_events_per_frame");
    ADD_PROPERTY(PropertyInfo(Variant::BOOL, "dsp_bypass"), "set_dsp_bypass", "get_dsp_bypass");
    ADD_PROPERTY(PropertyInfo(Variant::BOOL, "visemes_enabled"), "set_visemes_enabled", "get_visemes_enabled");

    // lifecycle
    ClassDB::bind_method(D_METHOD("connect_to_server", "ws_url", "token"), &AurixVoiceClient::connect_to_server);
    ClassDB::bind_method(D_METHOD("disconnect_from_server"), &AurixVoiceClient::disconnect_from_server);
    ClassDB::bind_method(D_METHOD("set_token", "token"), &AurixVoiceClient::set_token);
    ClassDB::bind_method(D_METHOD("is_client_created"), &AurixVoiceClient::is_client_created);
    ClassDB::bind_method(D_METHOD("get_client_generation"), &AurixVoiceClient::get_client_generation);
    ClassDB::bind_method(D_METHOD("get_connection_state"), &AurixVoiceClient::get_connection_state);
    ClassDB::bind_method(D_METHOD("get_session"), &AurixVoiceClient::get_session);
    ClassDB::bind_method(D_METHOD("get_endpoint"), &AurixVoiceClient::get_endpoint);
    ClassDB::bind_method(D_METHOD("get_failover_endpoints"), &AurixVoiceClient::get_failover_endpoints);
    ClassDB::bind_method(D_METHOD("get_media_path"), &AurixVoiceClient::get_media_path);
    ClassDB::bind_method(D_METHOD("network_changed"), &AurixVoiceClient::network_changed);
    ClassDB::bind_method(D_METHOD("get_last_error"), &AurixVoiceClient::get_last_error);
    ClassDB::bind_static_method("AurixVoiceClient", D_METHOD("get_native_version"), &AurixVoiceClient::get_native_version);

    // channels
    ClassDB::bind_method(D_METHOD("join_channel", "channel_id", "join_token"), &AurixVoiceClient::join_channel, DEFVAL(String()));
    ClassDB::bind_method(D_METHOD("leave_channel", "channel_id"), &AurixVoiceClient::leave_channel);
    ClassDB::bind_method(D_METHOD("get_joined_channels"), &AurixVoiceClient::get_joined_channels);
    ClassDB::bind_method(D_METHOD("get_participants", "channel_id"), &AurixVoiceClient::get_participants);
    ClassDB::bind_method(D_METHOD("get_channel_info", "channel_id"), &AurixVoiceClient::get_channel_info);
    ClassDB::bind_method(D_METHOD("can_speak_in", "channel_id"), &AurixVoiceClient::can_speak_in);
    ClassDB::bind_method(D_METHOD("channel_transcribes", "channel_id"), &AurixVoiceClient::channel_transcribes);
    ClassDB::bind_method(D_METHOD("channel_monitored", "channel_id"), &AurixVoiceClient::channel_monitored);
    ClassDB::bind_method(D_METHOD("get_channel_scope", "channel_id"), &AurixVoiceClient::get_channel_scope);
    ClassDB::bind_method(D_METHOD("user_for_ssrc", "ssrc"), &AurixVoiceClient::user_for_ssrc);

    // microphone / uplink
    ClassDB::bind_method(D_METHOD("start_capture"), &AurixVoiceClient::start_capture);
    ClassDB::bind_method(D_METHOD("stop_capture"), &AurixVoiceClient::stop_capture);
    ClassDB::bind_method(D_METHOD("is_capturing"), &AurixVoiceClient::is_capturing);
    ClassDB::bind_method(D_METHOD("push_capture", "frames", "sample_rate_hz"), &AurixVoiceClient::push_capture);
    ClassDB::bind_method(D_METHOD("push_capture_mono", "samples", "sample_rate_hz"), &AurixVoiceClient::push_capture_mono);
    ClassDB::bind_method(D_METHOD("set_muted", "muted"), &AurixVoiceClient::set_muted);
    ClassDB::bind_method(D_METHOD("is_muted"), &AurixVoiceClient::is_muted);
    ClassDB::bind_method(D_METHOD("is_speaking"), &AurixVoiceClient::is_speaking);
    ClassDB::bind_method(D_METHOD("set_input_gain", "gain"), &AurixVoiceClient::set_input_gain);
    ClassDB::bind_method(D_METHOD("get_input_energy"), &AurixVoiceClient::get_input_energy);
    ClassDB::bind_method(D_METHOD("set_vad", "threshold", "hangover_frames"), &AurixVoiceClient::set_vad);
    ClassDB::bind_method(D_METHOD("set_vad_gate", "enabled"), &AurixVoiceClient::set_vad_gate);
    ClassDB::bind_method(D_METHOD("set_bitrate", "bps"), &AurixVoiceClient::set_bitrate);
    ClassDB::bind_method(D_METHOD("set_complexity", "complexity"), &AurixVoiceClient::set_complexity);
    ClassDB::bind_method(D_METHOD("set_encoder_settings", "settings"), &AurixVoiceClient::set_encoder_settings);
    ClassDB::bind_method(D_METHOD("get_encoder_settings"), &AurixVoiceClient::get_encoder_settings);
    ClassDB::bind_method(D_METHOD("get_audio_policy"), &AurixVoiceClient::get_audio_policy);
    ClassDB::bind_method(D_METHOD("set_loss_adaptation", "adaptation"), &AurixVoiceClient::set_loss_adaptation);
    ClassDB::bind_method(D_METHOD("get_loss_adaptation"), &AurixVoiceClient::get_loss_adaptation);
    ClassDB::bind_method(D_METHOD("get_loss_profile"), &AurixVoiceClient::get_loss_profile);
    ClassDB::bind_method(D_METHOD("set_decoder_settings", "settings"), &AurixVoiceClient::set_decoder_settings);
    ClassDB::bind_method(D_METHOD("get_decoder_settings"), &AurixVoiceClient::get_decoder_settings);
    ClassDB::bind_static_method("AurixVoiceClient", D_METHOD("is_dred_supported"), &AurixVoiceClient::is_dred_supported);
    ClassDB::bind_method(D_METHOD("set_dsp", "config"), &AurixVoiceClient::set_dsp);
    ClassDB::bind_method(D_METHOD("get_dsp"), &AurixVoiceClient::get_dsp);
    ClassDB::bind_method(D_METHOD("get_dsp_stats"), &AurixVoiceClient::get_dsp_stats);
    ClassDB::bind_method(D_METHOD("set_voice_effects", "effects"), &AurixVoiceClient::set_voice_effects);
    ClassDB::bind_method(D_METHOD("get_voice_effects"), &AurixVoiceClient::get_voice_effects);
    ClassDB::bind_method(D_METHOD("get_voice_preset", "preset"), &AurixVoiceClient::get_voice_preset);
    ClassDB::bind_method(D_METHOD("set_voice_preset", "preset"), &AurixVoiceClient::set_voice_preset);
    ClassDB::bind_method(D_METHOD("get_participant_visemes", "user_id"), &AurixVoiceClient::get_participant_visemes);
    ClassDB::bind_method(D_METHOD("get_local_visemes"), &AurixVoiceClient::get_local_visemes);
    ClassDB::bind_method(D_METHOD("reset_capture"), &AurixVoiceClient::reset_capture);

    // playback / downlink
    ClassDB::bind_method(D_METHOD("start_playback"), &AurixVoiceClient::start_playback);
    ClassDB::bind_method(D_METHOD("stop_playback"), &AurixVoiceClient::stop_playback);
    ClassDB::bind_method(D_METHOD("is_playing"), &AurixVoiceClient::is_playing);
    ClassDB::bind_method(D_METHOD("mix_output", "frames"), &AurixVoiceClient::mix_output);
    ClassDB::bind_method(D_METHOD("pull_participant", "user_id", "frames"), &AurixVoiceClient::pull_participant);
    ClassDB::bind_method(D_METHOD("set_participant_claimed", "user_id", "claimed"), &AurixVoiceClient::set_participant_claimed);
    ClassDB::bind_method(D_METHOD("get_participant_streams"), &AurixVoiceClient::get_participant_streams);
    ClassDB::bind_method(D_METHOD("push_render", "frames"), &AurixVoiceClient::push_render);
    ClassDB::bind_method(D_METHOD("set_output_volume", "volume"), &AurixVoiceClient::set_output_volume);
    ClassDB::bind_method(D_METHOD("set_output_muted", "muted"), &AurixVoiceClient::set_output_muted);

    // receiver preferences
    ClassDB::bind_method(D_METHOD("set_participant_mute", "user_id", "channel_id", "muted"), &AurixVoiceClient::set_participant_mute);
    ClassDB::bind_method(D_METHOD("set_participant_volume", "user_id", "volume"), &AurixVoiceClient::set_participant_volume);
    ClassDB::bind_method(D_METHOD("set_user_block", "user_id", "blocked"), &AurixVoiceClient::set_user_block);
    ClassDB::bind_method(D_METHOD("set_priority", "channel_id", "user_id", "priority"), &AurixVoiceClient::set_priority, DEFVAL(String()), DEFVAL(true));
    ClassDB::bind_method(D_METHOD("is_ducking_active", "channel_id"), &AurixVoiceClient::is_ducking_active);
    ClassDB::bind_method(D_METHOD("set_transmission", "mode", "channel_id"), &AurixVoiceClient::set_transmission, DEFVAL(String()));
    ClassDB::bind_method(D_METHOD("set_channel_focus", "channel_id"), &AurixVoiceClient::set_channel_focus, DEFVAL(String()));
    ClassDB::bind_method(D_METHOD("set_audio_codec", "codec"), &AurixVoiceClient::set_audio_codec);
    ClassDB::bind_method(D_METHOD("get_audio_codec"), &AurixVoiceClient::get_audio_codec);
    ClassDB::bind_method(D_METHOD("set_downlink_mode", "mode"), &AurixVoiceClient::set_downlink_mode);
    ClassDB::bind_method(D_METHOD("get_downlink_mode"), &AurixVoiceClient::get_downlink_mode);
    ClassDB::bind_method(D_METHOD("set_transcripts", "enabled"), &AurixVoiceClient::set_transcripts);
    ClassDB::bind_method(D_METHOD("set_translation", "language", "spoken_language", "speech"), &AurixVoiceClient::set_translation, DEFVAL(String()), DEFVAL(false));
    ClassDB::bind_method(D_METHOD("update_positions", "channel_id", "positions"), &AurixVoiceClient::update_positions);
    ClassDB::bind_method(D_METHOD("update_transforms", "channel_id", "transforms"), &AurixVoiceClient::update_transforms);
    ClassDB::bind_method(D_METHOD("respond_recording_consent", "recording_id", "accepted"), &AurixVoiceClient::respond_recording_consent);
    ClassDB::bind_method(D_METHOD("send_control_json", "json"), &AurixVoiceClient::send_control_json);

    // chat / moderation / speech
    ClassDB::bind_method(D_METHOD("send_chat", "channel_id", "text", "metadata_json"), &AurixVoiceClient::send_chat, DEFVAL(String()));
    ClassDB::bind_method(D_METHOD("send_direct_chat", "user_id", "text", "metadata_json"), &AurixVoiceClient::send_direct_chat, DEFVAL(String()));
    ClassDB::bind_method(D_METHOD("set_typing", "channel_id", "typing"), &AurixVoiceClient::set_typing);
    ClassDB::bind_method(D_METHOD("channel_history", "channel_id", "before", "after", "limit"), &AurixVoiceClient::channel_history, DEFVAL(String()), DEFVAL(String()), DEFVAL(50));
    ClassDB::bind_method(D_METHOD("direct_history", "user_id", "before", "after", "limit"), &AurixVoiceClient::direct_history, DEFVAL(String()), DEFVAL(String()), DEFVAL(50));
    ClassDB::bind_method(D_METHOD("mark_channel_read", "channel_id", "message_id"), &AurixVoiceClient::mark_channel_read);
    ClassDB::bind_method(D_METHOD("mark_direct_read", "user_id", "message_id"), &AurixVoiceClient::mark_direct_read);
    ClassDB::bind_method(D_METHOD("channel_read_markers", "channel_id"), &AurixVoiceClient::channel_read_markers);
    ClassDB::bind_method(D_METHOD("direct_read_markers", "user_id"), &AurixVoiceClient::direct_read_markers);
    ClassDB::bind_method(D_METHOD("moderate", "channel_id", "user_id", "action", "action_token", "reason"), &AurixVoiceClient::moderate, DEFVAL(String()));
    ClassDB::bind_method(D_METHOD("speak", "text", "channel_id", "destination", "voice"), &AurixVoiceClient::speak, DEFVAL(String()), DEFVAL(TTS_BOTH), DEFVAL(String()));
    ClassDB::bind_method(D_METHOD("cancel_speech"), &AurixVoiceClient::cancel_speech);

    // diagnostics
    ClassDB::bind_method(D_METHOD("get_stats"), &AurixVoiceClient::get_stats);
    ClassDB::bind_method(D_METHOD("get_network_quality"), &AurixVoiceClient::get_network_quality);

    // signals
    ADD_SIGNAL(MethodInfo("raw_event", PropertyInfo(Variant::INT, "type"), PropertyInfo(Variant::STRING, "json")));
    ADD_SIGNAL(MethodInfo("state_changed", PropertyInfo(Variant::INT, "state")));
    ADD_SIGNAL(MethodInfo("session_ready", PropertyInfo(Variant::DICTIONARY, "session")));
    ADD_SIGNAL(MethodInfo("media_bound"));
    ADD_SIGNAL(MethodInfo("channel_joined", PropertyInfo(Variant::INT, "request_id"), PropertyInfo(Variant::STRING, "channel_id"),
                          PropertyInfo(Variant::ARRAY, "participants"), PropertyInfo(Variant::DICTIONARY, "info")));
    ADD_SIGNAL(MethodInfo("channel_left", PropertyInfo(Variant::STRING, "channel_id")));
    ADD_SIGNAL(MethodInfo("participant_joined", PropertyInfo(Variant::STRING, "channel_id"), PropertyInfo(Variant::DICTIONARY, "participant")));
    ADD_SIGNAL(MethodInfo("participant_left", PropertyInfo(Variant::STRING, "channel_id"), PropertyInfo(Variant::STRING, "user_id")));
    ADD_SIGNAL(MethodInfo("participant_mute_changed", PropertyInfo(Variant::STRING, "channel_id"), PropertyInfo(Variant::STRING, "user_id"),
                          PropertyInfo(Variant::BOOL, "muted"), PropertyInfo(Variant::BOOL, "server_muted")));
    ADD_SIGNAL(MethodInfo("participant_speaking", PropertyInfo(Variant::STRING, "channel_id"), PropertyInfo(Variant::STRING, "user_id"),
                          PropertyInfo(Variant::BOOL, "speaking")));
    ADD_SIGNAL(MethodInfo("participant_priority_changed", PropertyInfo(Variant::STRING, "channel_id"), PropertyInfo(Variant::STRING, "user_id"),
                          PropertyInfo(Variant::BOOL, "priority")));
    ADD_SIGNAL(MethodInfo("ducking_changed", PropertyInfo(Variant::STRING, "channel_id"), PropertyInfo(Variant::BOOL, "active"),
                          PropertyInfo(Variant::DICTIONARY, "ducking")));
    ADD_SIGNAL(MethodInfo("channel_energy", PropertyInfo(Variant::STRING, "channel_id"), PropertyInfo(Variant::DICTIONARY, "levels")));
    ADD_SIGNAL(MethodInfo("local_speaking", PropertyInfo(Variant::BOOL, "speaking")));
    ADD_SIGNAL(MethodInfo("transmission_changed", PropertyInfo(Variant::INT, "mode"), PropertyInfo(Variant::STRING, "channel_id")));
    ADD_SIGNAL(MethodInfo("channel_focus_changed", PropertyInfo(Variant::STRING, "channel_id")));
    ADD_SIGNAL(MethodInfo("user_block_changed", PropertyInfo(Variant::STRING, "user_id"), PropertyInfo(Variant::BOOL, "blocked")));
    ADD_SIGNAL(MethodInfo("recording", PropertyInfo(Variant::STRING, "channel_id"), PropertyInfo(Variant::STRING, "recording_id"),
                          PropertyInfo(Variant::BOOL, "active"), PropertyInfo(Variant::BOOL, "live"), PropertyInfo(Variant::STRING, "initiator_id")));
    ADD_SIGNAL(MethodInfo("bitrate_changed", PropertyInfo(Variant::INT, "bps"), PropertyInfo(Variant::STRING, "reason")));
    ADD_SIGNAL(MethodInfo("kicked", PropertyInfo(Variant::STRING, "channel_id"), PropertyInfo(Variant::STRING, "reason")));
    ADD_SIGNAL(MethodInfo("moderation_applied", PropertyInfo(Variant::INT, "request_id"), PropertyInfo(Variant::STRING, "channel_id"),
                          PropertyInfo(Variant::STRING, "user_id"), PropertyInfo(Variant::INT, "action")));
    ADD_SIGNAL(MethodInfo("chat_message", PropertyInfo(Variant::DICTIONARY, "message")));
    ADD_SIGNAL(MethodInfo("participant_typing", PropertyInfo(Variant::STRING, "channel_id"), PropertyInfo(Variant::STRING, "user_id"),
                          PropertyInfo(Variant::BOOL, "typing")));
    ADD_SIGNAL(MethodInfo("transcript", PropertyInfo(Variant::DICTIONARY, "transcript")));
    ADD_SIGNAL(MethodInfo("tts_status", PropertyInfo(Variant::DICTIONARY, "status")));
    ADD_SIGNAL(MethodInfo("positions", PropertyInfo(Variant::STRING, "channel_id"), PropertyInfo(Variant::STRING, "json")));
    ADD_SIGNAL(MethodInfo("rejoin_failed", PropertyInfo(Variant::STRING, "channel_id"), PropertyInfo(Variant::STRING, "code"),
                          PropertyInfo(Variant::STRING, "message")));
    ADD_SIGNAL(MethodInfo("request_failed", PropertyInfo(Variant::INT, "request_id"), PropertyInfo(Variant::STRING, "code"),
                          PropertyInfo(Variant::STRING, "message")));
    ADD_SIGNAL(MethodInfo("server_error", PropertyInfo(Variant::STRING, "code"), PropertyInfo(Variant::STRING, "message")));
    ADD_SIGNAL(MethodInfo("recovering", PropertyInfo(Variant::INT, "attempt"), PropertyInfo(Variant::INT, "delay_ms"), PropertyInfo(Variant::STRING, "cause")));
    ADD_SIGNAL(MethodInfo("recovered", PropertyInfo(Variant::BOOL, "resumed"), PropertyInfo(Variant::BOOL, "migrated")));
    ADD_SIGNAL(MethodInfo("failed_to_recover", PropertyInfo(Variant::STRING, "reason")));
    ADD_SIGNAL(MethodInfo("disconnected", PropertyInfo(Variant::STRING, "reason")));
    ADD_SIGNAL(MethodInfo("network_quality", PropertyInfo(Variant::DICTIONARY, "quality")));
    ADD_SIGNAL(MethodInfo("audio_policy_changed", PropertyInfo(Variant::DICTIONARY, "policy")));
    ADD_SIGNAL(MethodInfo("audio_codec_changed", PropertyInfo(Variant::INT, "codec")));
    ADD_SIGNAL(MethodInfo("loss_profile_changed", PropertyInfo(Variant::INT, "profile"), PropertyInfo(Variant::INT, "uplink_loss_percent")));
    ADD_SIGNAL(MethodInfo("media_path_changed", PropertyInfo(Variant::INT, "path"), PropertyInfo(Variant::STRING, "reason")));
    ADD_SIGNAL(MethodInfo("downlink_mode_changed", PropertyInfo(Variant::INT, "mode")));
    ADD_SIGNAL(MethodInfo("endpoint_changed", PropertyInfo(Variant::STRING, "ws_url")));
    ADD_SIGNAL(MethodInfo("chat_history", PropertyInfo(Variant::INT, "request_id"), PropertyInfo(Variant::STRING, "channel_id"),
                          PropertyInfo(Variant::STRING, "user_id"), PropertyInfo(Variant::ARRAY, "messages"),
                          PropertyInfo(Variant::STRING, "next_before"), PropertyInfo(Variant::STRING, "next_after")));
    ADD_SIGNAL(MethodInfo("chat_read_marker", PropertyInfo(Variant::DICTIONARY, "marker")));
    ADD_SIGNAL(MethodInfo("chat_read_markers", PropertyInfo(Variant::STRING, "channel_id"), PropertyInfo(Variant::STRING, "user_id"),
                          PropertyInfo(Variant::INT, "unread"), PropertyInfo(Variant::ARRAY, "markers")));
    ADD_SIGNAL(MethodInfo("chat_inbox_synced", PropertyInfo(Variant::INT, "delivered"), PropertyInfo(Variant::BOOL, "truncated")));
    ADD_SIGNAL(MethodInfo("translation_changed", PropertyInfo(Variant::DICTIONARY, "translation")));

    // enums
    BIND_ENUM_CONSTANT(RESULT_OK);
    BIND_ENUM_CONSTANT(RESULT_NULL_POINTER);
    BIND_ENUM_CONSTANT(RESULT_INVALID_ARGUMENT);
    BIND_ENUM_CONSTANT(RESULT_NOT_CONNECTED);
    BIND_ENUM_CONSTANT(RESULT_CLOSED);
    BIND_ENUM_CONSTANT(RESULT_TRANSPORT);
    BIND_ENUM_CONSTANT(RESULT_UNAUTHORIZED);
    BIND_ENUM_CONSTANT(RESULT_TIMEOUT);
    BIND_ENUM_CONSTANT(RESULT_SERVER_REJECTED);
    BIND_ENUM_CONSTANT(RESULT_CODEC);
    BIND_ENUM_CONSTANT(RESULT_PROTOCOL);
    BIND_ENUM_CONSTANT(STATE_DISCONNECTED);
    BIND_ENUM_CONSTANT(STATE_CONNECTING);
    BIND_ENUM_CONSTANT(STATE_CONNECTED);
    BIND_ENUM_CONSTANT(STATE_MEDIA_BOUND);
    BIND_ENUM_CONSTANT(STATE_RECONNECTING);
    BIND_ENUM_CONSTANT(STATE_FAILED);
    BIND_ENUM_CONSTANT(TRANSMIT_NONE);
    BIND_ENUM_CONSTANT(TRANSMIT_SINGLE);
    BIND_ENUM_CONSTANT(TRANSMIT_ALL);
    BIND_ENUM_CONSTANT(CODEC_OPUS);
    BIND_ENUM_CONSTANT(CODEC_PCMU);
    BIND_ENUM_CONSTANT(DOWNLINK_STREAMS);
    BIND_ENUM_CONSTANT(DOWNLINK_MIXED);
    BIND_ENUM_CONSTANT(MEDIA_NONE);
    BIND_ENUM_CONSTANT(MEDIA_UDP);
    BIND_ENUM_CONSTANT(MEDIA_TUNNEL);
    BIND_ENUM_CONSTANT(MEDIA_QUIC);
    BIND_ENUM_CONSTANT(MEDIA_PATH_AUTO);
    BIND_ENUM_CONSTANT(MEDIA_PATH_UDP_ONLY);
    BIND_ENUM_CONSTANT(MEDIA_PATH_TUNNEL_ONLY);
    BIND_ENUM_CONSTANT(MEDIA_PATH_QUIC_ONLY);
    BIND_ENUM_CONSTANT(MODERATION_KICK);
    BIND_ENUM_CONSTANT(MODERATION_MUTE);
    BIND_ENUM_CONSTANT(MODERATION_UNMUTE);
    BIND_ENUM_CONSTANT(ROLE_LISTENER);
    BIND_ENUM_CONSTANT(ROLE_SPEAKER);
    BIND_ENUM_CONSTANT(ROLE_MODERATOR);
    BIND_ENUM_CONSTANT(ROLE_ADMINISTRATOR);
    BIND_ENUM_CONSTANT(TTS_BOTH);
    BIND_ENUM_CONSTANT(TTS_CHANNEL);
    BIND_ENUM_CONSTANT(TTS_LOCAL);
    BIND_ENUM_CONSTANT(TTS_QUEUED);
    BIND_ENUM_CONSTANT(TTS_PLAYING);
    BIND_ENUM_CONSTANT(TTS_FINISHED);
    BIND_ENUM_CONSTANT(TTS_CANCELLED);
    BIND_ENUM_CONSTANT(TTS_FAILED);
    BIND_ENUM_CONSTANT(LOSS_PROFILE_LOW);
    BIND_ENUM_CONSTANT(LOSS_PROFILE_MODERATE);
    BIND_ENUM_CONSTANT(LOSS_PROFILE_HIGH);
    BIND_ENUM_CONSTANT(LOSS_ADAPTATION_AUTO);
    BIND_ENUM_CONSTANT(LOSS_ADAPTATION_FIXED_LOW);
    BIND_ENUM_CONSTANT(LOSS_ADAPTATION_FIXED_MODERATE);
    BIND_ENUM_CONSTANT(LOSS_ADAPTATION_FIXED_HIGH);
    BIND_ENUM_CONSTANT(NOISE_SUPPRESSION_OFF);
    BIND_ENUM_CONSTANT(NOISE_SUPPRESSION_LOW);
    BIND_ENUM_CONSTANT(NOISE_SUPPRESSION_MODERATE);
    BIND_ENUM_CONSTANT(NOISE_SUPPRESSION_HIGH);
    BIND_ENUM_CONSTANT(BANDWIDTH_NARROWBAND);
    BIND_ENUM_CONSTANT(BANDWIDTH_MEDIUMBAND);
    BIND_ENUM_CONSTANT(BANDWIDTH_WIDEBAND);
    BIND_ENUM_CONSTANT(BANDWIDTH_SUPERWIDEBAND);
    BIND_ENUM_CONSTANT(BANDWIDTH_FULLBAND);
    BIND_ENUM_CONSTANT(SIGNAL_AUTO);
    BIND_ENUM_CONSTANT(SIGNAL_VOICE);
    BIND_ENUM_CONSTANT(SIGNAL_MUSIC);
    BIND_ENUM_CONSTANT(PLAYBACK_MIXED);
    BIND_ENUM_CONSTANT(PLAYBACK_PER_PARTICIPANT);
    BIND_ENUM_CONSTANT(PLAYBACK_PER_PARTICIPANT_ONLY);
    BIND_ENUM_CONSTANT(VOICE_PRESET_ROBOT);
    BIND_ENUM_CONSTANT(VOICE_PRESET_MONSTER);
    BIND_ENUM_CONSTANT(VOICE_PRESET_RADIO);
    BIND_ENUM_CONSTANT(VOICE_PRESET_HELIUM);
    BIND_ENUM_CONSTANT(VOICE_PRESET_GHOST);
    BIND_ENUM_CONSTANT(VISEME_SILENCE);
    BIND_ENUM_CONSTANT(VISEME_PP);
    BIND_ENUM_CONSTANT(VISEME_FF);
    BIND_ENUM_CONSTANT(VISEME_SS);
    BIND_ENUM_CONSTANT(VISEME_AA);
    BIND_ENUM_CONSTANT(VISEME_E);
    BIND_ENUM_CONSTANT(VISEME_IH);
    BIND_ENUM_CONSTANT(VISEME_OH);
    BIND_ENUM_CONSTANT(VISEME_OU);
}

}  // namespace godot
