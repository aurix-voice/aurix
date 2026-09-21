# Headless smoke test for AurixWebVoiceClient (the JavaScriptBridge client) — no browser needed.
#
#   godot --headless --path sdk/godot -s tests/web_smoke.gd
#
# Outside a Web export the node must report itself unsupported without touching JavaScriptBridge;
# the rest feeds Web-SDK bridge events straight into the dispatcher and checks that they come out
# as the same signals/Dictionaries the native AurixVoiceClient emits. Exits 0 on success.
extends SceneTree

const Web := preload("res://addons/aurix_voice/web/aurix_web_voice_client.gd")

var _failures := 0
var _elapsed := 0.0
var _got: Dictionary = {}

func _process(delta: float) -> bool:
	_elapsed += delta
	if _elapsed > 20.0:
		printerr("web smoke: watchdog timeout")
		quit(2)
	return false

func _check(cond: bool, what: String) -> void:
	if cond:
		print("  ok   ", what)
	else:
		_failures += 1
		printerr("  FAIL ", what)

func _record(name: String) -> Callable:
	return func(a = null, b = null, c = null, d = null, e = null, f = null) -> void:
		_got[name] = [a, b, c, d, e, f]

func _init() -> void:
	print("AurixWebVoiceClient smoke test")
	var client := Web.new()
	root.add_child(client)

	# --- unsupported platform: honest errors, no JS access
	_check(not Web.is_supported(), "headless run is not a web export")
	_check(client.get_connection_state() == Web.STATE_DISCONNECTED, "initial state disconnected")
	_check(client.get_sdk_status() == Web.SDK_NOT_LOADED, "sdk not loaded")
	_check(client.get_sdk_version() == "", "no sdk version")
	_check(client.get_media_path() == Web.MEDIA_NONE, "media path none")
	_check(client.connect_to_server("", "tok") == Web.RESULT_TRANSPORT, "connect outside web → TRANSPORT")
	_check(client.get_last_error().contains("Web export"), "last error names the platform: %s" % client.get_last_error())
	_check(client.join_channel("00000000-0000-4000-8000-000000000001") == -Web.RESULT_NOT_CONNECTED, "join without client → -NOT_CONNECTED")
	_check(client.leave_channel("x") == Web.RESULT_NOT_CONNECTED, "leave without client → NOT_CONNECTED")
	_check(client.send_chat("x", "hi") == -Web.RESULT_NOT_CONNECTED, "chat without client → -NOT_CONNECTED")
	_check(client.set_participant_volume("u", 0.5) == Web.RESULT_NOT_CONNECTED, "volume without client → NOT_CONNECTED")
	_check(client.set_token("") == Web.RESULT_INVALID_ARGUMENT, "empty token rejected")
	_check(client.set_token("t") == Web.RESULT_OK, "set_token stores the credential")
	_check(not client.network_changed(), "network_changed without client → false")
	_check(client.get_stats().is_empty(), "stats empty without client")
	_check(client.get_participant_streams().is_empty(), "no participant streams without client")
	_check(client.get_session().is_empty(), "no session")
	_check(client.get_joined_channels().is_empty(), "no channels")
	_check(client.get_local_visemes().is_empty(), "no visemes")
	client.preload_sdk()
	_check(client.get_sdk_status() == Web.SDK_NOT_LOADED, "preload_sdk is a no-op outside web")
	client.disconnect_from_server()
	_check(client.get_connection_state() == Web.STATE_DISCONNECTED, "disconnect is idempotent")

	# --- helpers
	_check(Web._derive_api_url("wss://voice.example.com:8443/ws?x=1") == "https://voice.example.com:8443", "api url from wss")
	_check(Web._derive_api_url("ws://127.0.0.1:8081/ws") == "http://127.0.0.1:8081", "api url from ws")
	_check(Web._code_to_result("UNAUTHORIZED", "") == Web.RESULT_UNAUTHORIZED, "code map: unauthorized")
	_check(Web._code_to_result("", "channelId must be a string") == Web.RESULT_INVALID_ARGUMENT, "code map: validation message")
	_check(Web._code_to_result("CHANNEL_FULL", "") == Web.RESULT_SERVER_REJECTED, "code map: server code")
	_check(Web._ms("2026-01-02T03:04:05.678Z") == 1767323045678, "ISO timestamp → ms: %d" % Web._ms("2026-01-02T03:04:05.678Z"))
	_check(Web._ms("2026-01-02T03:04:05Z") == 1767323045000, "ISO without fraction")
	_check(Web._ms(1234) == 1234, "numeric timestamp passthrough")
	var snake: Dictionary = Web._snake({"rttMs": 12, "downlinkLossPercent": 1.5, "server": {"rFactor": 80}})
	_check(snake.get("rtt_ms") == 12 and snake.get("downlink_loss_percent") == 1.5 and (snake.get("server") as Dictionary).get("r_factor") == 80, "camelCase → snake_case: %s" % JSON.stringify(snake))
	var camel: Dictionary = Web._camel({"highpass_hz": 300.0, "ring_mod_hz": 30.0})
	_check(camel.has("highpassHz") and camel.has("ringModHz"), "snake_case → camelCase: %s" % JSON.stringify(camel))
	var frame: Dictionary = Web._viseme_frame({"weights": [0.1, 0.2, 0.3, 0.0, 0.4, 0.0, 0.0, 0.0, 0.0], "dominant": "aa", "mouthOpen": 0.5, "energy": 0.2, "confidence": 0.9, "sequence": 7})
	_check(frame["dominant"] == Web.VISEME_AA and (frame["weights"] as PackedFloat32Array).size() == 9 and frame["sequence"] == 7, "viseme frame conversion")

	# --- event dispatch: bridge JSON → native-shaped signals
	for sig in ["state_changed", "session_ready", "media_bound", "media_path_changed", "channel_joined", "participant_joined", "participant_speaking", "participant_mute_changed", "channel_energy", "chat_message", "transcript", "tts_status", "network_quality", "recovering", "recovered", "failed_to_recover", "disconnected", "token_requested", "participant_streams_changed", "ducking_changed", "request_failed", "remote_audio", "events_dropped", "server_error", "chat_history", "moderation_applied", "channel_left", "participant_left", "e2ee_peer_key"]:
		client.connect(sig, _record(sig))

	client._dispatch({"type": "connectionState", "state": "connecting"})
	_check(_got["state_changed"][0] == Web.STATE_CONNECTING, "connectionState connecting")
	client._dispatch({"type": "connectionState", "state": "connected"})
	_check(client.get_connection_state() == Web.STATE_CONNECTED, "connectionState connected")
	client._dispatch({"type": "sessionReady", "info": {"sessionId": "s1", "userId": "u1", "ssrc": 42, "resumed": false, "migrated": false, "endpoint": "wss://a/ws", "failover": ["wss://b/ws"], "participantStreamCap": 16, "translation": {"speech": true, "languages": ["en"]}}})
	var session: Dictionary = _got["session_ready"][0]
	_check(session["session_id"] == "s1" and session["user_id"] == "u1" and session["ssrc"] == 42 and session["media_webrtc"] == true and session["translation_speech"] == true, "session_ready dictionary uses native keys")
	_check(client.get_endpoint() == "wss://a/ws" and client.get_failover_endpoints()[0] == "wss://b/ws", "endpoint + failover from session")
	client._dispatch({"type": "connectionState", "state": "media-connected"})
	_check(_got.has("media_bound") and _got["media_path_changed"][0] == Web.MEDIA_WEBRTC, "media-connected → media_bound + MEDIA_WEBRTC")
	_check(client.get_media_path() == Web.MEDIA_WEBRTC, "media path webrtc")

	client._joins["c1"] = 7
	client._dispatch({"type": "channelJoined", "channelId": "c1", "participants": [{"userId": "u2", "displayName": "Bob", "ssrc": 5, "role": "moderator", "muted": false, "serverMuted": false, "speaking": false, "energy": 0.0, "priority": true}]})
	var joined: Array = _got["channel_joined"]
	_check(joined[0] == 7 and joined[1] == "c1" and (joined[2] as Array).size() == 1, "channel_joined carries the request id + roster")
	var bob: Dictionary = (joined[2] as Array)[0]
	_check(bob["user_id"] == "u2" and bob["display_name"] == "Bob" and bob["role"] == Web.ROLE_MODERATOR and bob["priority"] == true, "participant dictionary uses native keys/enums")
	_check((joined[3] as Dictionary).has("ducking") and (joined[3] as Dictionary)["role"] == Web.ROLE_SPEAKER, "channel info defaults without a bridge")
	_check(client.get_joined_channels() == PackedStringArray(["c1"]), "joined channels tracked")

	client._dispatch({"type": "participantJoined", "channelId": "c1", "participant": {"userId": "u3", "displayName": "Cy", "ssrc": 6, "role": "speaker", "muted": true, "serverMuted": false, "speaking": false, "energy": 0.0, "priority": false}})
	_check(client.get_participants("c1").size() == 2 and _got["participant_joined"][1]["user_id"] == "u3", "participant_joined")
	client._dispatch({"type": "speaking", "channelId": "c1", "userId": "u3", "speaking": true})
	_check(_got["participant_speaking"][2] == true, "participant_speaking")
	var cy_speaking := false
	for p in client.get_participants("c1"):
		if p["user_id"] == "u3":
			cy_speaking = p["speaking"]
	_check(cy_speaking, "roster patched by speaking event")
	client._dispatch({"type": "participantUpdated", "channelId": "c1", "participant": {"userId": "u3", "displayName": "Cy", "ssrc": 6, "role": "speaker", "muted": false, "serverMuted": true, "speaking": true, "energy": 0.0, "priority": false}})
	_check(_got["participant_mute_changed"][2] == false and _got["participant_mute_changed"][3] == true, "participantUpdated → participant_mute_changed")
	client._dispatch({"type": "energy", "channelId": "c1", "levels": [{"userId": "u3", "energy": 0.7}]})
	_check(is_equal_approx((_got["channel_energy"][1] as Dictionary)["u3"], 0.7), "channel_energy levels")
	client._dispatch({"type": "duckingChanged", "channelId": "c1", "active": true, "config": {"gain": 0.25, "attackMs": 60, "releaseMs": 400, "holdMs": 250, "moderators": false}})
	_check(_got["ducking_changed"][1] == true and (_got["ducking_changed"][2] as Dictionary)["attack_ms"] == 60, "ducking_changed")
	client._dispatch({"type": "participantStreams", "streams": [{"mid": "1", "userId": "u3", "live": true}, {"mid": "2", "userId": null, "live": false}]})
	var streams: Array = _got["participant_streams_changed"][0]
	_check(streams.size() == 2 and streams[0]["user_id"] == "u3" and streams[1]["user_id"] == "", "participant_streams_changed")
	client._dispatch({"type": "e2eePeerKey", "userId": "u3", "fingerprint": "ab", "previousFingerprint": null})
	_check(_got["e2ee_peer_key"][1] == "ab" and _got["e2ee_peer_key"][2] == "", "e2ee_peer_key with null previous")

	client._dispatch({"type": "chatMessage", "message": {"id": "m1", "channelId": "c1", "fromUserId": "u3", "displayName": "Cy", "text": "hi", "metadata": {"k": 1}, "sentAt": "2026-01-02T03:04:05.100Z", "own": false, "system": false, "clientRef": "12", "offline": false, "cursor": "cur"}})
	var msg: Dictionary = _got["chat_message"][0]
	_check(msg["message_id"] == "m1" and msg["sender_id"] == "u3" and msg["sender_name"] == "Cy" and msg["sent_at_ms"] == 1767323045100 and msg["metadata_json"] == '{"k":1}' and msg["request_id"] == 12, "chat_message uses native keys")
	client._dispatch({"type": "transcript", "transcript": {"id": "t", "channelId": "c1", "userId": "u3", "text": "hello there", "language": "en", "startedAt": "2026-01-02T03:04:05Z", "durationMs": 900, "words": [{"word": "hello"}, {"word": "there"}], "original": {"text": "hola", "language": "es"}}})
	var tr: Dictionary = _got["transcript"][0]
	_check(tr["word_count"] == 2 and tr["original_text"] == "hola" and tr["started_at_ms"] == 1767323045000, "transcript uses native keys")
	client._dispatch({"type": "ttsStatus", "status": {"requestId": "srv", "clientRef": "9", "state": "playing", "durationMs": 500}})
	_check(_got["tts_status"][0]["state"] == Web.TTS_PLAYING and _got["tts_status"][0]["request_id"] == 9, "tts_status enum + client ref")
	client._dispatch({"type": "networkQuality", "quality": {"bars": 4, "rFactor": 80.5, "mos": 4.1, "rttMs": 30, "downlinkJitterMs": 2, "downlinkLossPercent": 0.5, "uplinkJitterMs": 1, "uplinkLossPercent": 0, "uplinkBitrateKbps": 32, "uplinkPacketsReceived": 100, "uplinkPacketsLost": 0}})
	_check(client.get_network_quality()["bars"] == 4 and _got["network_quality"][0]["downlink_loss_percent"] == 0.5, "network_quality snake_case")
	client._dispatch({"type": "remoteAudio", "playing": false, "reason": "autoplay"})
	_check(_got["remote_audio"][0] == false and _got["remote_audio"][1] == "autoplay", "remote_audio autoplay block")
	client._dispatch({"type": "overflow", "dropped": 3})
	_check(_got["events_dropped"][0] == 3, "events_dropped")
	client._dispatch({"type": "error", "error": {"message": "boom", "name": "Error"}})
	_check(_got["server_error"][0] == "CLIENT_ERROR" and _got["server_error"][1] == "boom", "client error → server_error(CLIENT_ERROR)")

	# token requests: no handler connected → answered from set_token (bridge call fails gracefully here)
	client._dispatch({"type": "tokenRequest", "requestId": 1, "kind": "refresh", "channelId": null})
	_check(_got["token_requested"][0] == 1 and _got["token_requested"][1] == "refresh" and _got["token_requested"][2] == "", "token_requested surfaces to the connected handler")

	# async results
	client._requests[21] = {"kind": "join", "channel_id": "c2"}
	client._joins["c2"] = 21
	client._dispatch({"type": "result", "rid": 21, "ok": false, "error": {"message": "no", "code": "CHANNEL_FULL"}})
	_check(_got["request_failed"][0] == 21 and _got["request_failed"][1] == "CHANNEL_FULL", "join failure → request_failed")
	_check(not client._joins.has("c2"), "failed join forgotten")
	client._requests[22] = {"kind": "moderate", "channel_id": "c1", "user_id": "u3", "action": Web.MODERATION_MUTE}
	client._dispatch({"type": "result", "rid": 22, "ok": true, "value": null})
	_check(_got["moderation_applied"][0] == 22 and _got["moderation_applied"][3] == Web.MODERATION_MUTE, "moderation_applied")
	client._requests[23] = {"kind": "history", "channel_id": "c1", "user_id": ""}
	client._dispatch({"type": "result", "rid": 23, "ok": true, "value": {"messages": [{"id": "m0", "channelId": "c1", "fromUserId": "u2", "displayName": "Bob", "text": "old", "sentAt": "2026-01-01T00:00:00Z", "own": false, "system": false, "offline": false, "cursor": "c0"}], "nextBefore": "c0"}})
	_check(_got["chat_history"][0] == 23 and (_got["chat_history"][3] as Array).size() == 1 and _got["chat_history"][4] == "c0", "chat_history page")
	client._dispatch({"type": "result", "rid": 99, "ok": true, "value": 1})
	_check(true, "unknown result id ignored")

	client._dispatch({"type": "channelLeft", "channelId": "c1"})
	_check(_got["channel_left"][0] == "c1" and client.get_joined_channels().is_empty(), "channel_left clears roster")
	client._dispatch({"type": "recovering", "attempt": 2, "delayMs": 800, "cause": "socket closed"})
	_check(_got["recovering"][0] == 2 and _got["recovering"][1] == 800, "recovering")
	client._dispatch({"type": "connectionState", "state": "reconnecting"})
	client._dispatch({"type": "recovered", "info": {"sessionId": "s1", "userId": "u1", "ssrc": 42, "resumed": true, "migrated": false, "endpoint": "wss://b/ws", "failover": [], "participantStreamCap": 16}})
	_check(_got["recovered"][0] == true and _got["recovered"][1] == false and client.get_endpoint() == "wss://b/ws", "recovered resumed=true, endpoint follows")
	client._dispatch({"type": "connectionState", "state": "media-connected"})
	client._dispatch({"type": "failedToRecover", "error": {"message": "gave up"}})
	client._dispatch({"type": "connectionState", "state": "failed"})
	_check(_got["failed_to_recover"][0] == "gave up" and _got["disconnected"][0] == "gave up" and client.get_connection_state() == Web.STATE_FAILED, "failed_to_recover + disconnected(reason)")

	_finish()

func _finish() -> void:
	if _failures == 0:
		print("web smoke: all checks passed")
		quit(0)
	else:
		printerr("web smoke: %d check(s) failed" % _failures)
		quit(1)
