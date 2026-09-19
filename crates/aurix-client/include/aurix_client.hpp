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
    AurixModerationAction moderation_action() const { return aurix_event_moderation_action(ev_); }

    bool session(AurixSessionInfo& out) const { return aurix_event_session(ev_, &out); }
    bool chat(AurixChatMessage& out) const { return aurix_event_chat(ev_, &out); }
    bool transcript(AurixTranscript& out) const { return aurix_event_transcript(ev_, &out); }
    bool tts(AurixTtsStatus& out) const { return aurix_event_tts(ev_, &out); }

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
    void set_muted(bool muted) { aurix_client_set_muted(c_, muted); }
    bool is_muted() const { return aurix_client_is_muted(c_); }
    bool is_speaking() const { return aurix_client_is_speaking(c_); }
    void set_input_gain(float gain) { aurix_client_set_input_gain(c_, gain); }
    float input_energy() const { return aurix_client_input_energy(c_); }
    void set_vad(float threshold, std::uint32_t hangover_frames) { aurix_client_set_vad(c_, threshold, hangover_frames); }
    void set_vad_gate(bool enabled) { aurix_client_set_vad_gate(c_, enabled); }
    AurixResult set_bitrate(std::uint32_t bps) { return aurix_client_set_bitrate(c_, bps); }
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
    AurixResult set_transmission(AurixTransmissionMode mode, const Uuid* channel) {
        return aurix_client_set_transmission(c_, mode, channel ? &channel->raw : nullptr);
    }
    AurixResult set_channel_focus(const Uuid* channel) {
        return aurix_client_set_channel_focus(c_, channel ? &channel->raw : nullptr);
    }
    AurixResult set_transcripts(bool enabled) { return aurix_client_set_transcripts(c_, enabled); }
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

}  // namespace aurix

#endif  // AURIX_CLIENT_HPP
