// AurixVoiceClient — a Godot Node wrapping one native `aurix::Client`.
//
// The node pumps native events into signals from `_process`, feeds the microphone (an
// `AudioEffectCapture` on a bus it creates) into the client and plays the remote mix through an
// `AudioStreamGenerator`. Ids are RFC 4122 strings, records are Dictionaries (see
// `aurix_conversions.h`), enums mirror the C ABI values one-to-one.
#pragma once

#include "aurix_client.hpp"

#include <godot_cpp/classes/audio_effect_capture.hpp>
#include <godot_cpp/classes/audio_stream_generator_playback.hpp>
#include <godot_cpp/classes/audio_stream_player.hpp>
#include <godot_cpp/classes/node.hpp>
#include <godot_cpp/variant/packed_float32_array.hpp>
#include <godot_cpp/variant/packed_string_array.hpp>
#include <godot_cpp/variant/packed_vector2_array.hpp>
#include <godot_cpp/variant/transform3d.hpp>

#include <cstdint>
#include <vector>

namespace godot {

class AurixVoiceClient : public Node {
    GDCLASS(AurixVoiceClient, Node)

public:
    /// `AurixResult` codes returned by every call that reaches the native client.
    enum Result {
        RESULT_OK = AURIX_OK,
        RESULT_NULL_POINTER = AURIX_NULL_POINTER,
        RESULT_INVALID_ARGUMENT = AURIX_INVALID_ARGUMENT,
        RESULT_NOT_CONNECTED = AURIX_NOT_CONNECTED,
        RESULT_CLOSED = AURIX_CLOSED,
        RESULT_TRANSPORT = AURIX_TRANSPORT,
        RESULT_UNAUTHORIZED = AURIX_UNAUTHORIZED,
        RESULT_TIMEOUT = AURIX_TIMEOUT,
        RESULT_SERVER_REJECTED = AURIX_SERVER_REJECTED,
        RESULT_CODEC = AURIX_CODEC,
        RESULT_PROTOCOL = AURIX_PROTOCOL,
    };
    enum ConnectionState {
        STATE_DISCONNECTED = AURIX_STATE_DISCONNECTED,
        STATE_CONNECTING = AURIX_STATE_CONNECTING,
        STATE_CONNECTED = AURIX_STATE_CONNECTED,
        STATE_MEDIA_BOUND = AURIX_STATE_MEDIA_BOUND,
        STATE_RECONNECTING = AURIX_STATE_RECONNECTING,
        STATE_FAILED = AURIX_STATE_FAILED,
    };
    enum TransmissionMode {
        TRANSMIT_NONE = AURIX_TRANSMIT_NONE,
        TRANSMIT_SINGLE = AURIX_TRANSMIT_SINGLE,
        TRANSMIT_ALL = AURIX_TRANSMIT_ALL,
    };
    enum AudioCodec {
        CODEC_OPUS = AURIX_CODEC_OPUS,
        CODEC_PCMU = AURIX_CODEC_PCMU,
        CODEC_PCMA = AURIX_CODEC_PCMA,
    };
    enum DownlinkMode {
        DOWNLINK_STREAMS = AURIX_DOWNLINK_STREAMS,
        DOWNLINK_MIXED = AURIX_DOWNLINK_MIXED,
    };
    enum MediaPath {
        MEDIA_NONE = AURIX_MEDIA_NONE,
        MEDIA_UDP = AURIX_MEDIA_UDP,
        MEDIA_TUNNEL = AURIX_MEDIA_TUNNEL,
        MEDIA_QUIC = AURIX_MEDIA_QUIC,
        MEDIA_WEBRTC = AURIX_MEDIA_WEBRTC,
        MEDIA_TLS = AURIX_MEDIA_TLS,
    };
    enum MediaPathPolicy {
        MEDIA_PATH_AUTO = AURIX_MEDIA_PATH_AUTO,
        MEDIA_PATH_UDP_ONLY = AURIX_MEDIA_PATH_UDP_ONLY,
        MEDIA_PATH_TUNNEL_ONLY = AURIX_MEDIA_PATH_TUNNEL_ONLY,
        MEDIA_PATH_QUIC_ONLY = AURIX_MEDIA_PATH_QUIC_ONLY,
        MEDIA_PATH_TLS_ONLY = AURIX_MEDIA_PATH_TLS_ONLY,
    };
    enum ModerationAction {
        MODERATION_KICK = AURIX_MODERATION_KICK,
        MODERATION_MUTE = AURIX_MODERATION_MUTE,
        MODERATION_UNMUTE = AURIX_MODERATION_UNMUTE,
    };
    enum Role {
        ROLE_LISTENER = AURIX_ROLE_LISTENER,
        ROLE_SPEAKER = AURIX_ROLE_SPEAKER,
        ROLE_MODERATOR = AURIX_ROLE_MODERATOR,
        ROLE_ADMINISTRATOR = AURIX_ROLE_ADMINISTRATOR,
    };
    enum TtsDestination {
        TTS_BOTH = AURIX_TTS_BOTH,
        TTS_CHANNEL = AURIX_TTS_CHANNEL,
        TTS_LOCAL = AURIX_TTS_LOCAL,
    };
    enum TtsState {
        TTS_QUEUED = AURIX_TTS_QUEUED,
        TTS_PLAYING = AURIX_TTS_PLAYING,
        TTS_FINISHED = AURIX_TTS_FINISHED,
        TTS_CANCELLED = AURIX_TTS_CANCELLED,
        TTS_FAILED = AURIX_TTS_FAILED,
    };
    enum NoiseSuppression {
        NOISE_SUPPRESSION_OFF = AURIX_NOISE_SUPPRESSION_OFF,
        NOISE_SUPPRESSION_LOW = AURIX_NOISE_SUPPRESSION_LOW,
        NOISE_SUPPRESSION_MODERATE = AURIX_NOISE_SUPPRESSION_MODERATE,
        NOISE_SUPPRESSION_HIGH = AURIX_NOISE_SUPPRESSION_HIGH,
    };
    enum LossProfile {
        LOSS_PROFILE_LOW = AURIX_LOSS_PROFILE_LOW,
        LOSS_PROFILE_MODERATE = AURIX_LOSS_PROFILE_MODERATE,
        LOSS_PROFILE_HIGH = AURIX_LOSS_PROFILE_HIGH,
    };
    enum LossAdaptation {
        LOSS_ADAPTATION_AUTO = AURIX_LOSS_ADAPTATION_AUTO,
        LOSS_ADAPTATION_FIXED_LOW = AURIX_LOSS_ADAPTATION_FIXED_LOW,
        LOSS_ADAPTATION_FIXED_MODERATE = AURIX_LOSS_ADAPTATION_FIXED_MODERATE,
        LOSS_ADAPTATION_FIXED_HIGH = AURIX_LOSS_ADAPTATION_FIXED_HIGH,
    };
    enum OpusBandwidth {
        BANDWIDTH_NARROWBAND = AURIX_BANDWIDTH_NARROWBAND,
        BANDWIDTH_MEDIUMBAND = AURIX_BANDWIDTH_MEDIUMBAND,
        BANDWIDTH_WIDEBAND = AURIX_BANDWIDTH_WIDEBAND,
        BANDWIDTH_SUPERWIDEBAND = AURIX_BANDWIDTH_SUPERWIDEBAND,
        BANDWIDTH_FULLBAND = AURIX_BANDWIDTH_FULLBAND,
    };
    enum OpusSignal {
        SIGNAL_AUTO = AURIX_SIGNAL_AUTO,
        SIGNAL_VOICE = AURIX_SIGNAL_VOICE,
        SIGNAL_MUSIC = AURIX_SIGNAL_MUSIC,
    };
    /// Ready-made voices for `set_voice_preset` / `get_voice_preset`.
    enum VoicePreset {
        VOICE_PRESET_ROBOT = AURIX_VOICE_PRESET_ROBOT,
        VOICE_PRESET_MONSTER = AURIX_VOICE_PRESET_MONSTER,
        VOICE_PRESET_RADIO = AURIX_VOICE_PRESET_RADIO,
        VOICE_PRESET_HELIUM = AURIX_VOICE_PRESET_HELIUM,
        VOICE_PRESET_GHOST = AURIX_VOICE_PRESET_GHOST,
    };
    /// Mouth-shape buckets of a viseme frame's `weights` (index = value).
    enum Viseme {
        VISEME_SILENCE = AURIX_VISEME_SILENCE,
        VISEME_PP = AURIX_VISEME_PP,
        VISEME_FF = AURIX_VISEME_FF,
        VISEME_SS = AURIX_VISEME_SS,
        VISEME_AA = AURIX_VISEME_AA,
        VISEME_E = AURIX_VISEME_E,
        VISEME_IH = AURIX_VISEME_IH,
        VISEME_OH = AURIX_VISEME_OH,
        VISEME_OU = AURIX_VISEME_OU,
    };
    /// How remote voices reach the speakers.
    enum PlaybackMode {
        /// Everyone through the node's own stereo `AudioStreamPlayer` (`start_playback`).
        PLAYBACK_MIXED = 0,
        /// The mix plays the participants no `AurixParticipantPlayer` has claimed.
        PLAYBACK_PER_PARTICIPANT = 1,
        /// Only claimed participants are audible (unclaimed ones stay silent).
        PLAYBACK_PER_PARTICIPANT_ONLY = 2,
    };

    AurixVoiceClient();
    ~AurixVoiceClient() override;

    void _process(double delta) override;
    void _exit_tree() override;

    // --- configuration (before connect_to_server)
    void set_auto_reconnect(bool enabled);
    bool get_auto_reconnect() const;
    void set_reconnect_max_attempts(int attempts);
    int get_reconnect_max_attempts() const;
    void set_request_timeout_ms(int ms);
    int get_request_timeout_ms() const;
    void set_jitter_target_frames(int frames);
    int get_jitter_target_frames() const;
    void set_vad_gate_enabled(bool enabled);
    bool get_vad_gate_enabled() const;
    void set_follow_channel_policy(bool enabled);
    bool get_follow_channel_policy() const;
    void set_media_path_policy(MediaPathPolicy policy);
    MediaPathPolicy get_media_path_policy() const;
    void set_quic_enabled(bool enabled);
    bool get_quic_enabled() const;
    void set_tls_tunnel_enabled(bool enabled);
    bool get_tls_tunnel_enabled() const;
    void set_auto_capture(bool enabled);
    bool get_auto_capture() const;
    void set_auto_playback(bool enabled);
    bool get_auto_playback() const;
    void set_playback_buffer_seconds(double seconds);
    double get_playback_buffer_seconds() const;
    void set_playback_bus(const StringName& bus);
    StringName get_playback_bus() const;
    void set_playback_mode(PlaybackMode mode);
    PlaybackMode get_playback_mode() const;
    void set_max_events_per_frame(int count);
    int get_max_events_per_frame() const;
    void set_dsp_bypass(bool bypass);
    bool get_dsp_bypass() const;

    // --- lifecycle
    int connect_to_server(const String& ws_url, const String& token);
    void disconnect_from_server();
    int set_token(const String& token);
    bool is_client_created() const;
    /// Increments every time `connect_to_server` creates a native client; per-client state such
    /// as participant claims does not survive a new generation.
    int64_t get_client_generation() const;
    ConnectionState get_connection_state() const;
    Dictionary get_session() const;
    String get_endpoint() const;
    PackedStringArray get_failover_endpoints() const;
    MediaPath get_media_path() const;
    /// The device's network changed: a QUIC link migrates in place, a UDP link re-announces the
    /// session. `false` when not connected.
    bool network_changed();
    String get_last_error() const;
    static String get_native_version();

    // --- channels
    int64_t join_channel(const String& channel_id, const String& join_token);
    int leave_channel(const String& channel_id);
    PackedStringArray get_joined_channels() const;
    Array get_participants(const String& channel_id) const;
    Dictionary get_channel_info(const String& channel_id) const;
    bool can_speak_in(const String& channel_id) const;
    bool is_waiting_to_speak(const String& channel_id) const;
    bool channel_transcribes(const String& channel_id) const;
    bool channel_monitored(const String& channel_id) const;
    Dictionary get_channel_scope(const String& channel_id) const;
    String user_for_ssrc(int64_t ssrc) const;

    // --- microphone / uplink
    bool start_capture();
    void stop_capture();
    bool is_capturing() const;
    void push_capture(const PackedVector2Array& frames, int sample_rate_hz);
    void push_capture_mono(const PackedFloat32Array& samples, int sample_rate_hz);
    void set_muted(bool muted);
    bool is_muted() const;
    bool is_speaking() const;
    void set_input_gain(double gain);
    double get_input_energy() const;
    void set_vad(double threshold, int hangover_frames);
    void set_vad_gate(bool enabled);
    int set_bitrate(int bps);
    int set_complexity(int complexity);
    int set_encoder_settings(const Dictionary& settings);
    Dictionary get_encoder_settings() const;
    Dictionary get_audio_policy() const;
    int set_loss_adaptation(LossAdaptation adaptation);
    LossAdaptation get_loss_adaptation() const;
    LossProfile get_loss_profile() const;
    int set_decoder_settings(const Dictionary& settings);
    Dictionary get_decoder_settings() const;
    static bool is_dred_supported();
    int set_dsp(const Dictionary& config);
    Dictionary get_dsp() const;
    Dictionary get_dsp_stats() const;
    int set_voice_effects(const Dictionary& effects);
    Dictionary get_voice_effects() const;
    Dictionary get_voice_preset(VoicePreset preset) const;
    int set_voice_preset(VoicePreset preset);
    void reset_capture();

    // --- lip-sync (local analysis of decoded audio; nothing leaves the machine)
    void set_visemes_enabled(bool enabled);
    bool get_visemes_enabled() const;
    Dictionary get_participant_visemes(const String& user_id) const;
    Dictionary get_local_visemes() const;

    // --- playback / downlink
    bool start_playback();
    void stop_playback();
    bool is_playing() const;
    PackedVector2Array mix_output(int frames);
    PackedVector2Array pull_participant(const String& user_id, int frames);
    int set_participant_claimed(const String& user_id, bool claimed);
    Array get_participant_streams() const;
    void push_render(const PackedVector2Array& frames);
    void set_output_volume(double volume);
    void set_output_muted(bool muted);

    // --- receiver preferences
    int set_participant_mute(const String& user_id, const String& channel_id, bool muted);
    int set_participant_volume(const String& user_id, double volume);
    int set_user_block(const String& user_id, bool blocked);
    int set_priority(const String& channel_id, const String& user_id, bool priority);
    bool is_ducking_active(const String& channel_id) const;
    int set_transmission(TransmissionMode mode, const String& channel_id);
    int set_channel_focus(const String& channel_id);
    int set_audio_codec(AudioCodec codec);
    AudioCodec get_audio_codec() const;
    int set_downlink_mode(DownlinkMode mode);
    DownlinkMode get_downlink_mode() const;
    int set_server_noise_suppression(bool enabled);
    bool get_server_noise_suppression() const;
    int set_transcripts(bool enabled);
    int set_translation(const String& language, const String& spoken_language, bool speech);
    int update_positions(const String& channel_id, const Array& positions);
    int update_transforms(const String& channel_id, const Dictionary& transforms);
    int respond_recording_consent(const String& recording_id, bool accepted);
    int send_control_json(const String& json);

    // --- chat / moderation / speech
    int64_t send_chat(const String& channel_id, const String& text, const String& metadata_json);
    int64_t send_direct_chat(const String& user_id, const String& text, const String& metadata_json);
    int set_typing(const String& channel_id, bool typing);
    int64_t channel_history(const String& channel_id, const String& before, const String& after, int limit);
    int64_t direct_history(const String& user_id, const String& before, const String& after, int limit);
    int mark_channel_read(const String& channel_id, const String& message_id);
    int mark_direct_read(const String& user_id, const String& message_id);
    int channel_read_markers(const String& channel_id);
    int direct_read_markers(const String& user_id);
    int64_t edit_chat(const String& message_id, const String& text, const String& metadata_json);
    int64_t delete_chat(const String& message_id);
    int react_chat(const String& message_id, const String& reaction, bool add);
    int64_t search_channel_chat(const String& channel_id, const String& query, const String& from_user_id, const String& before, int limit);
    int64_t search_direct_chat(const String& user_id, const String& query, const String& from_user_id, const String& before, int limit);
    int64_t moderate(const String& channel_id, const String& user_id, ModerationAction action,
                     const String& action_token, const String& reason);
    int64_t speak(const String& text, const String& channel_id, TtsDestination destination, const String& voice);
    int cancel_speech();

    // --- diagnostics
    Dictionary get_stats() const;
    Dictionary get_network_quality() const;

    /// Native handle for sibling nodes (`AurixParticipantPlayer`); null before connect.
    aurix::Client* native() { return client_ ? &client_ : nullptr; }

protected:
    static void _bind_methods();

private:
    void pump_events();
    void dispatch(const aurix::Event& ev);
    void feed_capture();
    void feed_playback();
    void destroy_capture_bus();

    aurix::Client client_;
    AurixClientConfig config_{};
    bool dsp_bypass_ = false;
    bool visemes_enabled_ = false;
    int64_t client_generation_ = 0;
    bool auto_capture_ = true;
    bool auto_playback_ = true;
    double playback_buffer_seconds_ = 0.06;
    StringName playback_bus_ = StringName("Master");
    PlaybackMode playback_mode_ = PLAYBACK_MIXED;
    int max_events_per_frame_ = 64;

    // microphone: AudioStreamMicrophone → private muted bus → AudioEffectCapture → client
    AudioStreamPlayer* mic_player_ = nullptr;
    Ref<AudioEffectCapture> capture_effect_;
    int capture_bus_index_ = -1;
    std::vector<float> capture_scratch_;

    // playback: client mix → AudioStreamGenerator → AudioStreamPlayer
    AudioStreamPlayer* out_player_ = nullptr;
    Ref<AudioStreamGeneratorPlayback> out_playback_;
    bool playback_requested_ = false;
    std::vector<float> mix_scratch_;
};

}  // namespace godot

VARIANT_ENUM_CAST(godot::AurixVoiceClient::Result);
VARIANT_ENUM_CAST(godot::AurixVoiceClient::LossProfile);
VARIANT_ENUM_CAST(godot::AurixVoiceClient::LossAdaptation);
VARIANT_ENUM_CAST(godot::AurixVoiceClient::ConnectionState);
VARIANT_ENUM_CAST(godot::AurixVoiceClient::TransmissionMode);
VARIANT_ENUM_CAST(godot::AurixVoiceClient::AudioCodec);
VARIANT_ENUM_CAST(godot::AurixVoiceClient::DownlinkMode);
VARIANT_ENUM_CAST(godot::AurixVoiceClient::MediaPath);
VARIANT_ENUM_CAST(godot::AurixVoiceClient::MediaPathPolicy);
VARIANT_ENUM_CAST(godot::AurixVoiceClient::ModerationAction);
VARIANT_ENUM_CAST(godot::AurixVoiceClient::Role);
VARIANT_ENUM_CAST(godot::AurixVoiceClient::TtsDestination);
VARIANT_ENUM_CAST(godot::AurixVoiceClient::TtsState);
VARIANT_ENUM_CAST(godot::AurixVoiceClient::NoiseSuppression);
VARIANT_ENUM_CAST(godot::AurixVoiceClient::OpusBandwidth);
VARIANT_ENUM_CAST(godot::AurixVoiceClient::OpusSignal);
VARIANT_ENUM_CAST(godot::AurixVoiceClient::PlaybackMode);
VARIANT_ENUM_CAST(godot::AurixVoiceClient::VoicePreset);
VARIANT_ENUM_CAST(godot::AurixVoiceClient::Viseme);
