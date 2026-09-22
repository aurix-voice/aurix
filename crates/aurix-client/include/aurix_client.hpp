// Aurix native voice client — header-only C++11 wrapper over the C ABI in `aurix_client.h`.
//
// Engine-agnostic (no exceptions required, no RTTI, standard library only); the Unreal plugin
// and other C++ integrations build on it. Ownership mirrors the C ABI: `aurix::Client` owns the
// handle, `aurix::Event` owns one polled event, everything else is a plain value copy.
#ifndef AURIX_CLIENT_HPP
#define AURIX_CLIENT_HPP

#include "aurix_client.h"

#include <cstdint>
#include <cstring>
#include <string>
#include <utility>
#include <vector>

namespace aurix {

/// 128-bit id with value semantics and RFC 4122 text conversion.
struct Uuid {
    AurixUuid raw;

    Uuid() { std::memset(raw.bytes, 0, sizeof raw.bytes); }
    explicit Uuid(const AurixUuid& r) : raw(r) {}

    static bool parse(const std::string& text, Uuid& out) {
        return aurix_uuid_parse(text.c_str(), &out.raw) == AURIX_OK;
    }
    static Uuid parse_or_nil(const std::string& text) {
        Uuid u;
        if (!parse(text, u)) {
            return Uuid();
        }
        return u;
    }

    bool is_nil() const {
        for (unsigned char b : raw.bytes) {
            if (b != 0) {
                return false;
            }
        }
        return true;
    }
    std::string str() const {
        char buf[AURIX_UUID_STRING_LEN];
        if (aurix_uuid_format(&raw, buf, sizeof buf) != AURIX_OK) {
            return std::string();
        }
        return std::string(buf);
    }
    bool operator==(const Uuid& o) const { return std::memcmp(raw.bytes, o.raw.bytes, 16) == 0; }
    bool operator!=(const Uuid& o) const { return !(*this == o); }
    bool operator<(const Uuid& o) const { return std::memcmp(raw.bytes, o.raw.bytes, 16) < 0; }
};

/// Message of the last failed call on this thread.
inline std::string last_error() {
    const char* e = aurix_last_error();
    return e ? std::string(e) : std::string();
}

inline std::string version() {
    const char* v = aurix_version();
    return v ? std::string(v) : std::string();
}

inline std::string to_string(const char* s) { return s ? std::string(s) : std::string(); }

/// One event popped from the client queue; freed on destruction. Move-only.
class Event {
public:
    Event() : ev_(nullptr) {}
    explicit Event(AurixEvent* ev) : ev_(ev) {}
    Event(Event&& o) noexcept : ev_(o.ev_) { o.ev_ = nullptr; }
    Event& operator=(Event&& o) noexcept {
        if (this != &o) {
            reset();
            ev_ = o.ev_;
            o.ev_ = nullptr;
        }
        return *this;
    }
    ~Event() { reset(); }
    Event(const Event&) = delete;
    Event& operator=(const Event&) = delete;

    explicit operator bool() const { return ev_ != nullptr; }
    const AurixEvent* raw() const { return ev_; }
    void reset() {
        if (ev_) {
            aurix_event_free(ev_);
            ev_ = nullptr;
        }
    }

    AurixEventType type() const { return aurix_event_type(ev_); }
    std::string json() const { return to_string(aurix_event_json(ev_)); }
    AurixConnectionState state() const { return aurix_event_state(ev_); }
    std::uint64_t request_id() const { return aurix_event_request_id(ev_); }
    Uuid channel_id() const { return Uuid(aurix_event_channel_id(ev_)); }
    Uuid user_id() const { return Uuid(aurix_event_user_id(ev_)); }
    Uuid object_id() const { return Uuid(aurix_event_object_id(ev_)); }
    bool flag() const { return aurix_event_flag(ev_); }
    bool flag2() const { return aurix_event_flag2(ev_); }
    std::uint64_t number() const { return aurix_event_number(ev_); }
    std::uint64_t number2() const { return aurix_event_number2(ev_); }
    std::string code() const { return to_string(aurix_event_code(ev_)); }
    std::string message() const { return to_string(aurix_event_message(ev_)); }
    AurixTransmissionMode transmission() const { return aurix_event_transmission(ev_); }
    AurixAudioCodec audio_codec() const { return aurix_event_audio_codec(ev_); }
    /// Mode of `AURIX_EVENT_DOWNLINK_MODE_CHANGED`; `AURIX_DOWNLINK_STREAMS` for other events.
    AurixDownlinkMode downlink_mode() const { return aurix_event_downlink_mode(ev_); }
    /// Link of `AURIX_EVENT_MEDIA_PATH_CHANGED`; `AURIX_MEDIA_NONE` for other events.
    AurixMediaPath media_path() const { return aurix_event_media_path(ev_); }
    AurixModerationAction moderation_action() const { return aurix_event_moderation_action(ev_); }

    bool session(AurixSessionInfo& out) const { return aurix_event_session(ev_, &out); }
    /// `AURIX_EVENT_CHAT_MESSAGE` and `AURIX_EVENT_CHAT_MESSAGE_UPDATED` (edit / tombstone).
    bool chat(AurixChatMessage& out) const { return aurix_event_chat(ev_, &out); }
    /// `AURIX_EVENT_CHAT_REACTION_CHANGED`.
    bool reaction(AurixChatReaction& out) const { return aurix_event_reaction(ev_, &out); }
    /// `AURIX_EVENT_CHAT_HISTORY` / `AURIX_EVENT_CHAT_SEARCH_RESULT`: page summary (cursors
    /// are owned by the event).
    bool chat_history(AurixChatHistory& out) const { return aurix_event_chat_history(ev_, &out); }
    /// Messages of an `AURIX_EVENT_CHAT_HISTORY` / `AURIX_EVENT_CHAT_SEARCH_RESULT` page,
    /// newest first.
    std::vector<AurixChatMessage> chat_history_messages() const {
        AurixChatHistory page{};
        if (!aurix_event_chat_history(ev_, &page)) return {};
        std::vector<AurixChatMessage> out(page.count);
        for (std::size_t i = 0; i < out.size(); ++i) {
            if (!aurix_event_chat_history_message(ev_, i, &out[i])) {
                out.resize(i);
                break;
            }
        }
        return out;
    }
    /// `AURIX_EVENT_CHAT_READ_MARKER`.
    bool read_marker(AurixReadMarker& out) const { return aurix_event_read_marker(ev_, &out); }
    /// Markers of an `AURIX_EVENT_CHAT_READ_MARKERS` answer (`number()` = unread count).
    std::vector<AurixReadMarker> read_markers() const {
        std::vector<AurixReadMarker> out(aurix_event_number2(ev_));
        for (std::size_t i = 0; i < out.size(); ++i) {
            if (!aurix_event_read_marker_at(ev_, i, &out[i])) {
                out.resize(i);
                break;
            }
        }
        return out;
    }
    bool transcript(AurixTranscript& out) const { return aurix_event_transcript(ev_, &out); }
    /// Payload of `AURIX_EVENT_TRANSLATION_CHANGED` (the server-acked listener preference).
    bool translation(AurixTranslation& out) const { return aurix_event_translation(ev_, &out); }
    bool tts(AurixTtsStatus& out) const { return aurix_event_tts(ev_, &out); }
    /// Payload of `AURIX_EVENT_AUDIO_POLICY_CHANGED`.
    bool audio_policy(AurixAudioPolicy& out) const { return aurix_event_audio_policy(ev_, &out); }
    /// `AURIX_EVENT_CHANNEL_JOINED` only: this session's role, the member count (all nodes,
    /// hidden listeners included) and the roster policy.
    AurixChannelInfo channel_info() const { return aurix_event_channel_info(ev_); }
    /// `AURIX_EVENT_PARTICIPANT_ROLE_CHANGED` only: the member's effective role now (`flag()`
    /// says whether it gained or lost its speaker slot); `AURIX_ROLE_LISTENER` for other events.
    AurixRole role() const { return aurix_event_role(ev_); }
    /// `AURIX_EVENT_DUCKING_CHANGED` only: depth and timings for the game's own audio bus
    /// (`flag()` says whether ducking just started or stopped).
    AurixDucking ducking() const { return aurix_event_ducking(ev_); }
    /// Tier of `AURIX_EVENT_LOSS_PROFILE_CHANGED` (`number2()` = the uplink loss in whole
    /// percent that triggered it); `AURIX_LOSS_PROFILE_LOW` for other events.
    AurixLossProfile loss_profile() const { return aurix_event_loss_profile(ev_); }

    std::vector<AurixParticipant> participants() const {
        std::vector<AurixParticipant> out(aurix_event_participant_count(ev_));
        for (std::size_t i = 0; i < out.size(); ++i) {
            if (!aurix_event_participant(ev_, i, &out[i])) {
                out.resize(i);
                break;
            }
        }
        return out;
    }

private:
    AurixEvent* ev_;
};

/// Connection parameters with the ABI defaults filled in.
struct Config {
    std::string ws_url;
    std::string token;
    /// Stable installation id (`[A-Za-z0-9._~-]{1,128}`) for the per-device chat delivery
    /// cursor; empty = none (see `AurixClientConfig::device_id`).
    std::string device_id;
    AurixClientConfig raw;

    Config(std::string url, std::string tok) : ws_url(std::move(url)), token(std::move(tok)) {
        aurix_client_config_default(&raw);
    }
};

/// Owns one `AurixClient`. Move-only; destroying disconnects.
class Client {
public:
    Client() : c_(nullptr) {}
    Client(Client&& o) noexcept : c_(o.c_) { o.c_ = nullptr; }
    Client& operator=(Client&& o) noexcept {
        if (this != &o) {
            reset();
            c_ = o.c_;
            o.c_ = nullptr;
        }
        return *this;
    }
    ~Client() { reset(); }
    Client(const Client&) = delete;
    Client& operator=(const Client&) = delete;

    /// Create a client; returns an empty `Client` (see `last_error()`) on failure.
    static Client create(const Config& cfg) {
        AurixClientConfig raw = cfg.raw;
        raw.ws_url = cfg.ws_url.c_str();
        raw.token = cfg.token.c_str();
        raw.device_id = cfg.device_id.empty() ? nullptr : cfg.device_id.c_str();
        Client c;
        c.c_ = aurix_client_create(&raw);
        return c;
    }

    explicit operator bool() const { return c_ != nullptr; }
    AurixClient* raw() { return c_; }
    const AurixClient* raw() const { return c_; }
    void reset() {
        if (c_) {
            aurix_client_destroy(c_);
            c_ = nullptr;
        }
    }

    // --- lifecycle
    AurixResult connect() { return aurix_client_connect(c_); }
    void disconnect() { aurix_client_disconnect(c_); }
    AurixResult set_token(const std::string& token) { return aurix_client_set_token(c_, token.c_str()); }
    AurixConnectionState state() const { return aurix_client_state(c_); }
    bool session(AurixSessionInfo& out) const { return aurix_client_session(c_, &out); }
    /// WebSocket URL of the node serving the session (`ws_url` until a failover moved it).
    std::string endpoint() const {
        std::string out(aurix_client_endpoint(c_, nullptr, 0), '\0');
        if (!out.empty()) aurix_client_endpoint(c_, &out[0], out.size() + 1);
        return out;
    }
    /// Alternate nodes advertised for this session; reconnects try the current node first,
    /// then these in order (`AURIX_EVENT_ENDPOINT_CHANGED` reports a switch).
    std::vector<std::string> failover_endpoints() const {
        std::vector<std::string> out(aurix_client_failover_endpoint_count(c_));
        for (std::size_t i = 0; i < out.size(); ++i) {
            out[i].assign(aurix_client_failover_endpoint(c_, i, nullptr, 0), '\0');
            if (!out[i].empty()) aurix_client_failover_endpoint(c_, i, &out[i][0], out[i].size() + 1);
        }
        return out;
    }

    // --- events
    Event poll_event() { return Event(aurix_client_poll_event(c_)); }
    Event wait_event(std::uint32_t timeout_ms) { return Event(aurix_client_wait_event(c_, timeout_ms)); }
    std::size_t pending_events() const { return aurix_client_pending_events(c_); }
    void set_wake_callback(void (*cb)(void*), void* user_data) {
        aurix_client_set_wake_callback(c_, cb, user_data);
    }

    // --- channels
    AurixResult join_channel(const Uuid& channel, const char* join_token, std::uint64_t* request_id) {
        return aurix_client_join_channel(c_, &channel.raw, join_token, request_id);
    }
    AurixResult leave_channel(const Uuid& channel) { return aurix_client_leave_channel(c_, &channel.raw); }
    std::vector<Uuid> joined_channels() const {
        std::vector<AurixUuid> buf(16);
        std::size_t n = aurix_client_joined_channels(c_, buf.data(), buf.size());
        if (n > buf.size()) {
            buf.resize(n);
            n = aurix_client_joined_channels(c_, buf.data(), buf.size());
        }
        std::vector<Uuid> out;
        out.reserve(n);
        for (std::size_t i = 0; i < n && i < buf.size(); ++i) {
            out.push_back(Uuid(buf[i]));
        }
        return out;
    }
    bool channel_transcribes(const Uuid& channel) const { return aurix_client_channel_transcribes(c_, &channel.raw); }
    /// Speech in `channel` is analysed by the server's content-safety classifier — disclose it.
    bool channel_monitored(const Uuid& channel) const { return aurix_client_channel_monitored(c_, &channel.raw); }
    /// Presence / text range of a joined positional channel (radius `<= 0` = whole channel);
    /// `false` until the join is acknowledged.
    bool channel_scope(const Uuid& channel, AurixChannelScope& out) const {
        return aurix_client_channel_scope(c_, &channel.raw, &out);
    }
    /// Role / member count / roster policy of a joined channel; `false` until the join is
    /// acknowledged.
    bool channel_info(const Uuid& channel, AurixChannelInfo& out) const {
        return aurix_client_channel_info(c_, &channel.raw, &out);
    }
    /// Whether this session may transmit in `channel` (false for listeners and unknown channels).
    bool can_speak_in(const Uuid& channel) const {
        AurixChannelInfo info;
        return aurix_client_channel_info(c_, &channel.raw, &info) && info.role != AURIX_ROLE_LISTENER;
    }
    /// Whether we hold a speaking grant in `channel` but wait for an `audience.max_speakers`
    /// slot (false for unknown channels).
    bool waiting_to_speak(const Uuid& channel) const {
        AurixChannelInfo info;
        return aurix_client_channel_info(c_, &channel.raw, &info) && info.waiting_to_speak;
    }
    std::vector<AurixParticipant> participants(const Uuid& channel) const {
        std::vector<AurixParticipant> buf(32);
        std::size_t n = aurix_client_participants(c_, &channel.raw, buf.data(), buf.size());
        if (n > buf.size()) {
            buf.resize(n);
            n = aurix_client_participants(c_, &channel.raw, buf.data(), buf.size());
        }
        buf.resize(n < buf.size() ? n : buf.size());
        return buf;
    }
    bool user_for_ssrc(std::uint32_t ssrc, Uuid& out) const {
        return aurix_client_user_for_ssrc(c_, ssrc, &out.raw);
    }

    // --- end-to-end encryption
    /// Fingerprint of this client's identity key (shown to peers as `AURIX_EVENT_E2EE_PEER_KEY`).
    std::string e2ee_fingerprint() const {
        std::string out(aurix_client_e2ee_fingerprint(c_, nullptr, 0), '\0');
        if (!out.empty()) aurix_client_e2ee_fingerprint(c_, &out[0], out.size() + 1);
        return out;
    }
    /// Fingerprint of `user`'s identity key; empty when the peer never announced one.
    std::string e2ee_peer_fingerprint(const Uuid& user) const {
        std::string out(aurix_client_e2ee_peer_fingerprint(c_, user.raw, nullptr, 0), '\0');
        if (!out.empty()) aurix_client_e2ee_peer_fingerprint(c_, user.raw, &out[0], out.size() + 1);
        return out;
    }
    /// Whether `user`'s encrypted frames currently decode (their sender key arrived).
    bool e2ee_peer_decryptable(const Uuid& user) const { return aurix_client_e2ee_peer_decryptable(c_, user.raw); }

    // --- audio (real-time thread safe)
    void push_capture(const float* pcm, std::size_t samples, std::uint32_t rate, std::uint8_t channels) {
        aurix_client_push_capture_f32(c_, pcm, samples, rate, channels);
    }
    void push_capture(const std::int16_t* pcm, std::size_t samples, std::uint32_t rate, std::uint8_t channels) {
        aurix_client_push_capture_i16(c_, pcm, samples, rate, channels);
    }
    AurixResult send_opus(const std::uint8_t* opus, std::size_t len, std::int32_t level) {
        return aurix_client_send_opus(c_, opus, len, level);
    }
    std::size_t mix_output(float* out, std::size_t samples, std::uint8_t channels) {
        return aurix_client_mix_output_f32(c_, out, samples, channels);
    }
    std::size_t mix_output(std::int16_t* out, std::size_t samples, std::uint8_t channels) {
        return aurix_client_mix_output_i16(c_, out, samples, channels);
    }
    /// Per-participant playout for engine spatialization: overwrites `out` with `user`'s voice
    /// only (no local panning). Returns frames that carried audio; the rest is silence. Not fed
    /// to the AEC — `push_render` the engine's final output.
    std::size_t pull_participant(const Uuid& user, float* out, std::size_t samples, std::uint8_t channels) {
        return aurix_client_pull_participant_f32(c_, &user.raw, out, samples, channels);
    }
    std::size_t pull_participant(const Uuid& user, std::int16_t* out, std::size_t samples, std::uint8_t channels) {
        return aurix_client_pull_participant_i16(c_, &user.raw, out, samples, channels);
    }
    /// Take `user` out of `mix_output` while a per-participant emitter pulls it (and back).
    AurixResult set_participant_claimed(const Uuid& user, bool claimed) {
        return aurix_client_set_participant_claimed(c_, &user.raw, claimed);
    }
    /// Downlink streams currently buffered, with their owners.
    std::vector<AurixParticipantStream> participant_streams() const {
        std::vector<AurixParticipantStream> buf(32);
        std::size_t n = aurix_client_participant_streams(c_, buf.data(), buf.size());
        if (n > buf.size()) {
            buf.resize(n);
            n = aurix_client_participant_streams(c_, buf.data(), buf.size());
        }
        buf.resize(n < buf.size() ? n : buf.size());
        return buf;
    }
    /// Echo-canceller reference for audio played outside `mix_output` (48 kHz interleaved).
    void push_render(const float* pcm, std::size_t samples, std::uint8_t channels) {
        aurix_client_push_render_f32(c_, pcm, samples, channels);
    }
    void push_render(const std::int16_t* pcm, std::size_t samples, std::uint8_t channels) {
        aurix_client_push_render_i16(c_, pcm, samples, channels);
    }
    /// Capture DSP (high-pass / AEC / noise suppression / AGC); `aurix_dsp_config_default()`
    /// and `aurix_dsp_config_bypass()` give the two presets.
    AurixResult set_dsp(const AurixDspConfig& config) { return aurix_client_set_dsp(c_, &config); }
    bool dsp(AurixDspConfig& out) const { return aurix_client_dsp(c_, &out) == AURIX_OK; }
    bool dsp_stats(AurixDspStats& out) const { return aurix_client_dsp_stats(c_, &out) == AURIX_OK; }
    /// Built-in microphone voice effects (filters, formant / pitch shift, ring modulator,
    /// distortion, tremolo, static, reverb); all-zero = off. `voice_preset()` fills a struct
    /// from a named preset to tweak.
    AurixResult set_voice_effects(const AurixVoiceEffects& effects) { return aurix_client_set_voice_effects(c_, &effects); }
    bool voice_effects(AurixVoiceEffects& out) const { return aurix_client_voice_effects(c_, &out) == AURIX_OK; }
    static AurixVoiceEffects voice_preset(AurixVoicePreset preset) { return aurix_voice_effects_preset(preset); }
    AurixResult set_voice_preset(AurixVoicePreset preset) { return aurix_client_set_voice_preset(c_, preset); }
    /// Local lip-sync analysis of decoded participants and our own voice (off by default).
    AurixResult set_visemes(bool enabled) { return aurix_client_set_visemes(c_, enabled); }
    bool visemes_enabled() const { return aurix_client_visemes_enabled(c_); }
    /// Mouth state of `user` from the audio last played for them; `false` if not analysed.
    bool participant_visemes(const Uuid& user, AurixVisemeFrame& out) const {
        return aurix_client_participant_visemes(c_, &user.raw, &out);
    }
    bool local_visemes(AurixVisemeFrame& out) const { return aurix_client_local_visemes(c_, &out); }
    /// Host effect run on every 20 ms 48 kHz capture frame after the built-ins (`nullptr` removes it).
    AurixResult set_voice_effect_callback(AurixVoiceEffectFn callback, void* user_data) {
        return aurix_client_set_voice_effect_callback(c_, callback, user_data);
    }
    void set_muted(bool muted) { aurix_client_set_muted(c_, muted); }
    bool is_muted() const { return aurix_client_is_muted(c_); }
    bool is_speaking() const { return aurix_client_is_speaking(c_); }
    void set_input_gain(float gain) { aurix_client_set_input_gain(c_, gain); }
    float input_energy() const { return aurix_client_input_energy(c_); }
    void set_vad(float threshold, std::uint32_t hangover_frames) { aurix_client_set_vad(c_, threshold, hangover_frames); }
    void set_vad_gate(bool enabled) { aurix_client_set_vad_gate(c_, enabled); }
    AurixResult set_bitrate(std::uint32_t bps) { return aurix_client_set_bitrate(c_, bps); }
    /// Replace the baseline Opus settings (bitrate, complexity, bandwidth, VBR/FEC/DTX);
    /// the channel policy is laid over them when `follow_channel_policy` is on.
    AurixResult set_encoder_settings(const AurixEncoderSettings& settings) {
        return aurix_client_set_encoder_settings(c_, &settings);
    }
    /// What the encoder runs with right now.
    bool encoder_settings(AurixEncoderSettings& out) const { return aurix_client_encoder_settings(c_, &out); }
    /// Pin complexity 0..=10 over channel hints; a negative value unpins.
    AurixResult set_complexity(std::int8_t complexity) { return aurix_client_set_complexity(c_, complexity); }
    /// Uplink redundancy: adapt FEC / DRED to the server's loss reports (default) or pin a
    /// tier; a change fires `AURIX_EVENT_LOSS_PROFILE_CHANGED`.
    AurixResult set_loss_adaptation(AurixLossAdaptation adaptation) {
        return aurix_client_set_loss_adaptation(c_, adaptation);
    }
    AurixLossAdaptation loss_adaptation() const { return aurix_client_loss_adaptation(c_); }
    /// Redundancy tier the encoder runs with right now.
    AurixLossProfile loss_profile() const { return aurix_client_loss_profile(c_); }
    /// Downlink decoder tuning shared by every remote stream: complexity >= 5 neural PLC,
    /// >= 6 OSCE speech enhancement; optional OSCE bandwidth extension.
    AurixResult set_decoder_settings(const AurixDecoderSettings& settings) {
        return aurix_client_set_decoder_settings(c_, &settings);
    }
    bool decoder_settings(AurixDecoderSettings& out) const { return aurix_client_decoder_settings(c_, &out); }
    /// This build's libopus codes / decodes Deep REDundancy.
    static bool dred_supported() { return aurix_dred_supported(); }
    /// Merged policy of the joined channels; `false` before the first join.
    bool audio_policy(AurixAudioPolicy& out) const { return aurix_client_audio_policy(c_, &out); }
    void set_output_volume(float volume) { aurix_client_set_output_volume(c_, volume); }
    void set_output_muted(bool muted) { aurix_client_set_output_muted(c_, muted); }
    void reset_capture() { aurix_client_reset_capture(c_); }

    // --- receiver preferences
    AurixResult set_participant_mute(const Uuid& user, const Uuid* channel, bool muted) {
        return aurix_client_set_participant_mute(c_, &user.raw, channel ? &channel->raw : nullptr, muted);
    }
    AurixResult set_participant_volume(const Uuid& user, float volume) {
        return aurix_client_set_participant_volume(c_, &user.raw, volume);
    }
    AurixResult set_user_block(const Uuid& user, bool blocked) {
        return aurix_client_set_user_block(c_, &user.raw, blocked);
    }
    /// Grant / revoke priority speaker for `user` (`nullptr` = ourselves) in `channel`.
    AurixResult set_priority(const Uuid& channel, const Uuid* user, bool priority) {
        return aurix_client_set_priority(c_, &channel.raw, user ? &user->raw : nullptr, priority);
    }
    /// Another member's priority speech is ducking `channel` right now.
    bool ducking_active(const Uuid& channel) const { return aurix_client_ducking_active(c_, &channel.raw); }
    AurixResult set_transmission(AurixTransmissionMode mode, const Uuid* channel) {
        return aurix_client_set_transmission(c_, mode, channel ? &channel->raw : nullptr);
    }
    AurixResult set_channel_focus(const Uuid* channel) {
        return aurix_client_set_channel_focus(c_, channel ? &channel->raw : nullptr);
    }
    AurixResult set_audio_codec(AurixAudioCodec codec) { return aurix_client_set_audio_codec(c_, codec); }
    AurixAudioCodec audio_codec() const { return aurix_client_audio_codec(c_); }
    /// One server-mixed stereo stream per channel instead of one stream per speaker (large
    /// channels); needs `AurixSessionInfo.downlink_mix`. Acked by `AURIX_EVENT_DOWNLINK_MODE_CHANGED`.
    AurixResult set_downlink_mode(AurixDownlinkMode mode) { return aurix_client_set_downlink_mode(c_, mode); }
    AurixDownlinkMode downlink_mode() const { return aurix_client_downlink_mode(c_); }
    /// Node-side noise suppression of our uplink (for clients without their own capture DSP);
    /// needs `AurixSessionInfo.noise_suppression`. Acked by
    /// `AURIX_EVENT_SERVER_NOISE_SUPPRESSION_CHANGED` (`flag`).
    AurixResult set_server_noise_suppression(bool enabled) { return aurix_client_set_server_noise_suppression(c_, enabled); }
    bool server_noise_suppression() const { return aurix_client_server_noise_suppression(c_); }
    /// Link the media currently uses (QUIC, UDP, the TLS tunnel or the WebSocket tunnel); `AURIX_MEDIA_NONE` before bind.
    AurixMediaPath media_path() const { return aurix_client_media_path(c_); }
    /// The device's network changed: migrate a QUIC link in place / re-announce a UDP one.
    bool network_changed() { return aurix_client_network_changed(c_); }
    AurixResult set_transcripts(bool enabled) { return aurix_client_set_transcripts(c_, enabled); }
    /// Ask for transcripts translated into `language` (BCP-47, `nullptr` = off), optionally
    /// declaring the language you speak and requesting private TTS of the translation.
    AurixResult set_translation(const char* language, const char* spoken_language, bool speech) {
        return aurix_client_set_translation(c_, language, spoken_language, speech);
    }
    AurixResult update_positions(const Uuid& channel, const AurixPosition* positions, std::size_t count) {
        return aurix_client_update_positions(c_, &channel.raw, positions, count);
    }
    AurixResult respond_recording_consent(const Uuid& recording, AurixRecordingConsent consent) {
        return aurix_client_respond_recording_consent(c_, &recording.raw, consent);
    }
    AurixResult send_control_json(const std::string& json) { return aurix_client_send_control_json(c_, json.c_str()); }

    // --- chat / moderation / speech
    AurixResult moderate(const Uuid& channel, const Uuid& user, AurixModerationAction action,
                         const std::string& action_token, const char* reason, std::uint64_t* request_id) {
        return aurix_client_moderate(c_, &channel.raw, &user.raw, action, action_token.c_str(), reason, request_id);
    }
    AurixResult send_chat(const Uuid& channel, const std::string& text, const char* metadata_json,
                          std::uint64_t* request_id) {
        return aurix_client_send_chat(c_, &channel.raw, text.c_str(), metadata_json, request_id);
    }
    AurixResult send_direct_chat(const Uuid& user, const std::string& text, const char* metadata_json,
                                 std::uint64_t* request_id) {
        return aurix_client_send_direct_chat(c_, &user.raw, text.c_str(), metadata_json, request_id);
    }
    AurixResult set_typing(const Uuid& channel, bool typing) { return aurix_client_set_typing(c_, &channel.raw, typing); }
    /// Stored history of a joined channel, newest first; `before` / `after` may be null.
    AurixResult channel_history(const Uuid& channel, const char* before, const char* after, std::uint32_t limit,
                                std::uint64_t* request_id) {
        return aurix_client_chat_history(c_, &channel.raw, nullptr, before, after, limit, request_id);
    }
    /// Stored direct conversation with `user`, newest first.
    AurixResult direct_history(const Uuid& user, const char* before, const char* after, std::uint32_t limit,
                               std::uint64_t* request_id) {
        return aurix_client_chat_history(c_, nullptr, &user.raw, before, after, limit, request_id);
    }
    AurixResult mark_channel_read(const Uuid& channel, const Uuid& message) {
        return aurix_client_mark_chat_read(c_, &channel.raw, nullptr, &message.raw);
    }
    AurixResult mark_direct_read(const Uuid& user, const Uuid& message) {
        return aurix_client_mark_chat_read(c_, nullptr, &user.raw, &message.raw);
    }
    AurixResult channel_read_markers(const Uuid& channel) { return aurix_client_chat_read_markers(c_, &channel.raw, nullptr); }
    AurixResult direct_read_markers(const Uuid& user) { return aurix_client_chat_read_markers(c_, nullptr, &user.raw); }
    /// Edits own message; answered by `AURIX_EVENT_CHAT_MESSAGE_UPDATED` with `request_id`.
    AurixResult edit_chat(const Uuid& message, const std::string& text, const char* metadata_json,
                          std::uint64_t* request_id) {
        return aurix_client_edit_chat(c_, &message.raw, text.c_str(), metadata_json, request_id);
    }
    /// Deletes own (or, as channel moderator, anyone's) message; answered by a tombstone
    /// `AURIX_EVENT_CHAT_MESSAGE_UPDATED`.
    AurixResult delete_chat(const Uuid& message, std::uint64_t* request_id) {
        return aurix_client_delete_chat(c_, &message.raw, request_id);
    }
    /// Adds / removes this user's reaction; everyone gets `AURIX_EVENT_CHAT_REACTION_CHANGED`.
    AurixResult react_chat(const Uuid& message, const std::string& reaction, bool add) {
        return aurix_client_react_chat(c_, &message.raw, reaction.c_str(), add);
    }
    /// Full-text search in a joined channel; `from_user` / `before` may be null.
    AurixResult search_channel_chat(const Uuid& channel, const std::string& query, const Uuid* from_user,
                                    const char* before, std::uint32_t limit, std::uint64_t* request_id) {
        return aurix_client_search_chat(c_, &channel.raw, nullptr, query.c_str(), from_user ? &from_user->raw : nullptr,
                                        before, limit, request_id);
    }
    /// Full-text search in the direct conversation with `user` (null = every direct conversation).
    AurixResult search_direct_chat(const Uuid* user, const std::string& query, const Uuid* from_user,
                                   const char* before, std::uint32_t limit, std::uint64_t* request_id) {
        return aurix_client_search_chat(c_, nullptr, user ? &user->raw : nullptr, query.c_str(),
                                        from_user ? &from_user->raw : nullptr, before, limit, request_id);
    }
    AurixResult speak(const std::string& text, const Uuid* channel, AurixTtsDestination destination,
                      const char* voice, std::uint64_t* request_id) {
        return aurix_client_speak(c_, text.c_str(), channel ? &channel->raw : nullptr, destination, voice, request_id);
    }
    AurixResult cancel_speech() { return aurix_client_cancel_speech(c_); }

    bool stats(AurixStats& out) const { return aurix_client_stats(c_, &out) == AURIX_OK; }
    /// Latest server-reported quality; `false` until the server has sent one.
    bool network_quality(AurixNetworkQuality& out) const {
        return aurix_client_network_quality(c_, &out);
    }

private:
    AurixClient* c_;
};

/// Region discovery (`GET /v1/me/regions`): the host fetches the JSON with its own HTTP client,
/// parses it here, records one RTT per region and ranks. Entry 0 after `rank()` is the node to
/// connect to (`ws_url` → `Config::ws_url`). Move-only.
class Regions {
public:
    /// Request URL with optional preferred region and location hints (empty string = none).
    static std::string discovery_url(const std::string& api_url, const std::string& preferred_region,
                                     const double* latitude = nullptr, const double* longitude = nullptr) {
        const bool has_loc = latitude != nullptr && longitude != nullptr;
        const char* pref = preferred_region.empty() ? nullptr : preferred_region.c_str();
        const std::size_t n = aurix_regions_discovery_url(api_url.c_str(), pref, has_loc, has_loc ? *latitude : 0.0,
                                                          has_loc ? *longitude : 0.0, nullptr, 0);
        if (n == 0) {
            return std::string();
        }
        std::string out(n + 1, '\0');
        aurix_regions_discovery_url(api_url.c_str(), pref, has_loc, has_loc ? *latitude : 0.0,
                                    has_loc ? *longitude : 0.0, &out[0], out.size());
        out.resize(n);
        return out;
    }

    /// Parse a response body; `valid()` is false (see `last_error()`) on malformed input.
    static Regions parse(const std::string& json) { return Regions(aurix_regions_parse(json.c_str())); }

    Regions() : r_(nullptr) {}
    Regions(Regions&& o) noexcept : r_(o.r_) { o.r_ = nullptr; }
    Regions& operator=(Regions&& o) noexcept {
        if (this != &o) {
            reset();
            r_ = o.r_;
            o.r_ = nullptr;
        }
        return *this;
    }
    ~Regions() { reset(); }
    Regions(const Regions&) = delete;
    Regions& operator=(const Regions&) = delete;

    bool valid() const { return r_ != nullptr; }
    std::size_t size() const { return aurix_regions_len(r_); }
    bool get(std::size_t index, AurixRegionEndpoint& out) const { return aurix_regions_get(r_, index, &out); }
    std::vector<AurixRegionEndpoint> all() const {
        std::vector<AurixRegionEndpoint> out(size());
        for (std::size_t i = 0; i < out.size(); ++i) {
            aurix_regions_get(r_, i, &out[i]);
        }
        return out;
    }
    /// Best RTT sample in ms for entry `index`; negative when every probe request failed.
    AurixResult set_rtt(std::size_t index, double rtt_ms) { return aurix_regions_set_rtt(r_, index, rtt_ms); }
    /// Re-rank in place; `rtt_tolerance_ms <= 0` selects the default (15 ms).
    AurixResult rank(const std::string& preferred_region = std::string(), double rtt_tolerance_ms = 0.0) {
        return aurix_regions_rank(r_, preferred_region.empty() ? nullptr : preferred_region.c_str(), rtt_tolerance_ms);
    }

private:
    explicit Regions(AurixRegionList* r) : r_(r) {}
    void reset() {
        if (r_) {
            aurix_regions_free(r_);
            r_ = nullptr;
        }
    }
    AurixRegionList* r_;
};

/// Bare Opus encoder for hosts with their own capture pipeline (the `Client` already encodes
/// what it captures). Move-only.
class OpusEncoder {
public:
    OpusEncoder() : e_(nullptr) {}
    /// `settings == nullptr` selects the defaults. `valid()` is false on error (see `last_error()`).
    OpusEncoder(std::uint32_t sample_rate_hz, std::uint8_t channels, const AurixEncoderSettings* settings = nullptr)
        : e_(aurix_opus_encoder_create(sample_rate_hz, channels, settings)) {}
    OpusEncoder(OpusEncoder&& o) noexcept : e_(o.e_) { o.e_ = nullptr; }
    OpusEncoder& operator=(OpusEncoder&& o) noexcept {
        if (this != &o) {
            reset();
            e_ = o.e_;
            o.e_ = nullptr;
        }
        return *this;
    }
    ~OpusEncoder() { reset(); }
    OpusEncoder(const OpusEncoder&) = delete;
    OpusEncoder& operator=(const OpusEncoder&) = delete;

    bool valid() const { return e_ != nullptr; }
    AurixResult apply(const AurixEncoderSettings& settings) { return aurix_opus_encoder_apply(e_, &settings); }
    bool settings(AurixEncoderSettings& out) const { return aurix_opus_encoder_settings(e_, &out); }
    /// Packet length, or a negative `AurixResult`.
    int encode(const float* pcm, std::size_t frame_samples_per_channel, std::uint8_t* out, std::size_t out_len) {
        return aurix_opus_encoder_encode_f32(e_, pcm, frame_samples_per_channel, out, out_len);
    }
    int encode(const std::int16_t* pcm, std::size_t frame_samples_per_channel, std::uint8_t* out,
               std::size_t out_len) {
        return aurix_opus_encoder_encode_i16(e_, pcm, frame_samples_per_channel, out, out_len);
    }

private:
    void reset() {
        if (e_) {
            aurix_opus_encoder_destroy(e_);
            e_ = nullptr;
        }
    }
    AurixOpusEncoder* e_;
};

/// Bare Opus decoder with PLC/FEC. Move-only.
class OpusDecoder {
public:
    OpusDecoder() : d_(nullptr) {}
    OpusDecoder(std::uint32_t sample_rate_hz, std::uint8_t channels) : d_(aurix_opus_decoder_create(sample_rate_hz, channels)) {}
    OpusDecoder(OpusDecoder&& o) noexcept : d_(o.d_) { o.d_ = nullptr; }
    OpusDecoder& operator=(OpusDecoder&& o) noexcept {
        if (this != &o) {
            reset();
            d_ = o.d_;
            o.d_ = nullptr;
        }
        return *this;
    }
    ~OpusDecoder() { reset(); }
    OpusDecoder(const OpusDecoder&) = delete;
    OpusDecoder& operator=(const OpusDecoder&) = delete;

    bool valid() const { return d_ != nullptr; }
    /// Samples per channel, or a negative `AurixResult`. `packet == nullptr` runs PLC.
    int decode(const std::uint8_t* packet, std::size_t packet_len, float* pcm, std::size_t max_frame_samples_per_channel,
               bool fec = false) {
        return aurix_opus_decoder_decode_f32(d_, packet, packet_len, pcm, max_frame_samples_per_channel, fec);
    }
    int decode(const std::uint8_t* packet, std::size_t packet_len, std::int16_t* pcm,
               std::size_t max_frame_samples_per_channel, bool fec = false) {
        return aurix_opus_decoder_decode_i16(d_, packet, packet_len, pcm, max_frame_samples_per_channel, fec);
    }
    /// Neural PLC / OSCE tuning (complexity >= 5 deep PLC, >= 6 OSCE; optional bandwidth extension).
    AurixResult apply(const AurixDecoderSettings& settings) { return aurix_opus_decoder_apply(d_, &settings); }
    bool settings(AurixDecoderSettings& out) const { return aurix_opus_decoder_settings(d_, &out); }
    /// Rebuild the frame `frames_before` frames before `later_packet` from its Deep REDundancy
    /// (1 = the frame right before it; use `decode(..., fec = true)` for that one when the packet
    /// has in-band FEC — see `packet_has_fec`). Samples per channel written, 0 when the packet's
    /// DRED does not reach that far (or this libopus has none), negative `AurixResult` on error.
    int decode_dred(const std::uint8_t* later_packet, std::size_t packet_len, std::uint32_t frames_before, float* pcm,
                    std::size_t frame_samples_per_channel) {
        return aurix_opus_decoder_dred_decode_f32(d_, later_packet, packet_len, frames_before, pcm,
                                                  frame_samples_per_channel);
    }
    int decode_dred(const std::uint8_t* later_packet, std::size_t packet_len, std::uint32_t frames_before,
                    std::int16_t* pcm, std::size_t frame_samples_per_channel) {
        return aurix_opus_decoder_dred_decode_i16(d_, later_packet, packet_len, frames_before, pcm,
                                                  frame_samples_per_channel);
    }
    /// Whether an Opus packet carries in-band FEC (LBRR) for the frame before it.
    static bool packet_has_fec(const std::uint8_t* packet, std::size_t packet_len) {
        return aurix_opus_packet_has_fec(packet, packet_len);
    }

private:
    void reset() {
        if (d_) {
            aurix_opus_decoder_destroy(d_);
            d_ = nullptr;
        }
    }
    AurixOpusDecoder* d_;
};

/// Bare voice-effect chain (the same stages `Client::set_voice_effects` runs) for hosts with
/// their own capture pipeline. Interleaved 48 kHz PCM in place. Move-only.
class VoiceEffects {
public:
    /// `effects == nullptr` is the bypass.
    explicit VoiceEffects(const AurixVoiceEffects* effects = nullptr) : p_(aurix_voice_effects_create(effects)) {}
    explicit VoiceEffects(AurixVoicePreset preset) : p_(nullptr) {
        AurixVoiceEffects e = aurix_voice_effects_preset(preset);
        p_ = aurix_voice_effects_create(&e);
    }
    VoiceEffects(VoiceEffects&& o) noexcept : p_(o.p_) { o.p_ = nullptr; }
    VoiceEffects& operator=(VoiceEffects&& o) noexcept {
        if (this != &o) {
            release();
            p_ = o.p_;
            o.p_ = nullptr;
        }
        return *this;
    }
    ~VoiceEffects() { release(); }
    VoiceEffects(const VoiceEffects&) = delete;
    VoiceEffects& operator=(const VoiceEffects&) = delete;

    bool valid() const { return p_ != nullptr; }
    AurixResult set(const AurixVoiceEffects& effects) { return aurix_voice_effects_set(p_, &effects); }
    AurixResult set(AurixVoicePreset preset) {
        AurixVoiceEffects e = aurix_voice_effects_preset(preset);
        return aurix_voice_effects_set(p_, &e);
    }
    bool get(AurixVoiceEffects& out) const { return aurix_voice_effects_get(p_, &out) == AURIX_OK; }
    bool is_bypass() const { return aurix_voice_effects_is_bypass(p_); }
    AurixResult process(float* pcm, std::size_t sample_count, std::uint8_t channels) {
        return aurix_voice_effects_process_f32(p_, pcm, sample_count, channels);
    }
    void reset() { aurix_voice_effects_reset(p_); }

private:
    void release() {
        if (p_) {
            aurix_voice_effects_destroy(p_);
            p_ = nullptr;
        }
    }
    AurixVoiceEffectsProcessor* p_;
};

/// Bare lip-sync analyser for one audio stream (the `Client` runs these itself when
/// `set_visemes(true)`). Move-only.
class VisemeAnalyzer {
public:
    VisemeAnalyzer() : a_(aurix_viseme_analyzer_create()) {}
    VisemeAnalyzer(VisemeAnalyzer&& o) noexcept : a_(o.a_) { o.a_ = nullptr; }
    VisemeAnalyzer& operator=(VisemeAnalyzer&& o) noexcept {
        if (this != &o) {
            release();
            a_ = o.a_;
            o.a_ = nullptr;
        }
        return *this;
    }
    ~VisemeAnalyzer() { release(); }
    VisemeAnalyzer(const VisemeAnalyzer&) = delete;
    VisemeAnalyzer& operator=(const VisemeAnalyzer&) = delete;

    bool valid() const { return a_ != nullptr; }
    /// One 20 ms frame of interleaved 48 kHz PCM (`sample_count` total samples).
    AurixResult push(const float* pcm, std::size_t sample_count, std::uint8_t channels) {
        return aurix_viseme_analyzer_push_f32(a_, pcm, sample_count, channels);
    }
    bool frame(AurixVisemeFrame& out) const { return aurix_viseme_analyzer_frame(a_, &out) == AURIX_OK; }
    void reset() { aurix_viseme_analyzer_reset(a_); }

private:
    void release() {
        if (a_) {
            aurix_viseme_analyzer_destroy(a_);
            a_ = nullptr;
        }
    }
    AurixVisemeAnalyzer* a_;
};

}  // namespace aurix

#endif  // AURIX_CLIENT_HPP
