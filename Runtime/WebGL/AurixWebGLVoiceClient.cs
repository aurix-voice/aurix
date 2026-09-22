using System;
using System.Collections.Generic;
using System.Threading;
using System.Threading.Tasks;
using Aurix.Protocol;

namespace Aurix.WebGL
{
    /// <summary>
    /// Voice client for Unity WebGL players: the same <see cref="IAurixVoiceClient"/> surface as
    /// <see cref="AurixVoiceClient"/>, implemented on top of the browser's Web SDK (WebSocket control
    /// plane, WebRTC media, the browser's Opus/AEC/NS/AGC and audio output). Nothing here touches sockets:
    /// every call is a JSON round trip through <see cref="IWebGLBridge"/> and every browser event is
    /// pulled by <see cref="Update"/>, so all events fire on the thread that calls it (the main thread).
    /// <para>
    /// Media is played by the browser (a hidden <c>&lt;audio&gt;</c> element), not by Unity: remote voices
    /// do not pass through the AudioListener, the mixer or spatializer plugins, and there are no PCM
    /// frames to pull. Browsers only start audio after a user gesture — call <see cref="ResumeAudioAsync"/>
    /// from a click handler when <see cref="OnRemoteAudio"/> reports <c>playing == false</c>.
    /// </para>
    /// </summary>
    public sealed class AurixWebGLVoiceClient : IAurixVoiceClient
    {
        public const string SdkVersion = AurixVoiceClient.SdkVersion;
        /// <summary>Default location of the standalone Web SDK bundle, relative to the player's <c>index.html</c>.</summary>
        public const string DefaultSdkPath = "StreamingAssets/aurix-web-sdk.js";

        private readonly IWebGLBridge _bridge;
        private readonly string _apiUrl;
        private readonly string _wsUrl;
        private readonly string _token;
        private readonly Dictionary<Guid, Dictionary<Guid, Participant>> _channels = new Dictionary<Guid, Dictionary<Guid, Participant>>();
        private readonly Dictionary<int, PendingCall> _pending = new Dictionary<int, PendingCall>();
        private readonly Dictionary<Guid, TaskCompletionSource<TtsStatus>> _speech = new Dictionary<Guid, TaskCompletionSource<TtsStatus>>();
        private readonly Queue<Action> _mainThreadQueue = new Queue<Action>();
        private readonly List<TaskCompletionSource<bool>> _sdkWaiters = new List<TaskCompletionSource<bool>>();
        private int _handle;
        private int _nextRid;
        private bool _disposed;
        private bool _muted;
        private bool _wantTranscripts;
        private TranslationPrefs _translation = new TranslationPrefs();
        private bool _activeServerNoiseSuppression;
        private TransmissionMode _transmission = TransmissionMode.All;
        private Guid? _focus;
        private Guid _userId;
        private NetworkQuality? _quality;
        private string _closeReason;

        private sealed class PendingCall
        {
            public Action<Dictionary<string, object>> Complete;
            public Action<Exception> Fail;
            public DateTime Deadline;
        }

        /// <param name="apiUrl">REST base URL (<c>https://host:8080</c>), used for TURN credentials and history.</param>
        /// <param name="wsUrl">Control WebSocket URL (<c>wss://host:8081/ws</c>).</param>
        /// <param name="token">Per-user JWT from your game backend.</param>
        /// <param name="bridge">JavaScript boundary; null = the <c>AurixWebGL.jslib</c> plugin.</param>
        public AurixWebGLVoiceClient(string apiUrl, string wsUrl, string token, IWebGLBridge bridge = null)
        {
            _apiUrl = apiUrl ?? throw new ArgumentNullException(nameof(apiUrl));
            _wsUrl = wsUrl ?? throw new ArgumentNullException(nameof(wsUrl));
            _token = token ?? throw new ArgumentNullException(nameof(token));
            _bridge = bridge ?? NativeWebGLBridge.Instance;
            Endpoint = wsUrl;
        }

        /// <summary>Browser-side settings; applied when the browser client is created by the first <see cref="ConnectAsync"/>.</summary>
        public WebGLClientOptions Options { get; set; } = new WebGLClientOptions();
        /// <summary>URL of <c>aurix-web-sdk.js</c> (<see cref="DefaultSdkPath"/> by default). Ignored when the page already loaded the SDK.</summary>
        public string SdkUrl { get; set; } = DefaultSdkPath;
        /// <summary>Called when the server reports the access token expired; return a fresh JWT (or throw to give up).</summary>
        public Func<CancellationToken, Task<string>> TokenRefresher { get; set; }
        /// <summary>Called when a channel needs a join token this client does not have; return one for the channel.</summary>
        public Func<Guid, CancellationToken, Task<string>> JoinTokenProvider { get; set; }
        /// <summary>Time a request may wait for the browser before faulting with <see cref="TimeoutException"/>.</summary>
        public TimeSpan RequestTimeout { get; set; } = TimeSpan.FromSeconds(15);

        public VoiceConnectionState State { get; private set; } = VoiceConnectionState.Disconnected;
        public SessionInfo Session { get; private set; }
        /// <summary>This user's id as reported by the server in <c>SessionInitAck</c> (<see cref="Guid.Empty"/> before connect).</summary>
        public Guid UserId => _userId;
        public bool IsMuted => _muted;
        public string Endpoint { get; private set; }
        public IReadOnlyList<string> FailoverEndpoints { get; private set; } = Array.Empty<string>();
        /// <summary>Whether the browser client reconnects after a connection loss; read when the browser client is created (first connect).</summary>
        public bool AutoReconnect
        {
            get => Options.AutoReconnect;
            set => Options.AutoReconnect = value;
        }
        public IReadOnlyCollection<Guid> JoinedChannels { get { lock (_channels) return new List<Guid>(_channels.Keys); } }
        public TransmissionMode Transmission { get { lock (_channels) return _transmission; } }
        public Guid? FocusChannel { get { lock (_channels) return _focus; } }
        public bool TranscriptsEnabled { get { lock (_channels) return _wantTranscripts; } }
        public bool ServerNoiseSuppression { get { lock (_channels) return _activeServerNoiseSuppression; } }
        public TranslationPrefs TranslationPrefs { get { lock (_channels) return _translation.Clone(); } }
        public NetworkQuality? LastNetworkQuality => _quality;
        /// <summary>Whether the browser client exists (created by the first connect, destroyed by <see cref="Dispose"/>).</summary>
        public bool IsCreated => _handle > 0;
        /// <summary>Bridge handle of the browser client (0 before the first connect); for diagnostics.</summary>
        public int Handle => _handle;

        public event Action<VoiceConnectionState> OnStateChanged;
        public event Action<SessionInfo> OnSessionReady;
        public event Action<Guid, IReadOnlyList<Participant>> OnChannelJoined;
        public event Action<Guid> OnChannelLeft;
        public event Action<Guid, Participant> OnParticipantJoined;
        public event Action<Guid, Participant> OnParticipantLeft;
        public event Action<Guid, Participant> OnParticipantUpdated;
        public event Action<Guid, Participant, bool> OnSpeaking;
        public event Action<Guid, IReadOnlyList<ParticipantEnergy>> OnChannelEnergy;
        public event Action<Guid, IReadOnlyList<UserPosition>> OnPositions;
        public event Action<RecordingNotice> OnRecording;
        public event Action<BitrateCommand> OnBitrateCommand;
        public event Action<NetworkQuality> OnNetworkQuality;
        /// <summary>Periodic browser media statistics (every <see cref="WebGLClientOptions.QualityReportInterval"/>).</summary>
        public event Action<WebGLStats> OnStats;
        public event Action<Guid, string> OnKicked;
        public event Action<ReceiverPreferences> OnReceiverPreferences;
        public event Action<Guid, bool> OnUserBlockChanged;
        public event Action<Guid, Guid, bool> OnParticipantPriorityChanged;
        public event Action<Guid, Guid, ChannelRole, bool> OnParticipantRoleChanged;
        /// <summary>Game-audio ducking hook: a priority speaker started / stopped ducking a channel in the browser (transitions only).</summary>
        public event Action<Guid, bool, DuckingConfig> OnDuckingChanged;
        /// <summary>Only with <see cref="WebGLClientOptions.VisemeEvents"/>: a participant's mouth state, 50/s while it speaks.</summary>
        public event Action<Guid, Audio.VisemeFrame> OnParticipantVisemes;
        /// <summary>Only with <see cref="WebGLClientOptions.VisemeEvents"/>: the local microphone's mouth state, 50/s.</summary>
        public event Action<Audio.VisemeFrame> OnLocalVisemes;
        public event Action<TransmissionMode> OnTransmissionChanged;
        public event Action<Guid?> OnChannelFocusChanged;
        /// <summary>The node changed which participant is forwarded on which dedicated WebRTC track (see <see cref="GetParticipantStreamsAsync"/>).</summary>
        public event Action<IReadOnlyList<WebGLParticipantStream>> OnParticipantStreams;
        /// <summary>A peer in a shared encrypted channel announced its E2EE identity (fingerprint, previous fingerprint if it changed).</summary>
        public event Action<Guid, string, string> OnE2eePeerKey;
        /// <summary>Whether this client holds a sender key of the peer (true: its encrypted voice is audible).</summary>
        public event Action<Guid, bool> OnE2eePeerDecryptable;
        /// <summary>This client's sender key rotated (new generation) — on join/leave of members, or on demand.</summary>
        public event Action<int> OnE2eeKeyRotated;
        public event Action<ChatMessage> OnChatMessage;
        public event Action<ChatReadMarker> OnChatReadMarker;
        public event Action<int, bool, bool> OnChatInboxSynced;
        public event Action<ChatMessage> OnChatMessageUpdated;
        public event Action<ChatReactionChange> OnChatReactionChanged;
        public event Action<Guid, Guid, bool> OnParticipantTyping;
        public event Action<Transcript> OnTranscript;
        public event Action<TranslationPrefs> OnTranslationChanged;
        public event Action<bool> OnServerNoiseSuppressionChanged;
        public event Action<TtsStatus> OnTtsStatus;
        public event Action<string, string> OnServerError;
        public event Action<string> OnDisconnected;
        public event Action<int, TimeSpan, string> OnRecovering;
        public event Action<SessionInfo> OnRecovered;
        public event Action<string> OnEndpointChanged;
        public event Action<Exception> OnFailedToRecover;
        public event Action<string> OnSessionClosed;
        /// <summary>Local voice activity from the browser meter (<see cref="WebGLClientOptions.LocalVoiceActivity"/>).</summary>
        public event Action<bool> OnLocalSpeaking;
        /// <summary>
        /// Remote audio playback state: <c>(false, reason)</c> when the browser refused to start the hidden
        /// audio element (autoplay policy) — call <see cref="ResumeAudioAsync"/> from a user gesture.
        /// </summary>
        public event Action<bool, string> OnRemoteAudio;
        /// <summary>The browser's microphone/speaker list changed (a headset was plugged in, …).</summary>
        public event Action<WebGLAudioDevices> OnDevicesChanged;
        /// <summary>Errors the browser client raised outside a request (media renegotiation, transport).</summary>
        public event Action<Exception> OnError;
        /// <summary>Raw server control messages; only with <see cref="WebGLClientOptions.RawMessages"/>.</summary>
        public event Action<ControlMessage> OnControlMessage;
        /// <summary>Events the bridge dropped because <see cref="Update"/> was not called for too long.</summary>
        public event Action<int> OnEventsDropped;

        // ---- lifecycle ----------------------------------------------------------------------------

        public async Task<SessionInfo> ConnectAsync(CancellationToken ct = default)
        {
            ThrowIfDisposed();
            if (State != VoiceConnectionState.Disconnected && State != VoiceConnectionState.Failed)
                throw new InvalidOperationException($"cannot connect in state {State}");
            await EnsureSdkAsync(ct);
            EnsureCreated();
            _closeReason = null;
            return await CallAsync("connect", null, o =>
            {
                var session = BridgeJson.Session(o);
                if (session == null) throw new WebGLBridgeException("connect returned no session");
                if (Session == null || Session.SessionId != session.SessionId) ApplySession(o, session);
                return session;
            }, ct, timeout: false);
        }

        public Task DisconnectAsync(string reason = "client disconnect")
        {
            if (_handle > 0)
            {
                _closeReason = reason;
                Invoke("disconnect", new Dictionary<string, object> { { "reason", reason } });
                Drain();
            }
            return Task.CompletedTask;
        }

        public void ReconnectNow()
        {
            if (_handle > 0) Invoke("reconnectNow", null);
        }

        /// <summary>
        /// Start (or retry) loading the Web SDK bundle from <see cref="SdkUrl"/>. <see cref="ConnectAsync"/>
        /// does this on demand; call it early (e.g. in <c>Start</c>) to hide the download latency.
        /// </summary>
        public void PreloadSdk() => _bridge.LoadSdk(SdkUrl);

        /// <summary>Completes when <c>window.AurixWebSdk</c> is available; faults when the bundle failed to load.</summary>
        public Task EnsureSdkAsync(CancellationToken ct = default)
        {
            ThrowIfDisposed();
            var status = _bridge.SdkStatus;
            if (status == WebGLSdkStatus.Ready) return Task.CompletedTask;
            if (status == WebGLSdkStatus.NotLoaded || status == WebGLSdkStatus.Failed) _bridge.LoadSdk(SdkUrl);
            if (_bridge.SdkStatus == WebGLSdkStatus.Ready) return Task.CompletedTask;
            var tcs = new TaskCompletionSource<bool>();
            lock (_sdkWaiters) _sdkWaiters.Add(tcs);
            if (ct.CanBeCanceled) ct.Register(() => tcs.TrySetCanceled(ct));
            return tcs.Task;
        }

        private void EnsureCreated()
        {
            if (_handle > 0) return;
            var options = Options ?? new WebGLClientOptions();
            var json = MiniJson.Serialize(options.ToBridge(_apiUrl, _wsUrl, _token, TokenRefresher != null, JoinTokenProvider != null));
            var handle = _bridge.Create(json);
            if (handle <= 0) throw new WebGLBridgeException("the browser bridge refused to create a client (is aurix-web-sdk.js loaded?)");
            _handle = handle;
        }

        public void Dispose()
        {
            if (_disposed) return;
            _disposed = true;
            if (_handle > 0)
            {
                try { _bridge.Destroy(_handle); } catch (PlatformNotSupportedException) { }
                _handle = 0;
            }
            var disposed = new ObjectDisposedException(nameof(AurixWebGLVoiceClient));
            lock (_pending)
            {
                foreach (var p in _pending.Values) p.Fail(disposed);
                _pending.Clear();
            }
            lock (_speech)
            {
                foreach (var s in _speech.Values) s.TrySetException(disposed);
                _speech.Clear();
            }
            lock (_sdkWaiters)
            {
                foreach (var w in _sdkWaiters) w.TrySetException(disposed);
                _sdkWaiters.Clear();
            }
        }

        private void ThrowIfDisposed()
        {
            if (_disposed) throw new ObjectDisposedException(nameof(AurixWebGLVoiceClient));
        }

        // ---- channels -----------------------------------------------------------------------------

        public Task<IReadOnlyList<Participant>> JoinChannelAsync(Guid channelId, CancellationToken ct = default) =>
            JoinChannelAsync(channelId, null, ct);

        public Task<IReadOnlyList<Participant>> JoinChannelAsync(Guid channelId, string joinToken, CancellationToken ct = default)
        {
            var args = new Dictionary<string, object> { { "channelId", channelId } };
            if (joinToken != null) args["joinToken"] = joinToken;
            return CallAsync<IReadOnlyList<Participant>>("joinChannel", args, o => BridgeJson.Participants(BridgeJson.Arr(o, "value")), ct, timeout: false, rawValue: true);
        }

        public Task LeaveChannelAsync(Guid channelId, CancellationToken ct = default) =>
            Sync("leaveChannel", new Dictionary<string, object> { { "channelId", channelId } });

        public Task ModerateAsync(Guid channelId, Guid userId, ModerationAction action, string token, string reason = null, CancellationToken ct = default)
        {
            if (token == null) throw new ArgumentNullException(nameof(token));
            var args = new Dictionary<string, object>
            {
                { "channelId", channelId }, { "userId", userId }, { "action", BridgeJson.Moderation(action) }, { "token", token },
            };
            if (reason != null) args["reason"] = reason;
            return CallAsync("moderate", args, o => true, ct);
        }

        public IReadOnlyList<Participant> GetParticipants(Guid channelId)
        {
            lock (_channels)
                return _channels.TryGetValue(channelId, out var m) ? new List<Participant>(m.Values) : new List<Participant>();
        }

        public Participant FindByUser(Guid userId)
        {
            lock (_channels)
                foreach (var members in _channels.Values)
                    if (members.TryGetValue(userId, out var p)) return p;
            return null;
        }

        public ChannelInfo? GetChannelInfo(Guid channelId) =>
            _handle > 0 ? BridgeJson.ChannelInfo(MiniJson.AsObject(Value("channelInfo", Channel(channelId)))) : null;

        public ChannelScope? GetChannelScope(Guid channelId) =>
            _handle > 0 ? BridgeJson.ChannelScope(MiniJson.AsObject(Value("channelScope", Channel(channelId)))) : null;

        public bool CanSpeakIn(Guid channelId) => _handle > 0 && Value("canSpeakIn", Channel(channelId)) is bool b && b;

        public bool IsWaitingToSpeak(Guid channelId) => _handle > 0 && Value("isWaitingToSpeak", Channel(channelId)) is bool b && b;

        public bool IsChannelTranscribed(Guid channelId) => _handle > 0 && Value("isChannelTranscribed", Channel(channelId)) is bool b && b;

        public bool IsChannelMonitored(Guid channelId) => _handle > 0 && Value("isChannelMonitored", Channel(channelId)) is bool b && b;

        public Task SetTranscriptsAsync(bool enabled, CancellationToken ct = default)
        {
            lock (_channels) _wantTranscripts = enabled;
            return Sync("setTranscripts", new Dictionary<string, object> { { "enabled", enabled } });
        }

        public Task SetServerNoiseSuppressionAsync(bool enabled, CancellationToken ct = default) =>
            Sync("setServerNoiseSuppression", new Dictionary<string, object> { { "enabled", enabled } });

        public Task SetTranslationAsync(string language, string spokenLanguage = null, bool speech = false, CancellationToken ct = default)
        {
            var target = Protocol.TranslationPrefs.NormalizeTag(language);
            var prefs = new TranslationPrefs
            {
                Language = target,
                SpokenLanguage = Protocol.TranslationPrefs.NormalizeTag(spokenLanguage),
                Speech = speech && target != null,
            };
            lock (_channels) _translation = prefs;
            return Sync("setTranslation", new Dictionary<string, object>
            {
                { "language", prefs.Language },
                { "spokenLanguage", prefs.SpokenLanguage },
                { "speech", prefs.Speech },
            });
        }

        private static Dictionary<string, object> Channel(Guid channelId) => new Dictionary<string, object> { { "channelId", channelId } };

        // ---- receiver controls --------------------------------------------------------------------

        public void SetMuted(bool muted)
        {
            _muted = muted;
            if (_handle > 0) Invoke("setMuted", new Dictionary<string, object> { { "muted", muted } });
        }

        public Task SetParticipantMutedAsync(Guid userId, bool muted, Guid? channelId = null, CancellationToken ct = default)
        {
            var args = new Dictionary<string, object> { { "userId", userId }, { "muted", muted } };
            if (channelId.HasValue) args["channelId"] = channelId.Value;
            return Sync("setParticipantMuted", args);
        }

        public bool IsParticipantMuted(Guid userId, Guid? channelId = null)
        {
            if (_handle <= 0) return false;
            var args = new Dictionary<string, object> { { "userId", userId } };
            if (channelId.HasValue) args["channelId"] = channelId.Value;
            return Value("isParticipantMuted", args) is bool b && b;
        }

        public Task SetParticipantVolumeAsync(Guid userId, float volume, CancellationToken ct = default) =>
            Sync("setParticipantVolume", new Dictionary<string, object> { { "userId", userId }, { "volume", (double)volume } });

        public float GetParticipantVolume(Guid userId) =>
            _handle > 0 && Value("getParticipantVolume", new Dictionary<string, object> { { "userId", userId } }) is double d ? (float)d : 1f;

        public Task SetUserBlockedAsync(Guid userId, bool blocked, CancellationToken ct = default) =>
            Sync("setUserBlocked", new Dictionary<string, object> { { "userId", userId }, { "blocked", blocked } });

        public bool IsUserBlocked(Guid userId) =>
            _handle > 0 && Value("isUserBlocked", new Dictionary<string, object> { { "userId", userId } }) is bool b && b;

        public Task SetTransmissionAsync(TransmissionMode mode, CancellationToken ct = default)
        {
            lock (_channels) _transmission = mode;
            return Sync("setTransmission", new Dictionary<string, object> { { "mode", BridgeJson.TransmissionToBridge(mode) } });
        }

        public Task TransmitToChannelAsync(Guid channelId, CancellationToken ct = default) =>
            SetTransmissionAsync(TransmissionMode.Single(channelId), ct);

        public bool TransmitsTo(Guid channelId) => Transmission.Allows(channelId);

        public Task SetChannelFocusAsync(Guid? channelId, CancellationToken ct = default)
        {
            lock (_channels) _focus = channelId;
            var args = new Dictionary<string, object>();
            if (channelId.HasValue) args["channelId"] = channelId.Value;
            return Sync("setChannelFocus", args);
        }

        // ---- priority speaker + ducking ---------------------------------------------------------

        public Task SetPriorityAsync(Guid channelId, Guid? userId, bool priority, CancellationToken ct = default)
        {
            var args = new Dictionary<string, object> { { "channelId", channelId }, { "priority", priority } };
            if (userId.HasValue) args["userId"] = userId.Value;
            return Sync("setPriority", args);
        }

        public bool IsPriority(Guid channelId) => _handle > 0 && Value("isPriority", Channel(channelId)) is bool b && b;

        /// <summary>The ducking envelope of a joined channel, null when the channel has none (or is not joined).</summary>
        public DuckingConfig? GetChannelDucking(Guid channelId) =>
            _handle > 0 ? BridgeJson.Ducking(MiniJson.AsObject(Value("channelDucking", Channel(channelId)))) : null;

        /// <summary>Whether a priority speaker is ducking the channel in the browser right now (also reported by <see cref="OnDuckingChanged"/>).</summary>
        public bool IsDuckingActive(Guid channelId) => _handle > 0 && Value("isDuckingActive", Channel(channelId)) is bool b && b;

        // ---- lip-sync + voice effects -----------------------------------------------------------

        /// <summary>The browser can run the lip-sync AudioWorklet (false before the SDK is created).</summary>
        public bool SupportsVisemes => _handle > 0 && Value("supportsVisemes", null) is bool b && b;

        public bool VisemesEnabled => _handle > 0 && Value("visemesEnabled", null) is bool b && b;

        /// <summary>Turn browser-side lip-sync analysis on/off (every heard participant + the microphone); the browser needs a user gesture first.</summary>
        public Task SetVisemesAsync(bool enabled, CancellationToken ct = default) =>
            CallAsync("setVisemes", new Dictionary<string, object> { { "enabled", enabled } }, o => true, ct);

        /// <summary>Latest mouth state of a heard participant (its dedicated track, see <see cref="OnParticipantStreams"/>), null when unknown / off.</summary>
        public Audio.VisemeFrame? GetParticipantVisemes(Guid userId) =>
            _handle > 0 ? BridgeJson.Visemes(MiniJson.AsObject(Value("participantVisemes", new Dictionary<string, object> { { "userId", userId } }))) : null;

        /// <summary>Latest mouth state of the local microphone (after effects), null when off.</summary>
        public Audio.VisemeFrame? GetLocalVisemes() =>
            _handle > 0 ? BridgeJson.Visemes(MiniJson.AsObject(Value("localVisemes", null))) : null;

        /// <summary>The browser can run the voice-effects AudioWorklet (false before the SDK is created).</summary>
        public bool SupportsVoiceEffects => _handle > 0 && Value("supportsVoiceEffects", null) is bool b && b;

        /// <summary>The active (sanitised) microphone effect chain, bypass when none or before connect.</summary>
        public Audio.VoiceEffectParams VoiceEffects =>
            _handle > 0 ? BridgeJson.VoiceEffects(MiniJson.AsObject(Value("voiceEffects", null))) : Audio.VoiceEffectParams.Bypass;

        /// <summary>Apply a microphone effect chain in the browser (<see cref="Audio.VoiceEffectParams.Bypass"/> to switch off); presets via <see cref="Audio.VoiceEffectParams.Preset"/>.</summary>
        public Task SetVoiceEffectsAsync(Audio.VoiceEffectParams effects, CancellationToken ct = default)
        {
            var sanitized = effects.Sanitized();
            var args = new Dictionary<string, object> { { "effects", sanitized.IsBypass ? null : BridgeJson.VoiceEffectsToBridge(sanitized) } };
            return CallAsync("setVoiceEffects", args, o => true, ct);
        }

        public Task SetVoiceEffectsAsync(Audio.VoiceEffectPreset preset, CancellationToken ct = default) =>
            SetVoiceEffectsAsync(Audio.VoiceEffectParams.Preset(preset), ct);

        // ---- per-participant tracks ---------------------------------------------------------------

        /// <summary>
        /// Participants that must keep a dedicated WebRTC track while they are audible (at most
        /// <see cref="GetParticipantStreamCapAsync"/>); the rest of the slots follow who is speaking.
        /// Unpinned speakers without a slot stay audible through the mixed track. Replayed after reconnects.
        /// </summary>
        public Task SetPinnedParticipantsAsync(IReadOnlyList<Guid> userIds, CancellationToken ct = default)
        {
            var ids = new List<object>();
            if (userIds != null) foreach (var id in userIds) ids.Add(id);
            return Sync("setPinnedParticipants", new Dictionary<string, object> { { "userIds", ids } });
        }

        /// <summary>Dedicated tracks the node allows per browser session (0: mixed only / older node).</summary>
        public Task<int> GetParticipantStreamCapAsync(CancellationToken ct = default) =>
            CallAsync("participantStreamCap", null, o => (int)MiniJson.GetNumber(o, "value"), ct, rawValue: true);

        /// <summary>Current track → participant layout; also delivered through <see cref="OnParticipantStreams"/>.</summary>
        public Task<IReadOnlyList<WebGLParticipantStream>> GetParticipantStreamsAsync(CancellationToken ct = default) =>
            CallAsync("participantStreams", null, o => BridgeJson.ParticipantStreams(BridgeJson.Arr(o, "value")), ct, rawValue: true);

        /// <summary>True while <paramref name="userId"/> plays through the browser's HRTF/equal-power panner (positional channel, known positions).</summary>
        public bool IsParticipantSpatialized(Guid userId) =>
            _handle > 0 && Value("isParticipantSpatialized", new Dictionary<string, object> { { "userId", userId } }) is bool b && b;

        // ---- end-to-end encryption ----------------------------------------------------------------

        /// <summary>True when the browser can join encrypted channels (WebCrypto plus an encoded-frame API); false before connect.</summary>
        public bool E2eeAvailable => _handle > 0 && Value("e2eeAvailable", null) is bool b && b;

        /// <summary><c>"script"</c> (RTCRtpScriptTransform) or <c>"streams"</c> (createEncodedStreams); null when unavailable.</summary>
        public string E2eeTransformApi => _handle > 0 ? Value("e2eeTransformApi", null) as string : null;

        /// <summary>Hex SHA-256 of this client's E2EE identity key — show it so peers can verify it out of band.</summary>
        public string E2eeFingerprint => _handle > 0 ? Value("e2eeFingerprint", null) as string : null;

        /// <summary>Generation of this client's current sender key; null without E2EE.</summary>
        public int? E2eeGeneration => _handle > 0 && Value("e2eeGeneration", null) is double g ? (int)g : (int?)null;

        /// <summary>The 32-byte identity secret to store and pass as <see cref="WebGLClientOptions.E2eeIdentity"/> next time; null without E2EE.</summary>
        public byte[] ExportE2eeIdentity()
        {
            var b64 = _handle > 0 ? Value("e2eeIdentitySecret", null) as string : null;
            return b64 == null ? null : Convert.FromBase64String(b64);
        }

        /// <summary>Fingerprint of a peer's identity key once it announced itself in a shared encrypted channel.</summary>
        public string E2eePeerFingerprint(Guid userId) =>
            _handle > 0 ? Value("e2eePeerFingerprint", new Dictionary<string, object> { { "userId", userId } }) as string : null;

        /// <summary>Whether this client holds a sender key of <paramref name="userId"/> (its encrypted frames are audible).</summary>
        public bool IsE2eePeerDecryptable(Guid userId) =>
            _handle > 0 && Value("isE2eePeerDecryptable", new Dictionary<string, object> { { "userId", userId } }) is bool b && b;

        /// <summary>Peers whose encrypted frames this client can decrypt.</summary>
        public IReadOnlyList<Guid> GetE2eeDecryptablePeers()
        {
            var ids = new List<Guid>();
            if (_handle <= 0) return ids;
            foreach (var s in BridgeJson.Strings(Invoke("e2eeDecryptablePeers", null), "value"))
                if (Guid.TryParse(s, out var id)) ids.Add(id);
            return ids;
        }

        /// <summary>The node flagged this joined channel as end-to-end encrypted.</summary>
        public bool IsChannelEncrypted(Guid channelId) =>
            _handle > 0 && Value("isChannelEncrypted", new Dictionary<string, object> { { "channelId", channelId } }) is bool b && b;

        /// <summary>Fresh counters of the encrypted-frame path.</summary>
        public Task<WebGLE2eeStats> GetE2eeStatsAsync(CancellationToken ct = default) =>
            CallAsync("refreshE2eeStats", null, o => BridgeJson.E2eeStats(o), ct);

        /// <summary>Rotates this client's sender key now (normally automatic on join/leave); the new generation, or null without E2EE.</summary>
        public Task<int?> RotateE2eeKeyAsync(CancellationToken ct = default) =>
            CallAsync("rotateE2eeKey", null, o => o.TryGetValue("value", out var v) && v is double g ? (int)g : (int?)null, ct, rawValue: true);

        // ---- chat ---------------------------------------------------------------------------------

        public Task<ChatMessage> SendMessageAsync(Guid channelId, string text, object metadata = null, string clientRef = null, CancellationToken ct = default) =>
            CallAsync("sendMessage", ChatArgs(new Dictionary<string, object> { { "channelId", channelId }, { "text", text } }, metadata, clientRef),
                o => BridgeJson.Message(o), ct);

        public Task<ChatMessage> SendDirectMessageAsync(Guid userId, string text, object metadata = null, string clientRef = null, CancellationToken ct = default) =>
            CallAsync("sendDirectMessage", ChatArgs(new Dictionary<string, object> { { "userId", userId }, { "text", text } }, metadata, clientRef),
                o => BridgeJson.Message(o), ct);

        private static Dictionary<string, object> ChatArgs(Dictionary<string, object> args, object metadata, string clientRef)
        {
            if (metadata != null) args["metadata"] = metadata;
            if (clientRef != null) args["clientRef"] = clientRef;
            return args;
        }

        public Task<ChatHistoryPage> HistoryAsync(Guid channelId, string before = null, string after = null, int? limit = null, CancellationToken ct = default) =>
            CallAsync("history", HistoryArgs(Channel(channelId), before, after, limit), o => BridgeJson.History(o, channelId, null), ct);

        public Task<ChatHistoryPage> DirectHistoryAsync(Guid userId, string before = null, string after = null, int? limit = null, CancellationToken ct = default) =>
            CallAsync("history", HistoryArgs(new Dictionary<string, object> { { "userId", userId } }, before, after, limit), o => BridgeJson.History(o, null, userId), ct);

        private static Dictionary<string, object> HistoryArgs(Dictionary<string, object> args, string before, string after, int? limit)
        {
            if (before != null) args["before"] = before;
            if (after != null) args["after"] = after;
            if (limit.HasValue) args["limit"] = limit.Value;
            return args;
        }

        public Task<ChatMessage> EditMessageAsync(Guid messageId, string text, object metadata = null, CancellationToken ct = default) =>
            CallAsync("editMessage", ChatArgs(new Dictionary<string, object> { { "messageId", messageId }, { "text", text } }, metadata, null),
                o => BridgeJson.Message(o), ct);

        public Task<ChatMessage> DeleteMessageAsync(Guid messageId, CancellationToken ct = default) =>
            CallAsync("deleteMessage", new Dictionary<string, object> { { "messageId", messageId } }, o => BridgeJson.Message(o), ct);

        public Task ReactAsync(Guid messageId, string reaction, bool add = true, CancellationToken ct = default)
        {
            ChatReaction.Validate(reaction);
            return Sync("react", new Dictionary<string, object> { { "messageId", messageId }, { "reaction", reaction }, { "add", add } });
        }

        public Task<ChatHistoryPage> SearchAsync(Guid channelId, string query, Guid? fromUserId = null, string before = null, int? limit = null, CancellationToken ct = default) =>
            CallAsync("search", SearchArgs(Channel(channelId), query, fromUserId, before, limit), o => BridgeJson.History(o, channelId, null), ct);

        /// <summary>The Web SDK needs a conversation: searching every direct conversation (<paramref name="userId"/> null) is not available in WebGL.</summary>
        public Task<ChatHistoryPage> SearchDirectAsync(Guid? userId, string query, Guid? fromUserId = null, string before = null, int? limit = null, CancellationToken ct = default)
        {
            if (!userId.HasValue) throw new NotSupportedException("WebGL search needs a channel or a peer user");
            return CallAsync("search", SearchArgs(new Dictionary<string, object> { { "userId", userId.Value } }, query, fromUserId, before, limit),
                o => BridgeJson.History(o, null, userId), ct);
        }

        private static Dictionary<string, object> SearchArgs(Dictionary<string, object> args, string query, Guid? fromUserId, string before, int? limit)
        {
            if (string.IsNullOrWhiteSpace(query)) throw new ArgumentException("query is empty", nameof(query));
            args["query"] = query;
            if (fromUserId.HasValue) args["fromUserId"] = fromUserId.Value;
            if (before != null) args["before"] = before;
            if (limit.HasValue) args["limit"] = limit.Value;
            return args;
        }

        public Task MarkReadAsync(Guid channelId, Guid messageId, CancellationToken ct = default) =>
            Sync("markRead", new Dictionary<string, object> { { "channelId", channelId }, { "messageId", messageId } });

        public Task MarkDirectReadAsync(Guid userId, Guid messageId, CancellationToken ct = default) =>
            Sync("markRead", new Dictionary<string, object> { { "userId", userId }, { "messageId", messageId } });

        public Task<ChatReadMarkers> ReadMarkersAsync(Guid channelId, CancellationToken ct = default) =>
            CallAsync("readMarkers", Channel(channelId), o => BridgeJson.Markers(o, channelId, null), ct);

        public Task<ChatReadMarkers> DirectReadMarkersAsync(Guid userId, CancellationToken ct = default) =>
            CallAsync("readMarkers", new Dictionary<string, object> { { "userId", userId } }, o => BridgeJson.Markers(o, null, userId), ct);

        public Task SetTypingAsync(Guid channelId, bool typing, TimeSpan? interval = null, CancellationToken ct = default)
        {
            var args = new Dictionary<string, object> { { "channelId", channelId }, { "typing", typing } };
            if (interval.HasValue) args["intervalMs"] = interval.Value.TotalMilliseconds;
            return Sync("setTyping", args);
        }

        // ---- speech -------------------------------------------------------------------------------

        public async Task<SpeechRequest> SpeakAsync(string text, Guid? channelId = null, TtsDestination destination = TtsDestination.Channel,
            string voice = null, string clientRef = null, CancellationToken ct = default)
        {
            if (string.IsNullOrEmpty(text)) throw new ArgumentException("text required", nameof(text));
            var args = new Dictionary<string, object> { { "text", text }, { "destination", ControlMessage.TtsDestinationToWire(destination) } };
            if (channelId.HasValue) args["channelId"] = channelId.Value;
            if (voice != null) args["voice"] = voice;
            if (clientRef != null) args["clientRef"] = clientRef;
            var accepted = await CallAsync("speak", args, o => o, ct);
            var requestId = BridgeJson.Id(accepted, "requestId");
            var done = new TaskCompletionSource<TtsStatus>();
            lock (_speech) _speech[requestId] = done;
            return new SpeechRequest { RequestId = requestId, ClientRef = MiniJson.GetString(accepted, "clientRef") ?? clientRef, Done = done.Task };
        }

        public Task CancelSpeechAsync(CancellationToken ct = default) => Sync("cancelSpeech", null);

        // ---- positional / recording / quality -----------------------------------------------------

        public Task UpdatePositionAsync(Guid channelId, Guid selfUserId, Position3D position, Orientation3D orientation, CancellationToken ct = default) =>
            Sync("updatePosition", new Dictionary<string, object>
            {
                { "channelId", channelId }, { "position", BridgeJson.Position(position) }, { "orientation", BridgeJson.Orientation(orientation) },
            });

        public Task RespondToRecordingAsync(Guid recordingId, RecordingConsent consent, CancellationToken ct = default) =>
            Sync("respondToRecording", new Dictionary<string, object> { { "recordingId", recordingId }, { "consent", BridgeJson.Consent(consent) } });

        public Task ReportQualityAsync(CancellationToken ct = default) => CallAsync("reportQuality", null, o => true, ct);

        /// <summary>Fresh browser media statistics (WebRTC <c>getStats</c>).</summary>
        public Task<WebGLStats> GetStatsAsync(CancellationToken ct = default) => CallAsync("getStats", null, o => BridgeJson.Stats(o), ct);

        // ---- browser audio ------------------------------------------------------------------------

        /// <summary>
        /// Retry starting remote playback (the mixed track's element and the Web Audio graph of the
        /// per-participant tracks); call from a user gesture after <see cref="OnRemoteAudio"/> reported it blocked.
        /// </summary>
        public Task ResumeAudioAsync(CancellationToken ct = default) => CallAsync("resumeAudio", null, o => true, ct);

        /// <summary>Microphones and speakers the browser exposes; output selection needs <c>setSinkId</c> support (Chromium).</summary>
        public Task<WebGLAudioDevices> EnumerateDevicesAsync(CancellationToken ct = default) =>
            CallAsync("enumerateDevices", null, o => BridgeJson.Devices(o), ct);

        /// <summary>Switch the microphone (<see cref="WebGLAudioDevice.DeviceId"/>; null = browser default) — renegotiates the uplink.</summary>
        public Task SetInputDeviceAsync(string deviceId, CancellationToken ct = default)
        {
            var args = new Dictionary<string, object>();
            if (deviceId != null) args["deviceId"] = deviceId;
            return CallAsync("setInputDevice", args, o => true, ct);
        }

        /// <summary>Route remote voices to a speaker (<see cref="WebGLAudioDevice.DeviceId"/>; null = default). Chromium only.</summary>
        public Task SetOutputDeviceAsync(string deviceId, CancellationToken ct = default)
        {
            var args = new Dictionary<string, object>();
            if (deviceId != null) args["deviceId"] = deviceId;
            return CallAsync("setOutputDevice", args, o => true, ct);
        }

        /// <summary>Software microphone gain (1 = unity).</summary>
        public void SetInputGain(float gain)
        {
            Options.InputGain = gain;
            if (_handle > 0) Invoke("setInputGain", new Dictionary<string, object> { { "gain", (double)gain } });
        }

        /// <summary>Master volume of all remote voices (0..1, on top of per-participant volumes).</summary>
        public void SetOutputVolume(float volume)
        {
            if (_handle > 0) Invoke("setOutputVolume", new Dictionary<string, object> { { "volume", (double)volume } });
        }

        /// <summary>Speaker mute: hear nobody, without telling the server or affecting the microphone.</summary>
        public void SetOutputMuted(bool muted)
        {
            if (_handle > 0) Invoke("setOutputMuted", new Dictionary<string, object> { { "muted", muted } });
        }

        // ---- bridge plumbing ----------------------------------------------------------------------

        private Dictionary<string, object> Invoke(string method, Dictionary<string, object> args, int rid = 0)
        {
            if (_handle <= 0) throw new InvalidOperationException("not connected");
            var raw = _bridge.Invoke(_handle, method, args == null ? "{}" : MiniJson.Serialize(args), rid);
            var result = MiniJson.AsObject(MiniJson.Parse(raw));
            if (result == null) throw new WebGLBridgeException($"malformed bridge reply to {method}");
            if (!MiniJson.GetBool(result, "ok")) throw BridgeJson.Error(result, $"{method} failed");
            return result;
        }

        private object Value(string method, Dictionary<string, object> args)
        {
            var result = Invoke(method, args);
            return result.TryGetValue("value", out var v) ? v : null;
        }

        /// <summary>A fire-and-forget browser call surfaced as a completed (or faulted) task, like the native client's send-only methods.</summary>
        private Task Sync(string method, Dictionary<string, object> args)
        {
            try
            {
                Invoke(method, args);
                return Task.CompletedTask;
            }
            catch (Exception e)
            {
                return Task.FromException(e);
            }
        }

        /// <param name="rawValue">Pass the whole reply object (with the <c>value</c> key) to <paramref name="parse"/> instead of <c>value</c> as an object.</param>
        private Task<T> CallAsync<T>(string method, Dictionary<string, object> args, Func<Dictionary<string, object>, T> parse,
            CancellationToken ct, bool timeout = true, bool rawValue = false)
        {
            ThrowIfDisposed();
            var tcs = new TaskCompletionSource<T>();
            int rid = Interlocked.Increment(ref _nextRid);
            var pending = new PendingCall
            {
                Complete = reply =>
                {
                    try { tcs.TrySetResult(parse(rawValue ? reply : MiniJson.AsObject(reply.TryGetValue("value", out var v) ? v : null))); }
                    catch (Exception e) { tcs.TrySetException(e); }
                },
                Fail = e => tcs.TrySetException(e),
                Deadline = timeout && RequestTimeout > TimeSpan.Zero ? DateTime.UtcNow + RequestTimeout : DateTime.MaxValue,
            };
            Dictionary<string, object> result;
            try
            {
                lock (_pending) _pending[rid] = pending;
                result = Invoke(method, args, rid);
            }
            catch (Exception e)
            {
                lock (_pending) _pending.Remove(rid);
                tcs.TrySetException(e);
                return tcs.Task;
            }
            if (!MiniJson.GetBool(result, "pending"))
            {
                lock (_pending) _pending.Remove(rid);
                pending.Complete(result);
            }
            else if (ct.CanBeCanceled)
            {
                ct.Register(() =>
                {
                    lock (_pending) _pending.Remove(rid);
                    tcs.TrySetCanceled(ct);
                });
            }
            return tcs.Task;
        }

        /// <summary>
        /// Pull the browser's queued events and raise them on the calling thread; also completes pending
        /// requests and SDK-load waiters and applies request timeouts. Call once per frame.
        /// </summary>
        public void Update()
        {
            if (_disposed) return;
            lock (_mainThreadQueue)
                while (_mainThreadQueue.Count > 0) _mainThreadQueue.Dequeue()();

            List<TaskCompletionSource<bool>> waiters = null;
            lock (_sdkWaiters)
                if (_sdkWaiters.Count > 0) { waiters = new List<TaskCompletionSource<bool>>(_sdkWaiters); }
            if (waiters != null)
            {
                var status = _bridge.SdkStatus;
                if (status == WebGLSdkStatus.Ready || status == WebGLSdkStatus.Failed)
                {
                    lock (_sdkWaiters) _sdkWaiters.Clear();
                    foreach (var w in waiters)
                    {
                        if (status == WebGLSdkStatus.Ready) w.TrySetResult(true);
                        else w.TrySetException(new WebGLBridgeException(_bridge.SdkError ?? "failed to load aurix-web-sdk.js"));
                    }
                }
            }

            if (_handle > 0) Drain();

            List<KeyValuePair<int, PendingCall>> expired = null;
            var now = DateTime.UtcNow;
            lock (_pending)
                foreach (var kv in _pending)
                    if (kv.Value.Deadline < now) (expired ??= new List<KeyValuePair<int, PendingCall>>()).Add(kv);
            if (expired != null)
                foreach (var kv in expired)
                {
                    lock (_pending) _pending.Remove(kv.Key);
                    kv.Value.Fail(new TimeoutException("bridge request timeout"));
                }
        }

        private void Drain()
        {
            var raw = _bridge.Drain(_handle);
            var events = MiniJson.AsArray(MiniJson.Parse(raw));
            if (events == null) return;
            foreach (var item in events)
            {
                var e = MiniJson.AsObject(item);
                if (e == null) continue;
                HandleEvent(MiniJson.GetString(e, "type"), e);
            }
        }

        private void HandleEvent(string type, Dictionary<string, object> e)
        {
            switch (type)
            {
                case "result":
                {
                    int rid = (int)MiniJson.GetNumber(e, "rid");
                    PendingCall call;
                    lock (_pending)
                    {
                        if (!_pending.TryGetValue(rid, out call)) return;
                        _pending.Remove(rid);
                    }
                    if (MiniJson.GetBool(e, "ok")) call.Complete(e);
                    else call.Fail(BridgeJson.Error(e, "request failed"));
                    return;
                }
                case "overflow":
                    OnEventsDropped?.Invoke((int)MiniJson.GetNumber(e, "dropped"));
                    return;
                case "tokenRequest":
                    HandleTokenRequest(e);
                    return;
                case "connectionState":
                {
                    var next = BridgeJson.ConnectionState(MiniJson.GetString(e, "state"));
                    var previous = State;
                    if (next == previous) return;
                    State = next;
                    if (next == VoiceConnectionState.Disconnected || next == VoiceConnectionState.Failed)
                    {
                        lock (_channels) _channels.Clear();
                        lock (_speech)
                        {
                            foreach (var s in _speech.Values) s.TrySetException(new InvalidOperationException("disconnected"));
                            _speech.Clear();
                        }
                    }
                    OnStateChanged?.Invoke(next);
                    if ((next == VoiceConnectionState.Disconnected || next == VoiceConnectionState.Failed) && previous != VoiceConnectionState.Connecting)
                        OnDisconnected?.Invoke(_closeReason ?? (next == VoiceConnectionState.Failed ? "reconnect failed" : "connection closed"));
                    return;
                }
                case "sessionReady":
                case "recovered":
                {
                    var info = BridgeJson.Obj(e, "info");
                    var session = BridgeJson.Session(info);
                    if (session == null) return;
                    ApplySession(info, session);
                    if (type == "sessionReady") OnSessionReady?.Invoke(session);
                    else OnRecovered?.Invoke(session);
                    return;
                }
                case "channelJoined":
                {
                    var channelId = BridgeJson.Id(e, "channelId");
                    var participants = BridgeJson.Participants(BridgeJson.Arr(e, "participants"));
                    lock (_channels)
                    {
                        var members = new Dictionary<Guid, Participant>();
                        foreach (var p in participants) members[p.UserId] = p;
                        _channels[channelId] = members;
                    }
                    OnChannelJoined?.Invoke(channelId, participants);
                    return;
                }
                case "channelLeft":
                {
                    var channelId = BridgeJson.Id(e, "channelId");
                    lock (_channels) _channels.Remove(channelId);
                    OnChannelLeft?.Invoke(channelId);
                    return;
                }
                case "participantJoined":
                case "participantUpdated":
                {
                    var channelId = BridgeJson.Id(e, "channelId");
                    var update = BridgeJson.Participant(BridgeJson.Obj(e, "participant"));
                    if (update == null) return;
                    Participant p;
                    lock (_channels)
                    {
                        if (!_channels.TryGetValue(channelId, out var members)) _channels[channelId] = members = new Dictionary<Guid, Participant>();
                        if (members.TryGetValue(update.UserId, out p)) BridgeJson.Apply(p, update);
                        else members[update.UserId] = p = update;
                    }
                    if (type == "participantJoined") OnParticipantJoined?.Invoke(channelId, p);
                    else OnParticipantUpdated?.Invoke(channelId, p);
                    return;
                }
                case "participantLeft":
                {
                    var channelId = BridgeJson.Id(e, "channelId");
                    var userId = BridgeJson.Id(e, "userId");
                    Participant p = null;
                    lock (_channels)
                        if (_channels.TryGetValue(channelId, out var members) && members.TryGetValue(userId, out p)) members.Remove(userId);
                    OnParticipantLeft?.Invoke(channelId, p ?? new Participant { UserId = userId, DisplayName = string.Empty });
                    return;
                }
                case "speaking":
                {
                    var channelId = BridgeJson.Id(e, "channelId");
                    var userId = BridgeJson.Id(e, "userId");
                    bool speaking = MiniJson.GetBool(e, "speaking");
                    Participant p = null;
                    lock (_channels)
                        if (_channels.TryGetValue(channelId, out var members) && members.TryGetValue(userId, out p)) p.IsSpeaking = speaking;
                    if (p != null) OnSpeaking?.Invoke(channelId, p, speaking);
                    return;
                }
                case "energy":
                {
                    var channelId = BridgeJson.Id(e, "channelId");
                    var levels = new ControlMessage { Type = type, Data = e }.Levels();
                    lock (_channels)
                        if (_channels.TryGetValue(channelId, out var members))
                            foreach (var l in levels)
                                if (members.TryGetValue(l.UserId, out var p)) p.Energy = l.Energy;
                    OnChannelEnergy?.Invoke(channelId, levels);
                    return;
                }
                case "positions":
                    OnPositions?.Invoke(BridgeJson.Id(e, "channelId"), new ControlMessage { Type = type, Data = e }.Positions());
                    return;
                case "recording":
                    OnRecording?.Invoke(new RecordingNotice
                    {
                        ChannelId = BridgeJson.Id(e, "channelId"),
                        RecordingId = BridgeJson.Id(e, "recordingId"),
                        Active = MiniJson.GetBool(e, "active"),
                        InitiatedBy = BridgeJson.Id(e, "initiatedBy"),
                        Live = MiniJson.GetBool(e, "live"),
                    });
                    return;
                case "bitrate":
                    OnBitrateCommand?.Invoke(new BitrateCommand
                    {
                        TargetBitrateKbps = MiniJson.GetUInt32(e, "targetKbps"),
                        Reason = MiniJson.GetString(e, "reason"),
                        ExpectedLossPercent = (int)MiniJson.GetNumber(e, "expectedLossPercent"),
                    });
                    return;
                case "networkQuality":
                {
                    var q = BridgeJson.Obj(e, "quality");
                    if (q == null) return;
                    var quality = BridgeJson.Quality(q);
                    _quality = quality;
                    OnNetworkQuality?.Invoke(quality);
                    return;
                }
                case "stats":
                {
                    var s = BridgeJson.Obj(e, "stats");
                    if (s != null) OnStats?.Invoke(BridgeJson.Stats(s));
                    return;
                }
                case "kicked":
                    OnKicked?.Invoke(BridgeJson.Id(e, "channelId"), MiniJson.GetString(e, "reason") ?? string.Empty);
                    return;
                case "receiverPreferences":
                {
                    var prefs = BridgeJson.Preferences(BridgeJson.Obj(e, "prefs"));
                    lock (_channels)
                    {
                        _transmission = prefs.Transmission;
                        _focus = prefs.FocusChannel;
                    }
                    OnReceiverPreferences?.Invoke(prefs);
                    return;
                }
                case "userBlockChanged":
                    OnUserBlockChanged?.Invoke(BridgeJson.Id(e, "userId"), MiniJson.GetBool(e, "blocked"));
                    return;
                case "participantPriorityChanged":
                {
                    var channelId = BridgeJson.Id(e, "channelId");
                    var userId = BridgeJson.Id(e, "userId");
                    bool priority = MiniJson.GetBool(e, "priority");
                    lock (_channels)
                        if (_channels.TryGetValue(channelId, out var members) && members.TryGetValue(userId, out var p)) p.IsPriority = priority;
                    OnParticipantPriorityChanged?.Invoke(channelId, userId, priority);
                    return;
                }
                case "participantRoleChanged":
                {
                    var channelId = BridgeJson.Id(e, "channelId");
                    var userId = BridgeJson.Id(e, "userId");
                    var role = ControlMessage.ParseRole(MiniJson.GetString(e, "role"));
                    bool admitted = MiniJson.GetBool(e, "admitted");
                    lock (_channels)
                        if (_channels.TryGetValue(channelId, out var members) && members.TryGetValue(userId, out var p)) p.Role = role;
                    OnParticipantRoleChanged?.Invoke(channelId, userId, role, admitted);
                    return;
                }
                case "duckingChanged":
                    OnDuckingChanged?.Invoke(BridgeJson.Id(e, "channelId"), MiniJson.GetBool(e, "active"),
                        BridgeJson.Ducking(BridgeJson.Obj(e, "config")) ?? DuckingConfig.Default);
                    return;
                case "participantVisemes":
                {
                    var frame = BridgeJson.Visemes(BridgeJson.Obj(e, "frame"));
                    if (frame.HasValue) OnParticipantVisemes?.Invoke(BridgeJson.Id(e, "userId"), frame.Value);
                    return;
                }
                case "localVisemes":
                {
                    var frame = BridgeJson.Visemes(BridgeJson.Obj(e, "frame"));
                    if (frame.HasValue) OnLocalVisemes?.Invoke(frame.Value);
                    return;
                }
                case "transmissionChanged":
                {
                    var mode = BridgeJson.Transmission(e.TryGetValue("mode", out var m) ? m : null);
                    lock (_channels) _transmission = mode;
                    OnTransmissionChanged?.Invoke(mode);
                    return;
                }
                case "channelFocusChanged":
                {
                    var focus = MiniJson.GetGuid(e, "channelId");
                    lock (_channels) _focus = focus;
                    OnChannelFocusChanged?.Invoke(focus);
                    return;
                }
                case "participantStreams":
                    OnParticipantStreams?.Invoke(BridgeJson.ParticipantStreams(BridgeJson.Arr(e, "streams")));
                    return;
                case "e2eePeerKey":
                    OnE2eePeerKey?.Invoke(BridgeJson.Id(e, "userId"), MiniJson.GetString(e, "fingerprint") ?? string.Empty, MiniJson.GetString(e, "previousFingerprint"));
                    return;
                case "e2eePeerDecryptable":
                    OnE2eePeerDecryptable?.Invoke(BridgeJson.Id(e, "userId"), MiniJson.GetBool(e, "decryptable"));
                    return;
                case "e2eeKeyRotated":
                    OnE2eeKeyRotated?.Invoke((int)MiniJson.GetNumber(e, "generation"));
                    return;
                case "recovering":
                    OnRecovering?.Invoke((int)MiniJson.GetNumber(e, "attempt"), TimeSpan.FromMilliseconds(MiniJson.GetNumber(e, "delayMs")),
                        MiniJson.GetString(e, "cause") ?? string.Empty);
                    return;
                case "endpointChanged":
                {
                    var url = MiniJson.GetString(e, "url");
                    if (url != null) Endpoint = url;
                    OnEndpointChanged?.Invoke(url);
                    return;
                }
                case "failedToRecover":
                    OnFailedToRecover?.Invoke(BridgeJson.Error(e, "reconnect failed"));
                    return;
                case "sessionClosed":
                {
                    var reason = MiniJson.GetString(e, "reason") ?? "session closed";
                    _closeReason = reason;
                    OnSessionClosed?.Invoke(reason);
                    return;
                }
                case "chatMessage":
                {
                    var message = BridgeJson.Message(BridgeJson.Obj(e, "message"));
                    if (message != null) OnChatMessage?.Invoke(message);
                    return;
                }
                case "chatReadMarker":
                {
                    var marker = BridgeJson.Marker(BridgeJson.Obj(e, "marker"));
                    if (marker != null) OnChatReadMarker?.Invoke(marker);
                    return;
                }
                case "chatInboxSynced":
                    OnChatInboxSynced?.Invoke((int)MiniJson.GetNumber(e, "delivered"), MiniJson.GetBool(e, "truncated"), MiniJson.GetBool(e, "perDevice"));
                    return;
                case "chatMessageUpdated":
                {
                    var message = BridgeJson.Message(BridgeJson.Obj(e, "message"));
                    if (message != null) OnChatMessageUpdated?.Invoke(message);
                    return;
                }
                case "chatReactionChanged":
                {
                    var change = BridgeJson.ReactionChange(BridgeJson.Obj(e, "change"));
                    if (change != null) OnChatReactionChanged?.Invoke(change);
                    return;
                }
                case "participantTyping":
                    OnParticipantTyping?.Invoke(BridgeJson.Id(e, "channelId"), BridgeJson.Id(e, "userId"), MiniJson.GetBool(e, "typing"));
                    return;
                case "transcript":
                {
                    var t = BridgeJson.Transcript(BridgeJson.Obj(e, "transcript"));
                    if (t != null) OnTranscript?.Invoke(t);
                    return;
                }
                case "serverNoiseSuppressionChanged":
                {
                    var enabled = MiniJson.GetBool(e, "enabled");
                    lock (_channels) _activeServerNoiseSuppression = enabled;
                    OnServerNoiseSuppressionChanged?.Invoke(enabled);
                    return;
                }
                case "translationChanged":
                {
                    var prefs = BridgeJson.TranslationPrefs(BridgeJson.Obj(e, "prefs"));
                    lock (_channels) _translation = prefs.Clone();
                    OnTranslationChanged?.Invoke(prefs);
                    return;
                }
                case "ttsStatus":
                {
                    var status = BridgeJson.Tts(BridgeJson.Obj(e, "status"));
                    if (status == null) return;
                    if (status.IsTerminal)
                    {
                        TaskCompletionSource<TtsStatus> done;
                        lock (_speech)
                            if (_speech.TryGetValue(status.RequestId, out done)) _speech.Remove(status.RequestId);
                        done?.TrySetResult(status);
                    }
                    OnTtsStatus?.Invoke(status);
                    return;
                }
                case "serverError":
                    OnServerError?.Invoke(MiniJson.GetString(e, "code") ?? string.Empty, MiniJson.GetString(e, "message") ?? string.Empty);
                    return;
                case "error":
                    OnError?.Invoke(BridgeJson.Error(e, "browser client error"));
                    return;
                case "localSpeaking":
                    OnLocalSpeaking?.Invoke(MiniJson.GetBool(e, "speaking"));
                    return;
                case "remoteAudio":
                    OnRemoteAudio?.Invoke(MiniJson.GetBool(e, "playing"), MiniJson.GetString(e, "reason"));
                    return;
                case "devicesChanged":
                {
                    var devices = BridgeJson.Obj(e, "devices");
                    if (devices != null) OnDevicesChanged?.Invoke(BridgeJson.Devices(devices));
                    return;
                }
                case "message":
                {
                    var raw = BridgeJson.Obj(e, "message");
                    var t = MiniJson.GetString(raw, "type");
                    if (t != null) OnControlMessage?.Invoke(new ControlMessage { Type = t, Data = BridgeJson.Obj(raw, "data") });
                    return;
                }
                default:
                    return;
            }
        }

        private void ApplySession(Dictionary<string, object> raw, SessionInfo session)
        {
            _userId = BridgeJson.SessionUser(raw);
            Session = session;
            Endpoint = session.Endpoint ?? Endpoint;
            FailoverEndpoints = session.Failover;
        }

        private void HandleTokenRequest(Dictionary<string, object> e)
        {
            int requestId = (int)MiniJson.GetNumber(e, "requestId");
            var kind = MiniJson.GetString(e, "kind");
            var channelId = MiniJson.GetGuid(e, "channelId");
            Task<string> task;
            try
            {
                if (kind == "join")
                {
                    var provider = JoinTokenProvider;
                    if (provider == null || !channelId.HasValue) { Answer(requestId, null, "no join token provider"); return; }
                    task = provider(channelId.Value, CancellationToken.None);
                }
                else
                {
                    var refresher = TokenRefresher;
                    if (refresher == null) { Answer(requestId, null, "no token refresher"); return; }
                    task = refresher(CancellationToken.None);
                }
            }
            catch (Exception ex)
            {
                Answer(requestId, null, ex.Message);
                return;
            }
            if (task == null) { Answer(requestId, null, "token provider returned null"); return; }
            task.ContinueWith(t =>
            {
                string token = t.Status == TaskStatus.RanToCompletion ? t.Result : null;
                string error = t.IsFaulted ? (t.Exception?.GetBaseException().Message ?? "token provider failed")
                    : t.IsCanceled ? "token provider cancelled"
                    : token == null ? "token provider returned null" : null;
                lock (_mainThreadQueue) _mainThreadQueue.Enqueue(() => Answer(requestId, token, error));
            }, TaskContinuationOptions.ExecuteSynchronously);
        }

        private void Answer(int requestId, string token, string error)
        {
            if (_handle <= 0) return;
            var args = new Dictionary<string, object> { { "requestId", requestId } };
            if (error != null) args["error"] = error;
            else args["token"] = token;
            try { Invoke("provideToken", args); }
            catch (WebGLBridgeException ex) { OnError?.Invoke(ex); }
        }
    }
}
