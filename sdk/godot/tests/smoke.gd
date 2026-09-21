# Headless API smoke test for the AurixVoice GDExtension — no server needed.
#
#   godot --headless --path sdk/godot -s tests/smoke.gd
#
# Checks that the extension loads, the classes/enums/signals are registered, argument validation
# returns AURIX_INVALID_ARGUMENT instead of crashing, and audio/configuration calls are safe on a
# client that was never connected. Exits 0 on success, 1 on the first failed check.
extends SceneTree

var _failures := 0
var _elapsed := 0.0

# Watchdog: a script error inside _init would otherwise leave the headless process running.
func _process(delta: float) -> bool:
	_elapsed += delta
	if _elapsed > 20.0:
		printerr("smoke: watchdog timeout")
		quit(2)
	return false

func _check(cond: bool, what: String) -> void:
	if cond:
		print("  ok   ", what)
	else:
		_failures += 1
		printerr("  FAIL ", what)

func _init() -> void:
	print("AurixVoice smoke test")
	_check(ClassDB.class_exists("AurixVoiceClient"), "AurixVoiceClient registered")
	_check(ClassDB.class_exists("AurixParticipantPlayer"), "AurixParticipantPlayer registered")
	_check(ClassDB.class_exists("AurixRegions"), "AurixRegions registered")
	if _failures > 0:
		_finish()
		return

	var version: String = AurixVoiceClient.get_native_version()
	_check(version.length() > 0, "native version: %s" % version)

	var client := AurixVoiceClient.new()
	root.add_child(client)

	_check(client.get_connection_state() == AurixVoiceClient.STATE_DISCONNECTED, "initial state disconnected")
	_check(not client.is_client_created(), "no native client before connect")
	_check(client.get_session().is_empty(), "empty session before connect")
	_check(client.get_joined_channels().is_empty(), "no joined channels before connect")
	_check(client.get_endpoint() == "", "empty endpoint before connect")
	_check(client.get_failover_endpoints().is_empty(), "no failover endpoints before connect")
	_check(client.get_media_path() == AurixVoiceClient.MEDIA_NONE, "media path none before connect")

	# Argument validation must not touch the native client.
	_check(client.connect_to_server("", "tok") == AurixVoiceClient.RESULT_INVALID_ARGUMENT, "connect rejects empty url")
	_check(client.connect_to_server("ws://127.0.0.1:1", "") == AurixVoiceClient.RESULT_INVALID_ARGUMENT, "connect rejects empty token")
	_check(client.join_channel("00000000-0000-4000-8000-000000000001") == 0, "join without client → 0 (no request)")
	_check(client.leave_channel("00000000-0000-4000-8000-000000000001") == AurixVoiceClient.RESULT_NOT_CONNECTED, "leave without client → NOT_CONNECTED")
	_check(client.set_participant_volume("00000000-0000-4000-8000-000000000001", 1.0) == AurixVoiceClient.RESULT_NOT_CONNECTED, "volume without client → NOT_CONNECTED")
	_check(client.set_token("x") == AurixVoiceClient.RESULT_NOT_CONNECTED, "set_token without client → NOT_CONNECTED")
	_check(client.send_chat("00000000-0000-4000-8000-000000000001", "hi") == 0, "chat without client → 0")
	_check(client.get_participants("not-a-uuid").is_empty(), "participants of malformed id empty")
	_check(client.get_channel_info("00000000-0000-4000-8000-000000000001").is_empty(), "channel info empty")
	_check(client.user_for_ssrc(1) == "", "unknown ssrc → empty user")

	# Audio APIs are no-ops without a client and never crash.
	var frames := PackedVector2Array()
	frames.resize(480)
	client.push_capture(frames, 48000)
	client.push_capture_mono(PackedFloat32Array([0.0, 0.1, 0.2]), 48000)
	client.push_render(frames)
	_check(client.mix_output(480).size() == 480, "mix_output returns silence of requested size")
	_check(client.mix_output(0).is_empty(), "mix_output(0) empty")
	_check(client.pull_participant("00000000-0000-4000-8000-000000000001", 480).size() == 480, "pull_participant returns silence")
	_check(client.get_participant_streams().is_empty(), "no participant streams")
	_check(client.get_stats().is_empty(), "no stats before connect")
	_check(client.get_dsp_stats().is_empty(), "no dsp stats before connect")
	_check(client.get_audio_policy().is_empty(), "no audio policy before connect")
	_check(client.get_network_quality().is_empty(), "no network quality before connect")
	_check(not client.is_capturing(), "not capturing")
	_check(not client.is_muted() and not client.is_speaking(), "not muted / not speaking")
	_check(is_zero_approx(client.get_input_energy()), "zero input energy")
	client.set_muted(true)
	client.set_input_gain(2.0)
	client.set_output_volume(0.5)
	client.set_output_muted(true)
	client.set_vad(0.02, 5)
	client.reset_capture()
	_check(client.set_bitrate(24000) == AurixVoiceClient.RESULT_NOT_CONNECTED, "set_bitrate without client → NOT_CONNECTED")
	_check(client.set_voice_effects({"pitch_semitones": 3.0}) == AurixVoiceClient.RESULT_NOT_CONNECTED, "set_voice_effects without client → NOT_CONNECTED")
	_check(client.get_voice_effects().has("reverb_mix"), "voice effects dictionary")
	var monster: Dictionary = client.get_voice_preset(AurixVoiceClient.VOICE_PRESET_MONSTER)
	_check(monster.get("pitch_semitones", 0.0) < 0.0 and monster.get("reverb_mix", 0.0) > 0.0, "monster preset is pitched down with reverb")
	_check(client.get_voice_preset(AurixVoiceClient.VOICE_PRESET_HELIUM).get("formant_semitones", 0.0) > 0.0, "helium preset raises formants")
	_check(client.set_voice_preset(AurixVoiceClient.VOICE_PRESET_RADIO) == AurixVoiceClient.RESULT_NOT_CONNECTED, "set_voice_preset without client → NOT_CONNECTED")
	client.visemes_enabled = true
	_check(client.visemes_enabled, "visemes_enabled stored for connect_to_server()")
	_check(client.get_local_visemes().is_empty(), "no local visemes before connect")
	_check(client.get_participant_visemes("00000000-0000-4000-8000-000000000001").is_empty(), "no participant visemes before connect")
	_check(client.set_priority("00000000-0000-4000-8000-000000000001") == AurixVoiceClient.RESULT_NOT_CONNECTED, "set_priority without client → NOT_CONNECTED")
	_check(not client.is_ducking_active("00000000-0000-4000-8000-000000000001"), "ducking inactive before connect")
	_check(AurixVoiceClient.VISEME_OU == 8 and AurixVoiceClient.VISEME_SILENCE == 0, "viseme constants")

	# Configuration properties live on the node and are applied at connect_to_server().
	client.playback_mode = AurixVoiceClient.PLAYBACK_PER_PARTICIPANT
	_check(client.playback_mode == AurixVoiceClient.PLAYBACK_PER_PARTICIPANT, "playback mode stored")
	client.media_path_policy = AurixVoiceClient.MEDIA_PATH_TUNNEL_ONLY
	_check(client.media_path_policy == AurixVoiceClient.MEDIA_PATH_TUNNEL_ONLY, "media path policy stored")
	client.media_path_policy = AurixVoiceClient.MEDIA_PATH_QUIC_ONLY
	_check(client.media_path_policy == AurixVoiceClient.MEDIA_PATH_QUIC_ONLY, "quic-only media path policy stored")
	_check(client.quic, "quic preferred by default")
	client.quic = false
	_check(not client.quic, "quic switch stored")
	client.quic = true
	_check(not client.network_changed(), "network_changed without a client is false")
	_check(AurixVoiceClient.MEDIA_QUIC == 3, "media path quic constant")
	client.auto_reconnect = false
	client.reconnect_max_attempts = 3
	_check(not client.auto_reconnect and client.reconnect_max_attempts == 3, "reconnect settings stored")
	client.request_timeout_ms = 2500
	_check(client.request_timeout_ms == 2500, "request timeout stored")
	client.jitter_target_frames = 4
	_check(client.jitter_target_frames == 4, "jitter target stored")
	client.vad_gate = true
	_check(client.vad_gate, "vad gate stored")
	client.dsp_bypass = true
	_check(client.dsp_bypass, "dsp bypass stored")
	client.dsp_bypass = false
	client.playback_bus = &"Master"
	client.playback_buffer_seconds = 0.1
	_check(is_equal_approx(client.playback_buffer_seconds, 0.1), "playback buffer stored")
	var enc: Dictionary = client.get_encoder_settings()
	_check(enc.has("bitrate_bps") and enc.has("complexity") and enc.has("channels") and enc.has("max_bandwidth") and enc.has("signal") and enc.has("fec"), "encoder settings dictionary: %s" % enc)
	enc["bitrate_bps"] = 24000
	enc["channels"] = 2
	enc["signal"] = AurixVoiceClient.SIGNAL_MUSIC
	_check(client.set_encoder_settings(enc) == AurixVoiceClient.RESULT_OK, "set_encoder_settings ok")
	_check(int(client.get_encoder_settings()["bitrate_bps"]) == 24000, "encoder bitrate round-trips")
	_check(int(client.get_encoder_settings()["channels"]) == 2, "encoder channels round-trip")
	var dsp: Dictionary = client.get_dsp()
	_check(dsp.has("noise_suppression") and dsp.has("echo_cancellation") and dsp.has("agc"), "dsp dictionary: %s" % dsp)
	dsp["noise_suppression"] = AurixVoiceClient.NOISE_SUPPRESSION_OFF
	dsp["agc"] = false
	_check(client.set_dsp(dsp) == AurixVoiceClient.RESULT_OK, "set_dsp ok")
	_check(int(client.get_dsp()["noise_suppression"]) == AurixVoiceClient.NOISE_SUPPRESSION_OFF, "dsp round-trips")
	_check(client.get_dsp()["agc"] == false, "dsp agc round-trips")
	_check(enc.has("dred_duration_ms"), "encoder settings carry dred_duration_ms")
	_check(AurixVoiceClient.is_dred_supported(), "bundled libopus has DRED")
	enc["dred_duration_ms"] = 200
	_check(client.set_encoder_settings(enc) == AurixVoiceClient.RESULT_OK, "set dred duration ok")
	_check(int(client.get_encoder_settings()["dred_duration_ms"]) == 200, "dred duration round-trips")
	_check(client.get_loss_adaptation() == AurixVoiceClient.LOSS_ADAPTATION_AUTO, "default loss adaptation auto")
	_check(client.get_loss_profile() == AurixVoiceClient.LOSS_PROFILE_LOW, "default loss profile low")
	_check(client.set_loss_adaptation(AurixVoiceClient.LOSS_ADAPTATION_FIXED_HIGH) == AurixVoiceClient.RESULT_OK, "pin loss profile ok")
	_check(client.get_loss_adaptation() == AurixVoiceClient.LOSS_ADAPTATION_FIXED_HIGH, "loss adaptation round-trips")
	var dec: Dictionary = client.get_decoder_settings()
	_check(dec.has("complexity") and dec.has("osce_bwe"), "decoder settings dictionary: %s" % dec)
	dec["complexity"] = 7
	_check(client.set_decoder_settings(dec) == AurixVoiceClient.RESULT_OK, "set_decoder_settings ok")
	_check(int(client.get_decoder_settings()["complexity"]) == 7, "decoder complexity round-trips")
	_check(client.get_audio_codec() == AurixVoiceClient.CODEC_OPUS, "default codec opus")
	_check(client.get_downlink_mode() == AurixVoiceClient.DOWNLINK_STREAMS, "default downlink streams")

	# Malformed ids are rejected before the native client is involved (even when it exists).
	_check(client.update_transforms("00000000-0000-4000-8000-000000000001", {"not-a-uuid": Transform3D()}) == AurixVoiceClient.RESULT_NOT_CONNECTED, "update_transforms without client → NOT_CONNECTED")

	# Signals declared on the node.
	for sig in ["state_changed", "session_ready", "media_bound", "channel_joined", "channel_left",
			"participant_joined", "participant_left", "participant_speaking", "participant_mute_changed",
			"participant_priority_changed", "ducking_changed",
			"channel_energy", "local_speaking", "chat_message", "participant_typing", "transcript", "tts_status",
			"recovering", "recovered", "failed_to_recover", "network_quality", "media_path_changed",
			"downlink_mode_changed", "endpoint_changed", "chat_history", "chat_read_marker",
			"chat_inbox_synced", "translation_changed", "raw_event", "request_failed", "kicked",
			"recording", "audio_policy_changed", "audio_codec_changed", "disconnected", "server_error",
			"transmission_changed", "channel_focus_changed", "user_block_changed", "moderation_applied",
			"positions", "rejoin_failed", "bitrate_changed", "chat_read_markers", "loss_profile_changed"]:
		_check(client.has_signal(sig), "signal %s" % sig)

	# Region discovery helper.
	var regions := AurixRegions.new()
	var url: String = AurixRegions.discovery_url("https://voice.example.com", "eu_west")
	_check(url.begins_with("https://voice.example.com/v1/me/regions"), "discovery url: %s" % url)
	_check(not regions.parse("not json"), "regions parse rejects garbage")
	_check(regions.parse('{"regions":[{"region":"eu_west","node_id":"00000000-0000-4000-8000-0000000000e1","ws_url":"wss://eu.example.com/ws","probe_url":"https://eu.example.com/health","location":null,"distance_km":null,"nodes":1,"load_factor":0.1},{"region":"us_east","node_id":"00000000-0000-4000-8000-0000000000a1","ws_url":"wss://us.example.com/ws","probe_url":null,"location":null,"distance_km":null,"nodes":1,"load_factor":0.5}]}'), "regions parse ok")
	_check(regions.size() == 2, "two regions")
	_check(regions.set_rtt(0, 80.0) == AurixVoiceClient.RESULT_OK and regions.set_rtt(1, 20.0) == AurixVoiceClient.RESULT_OK, "set_rtt")
	_check(regions.rank() == AurixVoiceClient.RESULT_OK, "rank")
	_check(regions.get_endpoint(0).has("ws_url") and regions.get_all().size() == 2, "endpoint dictionaries")
	_check(regions.best_ws_url() == "wss://us.example.com/ws", "lowest RTT ranks first: %s" % regions.best_ws_url())

	# Per-participant player without a client is inert.
	var player := AurixParticipantPlayer.new()
	player.user_id = "00000000-0000-4000-8000-000000000002"
	client.add_child(player)
	_check(player.get_client() == client, "player finds ancestor client")
	_check(not player.is_active(), "player inactive")

	# disconnect on a never-connected node is a no-op, and freeing the node is clean.
	client.disconnect_from_server()
	root.remove_child(client)
	client.free()
	_finish()

func _finish() -> void:
	if _failures == 0:
		print("smoke: all checks passed")
		quit(0)
	else:
		printerr("smoke: %d check(s) failed" % _failures)
		quit(1)
