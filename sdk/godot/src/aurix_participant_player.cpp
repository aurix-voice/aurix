#include "aurix_participant_player.h"

#include "aurix_voice_client.h"

#include <godot_cpp/classes/audio_stream_generator.hpp>
#include <godot_cpp/classes/engine.hpp>
#include <godot_cpp/core/class_db.hpp>

#include <algorithm>

namespace godot {

AurixParticipantPlayer::AurixParticipantPlayer() { set_process(true); }

AurixVoiceClient* AurixParticipantPlayer::get_client() const {
    if (!client_path_.is_empty()) {
        return Object::cast_to<AurixVoiceClient>(get_node_or_null(client_path_));
    }
    for (Node* n = get_parent(); n != nullptr; n = n->get_parent()) {
        if (AurixVoiceClient* c = Object::cast_to<AurixVoiceClient>(n)) return c;
    }
    return nullptr;
}

void AurixParticipantPlayer::ensure_generator() {
    Ref<AudioStreamGenerator> gen = get_stream();
    if (gen.is_null()) {
        gen.instantiate();
        gen->set_mix_rate(48000.0f);
        gen->set_buffer_length(static_cast<float>(buffer_seconds_));
        set_stream(gen);
    }
    if (!is_playing()) play();
    playback_ = Ref<AudioStreamGeneratorPlayback>(Object::cast_to<AudioStreamGeneratorPlayback>(get_stream_playback().ptr()));
}

void AurixParticipantPlayer::claim(bool claimed) {
    if (claimed == claimed_) return;
    AurixVoiceClient* client = get_client();
    if (client && !user_id_.is_empty() && client->is_client_created() &&
        client->get_playback_mode() != AurixVoiceClient::PLAYBACK_MIXED) {
        client->set_participant_claimed(user_id_, claimed);
        claimed_generation_ = client->get_client_generation();
        claimed_ = claimed;
        return;
    }
    // Nothing to claim (mixed mode or no native client yet); retried from _process.
    claimed_ = false;
}

void AurixParticipantPlayer::_ready() {
    if (Engine::get_singleton()->is_editor_hint()) return;
    ensure_generator();
}

void AurixParticipantPlayer::_process(double) {
    if (Engine::get_singleton()->is_editor_hint()) return;
    AurixVoiceClient* client = get_client();
    if (!client || !client->is_client_created() || user_id_.is_empty()) {
        active_ = false;
        return;
    }
    // Claims are per user and survive reconnects inside the native client, but a client that was
    // (re)created after this node entered the tree needs the claim again.
    if (claimed_ && claimed_generation_ != client->get_client_generation()) claimed_ = false;
    if (!claimed_) claim(true);
    if (playback_.is_null() || !is_playing()) ensure_generator();
    if (playback_.is_null()) return;

    const int frames = playback_->get_frames_available();
    if (frames <= 0) return;
    const PackedVector2Array pcm = client->pull_participant(user_id_, frames);
    bool any = false;
    const Vector2* p = pcm.ptr();
    for (int64_t i = 0; i < pcm.size() && !any; ++i) {
        any = p[i].x != 0.0f || p[i].y != 0.0f;
    }
    active_ = any;
    playback_->push_buffer(pcm);
}

void AurixParticipantPlayer::_exit_tree() {
    claim(false);
    playback_.unref();
}

void AurixParticipantPlayer::set_user_id(const String& user_id) {
    if (user_id == user_id_) return;
    claim(false);
    user_id_ = user_id;
    claimed_ = false;
}
String AurixParticipantPlayer::get_user_id() const { return user_id_; }
void AurixParticipantPlayer::set_client_path(const NodePath& path) {
    claim(false);
    client_path_ = path;
    claimed_ = false;
}
NodePath AurixParticipantPlayer::get_client_path() const { return client_path_; }
void AurixParticipantPlayer::set_buffer_seconds(double seconds) { buffer_seconds_ = std::clamp(seconds, 0.02, 1.0); }
double AurixParticipantPlayer::get_buffer_seconds() const { return buffer_seconds_; }
bool AurixParticipantPlayer::is_active() const { return active_; }

void AurixParticipantPlayer::_bind_methods() {
    ClassDB::bind_method(D_METHOD("set_user_id", "user_id"), &AurixParticipantPlayer::set_user_id);
    ClassDB::bind_method(D_METHOD("get_user_id"), &AurixParticipantPlayer::get_user_id);
    ClassDB::bind_method(D_METHOD("set_client_path", "path"), &AurixParticipantPlayer::set_client_path);
    ClassDB::bind_method(D_METHOD("get_client_path"), &AurixParticipantPlayer::get_client_path);
    ClassDB::bind_method(D_METHOD("set_buffer_seconds", "seconds"), &AurixParticipantPlayer::set_buffer_seconds);
    ClassDB::bind_method(D_METHOD("get_buffer_seconds"), &AurixParticipantPlayer::get_buffer_seconds);
    ClassDB::bind_method(D_METHOD("is_active"), &AurixParticipantPlayer::is_active);
    ClassDB::bind_method(D_METHOD("get_client"), &AurixParticipantPlayer::get_client);

    ADD_PROPERTY(PropertyInfo(Variant::STRING, "user_id"), "set_user_id", "get_user_id");
    ADD_PROPERTY(PropertyInfo(Variant::NODE_PATH, "client_path", PROPERTY_HINT_NODE_PATH_VALID_TYPES, "AurixVoiceClient"), "set_client_path", "get_client_path");
    ADD_PROPERTY(PropertyInfo(Variant::FLOAT, "buffer_seconds", PROPERTY_HINT_RANGE, "0.02,1.0,0.01"), "set_buffer_seconds", "get_buffer_seconds");
}

}  // namespace godot
