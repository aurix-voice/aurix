#include "aurix_nodes.h"

#include "aurix_participant_player.h"
#include "aurix_voice_client.h"

#include <godot_cpp/classes/audio_server.hpp>
#include <godot_cpp/classes/engine.hpp>
#include <godot_cpp/classes/json.hpp>
#include <godot_cpp/classes/mesh.hpp>
#include <godot_cpp/core/class_db.hpp>
#include <godot_cpp/variant/utility_functions.hpp>

#include <algorithm>
#include <cmath>

namespace godot {

AurixVoiceClient* aurix_find_client(const Node* from, const NodePath& client_path) {
    if (!client_path.is_empty()) return Object::cast_to<AurixVoiceClient>(from->get_node_or_null(client_path));
    for (Node* n = from->get_parent(); n != nullptr; n = n->get_parent()) {
        if (AurixVoiceClient* c = Object::cast_to<AurixVoiceClient>(n)) return c;
    }
    return nullptr;
}

static bool in_editor() { return Engine::get_singleton()->is_editor_hint(); }
static bool joined(AurixVoiceClient* c, const String& channel) {
    if (!c || channel.is_empty() || !c->is_client_created()) return false;
    const Array chs = c->get_joined_channels();
    for (int64_t i = 0; i < chs.size(); ++i) if (String(chs[i]) == channel) return true;
    return false;
}

// ---- AurixPositionalSource ---------------------------------------------------------------------

AurixPositionalSource::AurixPositionalSource() { set_process(true); }

Transform3D AurixPositionalSource::read_pose() const {
    if (!pose_source_.is_empty()) {
        if (Node3D* n = Object::cast_to<Node3D>(get_node_or_null(pose_source_))) return n->get_global_transform();
    }
    return get_global_transform();
}

bool AurixPositionalSource::send_pose_now() {
    AurixVoiceClient* c = aurix_find_client(this, client_path_);
    if (!joined(c, channel_id_)) return false;
    const String self = c->get_user_id();
    if (self.is_empty()) return false;
    Transform3D t = read_pose();
    last_ = t; has_last_ = true; since_last_ = 0.0;
    t.origin = t.origin / static_cast<real_t>(world_to_meters_);
    Dictionary transforms;
    transforms[self] = t;
    if (c->update_transforms(channel_id_, transforms) != AURIX_OK) return false;
    ++updates_sent_;
    return true;
}

void AurixPositionalSource::_process(double delta) {
    if (in_editor() || channel_id_.is_empty()) return;
    since_last_ += delta;
    if (since_last_ < 1.0 / std::max(1.0, updates_per_second_)) return;
    const Transform3D t = read_pose();
    if (has_last_) {
        const double moved = (t.origin - last_.origin).length();
        const double turned = Math::rad_to_deg(std::acos(std::clamp(static_cast<double>(
            (-t.basis.get_column(2)).normalized().dot((-last_.basis.get_column(2)).normalized())), -1.0, 1.0)));
        if (moved < min_move_ * world_to_meters_ && turned < min_turn_degrees_) return;
    }
    send_pose_now();
}

void AurixPositionalSource::set_channel_id(const String& id) { channel_id_ = id; has_last_ = false; if (!id.is_empty() && is_inside_tree() && !in_editor()) send_pose_now(); }
String AurixPositionalSource::get_channel_id() const { return channel_id_; }
void AurixPositionalSource::set_client_path(const NodePath& p) { client_path_ = p; }
NodePath AurixPositionalSource::get_client_path() const { return client_path_; }
void AurixPositionalSource::set_pose_source(const NodePath& p) { pose_source_ = p; }
NodePath AurixPositionalSource::get_pose_source() const { return pose_source_; }
void AurixPositionalSource::set_updates_per_second(double hz) { updates_per_second_ = std::clamp(hz, 1.0, 60.0); }
double AurixPositionalSource::get_updates_per_second() const { return updates_per_second_; }
void AurixPositionalSource::set_min_move(double m) { min_move_ = std::max(0.0, m); }
double AurixPositionalSource::get_min_move() const { return min_move_; }
void AurixPositionalSource::set_min_turn_degrees(double d) { min_turn_degrees_ = std::clamp(d, 0.0, 180.0); }
double AurixPositionalSource::get_min_turn_degrees() const { return min_turn_degrees_; }
void AurixPositionalSource::set_world_to_meters(double s) { world_to_meters_ = s > 0.0 ? s : 1.0; }
double AurixPositionalSource::get_world_to_meters() const { return world_to_meters_; }
int AurixPositionalSource::get_updates_sent() const { return updates_sent_; }

void AurixPositionalSource::_bind_methods() {
    ClassDB::bind_method(D_METHOD("set_channel_id", "channel_id"), &AurixPositionalSource::set_channel_id);
    ClassDB::bind_method(D_METHOD("get_channel_id"), &AurixPositionalSource::get_channel_id);
    ClassDB::bind_method(D_METHOD("set_client_path", "path"), &AurixPositionalSource::set_client_path);
    ClassDB::bind_method(D_METHOD("get_client_path"), &AurixPositionalSource::get_client_path);
    ClassDB::bind_method(D_METHOD("set_pose_source", "path"), &AurixPositionalSource::set_pose_source);
    ClassDB::bind_method(D_METHOD("get_pose_source"), &AurixPositionalSource::get_pose_source);
    ClassDB::bind_method(D_METHOD("set_updates_per_second", "hz"), &AurixPositionalSource::set_updates_per_second);
    ClassDB::bind_method(D_METHOD("get_updates_per_second"), &AurixPositionalSource::get_updates_per_second);
    ClassDB::bind_method(D_METHOD("set_min_move", "meters"), &AurixPositionalSource::set_min_move);
    ClassDB::bind_method(D_METHOD("get_min_move"), &AurixPositionalSource::get_min_move);
    ClassDB::bind_method(D_METHOD("set_min_turn_degrees", "degrees"), &AurixPositionalSource::set_min_turn_degrees);
    ClassDB::bind_method(D_METHOD("get_min_turn_degrees"), &AurixPositionalSource::get_min_turn_degrees);
    ClassDB::bind_method(D_METHOD("set_world_to_meters", "scale"), &AurixPositionalSource::set_world_to_meters);
    ClassDB::bind_method(D_METHOD("get_world_to_meters"), &AurixPositionalSource::get_world_to_meters);
    ClassDB::bind_method(D_METHOD("get_updates_sent"), &AurixPositionalSource::get_updates_sent);
    ClassDB::bind_method(D_METHOD("send_pose_now"), &AurixPositionalSource::send_pose_now);
    ADD_PROPERTY(PropertyInfo(Variant::STRING, "channel_id"), "set_channel_id", "get_channel_id");
    ADD_PROPERTY(PropertyInfo(Variant::NODE_PATH, "client_path", PROPERTY_HINT_NODE_PATH_VALID_TYPES, "AurixVoiceClient"), "set_client_path", "get_client_path");
    ADD_PROPERTY(PropertyInfo(Variant::NODE_PATH, "pose_source", PROPERTY_HINT_NODE_PATH_VALID_TYPES, "Node3D"), "set_pose_source", "get_pose_source");
    ADD_PROPERTY(PropertyInfo(Variant::FLOAT, "updates_per_second", PROPERTY_HINT_RANGE, "1,60,1"), "set_updates_per_second", "get_updates_per_second");
    ADD_PROPERTY(PropertyInfo(Variant::FLOAT, "min_move", PROPERTY_HINT_RANGE, "0,5,0.01,suffix:m"), "set_min_move", "get_min_move");
    ADD_PROPERTY(PropertyInfo(Variant::FLOAT, "min_turn_degrees", PROPERTY_HINT_RANGE, "0,180,0.5"), "set_min_turn_degrees", "get_min_turn_degrees");
    ADD_PROPERTY(PropertyInfo(Variant::FLOAT, "world_to_meters", PROPERTY_HINT_RANGE, "0.001,1000,0.001"), "set_world_to_meters", "get_world_to_meters");
}

// ---- AurixLipSync ------------------------------------------------------------------------------

AurixLipSync::AurixLipSync() { set_process(true); current_.resize(AURIX_VISEME_COUNT); current_.fill(0.0f); }

void AurixLipSync::reset_state() { current_.fill(0.0f); mouth_open_ = 0.0; dominant_ = 0; last_sequence_ = -1; stale_for_ = 0.0; analysing_ = false; }

void AurixLipSync::apply(MeshInstance3D* mesh) {
    if (!mesh) return;
    const int n = static_cast<int>(std::min<int64_t>(blend_shapes_.size(), current_.size()));
    for (int i = 0; i < n; ++i) {
        const String& name = blend_shapes_[i];
        if (name.is_empty()) continue;
        const int idx = mesh->find_blend_shape_by_name(name);
        if (idx >= 0) mesh->set_blend_shape_value(idx, current_[i]);
    }
    if (!jaw_blend_shape_.is_empty()) {
        const int idx = mesh->find_blend_shape_by_name(jaw_blend_shape_);
        if (idx >= 0) mesh->set_blend_shape_value(idx, static_cast<float>(mouth_open_));
    }
}

void AurixLipSync::_process(double delta) {
    if (in_editor()) return;
    AurixVoiceClient* c = aurix_find_client(this, client_path_);
    Dictionary frame;
    if (c && c->is_client_created()) {
        if (enable_analysis_ && (!enabled_on_client_ || enabled_generation_ != c->get_client_generation())) {
            enabled_on_client_ = true; enabled_generation_ = c->get_client_generation();
            if (!c->get_visemes_enabled()) c->set_visemes_enabled(true);
        }
        frame = user_id_.is_empty() ? c->get_local_visemes() : c->get_participant_visemes(user_id_);
    }
    const int64_t seq = frame.has("sequence") ? static_cast<int64_t>(frame["sequence"]) : -1;
    PackedFloat32Array target; double target_open = 0.0;
    if (!frame.is_empty() && seq != last_sequence_) { last_sequence_ = seq; stale_for_ = 0.0; analysing_ = true; }
    else { stale_for_ += delta; if (stale_for_ > 0.1) analysing_ = false; }
    if (analysing_ && !frame.is_empty()) {
        target = frame["weights"]; target_open = frame["mouth_open"]; dominant_ = frame["dominant"];
    } else {
        target.resize(current_.size()); target.fill(0.0f); dominant_ = 0;
    }
    const double k = smoothing_ <= 0.0 || delta <= 0.0 ? 1.0 : 1.0 - std::exp(-delta / smoothing_);
    for (int64_t i = 0; i < current_.size() && i < target.size(); ++i) current_[i] += static_cast<float>((target[i] - current_[i]) * k);
    mouth_open_ += (target_open - mouth_open_) * k;
    apply(Object::cast_to<MeshInstance3D>(get_node_or_null(target_)));
    emit_signal("frame", current_, mouth_open_, dominant_, analysing_);
}

void AurixLipSync::_exit_tree() { reset_state(); apply(Object::cast_to<MeshInstance3D>(get_node_or_null(target_))); }

void AurixLipSync::set_user_id(const String& id) { user_id_ = id; reset_state(); }
String AurixLipSync::get_user_id() const { return user_id_; }
void AurixLipSync::set_client_path(const NodePath& p) { client_path_ = p; enabled_on_client_ = false; }
NodePath AurixLipSync::get_client_path() const { return client_path_; }
void AurixLipSync::set_target(const NodePath& p) { target_ = p; }
NodePath AurixLipSync::get_target() const { return target_; }
void AurixLipSync::set_blend_shapes(const PackedStringArray& n) { blend_shapes_ = n; }
PackedStringArray AurixLipSync::get_blend_shapes() const { return blend_shapes_; }
void AurixLipSync::set_jaw_blend_shape(const String& n) { jaw_blend_shape_ = n; }
String AurixLipSync::get_jaw_blend_shape() const { return jaw_blend_shape_; }
void AurixLipSync::set_smoothing(double s) { smoothing_ = std::clamp(s, 0.0, 0.3); }
double AurixLipSync::get_smoothing() const { return smoothing_; }
void AurixLipSync::set_enable_analysis(bool on) { enable_analysis_ = on; enabled_on_client_ = false; }
bool AurixLipSync::get_enable_analysis() const { return enable_analysis_; }
bool AurixLipSync::is_local() const { return user_id_.is_empty(); }
bool AurixLipSync::is_analysing() const { return analysing_; }
PackedFloat32Array AurixLipSync::get_weights() const { return current_; }
double AurixLipSync::get_mouth_open() const { return mouth_open_; }
int AurixLipSync::get_dominant() const { return dominant_; }

void AurixLipSync::_bind_methods() {
    ClassDB::bind_method(D_METHOD("set_user_id", "user_id"), &AurixLipSync::set_user_id);
    ClassDB::bind_method(D_METHOD("get_user_id"), &AurixLipSync::get_user_id);
    ClassDB::bind_method(D_METHOD("set_client_path", "path"), &AurixLipSync::set_client_path);
    ClassDB::bind_method(D_METHOD("get_client_path"), &AurixLipSync::get_client_path);
    ClassDB::bind_method(D_METHOD("set_target", "path"), &AurixLipSync::set_target);
    ClassDB::bind_method(D_METHOD("get_target"), &AurixLipSync::get_target);
    ClassDB::bind_method(D_METHOD("set_blend_shapes", "names"), &AurixLipSync::set_blend_shapes);
    ClassDB::bind_method(D_METHOD("get_blend_shapes"), &AurixLipSync::get_blend_shapes);
    ClassDB::bind_method(D_METHOD("set_jaw_blend_shape", "name"), &AurixLipSync::set_jaw_blend_shape);
    ClassDB::bind_method(D_METHOD("get_jaw_blend_shape"), &AurixLipSync::get_jaw_blend_shape);
    ClassDB::bind_method(D_METHOD("set_smoothing", "seconds"), &AurixLipSync::set_smoothing);
    ClassDB::bind_method(D_METHOD("get_smoothing"), &AurixLipSync::get_smoothing);
    ClassDB::bind_method(D_METHOD("set_enable_analysis", "on"), &AurixLipSync::set_enable_analysis);
    ClassDB::bind_method(D_METHOD("get_enable_analysis"), &AurixLipSync::get_enable_analysis);
    ClassDB::bind_method(D_METHOD("is_local"), &AurixLipSync::is_local);
    ClassDB::bind_method(D_METHOD("is_analysing"), &AurixLipSync::is_analysing);
    ClassDB::bind_method(D_METHOD("get_weights"), &AurixLipSync::get_weights);
    ClassDB::bind_method(D_METHOD("get_mouth_open"), &AurixLipSync::get_mouth_open);
    ClassDB::bind_method(D_METHOD("get_dominant"), &AurixLipSync::get_dominant);
    ADD_PROPERTY(PropertyInfo(Variant::STRING, "user_id"), "set_user_id", "get_user_id");
    ADD_PROPERTY(PropertyInfo(Variant::NODE_PATH, "client_path", PROPERTY_HINT_NODE_PATH_VALID_TYPES, "AurixVoiceClient"), "set_client_path", "get_client_path");
    ADD_PROPERTY(PropertyInfo(Variant::NODE_PATH, "target", PROPERTY_HINT_NODE_PATH_VALID_TYPES, "MeshInstance3D"), "set_target", "get_target");
    ADD_PROPERTY(PropertyInfo(Variant::PACKED_STRING_ARRAY, "blend_shapes"), "set_blend_shapes", "get_blend_shapes");
    ADD_PROPERTY(PropertyInfo(Variant::STRING, "jaw_blend_shape"), "set_jaw_blend_shape", "get_jaw_blend_shape");
    ADD_PROPERTY(PropertyInfo(Variant::FLOAT, "smoothing", PROPERTY_HINT_RANGE, "0,0.3,0.005,suffix:s"), "set_smoothing", "get_smoothing");
    ADD_PROPERTY(PropertyInfo(Variant::BOOL, "enable_analysis"), "set_enable_analysis", "get_enable_analysis");
    ADD_SIGNAL(MethodInfo("frame", PropertyInfo(Variant::PACKED_FLOAT32_ARRAY, "weights"), PropertyInfo(Variant::FLOAT, "mouth_open"),
                          PropertyInfo(Variant::INT, "dominant"), PropertyInfo(Variant::BOOL, "analysing")));
}

// ---- AurixGameAudioDucker ----------------------------------------------------------------------

AurixGameAudioDucker::AurixGameAudioDucker() { set_process(true); }

void AurixGameAudioDucker::connect_client() {
    AurixVoiceClient* c = aurix_find_client(this, client_path_);
    if (c == connected_) return;
    if (connected_) connected_->disconnect("ducking_changed", Callable(this, "_on_ducking_changed"));
    connected_ = c;
    if (connected_) connected_->connect("ducking_changed", Callable(this, "_on_ducking_changed"));
}

void AurixGameAudioDucker::on_ducking_changed(const String& channel_id, bool active, const Dictionary& ducking) {
    if (!channel_id_.is_empty() && channel_id != channel_id_) return;
    if (!ducking.is_empty()) {
        server_gain_ = ducking.get("gain", server_gain_);
        server_attack_ = static_cast<int>(static_cast<int64_t>(ducking.get("attack_ms", server_attack_)));
        server_release_ = static_cast<int>(static_cast<int64_t>(ducking.get("release_ms", server_release_)));
        server_hold_ = static_cast<int>(static_cast<int64_t>(ducking.get("hold_ms", server_hold_)));
    }
    active_ = active;
    const double g = override_envelope_ ? duck_gain_ : server_gain_;
    if (active) { target_ = std::clamp(g, 0.0, 1.0); hold_left_ = (override_envelope_ ? hold_ms_ : server_hold_) / 1000.0; }
    else target_ = 1.0;
}

void AurixGameAudioDucker::_process(double delta) {
    if (in_editor()) return;
    connect_client();
    if (!active_ && hold_left_ > 0.0) { hold_left_ -= delta; if (hold_left_ > 0.0) target_ = std::clamp(override_envelope_ ? duck_gain_ : server_gain_, 0.0, 1.0); }
    const int ms = target_ < current_ ? (override_envelope_ ? attack_ms_ : server_attack_) : (override_envelope_ ? release_ms_ : server_release_);
    const double tau = std::max(1, ms) / 1000.0;
    const double k = 1.0 - std::exp(-delta / tau);
    const double next = current_ + (target_ - current_) * k;
    if (std::abs(next - current_) < 1e-5 && std::abs(target_ - current_) < 1e-4) { if (current_ == target_) return; current_ = target_; }
    else current_ = next;
    if (!bus_.is_empty()) {
        AudioServer* as = AudioServer::get_singleton();
        const int idx = as->get_bus_index(bus_);
        if (idx >= 0) {
            if (!base_captured_) { base_volume_db_ = as->get_bus_volume_db(idx); base_captured_ = true; }
            as->set_bus_volume_db(idx, base_volume_db_ + static_cast<float>(20.0 * std::log10(std::max(current_, 0.0001))));
        }
    }
    emit_signal("gain_changed", current_);
}

void AurixGameAudioDucker::_exit_tree() {
    if (connected_) { connected_->disconnect("ducking_changed", Callable(this, "_on_ducking_changed")); connected_ = nullptr; }
    if (base_captured_ && !bus_.is_empty()) {
        const int idx = AudioServer::get_singleton()->get_bus_index(bus_);
        if (idx >= 0) AudioServer::get_singleton()->set_bus_volume_db(idx, base_volume_db_);
    }
    base_captured_ = false; current_ = target_ = 1.0; active_ = false;
}

void AurixGameAudioDucker::set_channel_id(const String& id) { channel_id_ = id; }
String AurixGameAudioDucker::get_channel_id() const { return channel_id_; }
void AurixGameAudioDucker::set_client_path(const NodePath& p) { client_path_ = p; }
NodePath AurixGameAudioDucker::get_client_path() const { return client_path_; }
void AurixGameAudioDucker::set_bus(const StringName& b) {
    if (base_captured_ && !bus_.is_empty()) { const int idx = AudioServer::get_singleton()->get_bus_index(bus_); if (idx >= 0) AudioServer::get_singleton()->set_bus_volume_db(idx, base_volume_db_); }
    bus_ = b; base_captured_ = false;
}
StringName AurixGameAudioDucker::get_bus() const { return bus_; }
void AurixGameAudioDucker::set_duck_gain(double g) { duck_gain_ = std::clamp(g, 0.0, 1.0); }
double AurixGameAudioDucker::get_duck_gain() const { return duck_gain_; }
void AurixGameAudioDucker::set_attack_ms(int ms) { attack_ms_ = std::clamp(ms, 1, 5000); }
int AurixGameAudioDucker::get_attack_ms() const { return attack_ms_; }
void AurixGameAudioDucker::set_release_ms(int ms) { release_ms_ = std::clamp(ms, 1, 10000); }
int AurixGameAudioDucker::get_release_ms() const { return release_ms_; }
void AurixGameAudioDucker::set_hold_ms(int ms) { hold_ms_ = std::clamp(ms, 0, 10000); }
int AurixGameAudioDucker::get_hold_ms() const { return hold_ms_; }
void AurixGameAudioDucker::set_override_envelope(bool on) { override_envelope_ = on; }
bool AurixGameAudioDucker::get_override_envelope() const { return override_envelope_; }
double AurixGameAudioDucker::get_current_gain() const { return current_; }
bool AurixGameAudioDucker::is_ducking() const { return active_; }

void AurixGameAudioDucker::_bind_methods() {
    ClassDB::bind_method(D_METHOD("_on_ducking_changed", "channel_id", "active", "ducking"), &AurixGameAudioDucker::on_ducking_changed);
    ClassDB::bind_method(D_METHOD("set_channel_id", "channel_id"), &AurixGameAudioDucker::set_channel_id);
    ClassDB::bind_method(D_METHOD("get_channel_id"), &AurixGameAudioDucker::get_channel_id);
    ClassDB::bind_method(D_METHOD("set_client_path", "path"), &AurixGameAudioDucker::set_client_path);
    ClassDB::bind_method(D_METHOD("get_client_path"), &AurixGameAudioDucker::get_client_path);
    ClassDB::bind_method(D_METHOD("set_bus", "bus"), &AurixGameAudioDucker::set_bus);
    ClassDB::bind_method(D_METHOD("get_bus"), &AurixGameAudioDucker::get_bus);
    ClassDB::bind_method(D_METHOD("set_duck_gain", "gain"), &AurixGameAudioDucker::set_duck_gain);
    ClassDB::bind_method(D_METHOD("get_duck_gain"), &AurixGameAudioDucker::get_duck_gain);
    ClassDB::bind_method(D_METHOD("set_attack_ms", "ms"), &AurixGameAudioDucker::set_attack_ms);
    ClassDB::bind_method(D_METHOD("get_attack_ms"), &AurixGameAudioDucker::get_attack_ms);
    ClassDB::bind_method(D_METHOD("set_release_ms", "ms"), &AurixGameAudioDucker::set_release_ms);
    ClassDB::bind_method(D_METHOD("get_release_ms"), &AurixGameAudioDucker::get_release_ms);
    ClassDB::bind_method(D_METHOD("set_hold_ms", "ms"), &AurixGameAudioDucker::set_hold_ms);
    ClassDB::bind_method(D_METHOD("get_hold_ms"), &AurixGameAudioDucker::get_hold_ms);
    ClassDB::bind_method(D_METHOD("set_override_envelope", "on"), &AurixGameAudioDucker::set_override_envelope);
    ClassDB::bind_method(D_METHOD("get_override_envelope"), &AurixGameAudioDucker::get_override_envelope);
    ClassDB::bind_method(D_METHOD("get_current_gain"), &AurixGameAudioDucker::get_current_gain);
    ClassDB::bind_method(D_METHOD("is_ducking"), &AurixGameAudioDucker::is_ducking);
    ADD_PROPERTY(PropertyInfo(Variant::STRING, "channel_id"), "set_channel_id", "get_channel_id");
    ADD_PROPERTY(PropertyInfo(Variant::NODE_PATH, "client_path", PROPERTY_HINT_NODE_PATH_VALID_TYPES, "AurixVoiceClient"), "set_client_path", "get_client_path");
    ADD_PROPERTY(PropertyInfo(Variant::STRING_NAME, "bus"), "set_bus", "get_bus");
    ADD_PROPERTY(PropertyInfo(Variant::BOOL, "override_envelope"), "set_override_envelope", "get_override_envelope");
    ADD_PROPERTY(PropertyInfo(Variant::FLOAT, "duck_gain", PROPERTY_HINT_RANGE, "0,1,0.01"), "set_duck_gain", "get_duck_gain");
    ADD_PROPERTY(PropertyInfo(Variant::INT, "attack_ms", PROPERTY_HINT_RANGE, "1,5000,1,suffix:ms"), "set_attack_ms", "get_attack_ms");
    ADD_PROPERTY(PropertyInfo(Variant::INT, "release_ms", PROPERTY_HINT_RANGE, "1,10000,1,suffix:ms"), "set_release_ms", "get_release_ms");
    ADD_PROPERTY(PropertyInfo(Variant::INT, "hold_ms", PROPERTY_HINT_RANGE, "0,10000,1,suffix:ms"), "set_hold_ms", "get_hold_ms");
    ADD_SIGNAL(MethodInfo("gain_changed", PropertyInfo(Variant::FLOAT, "gain")));
}

// ---- AurixVoiceLobby ---------------------------------------------------------------------------

AurixVoiceLobby::AurixVoiceLobby() { set_process(true); }

void AurixVoiceLobby::connect_client() {
    AurixVoiceClient* c = aurix_find_client(this, client_path_);
    if (c == connected_) return;
    static const char* sigs[] = {"channel_joined", "channel_left", "participant_joined", "participant_left", "participant_speaking", "state_changed"};
    static const char* slots[] = {"_on_channel_joined", "_on_channel_left", "_on_participant_joined", "_on_participant_left", "_on_participant_speaking", "_on_state_changed"};
    if (connected_) for (int i = 0; i < 6; ++i) connected_->disconnect(sigs[i], Callable(this, slots[i]));
    connected_ = c; joined_ = false; join_pending_ = false; roster_.clear();
    if (connected_) for (int i = 0; i < 6; ++i) connected_->connect(sigs[i], Callable(this, slots[i]));
}

void AurixVoiceLobby::_process(double) {
    if (in_editor()) return;
    connect_client();
    if (!connected_ || !auto_join_ || joined_ || join_pending_ || channel_id_.is_empty()) return;
    if (connected_->get_connection_state() != AurixVoiceClient::STATE_MEDIA_BOUND) return;
    if (joined(connected_, channel_id_)) { joined_ = true; return; }
    join_now();
}

int AurixVoiceLobby::join_now() {
    connect_client();
    if (!connected_ || channel_id_.is_empty()) return AURIX_INVALID_ARGUMENT;
    const int rc = connected_->join_channel(channel_id_, join_token_);
    join_pending_ = rc == AURIX_OK;
    return rc;
}
int AurixVoiceLobby::leave_now() {
    connect_client();
    if (!connected_ || channel_id_.is_empty()) return AURIX_INVALID_ARGUMENT;
    join_pending_ = false;
    return connected_->leave_channel(channel_id_);
}

void AurixVoiceLobby::on_channel_joined(int, const String& channel_id, const Array& participants, const Dictionary&) {
    if (channel_id != channel_id_) return;
    joined_ = true; join_pending_ = false; roster_.clear();
    for (int64_t i = 0; i < participants.size(); ++i) { const Dictionary p = participants[i]; roster_[p["user_id"]] = p; }
    emit_signal("joined", participants);
    emit_signal("roster_changed", get_roster());
}
void AurixVoiceLobby::on_channel_left(const String& channel_id) {
    if (channel_id != channel_id_) return;
    joined_ = false; join_pending_ = false; roster_.clear();
    emit_signal("left");
    emit_signal("roster_changed", Array());
}
void AurixVoiceLobby::on_participant_joined(const String& channel_id, const Dictionary& p) {
    if (channel_id != channel_id_) return;
    roster_[p["user_id"]] = p;
    emit_signal("member_joined", p);
    emit_signal("roster_changed", get_roster());
}
void AurixVoiceLobby::on_participant_left(const String& channel_id, const String& user_id) {
    if (channel_id != channel_id_) return;
    const Dictionary p = roster_.get(user_id, Dictionary());
    roster_.erase(user_id);
    emit_signal("member_left", user_id, p);
    emit_signal("roster_changed", get_roster());
}
void AurixVoiceLobby::on_participant_speaking(const String& channel_id, const String& user_id, bool speaking) {
    if (channel_id != channel_id_) return;
    if (roster_.has(user_id)) { Dictionary p = roster_[user_id]; p["speaking"] = speaking; roster_[user_id] = p; }
    emit_signal("member_speaking", user_id, speaking);
}
void AurixVoiceLobby::on_state_changed(int state) {
    if (state != AurixVoiceClient::STATE_MEDIA_BOUND && state != AurixVoiceClient::STATE_RECONNECTING) {
        if (joined_) { joined_ = false; roster_.clear(); emit_signal("left"); emit_signal("roster_changed", Array()); }
        join_pending_ = false;
    }
}

void AurixVoiceLobby::_exit_tree() {
    if (connected_) { AurixVoiceClient* c = connected_; connected_ = nullptr;
        static const char* sigs[] = {"channel_joined", "channel_left", "participant_joined", "participant_left", "participant_speaking", "state_changed"};
        static const char* slots[] = {"_on_channel_joined", "_on_channel_left", "_on_participant_joined", "_on_participant_left", "_on_participant_speaking", "_on_state_changed"};
        for (int i = 0; i < 6; ++i) c->disconnect(sigs[i], Callable(this, slots[i]));
        if (joined_ && auto_join_) c->leave_channel(channel_id_);
    }
    joined_ = false; roster_.clear();
}

void AurixVoiceLobby::set_channel_id(const String& id) { if (id == channel_id_) return; if (joined_ && connected_) connected_->leave_channel(channel_id_); channel_id_ = id; joined_ = false; join_pending_ = false; roster_.clear(); }
String AurixVoiceLobby::get_channel_id() const { return channel_id_; }
void AurixVoiceLobby::set_join_token(const String& t) { join_token_ = t; }
String AurixVoiceLobby::get_join_token() const { return join_token_; }
void AurixVoiceLobby::set_client_path(const NodePath& p) { client_path_ = p; }
NodePath AurixVoiceLobby::get_client_path() const { return client_path_; }
void AurixVoiceLobby::set_auto_join(bool on) { auto_join_ = on; }
bool AurixVoiceLobby::get_auto_join() const { return auto_join_; }
bool AurixVoiceLobby::is_joined() const { return joined_; }
Array AurixVoiceLobby::get_roster() const { return roster_.values(); }
Array AurixVoiceLobby::get_speakers() const {
    Array out; const Array v = roster_.values();
    for (int64_t i = 0; i < v.size(); ++i) { const Dictionary p = v[i]; if (static_cast<bool>(p.get("speaking", false))) out.push_back(p); }
    return out;
}

void AurixVoiceLobby::_bind_methods() {
    ClassDB::bind_method(D_METHOD("_on_channel_joined", "request_id", "channel_id", "participants", "info"), &AurixVoiceLobby::on_channel_joined);
    ClassDB::bind_method(D_METHOD("_on_channel_left", "channel_id"), &AurixVoiceLobby::on_channel_left);
    ClassDB::bind_method(D_METHOD("_on_participant_joined", "channel_id", "participant"), &AurixVoiceLobby::on_participant_joined);
    ClassDB::bind_method(D_METHOD("_on_participant_left", "channel_id", "user_id"), &AurixVoiceLobby::on_participant_left);
    ClassDB::bind_method(D_METHOD("_on_participant_speaking", "channel_id", "user_id", "speaking"), &AurixVoiceLobby::on_participant_speaking);
    ClassDB::bind_method(D_METHOD("_on_state_changed", "state"), &AurixVoiceLobby::on_state_changed);
    ClassDB::bind_method(D_METHOD("set_channel_id", "channel_id"), &AurixVoiceLobby::set_channel_id);
    ClassDB::bind_method(D_METHOD("get_channel_id"), &AurixVoiceLobby::get_channel_id);
    ClassDB::bind_method(D_METHOD("set_join_token", "token"), &AurixVoiceLobby::set_join_token);
    ClassDB::bind_method(D_METHOD("get_join_token"), &AurixVoiceLobby::get_join_token);
    ClassDB::bind_method(D_METHOD("set_client_path", "path"), &AurixVoiceLobby::set_client_path);
    ClassDB::bind_method(D_METHOD("get_client_path"), &AurixVoiceLobby::get_client_path);
    ClassDB::bind_method(D_METHOD("set_auto_join", "on"), &AurixVoiceLobby::set_auto_join);
    ClassDB::bind_method(D_METHOD("get_auto_join"), &AurixVoiceLobby::get_auto_join);
    ClassDB::bind_method(D_METHOD("is_joined"), &AurixVoiceLobby::is_joined);
    ClassDB::bind_method(D_METHOD("get_roster"), &AurixVoiceLobby::get_roster);
    ClassDB::bind_method(D_METHOD("get_speakers"), &AurixVoiceLobby::get_speakers);
    ClassDB::bind_method(D_METHOD("join_now"), &AurixVoiceLobby::join_now);
    ClassDB::bind_method(D_METHOD("leave_now"), &AurixVoiceLobby::leave_now);
    ADD_PROPERTY(PropertyInfo(Variant::STRING, "channel_id"), "set_channel_id", "get_channel_id");
    ADD_PROPERTY(PropertyInfo(Variant::STRING, "join_token"), "set_join_token", "get_join_token");
    ADD_PROPERTY(PropertyInfo(Variant::NODE_PATH, "client_path", PROPERTY_HINT_NODE_PATH_VALID_TYPES, "AurixVoiceClient"), "set_client_path", "get_client_path");
    ADD_PROPERTY(PropertyInfo(Variant::BOOL, "auto_join"), "set_auto_join", "get_auto_join");
    ADD_SIGNAL(MethodInfo("joined", PropertyInfo(Variant::ARRAY, "participants")));
    ADD_SIGNAL(MethodInfo("left"));
    ADD_SIGNAL(MethodInfo("member_joined", PropertyInfo(Variant::DICTIONARY, "participant")));
    ADD_SIGNAL(MethodInfo("member_left", PropertyInfo(Variant::STRING, "user_id"), PropertyInfo(Variant::DICTIONARY, "participant")));
    ADD_SIGNAL(MethodInfo("member_speaking", PropertyInfo(Variant::STRING, "user_id"), PropertyInfo(Variant::BOOL, "speaking")));
    ADD_SIGNAL(MethodInfo("roster_changed", PropertyInfo(Variant::ARRAY, "roster")));
}

// ---- AurixProximityVoice -----------------------------------------------------------------------

AurixProximityVoice::AurixProximityVoice() { set_process(true); }

void AurixProximityVoice::connect_client() {
    AurixVoiceClient* c = aurix_find_client(this, client_path_);
    if (c == connected_) return;
    static const char* sigs[] = {"channel_joined", "channel_left", "participant_joined", "participant_left", "positions", "state_changed"};
    static const char* slots[] = {"_on_channel_joined", "_on_channel_left", "_on_participant_joined", "_on_participant_left", "_on_positions", "_on_state_changed"};
    if (connected_) for (int i = 0; i < 6; ++i) connected_->disconnect(sigs[i], Callable(this, slots[i]));
    connected_ = c; joined_ = false; join_pending_ = false; release_all();
    if (connected_) {
        for (int i = 0; i < 6; ++i) connected_->connect(sigs[i], Callable(this, slots[i]));
        if (connected_->get_playback_mode() == AurixVoiceClient::PLAYBACK_MIXED) connected_->set_playback_mode(AurixVoiceClient::PLAYBACK_PER_PARTICIPANT);
    }
}

void AurixProximityVoice::_process(double delta) {
    if (in_editor()) return;
    connect_client();
    if (!connected_ || channel_id_.is_empty()) return;
    if (!joined_) {
        if (join_pending_ || connected_->get_connection_state() != AurixVoiceClient::STATE_MEDIA_BOUND) return;
        if (joined(connected_, channel_id_)) joined_ = true; else { join_pending_ = connected_->join_channel(channel_id_, join_token_) == AURIX_OK; return; }
    }
    since_last_ += delta;
    if (since_last_ < 1.0 / std::max(1.0, updates_per_second_)) return;
    since_last_ = 0.0;
    const String self = connected_->get_user_id();
    if (self.is_empty()) return;
    Transform3D t = get_global_transform();
    t.origin = t.origin / static_cast<real_t>(world_to_meters_);
    Dictionary transforms; transforms[self] = t;
    connected_->update_transforms(channel_id_, transforms);
}

void AurixProximityVoice::ensure_player(const Dictionary& p) {
    const String user_id = p["user_id"];
    if (user_id.is_empty() || !connected_ || user_id == connected_->get_user_id() || players_.has(user_id)) return;
    Node* root = speakers_root_.is_empty() ? this : get_node_or_null(speakers_root_);
    if (!root) root = this;
    AurixParticipantPlayer* player = nullptr;
    if (!player_template_.is_empty()) {
        if (Node* tpl = get_node_or_null(player_template_)) player = Object::cast_to<AurixParticipantPlayer>(tpl->duplicate());
    }
    if (!player) player = memnew(AurixParticipantPlayer);
    player->set_name(String("Voice_") + user_id.left(8));
    player->set_user_id(user_id);
    if (!client_path_.is_empty()) player->set_client_path(player->get_path_to(connected_));
    root->add_child(player);
    if (client_path_.is_empty() && !aurix_find_client(player, NodePath())) player->set_client_path(player->get_path_to(connected_));
    players_[user_id] = player;
    emit_signal("speaker_added", user_id, player);
}
void AurixProximityVoice::release_player(const String& user_id) {
    if (!players_.has(user_id)) return;
    AurixParticipantPlayer* player = Object::cast_to<AurixParticipantPlayer>(players_[user_id]);
    players_.erase(user_id);
    emit_signal("speaker_removed", user_id);
    if (player) player->queue_free();
}
void AurixProximityVoice::release_all() { const Array keys = players_.keys(); for (int64_t i = 0; i < keys.size(); ++i) release_player(keys[i]); }

void AurixProximityVoice::on_channel_joined(int, const String& channel_id, const Array& participants, const Dictionary&) {
    if (channel_id != channel_id_) return;
    joined_ = true; join_pending_ = false;
    for (int64_t i = 0; i < participants.size(); ++i) ensure_player(participants[i]);
    emit_signal("joined");
}
void AurixProximityVoice::on_channel_left(const String& channel_id) { if (channel_id != channel_id_) return; joined_ = false; join_pending_ = false; release_all(); emit_signal("left"); }
void AurixProximityVoice::on_participant_joined(const String& channel_id, const Dictionary& p) { if (channel_id == channel_id_) ensure_player(p); }
void AurixProximityVoice::on_participant_left(const String& channel_id, const String& user_id) { if (channel_id == channel_id_) release_player(user_id); }
void AurixProximityVoice::on_positions(const String& channel_id, const String& json) {
    if (channel_id != channel_id_ || !place_speakers_) return;
    const Variant parsed = JSON::parse_string(json);
    if (parsed.get_type() != Variant::ARRAY) return;
    const Array list = parsed;
    for (int64_t i = 0; i < list.size(); ++i) {
        const Dictionary e = list[i];
        const String user_id = e.get("user_id", String());
        if (!players_.has(user_id)) continue;
        const Dictionary pos = e.get("position", Dictionary());
        const Vector3 v(static_cast<real_t>(static_cast<double>(pos.get("x", 0.0))), static_cast<real_t>(static_cast<double>(pos.get("y", 0.0))), static_cast<real_t>(static_cast<double>(pos.get("z", 0.0))));
        set_speaker_position(user_id, v * static_cast<real_t>(world_to_meters_));
    }
}
void AurixProximityVoice::on_state_changed(int state) {
    if (state != AurixVoiceClient::STATE_MEDIA_BOUND && state != AurixVoiceClient::STATE_RECONNECTING) { if (joined_) { joined_ = false; release_all(); emit_signal("left"); } join_pending_ = false; }
}
void AurixProximityVoice::set_speaker_position(const String& user_id, const Vector3& position) {
    if (AurixParticipantPlayer* p = get_speaker_player(user_id)) p->set_global_position(position);
}
AurixParticipantPlayer* AurixProximityVoice::get_speaker_player(const String& user_id) const {
    return players_.has(user_id) ? Object::cast_to<AurixParticipantPlayer>(players_[user_id]) : nullptr;
}
Dictionary AurixProximityVoice::get_speaker_players() const { return players_.duplicate(); }

void AurixProximityVoice::_exit_tree() {
    if (connected_) { AurixVoiceClient* c = connected_; connected_ = nullptr;
        static const char* sigs[] = {"channel_joined", "channel_left", "participant_joined", "participant_left", "positions", "state_changed"};
        static const char* slots[] = {"_on_channel_joined", "_on_channel_left", "_on_participant_joined", "_on_participant_left", "_on_positions", "_on_state_changed"};
        for (int i = 0; i < 6; ++i) c->disconnect(sigs[i], Callable(this, slots[i]));
        if (joined_) c->leave_channel(channel_id_);
    }
    joined_ = false; release_all();
}

void AurixProximityVoice::set_channel_id(const String& id) { if (id == channel_id_) return; if (joined_ && connected_) connected_->leave_channel(channel_id_); channel_id_ = id; joined_ = false; join_pending_ = false; release_all(); }
String AurixProximityVoice::get_channel_id() const { return channel_id_; }
void AurixProximityVoice::set_join_token(const String& t) { join_token_ = t; }
String AurixProximityVoice::get_join_token() const { return join_token_; }
void AurixProximityVoice::set_client_path(const NodePath& p) { client_path_ = p; }
NodePath AurixProximityVoice::get_client_path() const { return client_path_; }
void AurixProximityVoice::set_speakers_root(const NodePath& p) { speakers_root_ = p; }
NodePath AurixProximityVoice::get_speakers_root() const { return speakers_root_; }
void AurixProximityVoice::set_player_template(const NodePath& p) { player_template_ = p; }
NodePath AurixProximityVoice::get_player_template() const { return player_template_; }
void AurixProximityVoice::set_updates_per_second(double hz) { updates_per_second_ = std::clamp(hz, 1.0, 60.0); }
double AurixProximityVoice::get_updates_per_second() const { return updates_per_second_; }
void AurixProximityVoice::set_world_to_meters(double s) { world_to_meters_ = s > 0.0 ? s : 1.0; }
double AurixProximityVoice::get_world_to_meters() const { return world_to_meters_; }
void AurixProximityVoice::set_place_speakers(bool on) { place_speakers_ = on; }
bool AurixProximityVoice::get_place_speakers() const { return place_speakers_; }
bool AurixProximityVoice::is_joined() const { return joined_; }

void AurixProximityVoice::_bind_methods() {
    ClassDB::bind_method(D_METHOD("_on_channel_joined", "request_id", "channel_id", "participants", "info"), &AurixProximityVoice::on_channel_joined);
    ClassDB::bind_method(D_METHOD("_on_channel_left", "channel_id"), &AurixProximityVoice::on_channel_left);
    ClassDB::bind_method(D_METHOD("_on_participant_joined", "channel_id", "participant"), &AurixProximityVoice::on_participant_joined);
    ClassDB::bind_method(D_METHOD("_on_participant_left", "channel_id", "user_id"), &AurixProximityVoice::on_participant_left);
    ClassDB::bind_method(D_METHOD("_on_positions", "channel_id", "json"), &AurixProximityVoice::on_positions);
    ClassDB::bind_method(D_METHOD("_on_state_changed", "state"), &AurixProximityVoice::on_state_changed);
    ClassDB::bind_method(D_METHOD("set_channel_id", "channel_id"), &AurixProximityVoice::set_channel_id);
    ClassDB::bind_method(D_METHOD("get_channel_id"), &AurixProximityVoice::get_channel_id);
    ClassDB::bind_method(D_METHOD("set_join_token", "token"), &AurixProximityVoice::set_join_token);
    ClassDB::bind_method(D_METHOD("get_join_token"), &AurixProximityVoice::get_join_token);
    ClassDB::bind_method(D_METHOD("set_client_path", "path"), &AurixProximityVoice::set_client_path);
    ClassDB::bind_method(D_METHOD("get_client_path"), &AurixProximityVoice::get_client_path);
    ClassDB::bind_method(D_METHOD("set_speakers_root", "path"), &AurixProximityVoice::set_speakers_root);
    ClassDB::bind_method(D_METHOD("get_speakers_root"), &AurixProximityVoice::get_speakers_root);
    ClassDB::bind_method(D_METHOD("set_player_template", "path"), &AurixProximityVoice::set_player_template);
    ClassDB::bind_method(D_METHOD("get_player_template"), &AurixProximityVoice::get_player_template);
    ClassDB::bind_method(D_METHOD("set_updates_per_second", "hz"), &AurixProximityVoice::set_updates_per_second);
    ClassDB::bind_method(D_METHOD("get_updates_per_second"), &AurixProximityVoice::get_updates_per_second);
    ClassDB::bind_method(D_METHOD("set_world_to_meters", "scale"), &AurixProximityVoice::set_world_to_meters);
    ClassDB::bind_method(D_METHOD("get_world_to_meters"), &AurixProximityVoice::get_world_to_meters);
    ClassDB::bind_method(D_METHOD("set_place_speakers", "on"), &AurixProximityVoice::set_place_speakers);
    ClassDB::bind_method(D_METHOD("get_place_speakers"), &AurixProximityVoice::get_place_speakers);
    ClassDB::bind_method(D_METHOD("is_joined"), &AurixProximityVoice::is_joined);
    ClassDB::bind_method(D_METHOD("get_speaker_players"), &AurixProximityVoice::get_speaker_players);
    ClassDB::bind_method(D_METHOD("get_speaker_player", "user_id"), &AurixProximityVoice::get_speaker_player);
    ClassDB::bind_method(D_METHOD("set_speaker_position", "user_id", "position"), &AurixProximityVoice::set_speaker_position);
    ADD_PROPERTY(PropertyInfo(Variant::STRING, "channel_id"), "set_channel_id", "get_channel_id");
    ADD_PROPERTY(PropertyInfo(Variant::STRING, "join_token"), "set_join_token", "get_join_token");
    ADD_PROPERTY(PropertyInfo(Variant::NODE_PATH, "client_path", PROPERTY_HINT_NODE_PATH_VALID_TYPES, "AurixVoiceClient"), "set_client_path", "get_client_path");
    ADD_PROPERTY(PropertyInfo(Variant::NODE_PATH, "speakers_root", PROPERTY_HINT_NODE_PATH_VALID_TYPES, "Node"), "set_speakers_root", "get_speakers_root");
    ADD_PROPERTY(PropertyInfo(Variant::NODE_PATH, "player_template", PROPERTY_HINT_NODE_PATH_VALID_TYPES, "AurixParticipantPlayer"), "set_player_template", "get_player_template");
    ADD_PROPERTY(PropertyInfo(Variant::FLOAT, "updates_per_second", PROPERTY_HINT_RANGE, "1,60,1"), "set_updates_per_second", "get_updates_per_second");
    ADD_PROPERTY(PropertyInfo(Variant::FLOAT, "world_to_meters", PROPERTY_HINT_RANGE, "0.001,1000,0.001"), "set_world_to_meters", "get_world_to_meters");
    ADD_PROPERTY(PropertyInfo(Variant::BOOL, "place_speakers"), "set_place_speakers", "get_place_speakers");
    ADD_SIGNAL(MethodInfo("joined"));
    ADD_SIGNAL(MethodInfo("left"));
    ADD_SIGNAL(MethodInfo("speaker_added", PropertyInfo(Variant::STRING, "user_id"), PropertyInfo(Variant::OBJECT, "player", PROPERTY_HINT_RESOURCE_TYPE, "AurixParticipantPlayer")));
    ADD_SIGNAL(MethodInfo("speaker_removed", PropertyInfo(Variant::STRING, "user_id")));
}

}  // namespace godot
