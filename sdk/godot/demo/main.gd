# Minimal voice lobby: connect with a server-issued token, join a channel, talk.
#
# The AurixVoiceClient node captures the default microphone (AudioStreamMicrophone → muted
# "AurixCapture" bus → AudioEffectCapture) and plays the remote mix through its own
# AudioStreamPlayer as soon as the session is ready; this script only wires UI and shows how
# to spawn one AurixParticipantPlayer (AudioStreamPlayer3D) per remote speaker when
# playback_mode is PER_PARTICIPANT.
extends Control

@onready var voice: AurixVoiceClient = $AurixVoiceClient
@onready var ws_url: LineEdit = %WsUrl
@onready var token: LineEdit = %Token
@onready var channel: LineEdit = %Channel
@onready var status: Label = %Status
@onready var roster: ItemList = %Roster
@onready var log_view: RichTextLabel = %Log
@onready var connect_button: Button = %ConnectButton
@onready var join_button: Button = %JoinButton
@onready var mute_button: CheckButton = %MuteButton
@onready var spatial_button: CheckButton = %SpatialButton

var _joined_channel := ""
var _players: Dictionary = {}  # user_id → AurixParticipantPlayer

func _ready() -> void:
	ws_url.text = OS.get_environment("AURIX_GODOT_WS") if OS.has_environment("AURIX_GODOT_WS") else "ws://127.0.0.1:8081/ws"
	token.text = OS.get_environment("AURIX_GODOT_TOKEN_A")
	channel.text = OS.get_environment("AURIX_GODOT_CHANNEL")
	status.text = "native core %s — disconnected" % AurixVoiceClient.get_native_version()

	voice.state_changed.connect(_on_state)
	voice.session_ready.connect(func(s: Dictionary) -> void: _log("session %s (user %s)" % [s["session_id"], s["user_id"]]))
	voice.media_bound.connect(func() -> void: _log("media bound (%s)" % _path_name(voice.get_media_path())))
	voice.media_path_changed.connect(func(path: int, reason: String) -> void: _log("media path → %s: %s" % [_path_name(path), reason]))
	voice.channel_joined.connect(_on_channel_joined)
	voice.channel_left.connect(func(_id: String) -> void: _joined_channel = ""; _refresh_roster())
	voice.participant_joined.connect(func(_ch: String, p: Dictionary) -> void: _log("%s joined" % p["display_name"]); _refresh_roster())
	voice.participant_left.connect(func(_ch: String, user_id: String) -> void: _release_player(user_id); _refresh_roster())
	voice.participant_speaking.connect(func(_ch: String, _u: String, _s: bool) -> void: _refresh_roster())
	voice.participant_mute_changed.connect(func(_ch: String, _u: String, _m: bool, _sm: bool) -> void: _refresh_roster())
	voice.local_speaking.connect(func(speaking: bool) -> void: mute_button.text = "Mute (talking)" if speaking else "Mute")
	voice.chat_message.connect(func(m: Dictionary) -> void: _log("[chat] %s: %s" % [m["sender_id"], m["text"]]))
	voice.transcript.connect(func(t: Dictionary) -> void: _log("[stt] %s: %s" % [t["user_id"], t["text"]]))
	voice.recovering.connect(func(attempt: int, delay_ms: int, cause: String) -> void: _log("reconnecting #%d in %d ms: %s" % [attempt, delay_ms, cause]))
	voice.recovered.connect(func(resumed: bool, migrated: bool) -> void: _log("recovered (resumed=%s migrated=%s)" % [resumed, migrated]))
	voice.failed_to_recover.connect(func(reason: String) -> void: _log("gave up: %s" % reason))
	voice.kicked.connect(func(ch: String, reason: String) -> void: _log("kicked from %s: %s" % [ch, reason]))
	voice.request_failed.connect(func(_rid: int, code: String, message: String) -> void: _log("request failed: %s %s" % [code, message]))
	voice.server_error.connect(func(code: String, message: String) -> void: _log("server error: %s %s" % [code, message]))
	voice.network_quality.connect(func(q: Dictionary) -> void: %Quality.text = "quality %d/5  rtt %d ms  loss %.1f%%" % [q["bars"], q["rtt_ms"], q["downlink_loss_percent"]])

	connect_button.pressed.connect(_on_connect_pressed)
	join_button.pressed.connect(_on_join_pressed)
	mute_button.toggled.connect(func(on: bool) -> void: voice.set_muted(on))
	spatial_button.toggled.connect(_on_spatial_toggled)
	%SendButton.pressed.connect(_on_send_pressed)
	%ChatInput.text_submitted.connect(func(_t: String) -> void: _on_send_pressed())
	_on_state(voice.get_connection_state())

func _on_connect_pressed() -> void:
	if voice.is_client_created():
		voice.disconnect_from_server()
		return
	var r := voice.connect_to_server(ws_url.text.strip_edges(), token.text.strip_edges())
	if r != AurixVoiceClient.RESULT_OK:
		_log("connect failed (%d): %s" % [r, voice.get_last_error()])

func _on_join_pressed() -> void:
	if not _joined_channel.is_empty():
		voice.leave_channel(_joined_channel)
		return
	var id := channel.text.strip_edges()
	if voice.join_channel(id) == 0:
		_log("join not sent: %s" % voice.get_last_error())

func _on_send_pressed() -> void:
	var text: String = %ChatInput.text.strip_edges()
	if text.is_empty() or _joined_channel.is_empty():
		return
	voice.send_chat(_joined_channel, text)
	%ChatInput.text = ""

func _on_spatial_toggled(on: bool) -> void:
	# PER_PARTICIPANT: claimed speakers leave the stereo mix and play through their own
	# AudioStreamPlayer3D; everyone else keeps coming through the mix.
	voice.playback_mode = AurixVoiceClient.PLAYBACK_PER_PARTICIPANT if on else AurixVoiceClient.PLAYBACK_MIXED
	if on:
		for p in voice.get_participants(_joined_channel):
			_ensure_player(p["user_id"])
	else:
		for user_id in _players.keys():
			_release_player(user_id)

func _ensure_player(user_id: String) -> void:
	if _players.has(user_id) or not spatial_button.button_pressed:
		return
	var player := AurixParticipantPlayer.new()
	player.user_id = user_id
	player.position = Vector3(randf_range(-3.0, 3.0), 0.0, randf_range(-3.0, 3.0))
	$World.add_child(player)
	# Without client_path the player would look for an AurixVoiceClient among its ancestors.
	player.client_path = player.get_path_to(voice)
	_players[user_id] = player

func _release_player(user_id: String) -> void:
	var player: AurixParticipantPlayer = _players.get(user_id)
	if player:
		player.queue_free()
		_players.erase(user_id)

func _on_state(state: int) -> void:
	status.text = "native core %s — %s" % [AurixVoiceClient.get_native_version(), _state_name(state)]
	connect_button.text = "Disconnect" if voice.is_client_created() else "Connect"
	join_button.disabled = state < AurixVoiceClient.STATE_CONNECTED
	if state == AurixVoiceClient.STATE_DISCONNECTED:
		_joined_channel = ""
		_refresh_roster()

func _on_channel_joined(_rid: int, channel_id: String, participants: Array, info: Dictionary) -> void:
	_joined_channel = channel_id
	_log("joined %s as %s (%d participants)" % [channel_id, "listener" if info["role"] == AurixVoiceClient.ROLE_LISTENER else "speaker", info["participant_count"]])
	for p in participants:
		_ensure_player(p["user_id"])
	_refresh_roster()

func _refresh_roster() -> void:
	roster.clear()
	join_button.text = "Leave" if not _joined_channel.is_empty() else "Join"
	if _joined_channel.is_empty():
		return
	for p in voice.get_participants(_joined_channel):
		var flags := ""
		if p["speaking"]:
			flags += " 🔊"
		if p["muted"] or p["server_muted"]:
			flags += " 🔇"
		roster.add_item("%s%s" % [p["display_name"], flags])

func _log(line: String) -> void:
	log_view.append_text(line + "\n")

static func _state_name(state: int) -> String:
	match state:
		AurixVoiceClient.STATE_CONNECTING: return "connecting"
		AurixVoiceClient.STATE_CONNECTED: return "connected"
		AurixVoiceClient.STATE_MEDIA_BOUND: return "media bound"
		AurixVoiceClient.STATE_RECONNECTING: return "reconnecting"
		AurixVoiceClient.STATE_FAILED: return "failed"
	return "disconnected"

static func _path_name(path: int) -> String:
	match path:
		AurixVoiceClient.MEDIA_UDP: return "UDP"
		AurixVoiceClient.MEDIA_TUNNEL: return "WebSocket tunnel"
	return "none"
