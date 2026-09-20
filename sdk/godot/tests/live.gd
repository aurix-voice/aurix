# Headless live test: two AurixVoiceClient nodes in one process talk through a real Aurix node.
#
#   AURIX_GODOT_WS=ws://127.0.0.1:8081/ws AURIX_GODOT_CHANNEL=<uuid> \
#   AURIX_GODOT_TOKEN_A=<jwt> AURIX_GODOT_TOKEN_B=<jwt> \
#   godot --headless --path sdk/godot -s tests/live.gd
#
# (tests/live.sh mints the tokens with an API key and sets these.) Alice pushes a 440 Hz tone
# through push_capture_mono, Bob receives it through mix_output; roster/speaking/chat signals are
# checked along the way. Exit 0 on success, 1 on failure, 2 on timeout.
extends SceneTree

const TONE_SECONDS := 2.0

var _alice: AurixVoiceClient
var _bob: AurixVoiceClient
var _channel := ""
var _failures := 0
var _elapsed := 0.0
var _step := "connect"
var _alice_ready := false
var _bob_ready := false
var _alice_joined := false
var _bob_joined := false
var _alice_saw_bob := false
var _bob_saw_alice_speaking := false
var _bob_got_chat := false
var _alice_left_seen_by_bob := false
var _phase := 0.0
var _tone_elapsed := 0.0
var _active_frames := 0
var _sum_sq := 0.0
var _samples := 0
var _alice_id := ""
var _bob_id := ""
var _pull_sum_sq := 0.0
var _pull_samples := 0

func _check(cond: bool, what: String) -> void:
	if cond:
		print("  ok   ", what)
	else:
		_failures += 1
		printerr("  FAIL ", what)

func _init() -> void:
	var ws: String = OS.get_environment("AURIX_GODOT_WS")
	_channel = OS.get_environment("AURIX_GODOT_CHANNEL")
	var tok_a: String = OS.get_environment("AURIX_GODOT_TOKEN_A")
	var tok_b: String = OS.get_environment("AURIX_GODOT_TOKEN_B")
	if ws.is_empty() or _channel.is_empty() or tok_a.is_empty() or tok_b.is_empty():
		printerr("live: AURIX_GODOT_WS / AURIX_GODOT_CHANNEL / AURIX_GODOT_TOKEN_A / AURIX_GODOT_TOKEN_B required")
		quit(2)
		return
	print("AurixVoice live test against ", ws)

	_alice = _make_client("Alice")
	_bob = _make_client("Bob")
	_alice.session_ready.connect(func(session: Dictionary) -> void:
		_alice_id = str(session.get("user_id", ""))
		_alice_ready = true)
	_bob.session_ready.connect(func(session: Dictionary) -> void:
		_bob_id = str(session.get("user_id", ""))
		_bob_ready = true)
	_alice.channel_joined.connect(func(_rid: int, channel_id: String, participants: Array, _info: Dictionary) -> void:
		if channel_id == _channel:
			_alice_joined = true
			_check(participants.is_empty(), "roster excludes the joiner (alice sees %d)" % participants.size()))
	_bob.channel_joined.connect(func(_rid: int, channel_id: String, participants: Array, _info: Dictionary) -> void:
		if channel_id == _channel:
			_bob_joined = true
			_check(participants.size() == 1 and str(participants[0].get("user_id")) == _alice_id, "bob's roster holds alice: %s" % str(participants)))
	_alice.participant_joined.connect(func(channel_id: String, participant: Dictionary) -> void:
		if channel_id == _channel and str(participant.get("user_id")) == _bob_id:
			_alice_saw_bob = true)
	_bob.participant_speaking.connect(func(channel_id: String, user_id: String, speaking: bool) -> void:
		if channel_id == _channel and user_id == _alice_id and speaking:
			_bob_saw_alice_speaking = true)
	_bob.chat_message.connect(func(message: Dictionary) -> void:
		if str(message.get("text")) == "hello from godot":
			_bob_got_chat = true)
	_bob.participant_left.connect(func(channel_id: String, user_id: String) -> void:
		if channel_id == _channel and user_id == _alice_id:
			_alice_left_seen_by_bob = true)
	for c in [_alice, _bob]:
		c.server_error.connect(func(code: String, message: String) -> void: printerr("  server_error ", code, " ", message))
		c.request_failed.connect(func(_rid: int, code: String, message: String) -> void: printerr("  request_failed ", code, " ", message))
		c.failed_to_recover.connect(func(message: String) -> void: printerr("  failed_to_recover ", message))

	_check(_alice.connect_to_server(ws, tok_a) == AurixVoiceClient.RESULT_OK, "alice connect_to_server")
	_check(_bob.connect_to_server(ws, tok_b) == AurixVoiceClient.RESULT_OK, "bob connect_to_server")

func _make_client(name: String) -> AurixVoiceClient:
	var c := AurixVoiceClient.new()
	c.name = name
	# Headless: no microphone / speakers, audio goes through push_capture_mono / mix_output.
	c.auto_capture = false
	c.auto_playback = false
	c.dsp_bypass = true
	c.vad_gate = false
	root.add_child(c)
	return c

func _process(delta: float) -> bool:
	_elapsed += delta
	if _elapsed > 60.0:
		printerr("live: timeout in step ", _step)
		quit(2)
		return false
	match _step:
		"connect":
			if _alice_ready and _bob_ready:
				_check(_alice.get_connection_state() >= AurixVoiceClient.STATE_CONNECTED, "alice state after session_ready")
				_check(not _alice.get_session().is_empty() and str(_alice.get_session().get("user_id")) == _alice_id, "alice session dictionary")
				_check(_alice.get_endpoint().begins_with("ws"), "alice endpoint: %s" % _alice.get_endpoint())
				_check(_alice.join_channel(_channel) > 0, "alice join request")
				_step = "alice_join"
		"alice_join":
			if _alice_joined:
				_check(_alice.get_joined_channels().has(_channel), "alice joined channel list")
				_check(_alice.can_speak_in(_channel), "alice can speak")
				_check(_bob.join_channel(_channel) > 0, "bob join request")
				_step = "bob_join"
		"bob_join":
			if _bob_joined and _alice_saw_bob:
				_check(_alice.get_participants(_channel).size() == 1, "alice sees one participant")
				_check(_alice.get_media_path() != AurixVoiceClient.MEDIA_NONE, "alice media bound (path %d)" % _alice.get_media_path())
				_alice.send_chat(_channel, "hello from godot")
				_step = "tone"
		"tone":
			_push_tone(delta)
			if _tone_elapsed >= TONE_SECONDS:
				var rms := sqrt(_sum_sq / maxf(1.0, float(_samples)))
				_check(_active_frames > 10, "bob mixed %d non-silent frames" % _active_frames)
				_check(rms > 0.05, "bob rms %.3f (alice tone 0.3 mono → stereo mix)" % rms)
				_check(_bob_saw_alice_speaking, "bob saw alice speaking")
				_check(_bob_got_chat, "bob received alice's chat")
				_check(_bob.get_participant_streams().size() >= 1, "bob has participant streams: %s" % str(_bob.get_participant_streams()))
				var stats: Dictionary = _bob.get_stats()
				_check(int(stats.get("packets_received", 0)) > 0, "bob stats packets_received=%s" % str(stats.get("packets_received")))
				# Per-participant pull: once claimed, alice leaves bob's aggregate mix and is only
				# reachable through pull_participant (what AurixParticipantPlayer does per frame).
				_check(_bob.set_participant_claimed(_alice_id, true) == AurixVoiceClient.RESULT_OK, "bob claims alice")
				_tone_elapsed = 0.0
				_active_frames = 0
				_sum_sq = 0.0
				_samples = 0
				_step = "claimed"
		"claimed":
			_push_tone(delta, true)
			if _tone_elapsed >= TONE_SECONDS:
				var mix_rms := sqrt(_sum_sq / maxf(1.0, float(_samples)))
				var pull_rms := sqrt(_pull_sum_sq / maxf(1.0, float(_pull_samples)))
				_check(mix_rms < 0.01, "claimed alice is out of bob's mix (rms %.3f)" % mix_rms)
				_check(pull_rms > 0.05, "pull_participant carries alice (rms %.3f)" % pull_rms)
				_check(_bob.set_participant_claimed(_alice_id, false) == AurixVoiceClient.RESULT_OK, "bob releases alice")
				_check(_alice.leave_channel(_channel) == AurixVoiceClient.RESULT_OK, "alice leave")
				_step = "leave"
		"leave":
			if _alice_left_seen_by_bob:
				_check(not _alice.get_joined_channels().has(_channel), "alice channel list empty after leave")
				_alice.disconnect_from_server()
				_bob.disconnect_from_server()
				_check(not _alice.is_client_created() and _alice.get_connection_state() == AurixVoiceClient.STATE_DISCONNECTED, "alice disconnected")
				_finish()
	return false

func _push_tone(delta: float, pull: bool = false) -> void:
	# Real time: delta seconds of 48 kHz mono at 0.3 amplitude, in whatever chunk the frame allows.
	var n := int(round(delta * 48000.0))
	if n <= 0:
		return
	var pcm := PackedFloat32Array()
	pcm.resize(n)
	for i in n:
		pcm[i] = 0.3 * sin(_phase)
		_phase += TAU * 440.0 / 48000.0
	_alice.push_capture_mono(pcm, 48000)
	if pull:
		for v in _bob.pull_participant(_alice_id, n):
			_pull_sum_sq += v.x * v.x + v.y * v.y
		_pull_samples += n * 2
	var mixed := _bob.mix_output(n)
	var loud := false
	for v in mixed:
		_sum_sq += v.x * v.x + v.y * v.y
		if absf(v.x) > 0.001:
			loud = true
	_samples += mixed.size() * 2
	if loud:
		_active_frames += 1
	_tone_elapsed += delta

func _finish() -> void:
	if _failures == 0:
		print("live: all checks passed")
		quit(0)
	else:
		printerr("live: %d check(s) failed" % _failures)
		quit(1)
