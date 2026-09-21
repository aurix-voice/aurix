## Aurix voice client for Godot **Web** exports.
##
## The native `AurixVoiceClient` (GDExtension over the C ABI) cannot run in a browser: there is no
## UDP/QUIC and no wasm build of the native core. This node fills the gap with the browser stack —
## it drives the Aurix Web SDK (`aurix-web-sdk.js`, WebRTC media + WebSocket control) through
## `JavaScriptBridge` and exposes the same GDScript surface: signal names, argument order, enum
## values and Dictionary keys mirror `AurixVoiceClient` one-to-one for everything the browser can
## do, so a lobby script can be written once and pick the node by platform:
##
## [codeblock]
## var voice: Node = AurixWebVoiceClient.new() if OS.has_feature("web") else AurixVoiceClient.new()
## [/codeblock]
##
## Microphone and playback are owned by the browser (getUserMedia, an `<audio>` element and a Web
## Audio graph for per-participant tracks) — nothing crosses into Godot's audio server, so
## `AurixParticipantPlayer` does not apply; spatialisation is the SDK's HRTF/equal-power panner fed
## by `update_positions`. Autoplay policies require a user gesture before remote audio is heard:
## call `resume_audio()` from an input handler. Events are pulled from the bridge queue in
## `_process` on the main thread.
##
## Loading the SDK: the node first looks for `window.AurixWebSdk` (add
## `<script src="aurix-web-sdk.js"></script>` to the export's *Head Include*), then for the bundle
## packed into the project at `sdk_resource` (needs `*.js` in the export's non-resource filter),
## then injects a `<script>` tag pointing at `sdk_url`. On every other platform `is_supported()`
## is `false` and `connect_to_server` returns `RESULT_TRANSPORT`.
class_name AurixWebVoiceClient
extends Node

## `AurixVoiceClient.Result` codes.
enum { RESULT_OK = 0, RESULT_NULL_POINTER = 1, RESULT_INVALID_ARGUMENT = 2, RESULT_NOT_CONNECTED = 3, RESULT_CLOSED = 4, RESULT_TRANSPORT = 5, RESULT_UNAUTHORIZED = 6, RESULT_TIMEOUT = 7, RESULT_SERVER_REJECTED = 8, RESULT_CODEC = 9, RESULT_PROTOCOL = 10 }
enum { STATE_DISCONNECTED = 0, STATE_CONNECTING = 1, STATE_CONNECTED = 2, STATE_MEDIA_BOUND = 3, STATE_RECONNECTING = 4, STATE_FAILED = 5 }
enum { TRANSMIT_NONE = 0, TRANSMIT_SINGLE = 1, TRANSMIT_ALL = 2 }
## Browsers always use WebRTC; `MEDIA_WEBRTC` extends the native `MediaPath` values.
enum { MEDIA_NONE = 0, MEDIA_UDP = 1, MEDIA_TUNNEL = 2, MEDIA_QUIC = 3, MEDIA_WEBRTC = 4 }
enum { MODERATION_KICK = 0, MODERATION_MUTE = 1, MODERATION_UNMUTE = 2 }
enum { ROLE_LISTENER = 0, ROLE_SPEAKER = 1, ROLE_MODERATOR = 2, ROLE_ADMINISTRATOR = 3 }
enum { TTS_BOTH = 0, TTS_CHANNEL = 1, TTS_LOCAL = 2 }
enum { TTS_QUEUED = 0, TTS_PLAYING = 1, TTS_FINISHED = 2, TTS_CANCELLED = 3, TTS_FAILED = 4 }
enum { VISEME_SILENCE = 0, VISEME_PP = 1, VISEME_FF = 2, VISEME_SS = 3, VISEME_AA = 4, VISEME_E = 5, VISEME_IH = 6, VISEME_OH = 7, VISEME_OU = 8 }
## HRTF (default), equal-power panning or no browser-side spatialisation.
enum { SPATIAL_HRTF = 0, SPATIAL_EQUAL_POWER = 1, SPATIAL_OFF = 2 }
## Web SDK bundle state.
enum { SDK_NOT_LOADED = 0, SDK_LOADING = 1, SDK_READY = 2, SDK_FAILED = 3 }

const VISEME_COUNT := 9
const _VISEME_NAMES: PackedStringArray = ["sil", "PP", "FF", "SS", "aa", "E", "ih", "oh", "ou"]
const _GLUE_NAME := "AurixGodot"

# --- signals shared with AurixVoiceClient (same names, same arguments)
signal raw_event(type: int, json: String)
signal state_changed(state: int)
signal session_ready(session: Dictionary)
signal media_bound
signal channel_joined(request_id: int, channel_id: String, participants: Array, info: Dictionary)
signal channel_left(channel_id: String)
signal participant_joined(channel_id: String, participant: Dictionary)
signal participant_left(channel_id: String, user_id: String)
signal participant_mute_changed(channel_id: String, user_id: String, muted: bool, server_muted: bool)
signal participant_speaking(channel_id: String, user_id: String, speaking: bool)
signal participant_priority_changed(channel_id: String, user_id: String, priority: bool)
signal ducking_changed(channel_id: String, active: bool, ducking: Dictionary)
signal channel_energy(channel_id: String, levels: Dictionary)
signal local_speaking(speaking: bool)
signal transmission_changed(mode: int, channel_id: String)
signal channel_focus_changed(channel_id: String)
signal user_block_changed(user_id: String, blocked: bool)
signal recording(channel_id: String, recording_id: String, active: bool, live: bool, initiator_id: String)
signal bitrate_changed(bps: int, reason: String)
signal kicked(channel_id: String, reason: String)
signal moderation_applied(request_id: int, channel_id: String, user_id: String, action: int)
signal chat_message(message: Dictionary)
signal participant_typing(channel_id: String, user_id: String, typing: bool)
signal transcript(entry: Dictionary)
signal tts_status(status: Dictionary)
signal positions(channel_id: String, json: String)
signal rejoin_failed(channel_id: String, code: String, message: String)
signal request_failed(request_id: int, code: String, message: String)
signal server_error(code: String, message: String)
signal recovering(attempt: int, delay_ms: int, cause: String)
signal recovered(resumed: bool, migrated: bool)
signal failed_to_recover(reason: String)
signal disconnected(reason: String)
signal network_quality(quality: Dictionary)
signal audio_policy_changed(policy: Dictionary)
signal media_path_changed(path: int, reason: String)
signal endpoint_changed(ws_url: String)
signal chat_history(request_id: int, channel_id: String, user_id: String, messages: Array, next_before: String, next_after: String)
signal chat_read_marker(marker: Dictionary)
signal chat_read_markers(channel_id: String, user_id: String, unread: int, markers: Array)
signal chat_inbox_synced(delivered: int, truncated: bool)
signal chat_message_updated(message: Dictionary)
signal chat_reaction_changed(change: Dictionary)
signal chat_search_result(request_id: int, channel_id: String, user_id: String, query: String, messages: Array, next_before: String)
signal translation_changed(translation: Dictionary)

# --- browser-only signals
## `SdkStatus` transitions while `aurix-web-sdk.js` loads.
signal sdk_status_changed(status: int)
## Remote audio started (`playing`) or was blocked by the autoplay policy (`reason`): call `resume_audio()`.
signal remote_audio(playing: bool, reason: String)
## The SDK needs a fresh token (`kind` is "refresh" or "join"); answer with `provide_token`.
signal token_requested(request_id: int, kind: String, channel_id: String)
## Per-participant WebRTC track layout changed: Array of `{mid, user_id, live}`.
signal participant_streams_changed(streams: Array)
signal participant_updated(channel_id: String, participant: Dictionary)
signal participant_visemes(user_id: String, frame: Dictionary)
signal local_visemes(frame: Dictionary)
signal e2ee_peer_key(user_id: String, fingerprint: String, previous_fingerprint: String)
signal e2ee_peer_decryptable(user_id: String, decryptable: bool)
signal e2ee_key_rotated(generation: int)
signal devices_changed(devices: Dictionary)
signal stats(sample: Dictionary)
## The bridge queue overflowed and dropped `count` events (raise `max_queued_events`).
signal events_dropped(count: int)

# --- configuration (before connect_to_server)
@export_group("Web SDK")
## Packed copy of `aurix-web-sdk.js` (evaluated in the page when present in the export).
@export var sdk_resource := "res://addons/aurix_voice/web/aurix_web_sdk.js"
## URL of `aurix-web-sdk.js` relative to the exported `index.html`, used when the page did not
## load the SDK itself and the packed copy is absent.
@export var sdk_url := "aurix-web-sdk.js"
@export_group("Connection")
## REST base URL for region discovery / token helpers; derived from the WebSocket URL when empty.
@export var api_url := ""
@export var auto_reconnect := true
@export_range(1, 100) var reconnect_max_attempts := 10
@export_range(1000, 120000) var request_timeout_ms := 10000
@export var use_turn := true
@export_group("Audio")
## Dedicated downlink tracks to request (0 = server mix only, hard cap 64).
@export_range(0, 64) var participant_streams := 16
@export var spatial_audio := SPATIAL_HRTF
@export var local_voice_activity := true
@export var visemes := false
## Group E2EE (Insertable Streams). The browser refuses to join `e2ee` channels without it.
@export var e2ee := false
@export_group("Events")
@export_range(16, 65536) var max_queued_events := 1024
## Forward every raw control message as `raw_event` (type 0, JSON).
@export var raw_events := false

var _glue: JavaScriptObject = null
var _handle := 0
var _state := STATE_DISCONNECTED
var _sdk_status := SDK_NOT_LOADED
var _pending_connect: Array = []  # [ws_url, token] while the SDK is still loading
var _session: Dictionary = {}
var _endpoint := ""
var _failover: PackedStringArray = PackedStringArray()
var _channels: Dictionary = {}  # channel_id → { user_id → participant Dictionary }
var _channel_info: Dictionary = {}  # channel_id → info Dictionary
var _joins: Dictionary = {}  # channel_id → request id
var _requests: Dictionary = {}  # rid → { kind, ... }
var _next_request := 1
var _muted := false
var _token := ""
var _last_error := ""
var _close_reason := ""
var _transmission := TRANSMIT_ALL
var _transmission_channel := ""
var _focus := ""
var _quality: Dictionary = {}
var _translation: Dictionary = {"language": "", "spoken_language": "", "speech": false}
var _audio_policy: Dictionary = {}


## `true` only inside a browser (Web export). Everything else is a no-op that reports `RESULT_TRANSPORT`.
static func is_supported() -> bool:
	return OS.has_feature("web")


## Version of the loaded Web SDK bundle ("" until `SDK_READY`).
func get_sdk_version() -> String:
	if _glue == null or _sdk_status != SDK_READY:
		return ""
	var v: Variant = _glue.version()
	return String(v) if v is String else ""


func get_sdk_status() -> int:
	return _sdk_status


func get_last_error() -> String:
	return _last_error


## Start loading the SDK bundle now (otherwise `connect_to_server` does it lazily).
func preload_sdk() -> void:
	_ensure_glue()
	_load_sdk()


func _process(_delta: float) -> void:
	if _glue == null:
		return
	if _sdk_status == SDK_LOADING:
		_poll_sdk()
	if _handle > 0:
		_drain()


func _exit_tree() -> void:
	disconnect_from_server()


# ------------------------------------------------------------------------------------------------
# lifecycle
# ------------------------------------------------------------------------------------------------

## Create the browser client and open the control WebSocket. Media (WebRTC) follows automatically;
## `session_ready` then `media_bound` fire when done. Returns a `Result`.
func connect_to_server(ws_url: String, token: String) -> int:
	if not is_supported():
		_last_error = "AurixWebVoiceClient only works in a Web export (use AurixVoiceClient natively)"
		return RESULT_TRANSPORT
	if ws_url.is_empty() or token.is_empty():
		_last_error = "ws_url and token are required"
		return RESULT_INVALID_ARGUMENT
	if _handle > 0:
		disconnect_from_server()
	_ensure_glue()
	if _glue == null:
		_last_error = "JavaScriptBridge is unavailable"
		return RESULT_TRANSPORT
	_close_reason = ""
	_last_error = ""
	if _sdk_status != SDK_READY:
		_load_sdk()
		if _sdk_status == SDK_FAILED:
			return RESULT_TRANSPORT
		_pending_connect = [ws_url, token]
		_set_state(STATE_CONNECTING)
		return RESULT_OK
	return _create_and_connect(ws_url, token)


## Close the session (if any), release the browser audio element and forget the client.
func disconnect_from_server() -> void:
	_pending_connect.clear()
	if _handle > 0 and _glue != null:
		_invoke("disconnect", {"reason": "client disconnect"})
		_drain()
		_glue.destroy(_handle)
	_handle = 0
	_reset_session()
	if _state != STATE_DISCONNECTED:
		_set_state(STATE_DISCONNECTED)
		disconnected.emit(_close_reason if not _close_reason.is_empty() else "client disconnect")


## Replace the credential used for the next reconnect (mirrors the native `set_token`). Ignored
## while a `token_requested` handler is connected — that handler answers refresh requests instead.
func set_token(token: String) -> int:
	if token.is_empty():
		return RESULT_INVALID_ARGUMENT
	_token = token
	return RESULT_OK


## Answer a `token_requested` signal (empty `token` declines with `error`).
func provide_token(request_id: int, token: String, error := "") -> int:
	var args := {"requestId": request_id}
	if token.is_empty():
		args["error"] = error if not error.is_empty() else "token request declined"
	else:
		args["token"] = token
	return _result(_invoke("provideToken", args))


## Force an immediate reconnect attempt while `STATE_RECONNECTING`.
func reconnect_now() -> int:
	return _result(_invoke("reconnectNow", {}))


func is_client_created() -> bool:
	return _handle > 0


func get_connection_state() -> int:
	return _state


func get_session() -> Dictionary:
	return _session.duplicate()


func get_endpoint() -> String:
	return _endpoint


func get_failover_endpoints() -> PackedStringArray:
	return _failover


## Browsers always use WebRTC once media is bound.
func get_media_path() -> int:
	return MEDIA_WEBRTC if _state == STATE_MEDIA_BOUND else MEDIA_NONE


## Browsers re-negotiate ICE themselves; this triggers a media renegotiation. `false` when not connected.
func network_changed() -> bool:
	if _handle <= 0 or _state < STATE_CONNECTED:
		return false
	return _result(_invoke("renegotiateMedia", {}, _request("renegotiate"))) == RESULT_OK


# ------------------------------------------------------------------------------------------------
# channels
# ------------------------------------------------------------------------------------------------

## Join a channel. Returns a request id (> 0) echoed by `channel_joined` / `request_failed`, or a
## negative `Result` when the call could not be issued.
func join_channel(channel_id: String, join_token := "") -> int:
	if _handle <= 0:
		_last_error = "not connected"
		return -RESULT_NOT_CONNECTED
	var rid := _request("join", {"channel_id": channel_id})
	_joins[channel_id] = rid
	var args := {"channelId": channel_id}
	if not join_token.is_empty():
		args["joinToken"] = join_token
	var r := _result(_invoke("joinChannel", args, rid))
	if r != RESULT_OK:
		_requests.erase(rid)
		_joins.erase(channel_id)
		return -r
	return rid


func leave_channel(channel_id: String) -> int:
	return _result(_invoke("leaveChannel", {"channelId": channel_id}))


func get_joined_channels() -> PackedStringArray:
	var out := PackedStringArray()
	for id in _channels.keys():
		out.append(id)
	return out


func get_participants(channel_id: String) -> Array:
	var members: Dictionary = _channels.get(channel_id, {})
	var out: Array = []
	for p in members.values():
		out.append((p as Dictionary).duplicate())
	return out


func get_channel_info(channel_id: String) -> Dictionary:
	return (_channel_info.get(channel_id, {}) as Dictionary).duplicate()


func can_speak_in(channel_id: String) -> bool:
	return _value(_invoke("canSpeakIn", {"channelId": channel_id}), false) == true


func channel_transcribes(channel_id: String) -> bool:
	return _value(_invoke("isChannelTranscribed", {"channelId": channel_id}), false) == true


func channel_monitored(channel_id: String) -> bool:
	return _value(_invoke("isChannelMonitored", {"channelId": channel_id}), false) == true


func get_channel_scope(channel_id: String) -> Dictionary:
	var v: Variant = _value(_invoke("channelScope", {"channelId": channel_id}), null)
	return _snake(v) if v is Dictionary else {}


## Mixed-only channels are E2EE-blind; `true` when the browser encrypts `channel_id`.
func is_channel_encrypted(channel_id: String) -> bool:
	return _value(_invoke("isChannelEncrypted", {"channelId": channel_id}), false) == true


# ------------------------------------------------------------------------------------------------
# microphone / uplink (owned by the browser)
# ------------------------------------------------------------------------------------------------

func set_muted(muted: bool) -> void:
	_muted = muted
	_invoke("setMuted", {"muted": muted})


func is_muted() -> bool:
	return _muted


func is_speaking() -> bool:
	return _value(_invoke("localSpeaking", {}), false) == true


func set_input_gain(gain: float) -> void:
	_invoke("setInputGain", {"gain": gain})


func get_input_energy() -> float:
	var v: Variant = _value(_invoke("localEnergy", {}), 0.0)
	return float(v) if v is float or v is int else 0.0


## Browser Opus is negotiated by WebRTC; `settings` accepts `bitrate_bps`, `dtx`, `fec`.
func set_encoder_settings(settings: Dictionary) -> int:
	var opus := {}
	if settings.has("bitrate_bps"):
		opus["maxBitrateBps"] = int(settings["bitrate_bps"])
	if settings.has("dtx"):
		opus["dtx"] = bool(settings["dtx"])
	if settings.has("fec"):
		opus["fec"] = bool(settings["fec"])
	return _result(_invoke("setOpusOptions", {"opus": opus}))


func set_bitrate(bps: int) -> int:
	return set_encoder_settings({"bitrate_bps": bps})


func get_audio_policy() -> Dictionary:
	return _audio_policy.duplicate()


## Uplink voice effects: a preset name ("robot", "monster", "radio", "helium", "ghost"), a parameter
## Dictionary with the native `AurixVoiceEffects` keys, or `{}` to bypass.
func set_voice_effects(effects: Variant) -> int:
	var payload: Variant = null
	if effects is String and not (effects as String).is_empty():
		payload = effects
	elif effects is Dictionary and not (effects as Dictionary).is_empty():
		payload = _camel(effects)
	return _result(_invoke("setVoiceEffects", {"effects": payload}, _request("effects")))


func get_voice_effects() -> Dictionary:
	var v: Variant = _value(_invoke("voiceEffects", {}), null)
	return _snake(v) if v is Dictionary else {}


func supports_voice_effects() -> bool:
	return _value(_invoke("supportsVoiceEffects", {}), false) == true


# ------------------------------------------------------------------------------------------------
# lip-sync
# ------------------------------------------------------------------------------------------------

func set_visemes_enabled(enabled: bool) -> void:
	visemes = enabled
	_invoke("setVisemes", {"enabled": enabled}, _request("visemes"))


func get_visemes_enabled() -> bool:
	return visemes


func get_participant_visemes(user_id: String) -> Dictionary:
	return _viseme_frame(_value(_invoke("participantVisemes", {"userId": user_id}), null))


func get_local_visemes() -> Dictionary:
	return _viseme_frame(_value(_invoke("localVisemes", {}), null))


# ------------------------------------------------------------------------------------------------
# playback / downlink
# ------------------------------------------------------------------------------------------------

## Retry playback after a user gesture (browser autoplay policy). Result arrives as `remote_audio`.
func resume_audio() -> int:
	return _result(_invoke("resumeAudio", {}, _request("resume_audio")))


func set_output_volume(volume: float) -> void:
	_invoke("setOutputVolume", {"volume": volume})


func set_output_muted(muted: bool) -> void:
	_invoke("setOutputMuted", {"muted": muted})


## Dedicated-track layout (`{mid, user_id, live}` per slot).
func get_participant_streams() -> Array:
	var v: Variant = _value(_invoke("participantStreams", {}), null)
	return _streams(v) if v is Array else []


## Pin speakers onto dedicated tracks (fails with `request_failed` beyond the cap).
func set_pinned_participants(user_ids: PackedStringArray) -> int:
	var ids: Array = []
	for id in user_ids:
		ids.append(id)
	return _result(_invoke("setPinnedParticipants", {"userIds": ids}))


func is_participant_spatialized(user_id: String) -> bool:
	return _value(_invoke("isParticipantSpatialized", {"userId": user_id}), false) == true


# ------------------------------------------------------------------------------------------------
# receiver preferences
# ------------------------------------------------------------------------------------------------

func set_participant_mute(user_id: String, channel_id: String, muted: bool) -> int:
	var args := {"userId": user_id, "muted": muted}
	if not channel_id.is_empty():
		args["channelId"] = channel_id
	return _result(_invoke("setParticipantMuted", args))


func set_participant_volume(user_id: String, volume: float) -> int:
	return _result(_invoke("setParticipantVolume", {"userId": user_id, "volume": volume}))


func set_user_block(user_id: String, blocked: bool) -> int:
	return _result(_invoke("setUserBlocked", {"userId": user_id, "blocked": blocked}))


func set_priority(channel_id: String, user_id: String, priority: bool) -> int:
	var args := {"channelId": channel_id, "priority": priority}
	if not user_id.is_empty():
		args["userId"] = user_id
	return _result(_invoke("setPriority", args))


func is_ducking_active(channel_id: String) -> bool:
	return _value(_invoke("isDuckingActive", {"channelId": channel_id}), false) == true


func set_transmission(mode: int, channel_id := "") -> int:
	var m: Dictionary
	match mode:
		TRANSMIT_NONE:
			m = {"type": "none"}
		TRANSMIT_SINGLE:
			m = {"type": "single", "channelId": channel_id}
		_:
			m = {"type": "all"}
	return _result(_invoke("setTransmission", {"mode": m}))


func set_channel_focus(channel_id: String) -> int:
	var args := {}
	if not channel_id.is_empty():
		args["channelId"] = channel_id
	return _result(_invoke("setChannelFocus", args))


func set_transcripts(enabled: bool) -> int:
	return _result(_invoke("setTranscripts", {"enabled": enabled}))


func set_translation(language: String, spoken_language := "", speech := false) -> int:
	var args := {"speech": speech}
	if not language.is_empty():
		args["language"] = language
	if not spoken_language.is_empty():
		args["spokenLanguage"] = spoken_language
	return _result(_invoke("setTranslation", args))


## Positional audio: the browser reports its own listener transform. `positions` holds one entry
## `{user_id, position: Vector3, orientation: Vector3}` — only the local user's entry is sent.
func update_positions(channel_id: String, positions_list: Array) -> int:
	var r := RESULT_OK
	for entry in positions_list:
		if not (entry is Dictionary):
			continue
		var e: Dictionary = entry
		var pos: Vector3 = e.get("position", Vector3.ZERO)
		var args := {"channelId": channel_id, "position": {"x": pos.x, "y": pos.y, "z": pos.z}}
		if e.has("orientation"):
			var o: Vector3 = e["orientation"]
			args["orientation"] = {"x": o.x, "y": o.y, "z": o.z}
		r = _result(_invoke("updatePosition", args))
	return r


## Convenience over `update_positions` for a listener `Transform3D` (forward = -Z).
func update_transforms(channel_id: String, transforms: Dictionary) -> int:
	var r := RESULT_OK
	for t in transforms.values():
		if t is Transform3D:
			var xf: Transform3D = t
			var fwd := -xf.basis.z
			r = update_positions(channel_id, [{"position": xf.origin, "orientation": fwd}])
	return r


func respond_recording_consent(recording_id: String, accepted: bool) -> int:
	return _result(_invoke("respondToRecording", {"recordingId": recording_id, "consent": "accepted" if accepted else "declined"}))


# ------------------------------------------------------------------------------------------------
# chat / moderation / speech
# ------------------------------------------------------------------------------------------------

func send_chat(channel_id: String, text: String, metadata_json := "") -> int:
	var args := {"channelId": channel_id, "text": text}
	_metadata(args, metadata_json)
	var rid := _request("chat")
	var r := _result(_invoke("sendMessage", args, rid))
	return rid if r == RESULT_OK else -r


func send_direct_chat(user_id: String, text: String, metadata_json := "") -> int:
	var args := {"userId": user_id, "text": text}
	_metadata(args, metadata_json)
	var rid := _request("chat")
	var r := _result(_invoke("sendDirectMessage", args, rid))
	return rid if r == RESULT_OK else -r


func set_typing(channel_id: String, typing: bool) -> int:
	return _result(_invoke("setTyping", {"channelId": channel_id, "typing": typing}))


func channel_history(channel_id: String, before := "", after := "", limit := 50) -> int:
	return _history({"channelId": channel_id}, channel_id, "", before, after, limit)


func direct_history(user_id: String, before := "", after := "", limit := 50) -> int:
	return _history({"userId": user_id}, "", user_id, before, after, limit)


func edit_chat(message_id: String, text: String, metadata_json := "") -> int:
	var args := {"messageId": message_id, "text": text}
	_metadata(args, metadata_json)
	var rid := _request("chat_update")
	var r := _result(_invoke("editMessage", args, rid))
	return rid if r == RESULT_OK else -r


func delete_chat(message_id: String) -> int:
	var rid := _request("chat_update")
	var r := _result(_invoke("deleteMessage", {"messageId": message_id}, rid))
	return rid if r == RESULT_OK else -r


func react_chat(message_id: String, reaction: String, add := true) -> int:
	return _result(_invoke("react", {"messageId": message_id, "reaction": reaction, "add": add}))


func search_channel_chat(channel_id: String, query: String, from_user_id := "", before := "", limit := 50) -> int:
	return _search({"channelId": channel_id}, channel_id, "", query, from_user_id, before, limit)


## The Web SDK searches one conversation: [param user_id] is required here (an empty one is
## RESULT_INVALID_ARGUMENT), unlike the native client's "every direct conversation" form.
func search_direct_chat(user_id: String, query: String, from_user_id := "", before := "", limit := 50) -> int:
	if user_id.is_empty():
		return -RESULT_INVALID_ARGUMENT
	return _search({"userId": user_id}, "", user_id, query, from_user_id, before, limit)


func mark_channel_read(channel_id: String, message_id: String) -> int:
	return _result(_invoke("markRead", {"channelId": channel_id, "messageId": message_id}))


func mark_direct_read(user_id: String, message_id: String) -> int:
	return _result(_invoke("markRead", {"userId": user_id, "messageId": message_id}))


func channel_read_markers(channel_id: String) -> int:
	return _result(_invoke("readMarkers", {"channelId": channel_id}, _request("read_markers", {"channel_id": channel_id, "user_id": ""})))


func direct_read_markers(user_id: String) -> int:
	return _result(_invoke("readMarkers", {"userId": user_id}, _request("read_markers", {"channel_id": "", "user_id": user_id})))


func moderate(channel_id: String, user_id: String, action: int, action_token: String, reason := "") -> int:
	var name := "kick"
	match action:
		MODERATION_MUTE:
			name = "mute"
		MODERATION_UNMUTE:
			name = "unmute"
	var args := {"channelId": channel_id, "userId": user_id, "action": name, "token": action_token}
	if not reason.is_empty():
		args["reason"] = reason
	var rid := _request("moderate", {"channel_id": channel_id, "user_id": user_id, "action": action})
	var r := _result(_invoke("moderate", args, rid))
	return rid if r == RESULT_OK else -r


## Text-to-speech. `tts_status` reports progress with `request_id` set to the returned id.
func speak(text: String, channel_id := "", destination := TTS_BOTH, voice := "") -> int:
	var args := {"text": text}
	if not channel_id.is_empty():
		args["channelId"] = channel_id
	if not voice.is_empty():
		args["voice"] = voice
	match destination:
		TTS_CHANNEL:
			args["destination"] = "channel"
		TTS_LOCAL:
			args["destination"] = "local"
		_:
			args["destination"] = "both"
	var rid := _request("speak")
	args["clientRef"] = str(rid)
	var r := _result(_invoke("speak", args, rid))
	return rid if r == RESULT_OK else -r


func cancel_speech() -> int:
	return _result(_invoke("cancelSpeech", {}))


# ------------------------------------------------------------------------------------------------
# diagnostics
# ------------------------------------------------------------------------------------------------

## Last WebRTC statistics sample (snake_case keys; `{}` before the first sample). A fresh sample is
## requested asynchronously and delivered through `stats`.
func get_stats() -> Dictionary:
	_invoke("getStats", {}, _request("stats"))
	var v: Variant = _value(_invoke("lastStats", {}), null)
	return _snake(v) if v is Dictionary else {}


func get_network_quality() -> Dictionary:
	return _quality.duplicate()


func get_round_trip_ms() -> int:
	var v: Variant = _value(_invoke("roundTripMs", {}), 0)
	return int(v) if v is float or v is int else 0


## Async device list; delivered through `devices_changed`.
func enumerate_devices() -> int:
	return _result(_invoke("enumerateDevices", {}, _request("devices")))


func set_input_device(device_id: String) -> int:
	var args := {}
	if not device_id.is_empty():
		args["deviceId"] = device_id
	return _result(_invoke("setInputDevice", args, _request("input_device")))


func set_output_device(device_id: String) -> int:
	var args := {}
	if not device_id.is_empty():
		args["deviceId"] = device_id
	return _result(_invoke("setOutputDevice", args, _request("output_device")))


func get_e2ee_fingerprint() -> String:
	var v: Variant = _value(_invoke("e2eeFingerprint", {}), "")
	return String(v) if v is String else ""


func get_e2ee_peer_fingerprint(user_id: String) -> String:
	var v: Variant = _value(_invoke("e2eePeerFingerprint", {"userId": user_id}), "")
	return String(v) if v is String else ""


## Raw bridge call for anything not wrapped here: `method` is a Web SDK bridge method, `args` its
## JSON object. Synchronous values are returned as-is; promises resolve through `request_failed` on
## error only.
func invoke_raw(method: String, args: Dictionary) -> Variant:
	return _value(_invoke(method, args), null)


# ------------------------------------------------------------------------------------------------
# SDK loading
# ------------------------------------------------------------------------------------------------

func _ensure_glue() -> void:
	if _glue != null or not is_supported():
		return
	JavaScriptBridge.eval(_GLUE_SOURCE, true)
	_glue = JavaScriptBridge.get_interface(_GLUE_NAME)


func _load_sdk() -> void:
	if _glue == null or _sdk_status == SDK_READY or _sdk_status == SDK_LOADING:
		return
	if _glue.ready():
		_set_sdk_status(SDK_READY)
		return
	if not sdk_resource.is_empty() and FileAccess.file_exists(sdk_resource):
		var source := FileAccess.get_file_as_string(sdk_resource)
		if not source.is_empty() and _glue.evalSource(source):
			_set_sdk_status(SDK_READY)
			return
		_last_error = String(_glue.error)
	_glue.load(sdk_url)
	_set_sdk_status(SDK_LOADING)
	_poll_sdk()


func _poll_sdk() -> void:
	var status := int(_glue.status())
	if status == SDK_READY:
		_set_sdk_status(SDK_READY)
	elif status == SDK_FAILED:
		_last_error = String(_glue.error)
		_set_sdk_status(SDK_FAILED)
		if not _pending_connect.is_empty():
			_pending_connect.clear()
			_set_state(STATE_FAILED)
			failed_to_recover.emit(_last_error)


func _set_sdk_status(status: int) -> void:
	if status == _sdk_status:
		return
	_sdk_status = status
	sdk_status_changed.emit(status)
	if status == SDK_READY and not _pending_connect.is_empty():
		var pending := _pending_connect
		_pending_connect = []
		var r := _create_and_connect(pending[0], pending[1])
		if r != RESULT_OK:
			_set_state(STATE_FAILED)
			failed_to_recover.emit(_last_error)


func _create_and_connect(ws_url: String, token: String) -> int:
	var options := {
		"apiUrl": api_url if not api_url.is_empty() else _derive_api_url(ws_url),
		"wsUrl": ws_url,
		"token": token,
		"refreshToken": true,
		"joinToken": false,
		"useTurn": use_turn,
		"requestTimeoutMs": request_timeout_ms,
		"autoReconnect": auto_reconnect,
		"reconnect": {"maxAttempts": reconnect_max_attempts},
		"participantStreams": participant_streams,
		"localVoiceActivity": local_voice_activity,
		"rawMessages": raw_events,
		"visemes": visemes,
		"visemeEvents": visemes,
		"e2ee": e2ee,
	}
	match spatial_audio:
		SPATIAL_EQUAL_POWER:
			options["spatialAudio"] = "equalpower"
		SPATIAL_OFF:
			options["spatialAudio"] = false
		_:
			options["spatialAudio"] = true
	_token = token
	var handle := int(_glue.create(JSON.stringify(options), max_queued_events))
	if handle <= 0:
		_last_error = "bridge refused to create a client: %s" % String(_glue.error)
		return RESULT_TRANSPORT
	_handle = handle
	_set_state(STATE_CONNECTING)
	var r := _result(_invoke("connect", {}, _request("connect")))
	if r != RESULT_OK:
		_glue.destroy(_handle)
		_handle = 0
		_set_state(STATE_FAILED)
	return r


static func _derive_api_url(ws_url: String) -> String:
	var scheme := "https"
	var rest := ws_url
	if ws_url.begins_with("wss://"):
		rest = ws_url.substr(6)
	elif ws_url.begins_with("ws://"):
		scheme = "http"
		rest = ws_url.substr(5)
	elif ws_url.begins_with("https://"):
		rest = ws_url.substr(8)
	elif ws_url.begins_with("http://"):
		scheme = "http"
		rest = ws_url.substr(7)
	var slash := rest.find("/")
	if slash >= 0:
		rest = rest.substr(0, slash)
	return "%s://%s" % [scheme, rest]


# ------------------------------------------------------------------------------------------------
# bridge plumbing
# ------------------------------------------------------------------------------------------------

func _request(kind: String, extra: Dictionary = {}) -> int:
	var rid := _next_request
	_next_request += 1
	var entry := extra.duplicate()
	entry["kind"] = kind
	_requests[rid] = entry
	return rid


## Returns the parsed bridge reply: `{ok: bool, value|pending|error}`.
func _invoke(method: String, args: Dictionary, rid := 0) -> Dictionary:
	if _handle <= 0 or _glue == null:
		if rid > 0:
			_requests.erase(rid)
		return {"ok": false, "error": {"message": "not connected", "code": "NOT_CONNECTED"}}
	var raw: Variant = _glue.invoke(_handle, method, JSON.stringify(args), rid)
	var parsed: Variant = JSON.parse_string(String(raw)) if raw is String else null
	if not (parsed is Dictionary):
		return {"ok": false, "error": {"message": "malformed bridge reply"}}
	var reply: Dictionary = parsed
	if reply.get("ok", false) != true:
		var err: Dictionary = reply.get("error", {}) if reply.get("error") is Dictionary else {}
		_last_error = String(err.get("message", "bridge call failed"))
		if rid > 0:
			_requests.erase(rid)
	elif rid > 0 and reply.get("pending", false) != true:
		_requests.erase(rid)
	return reply


func _result(reply: Dictionary) -> int:
	if reply.get("ok", false) == true:
		return RESULT_OK
	var err: Dictionary = reply.get("error", {}) if reply.get("error") is Dictionary else {}
	return _code_to_result(String(err.get("code", "")), String(err.get("message", "")))


func _value(reply: Dictionary, fallback: Variant) -> Variant:
	if reply.get("ok", false) != true or not reply.has("value"):
		return fallback
	var v: Variant = reply["value"]
	return fallback if v == null else v


static func _code_to_result(code: String, message: String) -> int:
	match code:
		"NOT_CONNECTED":
			return RESULT_NOT_CONNECTED
		"UNAUTHORIZED", "AUTH_FAILED", "TOKEN_EXPIRED", "FORBIDDEN":
			return RESULT_UNAUTHORIZED
		"TIMEOUT", "REQUEST_TIMEOUT":
			return RESULT_TIMEOUT
		"VALIDATION_ERROR", "INVALID_ARGUMENT":
			return RESULT_INVALID_ARGUMENT
	if not code.is_empty():
		return RESULT_SERVER_REJECTED
	var m := message.to_lower()
	if m.contains("not connected") or m.contains("unknown handle"):
		return RESULT_NOT_CONNECTED
	if m.contains("must be") or m.contains("required") or m.contains("unknown method"):
		return RESULT_INVALID_ARGUMENT
	if m.contains("timed out") or m.contains("timeout"):
		return RESULT_TIMEOUT
	return RESULT_TRANSPORT


func _history(scope: Dictionary, channel_id: String, user_id: String, before: String, after: String, limit: int) -> int:
	var args := scope.duplicate()
	if not before.is_empty():
		args["before"] = before
	if not after.is_empty():
		args["after"] = after
	args["limit"] = limit
	var rid := _request("history", {"channel_id": channel_id, "user_id": user_id})
	var r := _result(_invoke("history", args, rid))
	return rid if r == RESULT_OK else -r


func _search(scope: Dictionary, channel_id: String, user_id: String, query: String, from_user_id: String, before: String, limit: int) -> int:
	var args := scope.duplicate()
	args["query"] = query
	if not from_user_id.is_empty():
		args["fromUserId"] = from_user_id
	if not before.is_empty():
		args["before"] = before
	args["limit"] = limit
	var rid := _request("search", {"channel_id": channel_id, "user_id": user_id, "query": query})
	var r := _result(_invoke("search", args, rid))
	return rid if r == RESULT_OK else -r


static func _metadata(args: Dictionary, metadata_json: String) -> void:
	if metadata_json.is_empty():
		return
	var parsed: Variant = JSON.parse_string(metadata_json)
	if parsed != null:
		args["metadata"] = parsed


func _set_state(state: int) -> void:
	if state == _state:
		return
	_state = state
	state_changed.emit(state)


func _reset_session() -> void:
	_session = {}
	_channels.clear()
	_channel_info.clear()
	_joins.clear()
	_requests.clear()
	_quality = {}
	_focus = ""


# ------------------------------------------------------------------------------------------------
# events
# ------------------------------------------------------------------------------------------------

func _drain() -> void:
	var raw: Variant = _glue.drain(_handle)
	if not (raw is String) or raw == "[]":
		return
	var parsed: Variant = JSON.parse_string(raw)
	if not (parsed is Array):
		return
	for item in parsed:
		if item is Dictionary:
			_dispatch(item)
		if _handle <= 0:
			return  # disconnected from a handler


func _dispatch(e: Dictionary) -> void:
	var type := String(e.get("type", ""))
	match type:
		"result":
			_on_result(e)
		"overflow":
			events_dropped.emit(int(e.get("dropped", 0)))
		"tokenRequest":
			var request_id := int(e.get("requestId", 0))
			var kind := String(e.get("kind", "refresh"))
			if token_requested.get_connections().is_empty():
				provide_token(request_id, _token if kind == "refresh" else "", "no token_requested handler")
			else:
				token_requested.emit(request_id, kind, _str(e.get("channelId")))
		"connectionState":
			_on_connection_state(String(e.get("state", "disconnected")))
		"sessionReady":
			_apply_session(e.get("info"))
			session_ready.emit(get_session())
		"recovered":
			_apply_session(e.get("info"))
			recovered.emit(bool(_session.get("resumed", false)), bool(_session.get("migrated", false)))
		"channelJoined":
			_on_channel_joined(e)
		"channelLeft":
			var id := _str(e.get("channelId"))
			_channels.erase(id)
			_channel_info.erase(id)
			channel_left.emit(id)
		"participantJoined", "participantUpdated":
			var channel_id := _str(e.get("channelId"))
			var p := _participant(e.get("participant"))
			if p.is_empty():
				return
			var members: Dictionary = _channels.get(channel_id, {})
			var previous: Dictionary = members.get(p["user_id"], {})
			members[p["user_id"]] = p
			_channels[channel_id] = members
			if type == "participantJoined":
				participant_joined.emit(channel_id, p.duplicate())
			else:
				participant_updated.emit(channel_id, p.duplicate())
				if previous.is_empty() or previous.get("muted") != p["muted"] or previous.get("server_muted") != p["server_muted"]:
					participant_mute_changed.emit(channel_id, p["user_id"], p["muted"], p["server_muted"])
		"participantLeft":
			var channel_id := _str(e.get("channelId"))
			var user_id := _str(e.get("userId"))
			if _channels.has(channel_id):
				(_channels[channel_id] as Dictionary).erase(user_id)
			participant_left.emit(channel_id, user_id)
		"speaking":
			var channel_id := _str(e.get("channelId"))
			var user_id := _str(e.get("userId"))
			var speaking: bool = e.get("speaking", false) == true
			_patch(channel_id, user_id, "speaking", speaking)
			participant_speaking.emit(channel_id, user_id, speaking)
		"energy":
			var channel_id := _str(e.get("channelId"))
			var levels := {}
			var raw_levels: Variant = e.get("levels", [])
			if raw_levels is Array:
				for l in raw_levels:
					if l is Dictionary:
						var uid := _str((l as Dictionary).get("userId", (l as Dictionary).get("user_id")))
						var energy := float((l as Dictionary).get("energy", 0.0))
						levels[uid] = energy
						_patch(channel_id, uid, "energy", energy)
			elif raw_levels is Dictionary:
				for uid in raw_levels.keys():
					levels[String(uid)] = float(raw_levels[uid])
			channel_energy.emit(channel_id, levels)
		"localSpeaking":
			local_speaking.emit(e.get("speaking", false) == true)
		"positions":
			positions.emit(_str(e.get("channelId")), JSON.stringify(_snake(e.get("positions", []))))
		"recording":
			recording.emit(_str(e.get("channelId")), _str(e.get("recordingId")), e.get("active", false) == true, e.get("live", false) == true, _str(e.get("initiatedBy")))
		"bitrate":
			bitrate_changed.emit(int(e.get("targetKbps", 0)) * 1000, _str(e.get("reason")))
		"audioPolicy":
			var policy: Variant = e.get("policy")
			_audio_policy = _snake(policy) if policy is Dictionary else {}
			audio_policy_changed.emit(_audio_policy.duplicate())
		"networkQuality":
			var q: Variant = e.get("quality")
			if q is Dictionary:
				_quality = _snake(q)
				network_quality.emit(_quality.duplicate())
		"stats":
			var s: Variant = e.get("stats")
			if s is Dictionary:
				stats.emit(_snake(s))
		"kicked":
			kicked.emit(_str(e.get("channelId")), _str(e.get("reason")))
		"receiverPreferences":
			var prefs: Variant = e.get("prefs")
			if prefs is Dictionary:
				_apply_transmission((prefs as Dictionary).get("transmission"))
				_focus = _str((prefs as Dictionary).get("focusChannel"))
		"userBlockChanged":
			user_block_changed.emit(_str(e.get("userId")), e.get("blocked", false) == true)
		"transmissionChanged":
			_apply_transmission(e.get("mode"))
			transmission_changed.emit(_transmission, _transmission_channel)
		"channelFocusChanged":
			_focus = _str(e.get("channelId"))
			channel_focus_changed.emit(_focus)
		"participantPriorityChanged":
			var channel_id := _str(e.get("channelId"))
			var user_id := _str(e.get("userId"))
			var priority: bool = e.get("priority", false) == true
			_patch(channel_id, user_id, "priority", priority)
			participant_priority_changed.emit(channel_id, user_id, priority)
		"duckingChanged":
			ducking_changed.emit(_str(e.get("channelId")), e.get("active", false) == true, _ducking(e.get("config")))
		"participantStreams":
			var streams: Variant = e.get("streams", [])
			participant_streams_changed.emit(_streams(streams) if streams is Array else [])
		"participantVisemes":
			participant_visemes.emit(_str(e.get("userId")), _viseme_frame(e.get("frame")))
		"localVisemes":
			local_visemes.emit(_viseme_frame(e.get("frame")))
		"e2eePeerKey":
			e2ee_peer_key.emit(_str(e.get("userId")), _str(e.get("fingerprint")), _str(e.get("previousFingerprint")))
		"e2eePeerDecryptable":
			e2ee_peer_decryptable.emit(_str(e.get("userId")), e.get("decryptable", false) == true)
		"e2eeKeyRotated":
			e2ee_key_rotated.emit(int(e.get("generation", 0)))
		"recovering":
			recovering.emit(int(e.get("attempt", 0)), int(e.get("delayMs", 0)), _str(e.get("cause")))
		"endpointChanged":
			_endpoint = _str(e.get("url"))
			endpoint_changed.emit(_endpoint)
		"failedToRecover":
			var err := _error(e.get("error"))
			_close_reason = err["message"]
			failed_to_recover.emit(err["message"])
		"sessionClosed":
			_close_reason = _str(e.get("reason"))
			if _close_reason.is_empty():
				_close_reason = "session closed"
		"chatMessage":
			var m := _chat_message(e.get("message"))
			if not m.is_empty():
				chat_message.emit(m)
		"chatReadMarker":
			var marker := _read_marker(e.get("marker"))
			if not marker.is_empty():
				chat_read_marker.emit(marker)
		"chatInboxSynced":
			chat_inbox_synced.emit(int(e.get("delivered", 0)), e.get("truncated", false) == true)
		"chatMessageUpdated":
			var m := _chat_message(e.get("message"))
			if not m.is_empty():
				chat_message_updated.emit(m)
		"chatReactionChanged":
			var change := _reaction_change(e.get("change"))
			if not change.is_empty():
				chat_reaction_changed.emit(change)
		"participantTyping":
			participant_typing.emit(_str(e.get("channelId")), _str(e.get("userId")), e.get("typing", false) == true)
		"transcript":
			var t := _transcript(e.get("transcript"))
			if not t.is_empty():
				transcript.emit(t)
		"translationChanged":
			var prefs: Variant = e.get("prefs")
			if prefs is Dictionary:
				_translation = {
					"language": _str((prefs as Dictionary).get("language")),
					"spoken_language": _str((prefs as Dictionary).get("spokenLanguage")),
					"speech": (prefs as Dictionary).get("speech", false) == true,
				}
				translation_changed.emit(_translation.duplicate())
		"ttsStatus":
			var s := _tts(e.get("status"))
			if not s.is_empty():
				tts_status.emit(s)
		"serverError":
			server_error.emit(_str(e.get("code")), _str(e.get("message")))
		"error":
			var err := _error(e.get("error"))
			_last_error = err["message"]
			server_error.emit(err["code"] if not err["code"].is_empty() else "CLIENT_ERROR", err["message"])
		"remoteAudio":
			remote_audio.emit(e.get("playing", false) == true, _str(e.get("reason")))
		"devicesChanged":
			var d: Variant = e.get("devices")
			if d is Dictionary:
				devices_changed.emit(_snake(d))
		"message":
			raw_event.emit(0, JSON.stringify(e.get("message", {})))
		_:
			pass


func _on_result(e: Dictionary) -> void:
	var rid := int(e.get("rid", 0))
	if not _requests.has(rid):
		return
	var req: Dictionary = _requests[rid]
	_requests.erase(rid)
	var kind := String(req.get("kind", ""))
	if e.get("ok", false) != true:
		var err := _error(e.get("error"))
		_last_error = err["message"]
		match kind:
			"join":
				var channel_id := String(req.get("channel_id", ""))
				if _joins.get(channel_id, 0) == rid:
					_joins.erase(channel_id)
				request_failed.emit(rid, err["code"], err["message"])
			"connect":
				_set_state(STATE_FAILED)
				failed_to_recover.emit(err["message"])
			"speak":
				tts_status.emit({"request_id": rid, "server_request_id": "", "state": TTS_FAILED, "duration_ms": 0, "message": err["message"]})
			"stats", "devices", "resume_audio", "renegotiate", "visemes", "effects", "input_device", "output_device":
				server_error.emit(err["code"] if not err["code"].is_empty() else "CLIENT_ERROR", err["message"])
			_:
				request_failed.emit(rid, err["code"], err["message"])
		return
	var value: Variant = e.get("value")
	match kind:
		"join":
			pass  # channel_joined is emitted from the channelJoined event (carries the roster + info)
		"moderate":
			moderation_applied.emit(rid, String(req.get("channel_id", "")), String(req.get("user_id", "")), int(req.get("action", 0)))
		"history":
			var page: Dictionary = value if value is Dictionary else {}
			var messages: Array = []
			for m in page.get("messages", []):
				var cm := _chat_message(m)
				if not cm.is_empty():
					messages.append(cm)
			chat_history.emit(rid, String(req.get("channel_id", "")), String(req.get("user_id", "")), messages, _str(page.get("nextBefore")), _str(page.get("nextAfter")))
		"search":
			var page: Dictionary = value if value is Dictionary else {}
			var messages: Array = []
			for m in page.get("messages", []):
				var cm := _chat_message(m)
				if not cm.is_empty():
					messages.append(cm)
			chat_search_result.emit(rid, String(req.get("channel_id", "")), String(req.get("user_id", "")), String(req.get("query", "")), messages, _str(page.get("nextBefore")))
		"read_markers":
			var rm: Dictionary = value if value is Dictionary else {}
			var markers: Array = []
			for m in rm.get("markers", []):
				var marker := _read_marker(m)
				if not marker.is_empty():
					markers.append(marker)
			chat_read_markers.emit(String(req.get("channel_id", "")), String(req.get("user_id", "")), int(rm.get("unreadCount", 0)), markers)
		"stats":
			if value is Dictionary:
				stats.emit(_snake(value))
		"devices":
			if value is Dictionary:
				devices_changed.emit(_snake(value))
		"speak":
			# Queued acknowledgement; terminal states arrive as ttsStatus events keyed by clientRef.
			var v: Dictionary = value if value is Dictionary else {}
			tts_status.emit({"request_id": rid, "server_request_id": _str(v.get("requestId")), "state": TTS_QUEUED, "duration_ms": 0, "message": ""})
		_:
			pass


func _on_connection_state(s: String) -> void:
	var next := STATE_DISCONNECTED
	match s:
		"connecting":
			next = STATE_CONNECTING
		"connected", "media-connecting":
			next = STATE_CONNECTED
		"media-connected":
			next = STATE_MEDIA_BOUND
		"reconnecting":
			next = STATE_RECONNECTING
		"failed":
			next = STATE_FAILED
	var previous := _state
	if next == previous:
		return
	_set_state(next)
	if next == STATE_MEDIA_BOUND:
		media_bound.emit()
		media_path_changed.emit(MEDIA_WEBRTC, "WebRTC media connected")
	elif next == STATE_DISCONNECTED or next == STATE_FAILED:
		_channels.clear()
		_channel_info.clear()
		_joins.clear()
		if previous != STATE_CONNECTING:
			var reason := _close_reason
			if reason.is_empty():
				reason = "reconnect failed" if next == STATE_FAILED else "connection closed"
			disconnected.emit(reason)


func _on_channel_joined(e: Dictionary) -> void:
	var channel_id := _str(e.get("channelId"))
	var members := {}
	var participants: Array = []
	for raw in e.get("participants", []):
		var p := _participant(raw)
		if not p.is_empty():
			members[p["user_id"]] = p
			participants.append(p.duplicate())
	_channels[channel_id] = members
	var info_raw: Variant = _value(_invoke("channelInfo", {"channelId": channel_id}), null)
	var info := _channel_info_dict(info_raw)
	_channel_info[channel_id] = info
	var rid := int(_joins.get(channel_id, 0))
	_joins.erase(channel_id)
	channel_joined.emit(rid, channel_id, participants, info.duplicate())


func _apply_session(info: Variant) -> void:
	if not (info is Dictionary):
		return
	var i: Dictionary = info
	_session = {
		"session_id": _str(i.get("sessionId")),
		"user_id": _str(i.get("userId")),
		"ssrc": int(i.get("ssrc", 0)),
		"resume_grace_ms": 0,
		"resumed": i.get("resumed", false) == true,
		"migrated": i.get("migrated", false) == true,
		"media_tunnel": false,
		"media_quic": false,
		"media_webrtc": true,
		"downlink_mix": true,
		"participant_stream_cap": int(i.get("participantStreamCap", 0)),
		"translation": false,
		"translation_speech": false,
	}
	var tr: Variant = i.get("translation")
	if tr is Dictionary:
		_session["translation"] = true
		_session["translation_speech"] = (tr as Dictionary).get("speech", false) == true
	_endpoint = _str(i.get("endpoint"))
	_failover = PackedStringArray()
	for f in i.get("failover", []):
		_failover.append(String(f))
	var grace: Variant = _value(_invoke("resumeGrace", {}), 0)
	if grace is int or grace is float:
		_session["resume_grace_ms"] = int(grace)


func _apply_transmission(mode: Variant) -> void:
	if not (mode is Dictionary):
		return
	var m: Dictionary = mode
	var kind: Variant = m.get("type", m.get("mode"))
	match kind:
		"none":
			_transmission = TRANSMIT_NONE
			_transmission_channel = ""
		"single":
			_transmission = TRANSMIT_SINGLE
			_transmission_channel = _str(m.get("channelId", m.get("channel_id")))
		_:
			_transmission = TRANSMIT_ALL
			_transmission_channel = ""


func _patch(channel_id: String, user_id: String, key: String, value: Variant) -> void:
	if _channels.has(channel_id):
		var members: Dictionary = _channels[channel_id]
		if members.has(user_id):
			(members[user_id] as Dictionary)[key] = value


# ------------------------------------------------------------------------------------------------
# record conversions (Web SDK camelCase → the native node's snake_case Dictionaries)
# ------------------------------------------------------------------------------------------------

static func _str(v: Variant) -> String:
	return String(v) if v is String else ""


static func _error(v: Variant) -> Dictionary:
	if v is Dictionary:
		return {"message": _str((v as Dictionary).get("message", "error")), "code": _str((v as Dictionary).get("code")), "name": _str((v as Dictionary).get("name"))}
	return {"message": String(v) if v != null else "error", "code": "", "name": ""}


static func _role(v: Variant) -> int:
	match v:
		"speaker":
			return ROLE_SPEAKER
		"moderator":
			return ROLE_MODERATOR
		"administrator":
			return ROLE_ADMINISTRATOR
		_:
			return ROLE_LISTENER


static func _ms(v: Variant) -> int:
	if v is float or v is int:
		return int(v)
	if not (v is String) or (v as String).is_empty():
		return 0
	var iso: String = v
	var ms := 0
	var dot := iso.find(".")
	if dot >= 0:
		var frac := ""
		for i in range(dot + 1, iso.length()):
			if not iso[i].is_valid_int():
				break
			frac += iso[i]
		if not frac.is_empty():
			ms = int((frac + "000").substr(0, 3))
		iso = iso.substr(0, dot)
	iso = iso.trim_suffix("Z")
	if iso.length() < 19:
		return 0
	return int(Time.get_unix_time_from_datetime_string(iso.substr(0, 19))) * 1000 + ms


static func _participant(v: Variant) -> Dictionary:
	if not (v is Dictionary):
		return {}
	var p: Dictionary = v
	return {
		"user_id": _str(p.get("userId")),
		"ssrc": int(p.get("ssrc", 0)),
		"role": _role(p.get("role")),
		"muted": p.get("muted", false) == true,
		"server_muted": p.get("serverMuted", false) == true,
		"speaking": p.get("speaking", false) == true,
		"energy": float(p.get("energy", 0.0)),
		"priority": p.get("priority", false) == true,
		"display_name": _str(p.get("displayName")),
	}


static func _ducking(v: Variant) -> Dictionary:
	var d: Dictionary = v if v is Dictionary else {}
	return {
		"enabled": v is Dictionary,
		"gain": float(d.get("gain", 0.25)),
		"attack_ms": int(d.get("attackMs", 60)),
		"release_ms": int(d.get("releaseMs", 400)),
		"hold_ms": int(d.get("holdMs", 250)),
		"moderators": d.get("moderators", false) == true,
	}


static func _channel_info_dict(v: Variant) -> Dictionary:
	var c: Dictionary = v if v is Dictionary else {}
	return {
		"role": _role(c.get("role", "speaker")),
		"participant_count": int(c.get("participantCount", 0)),
		"hidden_listeners": c.get("hiddenListeners", false) == true,
		"transcription": c.get("transcription", false) == true,
		"safety_voice": c.get("safetyVoice", false) == true,
		"priority": c.get("priority", false) == true,
		"ducking": _ducking(c.get("ducking")),
	}


static func _chat_message(v: Variant) -> Dictionary:
	if not (v is Dictionary):
		return {}
	var m: Dictionary = v
	var metadata: Variant = m.get("metadata")
	return {
		"message_id": _str(m.get("id")),
		"channel_id": _str(m.get("channelId")),
		"sender_id": _str(m.get("fromUserId")),
		"recipient_id": _str(m.get("toUserId")),
		"sender_name": _str(m.get("displayName")),
		"text": _str(m.get("text")),
		"metadata_json": JSON.stringify(metadata) if metadata != null else "",
		"sent_at_ms": _ms(m.get("sentAt")),
		"request_id": int(m.get("clientRef", "0")) if _str(m.get("clientRef")).is_valid_int() else 0,
		"offline": m.get("offline", false) == true,
		"cursor": _str(m.get("cursor")),
		"own": m.get("own", false) == true,
		"system": m.get("system", false) == true,
		"edited_at_ms": _ms(m.get("editedAt")) if m.get("editedAt") != null else 0,
		"deleted_at_ms": _ms(m.get("deletedAt")) if m.get("deletedAt") != null else 0,
		"deleted_by": _str(m.get("deletedBy")),
		"reactions_json": _reactions_json(m.get("reactions")),
	}


## Same shape as the native client's `reactions_json`: `[{"reaction","count","user_ids"}]`,
## empty when there are none.
static func _reactions_json(v: Variant) -> String:
	if not (v is Array) or (v as Array).is_empty():
		return ""
	var out: Array = []
	for r in v:
		if r is Dictionary:
			out.append({"reaction": _str(r.get("reaction")), "count": int(r.get("count", 0)), "user_ids": r.get("userIds", [])})
	return JSON.stringify(out)


static func _reaction_change(v: Variant) -> Dictionary:
	if not (v is Dictionary):
		return {}
	var c: Dictionary = v
	return {
		"message_id": _str(c.get("messageId")),
		"channel_id": _str(c.get("channelId")),
		"message_sender_id": _str(c.get("messageFromUserId")),
		"message_recipient_id": _str(c.get("messageToUserId")),
		"user_id": _str(c.get("userId")),
		"reaction": _str(c.get("reaction")),
		"added": c.get("added", false) == true,
		"count": int(c.get("count", 0)),
		"timestamp_ms": _ms(c.get("timestamp")),
	}


static func _read_marker(v: Variant) -> Dictionary:
	if not (v is Dictionary):
		return {}
	var r: Dictionary = v
	return {
		"user_id": _str(r.get("userId")),
		"channel_id": _str(r.get("channelId")),
		"peer_user_id": _str(r.get("peerUserId")),
		"message_id": _str(r.get("messageId")),
		"message_sent_at_ms": _ms(r.get("messageSentAt")),
		"read_at_ms": _ms(r.get("readAt")),
	}


static func _transcript(v: Variant) -> Dictionary:
	if not (v is Dictionary):
		return {}
	var t: Dictionary = v
	var original: Dictionary = t.get("original") if t.get("original") is Dictionary else {}
	var words: Variant = t.get("words", [])
	return {
		"channel_id": _str(t.get("channelId")),
		"user_id": _str(t.get("userId")),
		"text": _str(t.get("text")),
		"language": _str(t.get("language")),
		"started_at_ms": _ms(t.get("startedAt")),
		"duration_ms": int(t.get("durationMs", 0)),
		"word_count": (words as Array).size() if words is Array else 0,
		"original_text": _str(original.get("text")),
		"original_language": _str(original.get("language")),
	}


static func _tts(v: Variant) -> Dictionary:
	if not (v is Dictionary):
		return {}
	var s: Dictionary = v
	var state := TTS_QUEUED
	match s.get("state"):
		"playing":
			state = TTS_PLAYING
		"finished":
			state = TTS_FINISHED
		"cancelled":
			state = TTS_CANCELLED
		"failed":
			state = TTS_FAILED
	var client_ref := _str(s.get("clientRef"))
	return {
		"request_id": int(client_ref) if client_ref.is_valid_int() else 0,
		"server_request_id": _str(s.get("requestId")),
		"state": state,
		"duration_ms": int(s.get("durationMs", 0)),
		"message": _str(s.get("message")),
	}


static func _viseme_frame(v: Variant) -> Dictionary:
	if not (v is Dictionary):
		return {}
	var f: Dictionary = v
	var weights := PackedFloat32Array()
	weights.resize(VISEME_COUNT)
	var raw: Variant = f.get("weights", [])
	if raw is Array:
		for i in mini(VISEME_COUNT, (raw as Array).size()):
			weights[i] = float(raw[i])
	var dominant: Variant = f.get("dominant", 0)
	var dominant_index := 0
	if dominant is String:
		dominant_index = maxi(0, _VISEME_NAMES.find(dominant))
	elif dominant is int or dominant is float:
		dominant_index = int(dominant)
	return {
		"weights": weights,
		"dominant": dominant_index,
		"mouth_open": float(f.get("mouthOpen", 0.0)),
		"energy": float(f.get("energy", 0.0)),
		"confidence": float(f.get("confidence", 0.0)),
		"sequence": int(f.get("sequence", 0)),
	}


static func _streams(v: Array) -> Array:
	var out: Array = []
	for s in v:
		if s is Dictionary:
			out.append({"mid": _str((s as Dictionary).get("mid")), "user_id": _str((s as Dictionary).get("userId")), "live": (s as Dictionary).get("live", false) == true})
	return out


## Recursive camelCase → snake_case key conversion for generic records (stats, policies, devices).
static func _snake(v: Variant) -> Variant:
	if v is Dictionary:
		var out := {}
		for k in (v as Dictionary).keys():
			out[_snake_key(String(k))] = _snake((v as Dictionary)[k])
		return out
	if v is Array:
		var arr: Array = []
		for item in v:
			arr.append(_snake(item))
		return arr
	return v


static func _snake_key(key: String) -> String:
	var out := ""
	for i in key.length():
		var c := key[i]
		if c != c.to_lower() and c == c.to_upper() and i > 0:
			out += "_"
		out += c.to_lower()
	return out


## Recursive snake_case → camelCase key conversion (parameters sent to the SDK).
static func _camel(v: Variant) -> Variant:
	if v is Dictionary:
		var out := {}
		for k in (v as Dictionary).keys():
			out[_camel_key(String(k))] = _camel((v as Dictionary)[k])
		return out
	if v is Array:
		var arr: Array = []
		for item in v:
			arr.append(_camel(item))
		return arr
	return v


static func _camel_key(key: String) -> String:
	var parts := key.split("_", false)
	if parts.is_empty():
		return key
	var out := parts[0]
	for i in range(1, parts.size()):
		out += parts[i].capitalize().replace(" ", "")
	return out


# ------------------------------------------------------------------------------------------------
# JavaScript glue installed once per page: owns the single AurixBridge, loads the bundle, and
# turns every call into a primitive (int / string) the JavaScriptBridge can marshal.
# ------------------------------------------------------------------------------------------------

const _GLUE_SOURCE := """
(function () {
  if (globalThis.AurixGodot) return;
  var G = { bridge: null, st: 0, error: '', maxQueued: 0 };
  function ready() {
    if (G.bridge) return true;
    var sdk = globalThis.AurixWebSdk;
    if (!sdk || typeof sdk.AurixBridge !== 'function') return false;
    try { G.bridge = new sdk.AurixBridge(G.maxQueued > 0 ? { maxQueuedEvents: G.maxQueued } : {}); } catch (e) { G.st = 3; G.error = String(e && e.message || e); return false; }
    G.st = 2; G.error = '';
    return true;
  }
  function failure(message) { return JSON.stringify({ ok: false, error: { message: String(message), name: 'Error' } }); }
  globalThis.AurixGodot = {
    ready: ready,
    status: function () { if (G.st !== 2 && ready()) return 2; return G.st; },
    get error() { return G.error; },
    version: function () { var sdk = globalThis.AurixWebSdk; return sdk && sdk.version ? String(sdk.version) : ''; },
    evalSource: function (source) {
      try { (0, eval)(source); } catch (e) { G.st = 3; G.error = 'aurix-web-sdk.js failed to evaluate: ' + String(e && e.message || e); return false; }
      if (!ready()) { G.st = 3; G.error = 'evaluated the packed bundle but AurixWebSdk.AurixBridge is missing'; return false; }
      return true;
    },
    load: function (url) {
      if (ready() || G.st === 1) return;
      if (typeof document === 'undefined') { G.st = 3; G.error = 'no document'; return; }
      G.st = 1; G.error = '';
      var tag = document.createElement('script');
      tag.async = true;
      tag.src = url;
      tag.onload = function () { if (!ready()) { G.st = 3; G.error = 'loaded ' + url + ' but AurixWebSdk.AurixBridge is missing'; } };
      tag.onerror = function () { G.st = 3; G.error = 'failed to load ' + url; if (tag.parentNode) tag.parentNode.removeChild(tag); };
      document.head.appendChild(tag);
    },
    create: function (json, maxQueued) {
      G.maxQueued = maxQueued;
      if (!ready()) { G.error = 'Aurix Web SDK is not loaded'; return 0; }
      try { return G.bridge.create(json); } catch (e) { G.error = String(e && e.message || e); return 0; }
    },
    invoke: function (handle, method, args, rid) {
      if (!ready()) return failure('Aurix Web SDK is not loaded');
      try { return G.bridge.invoke(handle, method, args, rid); } catch (e) { return failure(e && e.message || e); }
    },
    drain: function (handle) {
      if (!G.bridge) return '[]';
      try { return G.bridge.drain(handle); } catch (e) { return '[]'; }
    },
    pending: function (handle) { try { return G.bridge ? G.bridge.pending(handle) : 0; } catch (e) { return 0; } },
    destroy: function (handle) { if (!G.bridge) return; try { G.bridge.destroy(handle); } catch (e) {} },
  };
})();
"""
