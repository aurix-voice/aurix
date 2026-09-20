// AurixParticipantPlayer — an AudioStreamPlayer3D that plays exactly one remote participant.
//
// Set `user_id` and point `client_path` at an AurixVoiceClient (or leave it empty to use the
// nearest ancestor). While in the tree the participant is *claimed*: their decoded voice is pulled
// out of the client's mix and pushed into this node's AudioStreamGenerator, so Godot's own
// attenuation, doppler, reverb buses and listener handle spatialisation.
#pragma once

#include <godot_cpp/classes/audio_stream_generator_playback.hpp>
#include <godot_cpp/classes/audio_stream_player3d.hpp>

namespace godot {

class AurixVoiceClient;

class AurixParticipantPlayer : public AudioStreamPlayer3D {
    GDCLASS(AurixParticipantPlayer, AudioStreamPlayer3D)

public:
    AurixParticipantPlayer();

    void _ready() override;
    void _process(double delta) override;
    void _exit_tree() override;

    void set_user_id(const String& user_id);
    String get_user_id() const;
    void set_client_path(const NodePath& path);
    NodePath get_client_path() const;
    void set_buffer_seconds(double seconds);
    double get_buffer_seconds() const;
    /// True while the participant produced audio in the last frame (lip-sync / speaking icons).
    bool is_active() const;
    AurixVoiceClient* get_client() const;

protected:
    static void _bind_methods();

private:
    void claim(bool claimed);
    void ensure_generator();

    String user_id_;
    NodePath client_path_;
    double buffer_seconds_ = 0.06;
    bool claimed_ = false;
    int64_t claimed_generation_ = 0;
    bool active_ = false;
    Ref<AudioStreamGeneratorPlayback> playback_;
};

}  // namespace godot
