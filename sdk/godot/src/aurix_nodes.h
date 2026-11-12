// Scene-level helpers on top of AurixVoiceClient — the Godot counterparts of the Unity components
// (AurixPositionalSource, AurixLipSync, AurixGameAudioDucker) and of the Unreal sample components
// (lobby / proximity voice). Each node finds its client through `client_path` or the nearest
// AurixVoiceClient ancestor, the same way AurixParticipantPlayer does.
#pragma once

#include <godot_cpp/classes/mesh_instance3d.hpp>
#include <godot_cpp/classes/node.hpp>
#include <godot_cpp/classes/node3d.hpp>
#include <godot_cpp/variant/packed_float32_array.hpp>

namespace godot {

class AurixVoiceClient;
class AurixParticipantPlayer;

/// Resolves the client for a helper node: explicit `client_path`, else the nearest ancestor.
AurixVoiceClient* aurix_find_client(const Node* from, const NodePath& client_path);

/// Reports this node's (or `pose_source`'s) transform as the local player's position in a channel,
/// rate-limited and only when it moved or turned. Attach to the player's head or camera.
class AurixPositionalSource : public Node3D {
    GDCLASS(AurixPositionalSource, Node3D)
public:
    AurixPositionalSource();
    void _process(double delta) override;

    void set_channel_id(const String& id);
    String get_channel_id() const;
    void set_client_path(const NodePath& path);
    NodePath get_client_path() const;
    void set_pose_source(const NodePath& path);
    NodePath get_pose_source() const;
    void set_updates_per_second(double hz);
    double get_updates_per_second() const;
    void set_min_move(double meters);
    double get_min_move() const;
    void set_min_turn_degrees(double degrees);
    double get_min_turn_degrees() const;
    void set_world_to_meters(double scale);
    double get_world_to_meters() const;
    int get_updates_sent() const;
    /// Send the current pose now regardless of the rate limit. False when not connected/joined.
    bool send_pose_now();

protected:
    static void _bind_methods();

private:
    Transform3D read_pose() const;

    String channel_id_;
    NodePath client_path_;
    NodePath pose_source_;
    double updates_per_second_ = 10.0;
    double min_move_ = 0.1;
    double min_turn_degrees_ = 5.0;
    double world_to_meters_ = 1.0;
    double since_last_ = 0.0;
    bool has_last_ = false;
    Transform3D last_;
    int updates_sent_ = 0;
};

/// Drives blend shapes of a MeshInstance3D from the viseme analysis of one participant (or the
/// local microphone when `user_id` is empty). Weights are smoothed; `frame` fires every frame.
class AurixLipSync : public Node {
    GDCLASS(AurixLipSync, Node)
public:
    AurixLipSync();
    void _process(double delta) override;
    void _exit_tree() override;

    void set_user_id(const String& id);
    String get_user_id() const;
    void set_client_path(const NodePath& path);
    NodePath get_client_path() const;
    void set_target(const NodePath& path);
    NodePath get_target() const;
    void set_blend_shapes(const PackedStringArray& names);
    PackedStringArray get_blend_shapes() const;
    void set_jaw_blend_shape(const String& name);
    String get_jaw_blend_shape() const;
    void set_smoothing(double seconds);
    double get_smoothing() const;
    void set_enable_analysis(bool on);
    bool get_enable_analysis() const;
    bool is_local() const;
    bool is_analysing() const;
    PackedFloat32Array get_weights() const;
    double get_mouth_open() const;
    int get_dominant() const;

protected:
    static void _bind_methods();

private:
    void apply(MeshInstance3D* mesh);
    void reset_state();

    String user_id_;
    NodePath client_path_;
    NodePath target_;
    PackedStringArray blend_shapes_;
    String jaw_blend_shape_;
    double smoothing_ = 0.04;
    bool enable_analysis_ = true;
    bool enabled_on_client_ = false;
    int64_t enabled_generation_ = -1;
    int64_t last_sequence_ = -1;
    double stale_for_ = 0.0;
    bool analysing_ = false;
    PackedFloat32Array current_;
    double mouth_open_ = 0.0;
    int dominant_ = 0;
};

/// Attenuates an audio bus while someone speaks in a channel whose policy has ducking enabled
/// (server-driven). Reads `ducking_changed` and shapes a smooth gain; `gain_changed` exposes it
/// for anything that is not a bus (subtitles, UI, your own mixer).
class AurixGameAudioDucker : public Node {
    GDCLASS(AurixGameAudioDucker, Node)
public:
    AurixGameAudioDucker();
    void _process(double delta) override;
    void _exit_tree() override;

    void set_channel_id(const String& id);
    String get_channel_id() const;
    void set_client_path(const NodePath& path);
    NodePath get_client_path() const;
    void set_bus(const StringName& bus);
    StringName get_bus() const;
    void set_duck_gain(double gain);
    double get_duck_gain() const;
    void set_attack_ms(int ms);
    int get_attack_ms() const;
    void set_release_ms(int ms);
    int get_release_ms() const;
    void set_hold_ms(int ms);
    int get_hold_ms() const;
    void set_override_envelope(bool on);
    bool get_override_envelope() const;
    double get_current_gain() const;
    bool is_ducking() const;

protected:
    static void _bind_methods();

private:
    void on_ducking_changed(const String& channel_id, bool active, const Dictionary& ducking);
    void connect_client();

    String channel_id_;
    NodePath client_path_;
    StringName bus_;
    double duck_gain_ = 0.25;
    int attack_ms_ = 60;
    int release_ms_ = 400;
    int hold_ms_ = 250;
    bool override_envelope_ = false;
    AurixVoiceClient* connected_ = nullptr;
    bool active_ = false;
    double target_ = 1.0;
    double current_ = 1.0;
    double hold_left_ = 0.0;
    double server_gain_ = 0.25;
    int server_attack_ = 60, server_release_ = 400, server_hold_ = 250;
    float base_volume_db_ = 0.0f;
    bool base_captured_ = false;
};

/// Party / lobby voice in one node: joins `channel_id` when the session is ready (and again after a
/// reconnect), keeps a roster, mirrors speaking state. Everyone is heard through the client's mix.
class AurixVoiceLobby : public Node {
    GDCLASS(AurixVoiceLobby, Node)
public:
    AurixVoiceLobby();
    void _process(double delta) override;
    void _exit_tree() override;

    void set_channel_id(const String& id);
    String get_channel_id() const;
    void set_join_token(const String& token);
    String get_join_token() const;
    void set_client_path(const NodePath& path);
    NodePath get_client_path() const;
    void set_auto_join(bool on);
    bool get_auto_join() const;
    bool is_joined() const;
    Array get_roster() const;
    Array get_speakers() const;
    int join_now();
    int leave_now();

protected:
    static void _bind_methods();

private:
    void connect_client();
    void on_channel_joined(int request_id, const String& channel_id, const Array& participants, const Dictionary& info);
    void on_channel_left(const String& channel_id);
    void on_participant_joined(const String& channel_id, const Dictionary& participant);
    void on_participant_left(const String& channel_id, const String& user_id);
    void on_participant_speaking(const String& channel_id, const String& user_id, bool speaking);
    void on_state_changed(int state);

    String channel_id_;
    String join_token_;
    NodePath client_path_;
    bool auto_join_ = true;
    bool joined_ = false;
    bool join_pending_ = false;
    AurixVoiceClient* connected_ = nullptr;
    Dictionary roster_;  // user_id → participant dictionary
};

/// Proximity voice: joins a positional channel, sends this node's transform as the player's pose and
/// spawns one AurixParticipantPlayer (AudioStreamPlayer3D) per remote speaker under `speakers_root`,
/// placing it at the position the server reports — attenuation, reverb and occlusion are Godot's.
class AurixProximityVoice : public Node3D {
    GDCLASS(AurixProximityVoice, Node3D)
public:
    AurixProximityVoice();
    void _process(double delta) override;
    void _exit_tree() override;

    void set_channel_id(const String& id);
    String get_channel_id() const;
    void set_join_token(const String& token);
    String get_join_token() const;
    void set_client_path(const NodePath& path);
    NodePath get_client_path() const;
    void set_speakers_root(const NodePath& path);
    NodePath get_speakers_root() const;
    void set_player_template(const NodePath& path);
    NodePath get_player_template() const;
    void set_updates_per_second(double hz);
    double get_updates_per_second() const;
    void set_world_to_meters(double scale);
    double get_world_to_meters() const;
    void set_place_speakers(bool on);
    bool get_place_speakers() const;
    bool is_joined() const;
    Dictionary get_speaker_players() const;
    AurixParticipantPlayer* get_speaker_player(const String& user_id) const;
    /// Place a remote speaker yourself (e.g. from your own netcode) instead of the server positions.
    void set_speaker_position(const String& user_id, const Vector3& position);

protected:
    static void _bind_methods();

private:
    void connect_client();
    void ensure_player(const Dictionary& participant);
    void release_player(const String& user_id);
    void release_all();
    void on_channel_joined(int request_id, const String& channel_id, const Array& participants, const Dictionary& info);
    void on_channel_left(const String& channel_id);
    void on_participant_joined(const String& channel_id, const Dictionary& participant);
    void on_participant_left(const String& channel_id, const String& user_id);
    void on_positions(const String& channel_id, const String& json);
    void on_state_changed(int state);

    String channel_id_;
    String join_token_;
    NodePath client_path_;
    NodePath speakers_root_;
    NodePath player_template_;
    double updates_per_second_ = 10.0;
    double world_to_meters_ = 1.0;
    bool place_speakers_ = true;
    bool joined_ = false;
    bool join_pending_ = false;
    double since_last_ = 0.0;
    AurixVoiceClient* connected_ = nullptr;
    Dictionary players_;  // user_id → AurixParticipantPlayer*
};

}  // namespace godot
