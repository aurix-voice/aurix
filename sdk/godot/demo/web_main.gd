# Web-export lobby: the same flow as demo/main.gd, but built on AurixWebVoiceClient — the GDScript
# node that drives the Aurix Web SDK through JavaScriptBridge. Nothing here touches the native
# GDExtension (it is not shipped to the browser), so this scene is the `web` override of the
# project's main scene (see project.godot: `run/main_scene.web`).
#
# Query parameters (`index.html?ws=…&api=…&token=…&channel=…`) pre-fill the form so an exported build
# can be driven from a test harness; the harness can also poll `window.__aurixGodot` (a JSON
# snapshot refreshed on every event) and push commands into `window.__aurixGodotCommands`.
extends Control

const WebClient := preload("res://addons/aurix_voice/web/aurix_web_voice_client.gd")

@onready var ws_url: LineEdit = %WsUrl
@onready var token: LineEdit = %Token
@onready var channel: LineEdit = %Channel
@onready var status: Label = %Status
@onready var roster: ItemList = %Roster
@onready var log_view: RichTextLabel = %Log
@onready var connect_button: Button = %ConnectButton
@onready var join_button: Button = %JoinButton
@onready var mute_button: CheckButton = %MuteButton
@onready var audio_button: Button = %AudioButton

var voice: Node
var _joined_channel := ""
var _events: Array = []
var _snapshot: Dictionary = {"state": 0, "sdk": 0, "events": []}
var _harness := false

func _ready() -> void:
	voice = WebClient.new()
	voice.name = "AurixWebVoiceClient"
	add_child(voice)

	var query := _query_params()
	ws_url.text = query.get("ws", "ws://127.0.0.1:8081/ws")
	token.text = query.get("token", "")
	channel.text = query.get("channel", "")
	_harness = query.has("harness")
	if query.has("streams"):
		voice.participant_streams = int(query["streams"])
	if query.has("spatial"):
		voice.spatial_audio = int(query["spatial"])
	if query.has("sdk"):
		voice.sdk_url = query["sdk"]
	if query.has("api"):
		voice.api_url = query["api"]

	if not WebClient.is_supported():
		status.text = "this scene only runs in a Web export — use demo/main.tscn natively"
		connect_button.disabled = true
		_publish()
		return

	voice.sdk_status_changed.connect(func(s: int) -> void:
		_log("sdk status %d%s" % [s, (" — " + voice.get_last_error()) if s == WebClient.SDK_FAILED else ""])
		_publish())
	voice.state_changed.connect(_on_state)
	voice.session_ready.connect(func(s: Dictionary) -> void: _log("session %s (user %s, cap %d)" % [s["session_id"], s["user_id"], s["participant_stream_cap"]]))
	voice.media_bound.connect(func() -> void: _log("media bound (webrtc)"))
	voice.channel_joined.connect(_on_channel_joined)
	voice.channel_left.connect(func(_id: String) -> void: _joined_channel = ""; _refresh_roster())
	voice.participant_joined.connect(func(_ch: String, p: Dictionary) -> void: _log("%s joined" % p["display_name"]); _refresh_roster())
	voice.participant_left.connect(func(_ch: String, user_id: String) -> void: _log("%s left" % user_id); _refresh_roster())
	voice.participant_speaking.connect(func(_ch: String, u: String, s: bool) -> void: _event("speaking", {"user_id": u, "speaking": s}); _refresh_roster())
	voice.participant_mute_changed.connect(func(_ch: String, _u: String, _m: bool, _sm: bool) -> void: _refresh_roster())
	voice.local_speaking.connect(func(speaking: bool) -> void: mute_button.text = "Mute (talking)" if speaking else "Mute")
	voice.chat_message.connect(func(m: Dictionary) -> void: _log("[chat] %s: %s" % [m["sender_name"], m["text"]]); _event("chat", m))
	voice.transcript.connect(func(t: Dictionary) -> void: _log("[stt] %s: %s" % [t["user_id"], t["text"]]))
	voice.recovering.connect(func(attempt: int, delay_ms: int, cause: String) -> void: _log("reconnecting #%d in %d ms: %s" % [attempt, delay_ms, cause]))
	voice.recovered.connect(func(resumed: bool, migrated: bool) -> void: _log("recovered (resumed=%s migrated=%s)" % [resumed, migrated]); _event("recovered", {"resumed": resumed}))
	voice.failed_to_recover.connect(func(reason: String) -> void: _log("gave up: %s" % reason); _event("failed", {"reason": reason}))
	voice.disconnected.connect(func(reason: String) -> void: _log("disconnected: %s" % reason))
	voice.kicked.connect(func(ch: String, reason: String) -> void: _log("kicked from %s: %s" % [ch, reason]))
	voice.request_failed.connect(func(rid: int, code: String, message: String) -> void: _log("request %d failed: %s %s" % [rid, code, message]); _event("request_failed", {"code": code, "message": message}))
	voice.server_error.connect(func(code: String, message: String) -> void: _log("server error: %s %s" % [code, message]))
	voice.network_quality.connect(func(q: Dictionary) -> void: %Quality.text = "quality %d/5  rtt %d ms  loss %.1f%%" % [q["bars"], q["rtt_ms"], q["downlink_loss_percent"]]; _event("quality", q))
	voice.participant_streams_changed.connect(func(streams: Array) -> void: _event("streams", {"streams": streams}))
	voice.remote_audio.connect(func(playing: bool, reason: String) -> void:
		audio_button.visible = not playing
		_log("remote audio %s%s" % ["playing" if playing else "blocked", (": " + reason) if not reason.is_empty() else ""]))
	voice.token_requested.connect(func(rid: int, kind: String, _ch: String) -> void:
		# A real game asks its backend here; the lobby reuses the token typed into the form.
		if kind == "refresh":
			voice.provide_token(rid, token.text)
		else:
			voice.provide_token(rid, "", "no join tokens"))

	connect_button.pressed.connect(_on_connect_pressed)
	join_button.pressed.connect(_on_join_pressed)
	mute_button.toggled.connect(func(on: bool) -> void: voice.set_muted(on))
	audio_button.pressed.connect(func() -> void: voice.resume_audio())
	%SendButton.pressed.connect(_on_send_pressed)
	%ChatInput.text_submitted.connect(func(_t: String) -> void: _on_send_pressed())

	voice.preload_sdk()
	status.text = "web sdk loading — disconnected"
	_publish()
	if _harness and not token.text.is_empty():
		_on_connect_pressed()

func _process(_delta: float) -> void:
	if _harness:
		_run_harness_commands()

func _on_connect_pressed() -> void:
	if voice.get_connection_state() != WebClient.STATE_DISCONNECTED and voice.get_connection_state() != WebClient.STATE_FAILED:
		voice.disconnect_from_server()
		return
	var r: int = voice.connect_to_server(ws_url.text.strip_edges(), token.text.strip_edges())
	if r != WebClient.RESULT_OK:
		_log("connect failed: %d %s" % [r, voice.get_last_error()])

func _on_join_pressed() -> void:
	if _joined_channel.is_empty():
		var rid: int = voice.join_channel(channel.text.strip_edges())
		_log("join request %d" % rid if rid > 0 else "join refused: %d" % -rid)
	else:
		voice.leave_channel(_joined_channel)

func _on_send_pressed() -> void:
	var text: String = %ChatInput.text.strip_edges()
	if text.is_empty() or _joined_channel.is_empty():
		return
	voice.send_chat(_joined_channel, text)
	%ChatInput.text = ""

func _on_state(state: int) -> void:
	var names := ["disconnected", "connecting", "connected", "media bound", "reconnecting", "failed"]
	status.text = "web sdk %s — %s" % [voice.get_sdk_version(), names[state] if state < names.size() else str(state)]
	connect_button.text = "Disconnect" if state in [WebClient.STATE_CONNECTED, WebClient.STATE_MEDIA_BOUND, WebClient.STATE_RECONNECTING, WebClient.STATE_CONNECTING] else "Connect"
	join_button.disabled = state != WebClient.STATE_CONNECTED and state != WebClient.STATE_MEDIA_BOUND
	if state == WebClient.STATE_DISCONNECTED or state == WebClient.STATE_FAILED:
		_joined_channel = ""
		_refresh_roster()
	_publish()

func _on_channel_joined(request_id: int, channel_id: String, participants: Array, info: Dictionary) -> void:
	_joined_channel = channel_id
	join_button.text = "Leave"
	_log("joined %s (request %d, role %d, %d participants)" % [channel_id, request_id, info.get("role", -1), participants.size()])
	_event("joined", {"channel_id": channel_id, "participants": participants.size(), "role": info.get("role", -1)})
	_refresh_roster()

func _refresh_roster() -> void:
	roster.clear()
	if _joined_channel.is_empty():
		join_button.text = "Join"
		_publish()
		return
	for p in voice.get_participants(_joined_channel):
		var flags := ""
		if p["speaking"]:
			flags += " 🔊"
		if p["muted"] or p["server_muted"]:
			flags += " (muted)"
		if p["priority"]:
			flags += " ★"
		roster.add_item("%s%s" % [p["display_name"], flags])
	_publish()

func _log(line: String) -> void:
	log_view.append_text(line + "\n")
	print("[web lobby] ", line)

func _event(kind: String, data: Dictionary) -> void:
	_events.append({"kind": kind, "data": data})
	if _events.size() > 200:
		_events.pop_front()
	_publish()

# --- harness plumbing (only active with ?harness=1) ---

func _publish() -> void:
	if not WebClient.is_supported():
		return
	_snapshot = {
		"state": voice.get_connection_state(),
		"sdk": voice.get_sdk_status(),
		"sdk_version": voice.get_sdk_version(),
		"error": voice.get_last_error(),
		"session": voice.get_session(),
		"channel": _joined_channel,
		"participants": voice.get_participants(_joined_channel) if not _joined_channel.is_empty() else [],
		"streams": voice.get_participant_streams(),
		"muted": voice.is_muted(),
		"events": _events,
	}
	JavaScriptBridge.eval("globalThis.__aurixGodot = %s;" % JSON.stringify(_snapshot), true)

func _run_harness_commands() -> void:
	var raw: Variant = JavaScriptBridge.eval("JSON.stringify((globalThis.__aurixGodotCommands || []).splice(0))", true)
	if not (raw is String) or raw == "[]":
		return
	var commands: Variant = JSON.parse_string(raw)
	if not (commands is Array):
		return
	for cmd in commands:
		if not (cmd is Dictionary):
			continue
		var result: Variant = null
		match String(cmd.get("op", "")):
			"join":
				channel.text = String(cmd.get("channel", channel.text))
				result = voice.join_channel(channel.text)
			"leave":
				result = voice.leave_channel(_joined_channel)
			"mute":
				mute_button.button_pressed = bool(cmd.get("on", true))
				result = voice.is_muted()
			"chat":
				result = voice.send_chat(_joined_channel, String(cmd.get("text", "")))
			"volume":
				result = voice.set_participant_volume(String(cmd.get("user_id", "")), float(cmd.get("volume", 1.0)))
			"pin":
				result = voice.set_pinned_participants(PackedStringArray(cmd.get("user_ids", [])))
			"resume_audio":
				result = voice.resume_audio()
			"stats":
				# `get_stats()` returns the last cached sample (`{}` before the first) and asks the
				# bridge for a fresh one, delivered through `stats`; wait for it when nothing is cached.
				result = voice.get_stats()
				if (result as Dictionary).is_empty():
					result = await voice.stats
			"quality":
				result = voice.get_network_quality()
			"disconnect":
				voice.disconnect_from_server()
			"raw":
				result = voice.invoke_raw(String(cmd.get("method", "")), cmd.get("args", {}))
			_:
				result = "unknown op"
		_event("command", {"op": cmd.get("op", ""), "id": cmd.get("id", 0), "result": result})

static func _query_params() -> Dictionary:
	var out: Dictionary = {}
	if not OS.has_feature("web"):
		return out
	var raw: Variant = JavaScriptBridge.eval("location.search", true)
	if not (raw is String) or (raw as String).length() < 2:
		return out
	for pair in (raw as String).substr(1).split("&", false):
		var kv: PackedStringArray = pair.split("=", true, 1)
		out[kv[0].uri_decode()] = kv[1].uri_decode() if kv.size() > 1 else ""
	return out
