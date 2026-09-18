using System;
using System.Collections.Generic;
using System.Threading;
using System.Threading.Tasks;
using Aurix.Protocol;
using Aurix.Transport;

namespace Aurix
{
    public enum VoiceConnectionState { Disconnected, Connecting, Connected, MediaBound, Reconnecting, Failed }

    /// <summary>Exponential backoff used by <see cref="AurixVoiceClient"/> after an unexpected connection loss.</summary>
    public sealed class ReconnectPolicy
    {
        public TimeSpan InitialDelay = TimeSpan.FromMilliseconds(500);
        public TimeSpan MaxDelay = TimeSpan.FromSeconds(8);
        public double Factor = 2.0;
        /// <summary>Random ±fraction applied to every delay (0.3 = ±30%).</summary>
        public double Jitter = 0.3;
        public int MaxAttempts = 10;
    }

    public sealed class Participant
    {
        public Guid UserId;
        public string DisplayName;
        public uint Ssrc;
        public ChannelRole Role;
        public bool IsMuted;
        public bool IsServerMuted;
        public bool IsSpeaking;
    }

    public sealed class SessionInfo
    {
        public Guid SessionId;
        public uint Ssrc;
        public string MediaAddr;
        /// <summary>True when this is the same session as before a connection loss (same SSRC, channels kept).</summary>
        public bool Resumed;
    }

    public sealed class RecordingNotice
    {
        public Guid ChannelId;
        public Guid RecordingId;
        public bool Active;
        public Guid InitiatedBy;
    }

    /// <summary>
    /// High-level Aurix client for .NET / Unity: WebSocket control plane + native AURX/UDP media.
    /// Thread model: network I/O runs on background tasks; all events are raised from
    /// <see cref="Update"/>, which the host must call periodically (e.g. from a MonoBehaviour's
    /// Update). Audio frames are exposed through <see cref="TryDequeueAudio"/> / <see cref="OnAudio"/>.
    /// </summary>
    public sealed class AurixVoiceClient : IDisposable
    {
        public const string SdkVersion = "1.0.0";

        private readonly string _wsUrl;
        private readonly string _token;
        private readonly Dictionary<Guid, Dictionary<Guid, Participant>> _channels = new Dictionary<Guid, Dictionary<Guid, Participant>>();
        private readonly Dictionary<uint, Participant> _bySsrc = new Dictionary<uint, Participant>();
        private readonly Queue<Action> _mainThreadQueue = new Queue<Action>();
        private readonly HashSet<Guid> _joinedChannels = new HashSet<Guid>();
        /// <summary>Receiver-local mutes held by this client: user → channels (Guid.Empty = every channel).</summary>
        private readonly Dictionary<Guid, HashSet<Guid>> _localMutes = new Dictionary<Guid, HashSet<Guid>>();
        private readonly Dictionary<Guid, float> _volumes = new Dictionary<Guid, float>();
        private readonly HashSet<Guid> _blockedUsers = new HashSet<Guid>();
        private readonly Random _random = new Random();
        private ControlChannel _control;
        private MediaTransport _media;
        private CancellationTokenSource _cts;
        private DateTime _lastPing = DateTime.MinValue;
        private DateTime _lastPong = DateTime.MinValue;
        private uint _rtpTimestamp;
        private bool _muted;
        private Guid? _pendingChannelJoin;
        private TaskCompletionSource<List<Participant>> _joinTcs;
        private byte[] _mediaKey;
        private string _resumeToken;
        private uint _lastMediaSequence;
        private bool _closedByUser;
        private CancellationTokenSource _skipBackoff;
        private int _reconnectLoopActive;

        public VoiceConnectionState State { get; private set; } = VoiceConnectionState.Disconnected;
        public SessionInfo Session { get; private set; }
        public bool IsMuted => _muted;
        public MediaTransport Media => _media;
        /// <summary>Wall-clock RTT of the last WebSocket ping, in ms.</summary>
        public float ControlRttMs { get; private set; }
        public TimeSpan PingInterval { get; set; } = TimeSpan.FromSeconds(15);
        public TimeSpan RequestTimeout { get; set; } = TimeSpan.FromSeconds(10);
        /// <summary>
        /// Reconnect automatically after an unexpected connection loss, resuming the same session when
        /// the server still holds it (see <see cref="ResumeGrace"/>) and re-joining channels otherwise.
        /// Never triggers after <see cref="DisconnectAsync"/> or a server-initiated <c>SessionClose</c>.
        /// </summary>
        public bool AutoReconnect { get; set; } = true;
        public ReconnectPolicy Reconnect { get; } = new ReconnectPolicy();
        /// <summary>How long the server keeps a dropped session resumable (zero = resume disabled).</summary>
        public TimeSpan ResumeGrace { get; private set; }
        /// <summary>Channels currently joined (restored across reconnects).</summary>
        public IReadOnlyCollection<Guid> JoinedChannels { get { lock (_channels) return new List<Guid>(_joinedChannels); } }

        public event Action<VoiceConnectionState> OnStateChanged;
        public event Action<SessionInfo> OnSessionReady;
        public event Action<Guid, IReadOnlyList<Participant>> OnChannelJoined;
        public event Action<Guid> OnChannelLeft;
        public event Action<Guid, Participant> OnParticipantJoined;
        public event Action<Guid, Participant> OnParticipantLeft;
        public event Action<Guid, Participant> OnParticipantUpdated;
        public event Action<Guid, Participant, bool> OnSpeaking;
        public event Action<Guid, IReadOnlyList<UserPosition>> OnPositions;
        public event Action<RecordingNotice> OnRecording;
        public event Action<uint, string> OnBitrateCommand;
        public event Action<Guid, string> OnKicked;
        /// <summary>Snapshot of persistent cross-mutes (and, on a resumed session, local mutes/volumes) from the server.</summary>
        public event Action<ReceiverPreferences> OnReceiverPreferences;
        /// <summary>A cross-mute placed or lifted by this user, from this or any other device / the REST API.</summary>
        public event Action<Guid, bool> OnUserBlockChanged;
        public event Action<string, string> OnServerError;
        /// <summary>Connection closed for good: after <see cref="DisconnectAsync"/>, a server <c>SessionClose</c>, or when reconnecting gave up.</summary>
        public event Action<string> OnDisconnected;
        /// <summary>Connection lost; reconnect attempt N will run after the given delay. (attempt, delay, cause)</summary>
        public event Action<int, TimeSpan, string> OnRecovering;
        /// <summary>Reconnected. <see cref="SessionInfo.Resumed"/> tells whether the old session (and SSRC) survived.</summary>
        public event Action<SessionInfo> OnRecovered;
        /// <summary>All reconnect attempts failed; the client is now <see cref="VoiceConnectionState.Failed"/>.</summary>
        public event Action<Exception> OnFailedToRecover;
        /// <summary>The server ended the session (kick, ban, shutdown, replaced by another login); no reconnect follows.</summary>
        public event Action<string> OnSessionClosed;
        /// <summary>Verified downlink frame: (participant or null if unknown SSRC, frame).</summary>
        public event Action<Participant, IncomingAudio> OnAudio;
        /// <summary>Every raw control message, for diagnostics/extensions.</summary>
        public event Action<ControlMessage> OnControlMessage;

        /// <param name="wsUrl">e.g. <c>ws://host:8081/ws</c> (or <c>wss://</c>).</param>
        /// <param name="token">Per-user JWT issued by your backend via <c>POST /v1/tokens</c>.</param>
        public AurixVoiceClient(string wsUrl, string token)
        {
            _wsUrl = wsUrl ?? throw new ArgumentNullException(nameof(wsUrl));
            _token = token ?? throw new ArgumentNullException(nameof(token));
        }

        /// <summary>Open the control channel, receive the session, then authenticate the UDP media path.</summary>
        public async Task<SessionInfo> ConnectAsync(CancellationToken ct = default)
        {
            if (State != VoiceConnectionState.Disconnected && State != VoiceConnectionState.Failed)
                throw new InvalidOperationException("already connected");
            _cts = CancellationTokenSource.CreateLinkedTokenSource(ct);
            _closedByUser = false;
            _resumeToken = null;
            _lastMediaSequence = 0;
            SetState(VoiceConnectionState.Connecting);
            try
            {
                var info = await OpenSessionAsync(null, _cts.Token).ConfigureAwait(false);
                Post(() => OnSessionReady?.Invoke(info));
                await ReplayReceiverPrefsAsync(_cts.Token).ConfigureAwait(false);
                await BindMediaAsync(info, _cts.Token).ConfigureAwait(false);
                return info;
            }
            catch
            {
                SetState(VoiceConnectionState.Failed);
                Teardown();
                throw;
            }
        }

        /// <summary>
        /// Skip the current backoff delay and retry now (e.g. when the OS reports the network is back).
        /// No-op unless the client is <see cref="VoiceConnectionState.Reconnecting"/>.
        /// </summary>
        public void ReconnectNow() => _skipBackoff?.Cancel();

        /// <summary>Open a control channel (optionally resuming) and wait for <c>SessionInitAck</c>.</summary>
        private async Task<SessionInfo> OpenSessionAsync(string resume, CancellationToken ct)
        {
            var control = new ControlChannel();
            control.Closed += reason => Post(() => HandleClosed(control, reason));
            control.Received += CompletePendingJoin;
            try
            {
                await control.ConnectAsync(new Uri(_wsUrl), _token, ct, resume).ConfigureAwait(false);
                ControlMessage ack;
                while (true)
                {
                    ack = await control.NextAsync(RequestTimeout, ct).ConfigureAwait(false);
                    if (ack.Type == "SessionInitAck") break;
                    if (ack.Type == "Error") throw new InvalidOperationException($"{ack.Str("code")}: {ack.Str("message")}");
                }
                var info = new SessionInfo
                {
                    SessionId = ack.Id("session_id"),
                    Ssrc = ack.U32("ssrc"),
                    MediaAddr = ack.Str("media_addr"),
                    Resumed = ack.Bool("resumed"),
                };
                _mediaKey = Convert.FromBase64String(ack.Str("media_key") ?? throw new InvalidOperationException("SessionInitAck without media_key"));
                var token = ack.Str("resume_token");
                _resumeToken = string.IsNullOrEmpty(token) ? null : token;
                ResumeGrace = TimeSpan.FromMilliseconds(ack.Num("resume_grace_ms"));
                Session = info;
                // Publish only now so Update() cannot drain the handshake messages from under us.
                _control = control;
                _lastPing = DateTime.UtcNow;
                _lastPong = _lastPing;
                SetState(VoiceConnectionState.Connected);
                return info;
            }
            catch
            {
                control.Dispose();
                throw;
            }
        }

        /// <summary>Bind (or rebind, from a fresh UDP port) the native media path for the current session.</summary>
        private async Task BindMediaAsync(SessionInfo info, CancellationToken ct)
        {
            var old = _media;
            _media = null;
            if (old != null)
            {
                _lastMediaSequence = old.CurrentSequence;
                old.Dispose();
            }
            uint firstSeq = info.Resumed ? _lastMediaSequence : 0;
            var endpoint = await MediaTransport.ResolveAsync(info.MediaAddr).ConfigureAwait(false);
            var media = new MediaTransport(endpoint, info.SessionId, info.Ssrc, _mediaKey, firstSeq);
            try
            {
                await media.BindAsync(ct).ConfigureAwait(false);
            }
            catch
            {
                media.Dispose();
                throw;
            }
            _media = media;
            if (_muted) media.SendMuteState(true);
            SetState(VoiceConnectionState.MediaBound);
        }

        /// <summary>Join a channel the token grants access to. Returns the current roster.</summary>
        public async Task<IReadOnlyList<Participant>> JoinChannelAsync(Guid channelId, CancellationToken ct = default)
        {
            EnsureConnected();
            if (_joinTcs != null) throw new InvalidOperationException("a join is already in progress");
            var tcs = new TaskCompletionSource<List<Participant>>(TaskCreationOptions.RunContinuationsAsynchronously);
            _joinTcs = tcs;
            _pendingChannelJoin = channelId;
            await _control.SendAsync(ControlMessage.ChannelJoin(channelId, _token), ct).ConfigureAwait(false);
            using (var timeout = new CancellationTokenSource(RequestTimeout))
            using (timeout.Token.Register(() => tcs.TrySetException(new TimeoutException("ChannelJoinAck timeout"))))
            using (ct.Register(() => tcs.TrySetCanceled()))
            {
                try
                {
                    var roster = await tcs.Task.ConfigureAwait(false);
                    lock (_channels) _joinedChannels.Add(channelId);
                    await ReplayChannelMutesAsync(channelId, ct).ConfigureAwait(false);
                    return roster;
                }
                finally { _joinTcs = null; _pendingChannelJoin = null; }
            }
        }

        /// <summary>Leave a channel. The server does not acknowledge; membership is dropped locally at once.</summary>
        public async Task LeaveChannelAsync(Guid channelId, CancellationToken ct = default)
        {
            EnsureConnected();
            await _control.SendAsync(ControlMessage.ChannelLeave(channelId), ct).ConfigureAwait(false);
            lock (_channels)
            {
                _joinedChannels.Remove(channelId);
                if (_channels.TryGetValue(channelId, out var map))
                {
                    foreach (var p in map.Values) _bySsrc.Remove(p.Ssrc);
                    _channels.Remove(channelId);
                }
            }
            Post(() => OnChannelLeft?.Invoke(channelId));
        }

        /// <summary>Mute/unmute the local microphone. Propagates to other participants via the server.</summary>
        public void SetMuted(bool muted)
        {
            _muted = muted;
            _media?.SendMuteState(muted);
        }

        // ---- receiver-side controls: affect only what *this* client hears ----------------------

        /// <summary>Upper bound the server accepts for <see cref="SetParticipantVolumeAsync"/>.</summary>
        public const float MaxParticipantVolume = 2.0f;

        /// <summary>
        /// Stop hearing <paramref name="userId"/> in <paramref name="channelId"/>, or in every channel
        /// when null. The other participant is not told. Re-applied by the client after a reconnect.
        /// </summary>
        public Task SetParticipantMutedAsync(Guid userId, bool muted, Guid? channelId = null, CancellationToken ct = default)
        {
            var key = channelId ?? Guid.Empty;
            lock (_localMutes)
            {
                if (!_localMutes.TryGetValue(userId, out var scopes)) _localMutes[userId] = scopes = new HashSet<Guid>();
                if (muted) scopes.Add(key);
                else if (channelId == null) scopes.Clear();
                else scopes.Remove(key);
                if (scopes.Count == 0) _localMutes.Remove(userId);
            }
            if (_control == null) return Task.CompletedTask;
            if (channelId != null) lock (_channels) { if (!_joinedChannels.Contains(channelId.Value)) return Task.CompletedTask; }
            return _control.SendAsync(ControlMessage.SetParticipantMute(userId, channelId, muted), ct);
        }

        /// <summary>True when this client muted <paramref name="userId"/> in <paramref name="channelId"/> (or everywhere).</summary>
        public bool IsParticipantMuted(Guid userId, Guid? channelId = null)
        {
            lock (_localMutes)
                return _localMutes.TryGetValue(userId, out var scopes)
                    && (scopes.Contains(Guid.Empty) || (channelId != null && scopes.Contains(channelId.Value)));
        }

        /// <summary>
        /// Receiver-local gain for <paramref name="userId"/>: 0 silence, 1 as sent, up to
        /// <see cref="MaxParticipantVolume"/>. Multiplies positional attenuation; applied by the server.
        /// </summary>
        public Task SetParticipantVolumeAsync(Guid userId, float volume, CancellationToken ct = default)
        {
            if (float.IsNaN(volume) || float.IsInfinity(volume) || volume < 0f || volume > MaxParticipantVolume)
                throw new ArgumentOutOfRangeException(nameof(volume), $"volume must be within 0..{MaxParticipantVolume}");
            lock (_volumes)
            {
                if (volume == 1f) _volumes.Remove(userId);
                else _volumes[userId] = volume;
            }
            return _control == null ? Task.CompletedTask : _control.SendAsync(ControlMessage.SetParticipantVolume(userId, volume), ct);
        }

        public float GetParticipantVolume(Guid userId)
        {
            lock (_volumes) return _volumes.TryGetValue(userId, out var v) ? v : 1f;
        }

        /// <summary>
        /// Persistent, mutual cross-mute: neither side hears the other in any channel, on any device,
        /// until lifted. Stored server-side; confirmed through <see cref="OnUserBlockChanged"/>.
        /// </summary>
        public Task SetUserBlockedAsync(Guid userId, bool blocked, CancellationToken ct = default)
        {
            EnsureConnected();
            return _control.SendAsync(ControlMessage.SetUserBlock(userId, blocked), ct);
        }

        public bool IsUserBlocked(Guid userId)
        {
            lock (_blockedUsers) return _blockedUsers.Contains(userId);
        }

        public IReadOnlyCollection<Guid> BlockedUsers { get { lock (_blockedUsers) return new List<Guid>(_blockedUsers); } }

        /// <summary>A fresh (non-resumed) session forgot our local mutes/volumes: send them again.</summary>
        private async Task ReplayReceiverPrefsAsync(CancellationToken ct)
        {
            var control = _control;
            if (control == null) return;
            List<Guid> everywhere;
            List<KeyValuePair<Guid, float>> volumes;
            lock (_localMutes)
            {
                everywhere = new List<Guid>();
                foreach (var kv in _localMutes) if (kv.Value.Contains(Guid.Empty)) everywhere.Add(kv.Key);
            }
            lock (_volumes) volumes = new List<KeyValuePair<Guid, float>>(_volumes);
            foreach (var user in everywhere)
                await control.SendAsync(ControlMessage.SetParticipantMute(user, null, true), ct).ConfigureAwait(false);
            foreach (var kv in volumes)
                await control.SendAsync(ControlMessage.SetParticipantVolume(kv.Key, kv.Value), ct).ConfigureAwait(false);
        }

        /// <summary>Channel-scoped mutes need membership, so they are re-sent after each successful join.</summary>
        private async Task ReplayChannelMutesAsync(Guid channelId, CancellationToken ct)
        {
            var control = _control;
            if (control == null) return;
            var users = new List<Guid>();
            lock (_localMutes)
                foreach (var kv in _localMutes)
                    if (kv.Value.Contains(channelId) && !kv.Value.Contains(Guid.Empty)) users.Add(kv.Key);
            foreach (var user in users)
                await control.SendAsync(ControlMessage.SetParticipantMute(user, channelId, true), ct).ConfigureAwait(false);
        }

        /// <summary>
        /// Send one encoded Opus frame (20 ms @ 48 kHz recommended) to a channel. No-op while muted.
        /// <paramref name="channelHash"/> comes from <see cref="ChannelHash"/>.
        /// </summary>
        public void SendOpusFrame(uint channelHash, byte[] opus, int length = -1, int samplesPerChannel = Audio.AudioFormat.FrameSamples)
        {
            if (_media == null || State != VoiceConnectionState.MediaBound) return;
            _rtpTimestamp = unchecked(_rtpTimestamp + (uint)samplesPerChannel);
            if (_muted) return;
            _media.SendAudio(channelHash, _rtpTimestamp, opus, length);
        }

        public static uint ChannelHash(Guid channelId) => AurxPacket.ChannelIdHash(channelId);

        public Task UpdatePositionAsync(Guid channelId, Guid selfUserId, Position3D position, Orientation3D orientation, CancellationToken ct = default)
        {
            EnsureConnected();
            return _control.SendAsync(ControlMessage.PositionUpdate(channelId, selfUserId, position, orientation), ct);
        }

        public Task RespondToRecordingAsync(Guid recordingId, RecordingConsent consent, CancellationToken ct = default)
        {
            EnsureConnected();
            return _control.SendAsync(ControlMessage.RecordingConsentResponse(recordingId, consent), ct);
        }

        /// <summary>Report link quality (also sent over UDP so the SFU can adapt the downlink bitrate).</summary>
        public Task ReportQualityAsync(float rttMs, float jitterMs, float lossPercent, CancellationToken ct = default)
        {
            EnsureConnected();
            _media?.SendQualityReport(rttMs, jitterMs, lossPercent);
            return _control.SendAsync(ControlMessage.QualityReport(rttMs, jitterMs, lossPercent), ct);
        }

        public IReadOnlyList<Participant> GetParticipants(Guid channelId)
        {
            lock (_channels)
                return _channels.TryGetValue(channelId, out var m) ? new List<Participant>(m.Values) : new List<Participant>();
        }

        public Participant FindBySsrc(uint ssrc)
        {
            lock (_channels) return _bySsrc.TryGetValue(ssrc, out var p) ? p : null;
        }

        /// <summary>Non-event alternative to <see cref="OnAudio"/> for audio-thread consumers.</summary>
        public bool TryDequeueAudio(out IncomingAudio audio)
        {
            if (_media != null) return _media.TryDequeueAudio(out audio);
            audio = default;
            return false;
        }

        /// <summary>
        /// Pump control messages and raise events on the calling thread. Call every frame.
        /// When <paramref name="drainAudioToEvent"/> is true, queued downlink frames are delivered via <see cref="OnAudio"/>.
        /// </summary>
        public void Update(bool drainAudioToEvent = false)
        {
            lock (_mainThreadQueue)
                while (_mainThreadQueue.Count > 0) _mainThreadQueue.Dequeue()();

            _control?.Drain(HandleMessage);

            if (drainAudioToEvent && _media != null)
            {
                while (_media.TryDequeueAudio(out var a)) OnAudio?.Invoke(FindBySsrc(a.SenderSsrc), a);
            }

            var control = _control;
            if (control != null && control.IsOpen && PingInterval > TimeSpan.Zero)
            {
                var now = DateTime.UtcNow;
                if (now - _lastPing > PingInterval)
                {
                    _lastPing = now;
                    var nonce = (ulong)(DateTimeOffset.UtcNow.ToUnixTimeMilliseconds() & 0x1F_FFFF_FFFF_FFFF);
                    _ = control.SendAsync(ControlMessage.Ping(nonce));
                }
                // Two unanswered pings: the socket is half-open, treat it as lost so a reconnect can start.
                if (_lastPong < _lastPing && now - _lastPong > PingInterval + PingInterval + PingInterval)
                {
                    control.Dispose();
                    HandleClosed(control, "ping timeout");
                }
            }
        }

        public async Task DisconnectAsync(string reason = "client disconnect")
        {
            _closedByUser = true;
            var control = _control;
            Teardown();
            if (control != null) await control.CloseAsync(reason).ConfigureAwait(false);
            SetState(VoiceConnectionState.Disconnected);
            OnDisconnected?.Invoke(reason);
        }

        public void Dispose()
        {
            _closedByUser = true;
            Teardown();
        }

        // ---- internals ----------------------------------------------------------------------

        private void EnsureConnected()
        {
            if (_control == null || !_control.IsOpen) throw new InvalidOperationException("not connected");
        }

        private void Post(Action a)
        {
            lock (_mainThreadQueue) _mainThreadQueue.Enqueue(a);
        }

        private void SetState(VoiceConnectionState s)
        {
            if (State == s) return;
            State = s;
            Post(() => OnStateChanged?.Invoke(s));
        }

        private void Teardown()
        {
            _cts?.Cancel();
            _skipBackoff?.Cancel();
            _media?.Dispose();
            _media = null;
            _control?.Dispose();
            _control = null;
            _resumeToken = null;
            _joinTcs?.TrySetException(new OperationCanceledException("disconnected"));
            lock (_channels) { _channels.Clear(); _bySsrc.Clear(); _joinedChannels.Clear(); }
        }

        /// <summary>Runs on the Update thread when a control channel closes or fails.</summary>
        private void HandleClosed(ControlChannel control, string reason)
        {
            if (!ReferenceEquals(control, _control)) return; // stale channel from before a reconnect
            _control = null;
            control.Dispose();
            _joinTcs?.TrySetException(new System.IO.IOException("connection lost"));
            if (_closedByUser || State == VoiceConnectionState.Disconnected) return;
            if (!AutoReconnect || Session == null || _cts == null || _cts.IsCancellationRequested)
            {
                Teardown();
                SetState(VoiceConnectionState.Failed);
                OnDisconnected?.Invoke(reason);
                return;
            }
            SetState(VoiceConnectionState.Reconnecting);
            // A loop may already be running (the socket dropped again mid-attempt); it will retry on its own.
            if (Interlocked.CompareExchange(ref _reconnectLoopActive, 1, 0) == 0)
                _ = ReconnectLoopAsync(reason, _cts.Token);
        }

        private async Task ReconnectLoopAsync(string cause, CancellationToken ct)
        {
            try { await ReconnectAttemptsAsync(cause, ct).ConfigureAwait(false); }
            finally { Interlocked.Exchange(ref _reconnectLoopActive, 0); }
        }

        private async Task ReconnectAttemptsAsync(string cause, CancellationToken ct)
        {
            Exception last = new System.IO.IOException(cause);
            for (int attempt = 1; attempt <= Reconnect.MaxAttempts; attempt++)
            {
                var delay = Backoff(attempt);
                Post(() => OnRecovering?.Invoke(attempt, delay, cause));
                using (var skip = CancellationTokenSource.CreateLinkedTokenSource(ct))
                {
                    _skipBackoff = skip;
                    try { await Task.Delay(delay, skip.Token).ConfigureAwait(false); }
                    catch (OperationCanceledException) { if (ct.IsCancellationRequested) return; }
                    finally { _skipBackoff = null; }
                }
                if (_closedByUser) return;
                try
                {
                    await ReattachAsync(ct).ConfigureAwait(false);
                    return;
                }
                catch (OperationCanceledException) when (ct.IsCancellationRequested) { return; }
                catch (Exception e)
                {
                    last = e;
                    cause = e.Message;
                    var stale = _control;
                    _control = null;
                    stale?.Dispose();
                    SetState(VoiceConnectionState.Reconnecting);
                }
            }
            if (_closedByUser) return;
            var error = new System.IO.IOException($"reconnect failed after {Reconnect.MaxAttempts} attempts ({last.Message})", last);
            Post(() =>
            {
                if (_closedByUser) return;
                OnFailedToRecover?.Invoke(error);
                Teardown();
                SetState(VoiceConnectionState.Failed);
                OnDisconnected?.Invoke(error.Message);
            });
        }

        private TimeSpan Backoff(int attempt)
        {
            double ms = Reconnect.InitialDelay.TotalMilliseconds * Math.Pow(Reconnect.Factor, attempt - 1);
            ms = Math.Min(ms, Reconnect.MaxDelay.TotalMilliseconds);
            double jitter;
            lock (_random) jitter = (_random.NextDouble() * 2 - 1) * Reconnect.Jitter;
            return TimeSpan.FromMilliseconds(Math.Max(0, ms * (1 + jitter)));
        }

        /// <summary>
        /// One reconnect attempt: resume the previous session if the server still holds it, otherwise
        /// accept the fresh session and re-join the channels we were in. Rebinds media either way,
        /// since the UDP path may have changed with the network.
        /// </summary>
        private async Task ReattachAsync(CancellationToken ct)
        {
            var previous = Session;
            string resume = previous != null && _resumeToken != null ? $"{previous.SessionId:D}.{_resumeToken}" : null;
            var info = await OpenSessionAsync(resume, ct).ConfigureAwait(false);
            List<Guid> rejoin = null;
            if (!info.Resumed)
            {
                // New session: the old memberships are gone on the server; drop the stale rosters and re-join.
                lock (_channels)
                {
                    rejoin = new List<Guid>(_joinedChannels);
                    _joinedChannels.Clear();
                    _channels.Clear();
                    _bySsrc.Clear();
                }
                foreach (var ch in rejoin) { var id = ch; Post(() => OnChannelLeft?.Invoke(id)); }
                await ReplayReceiverPrefsAsync(ct).ConfigureAwait(false);
            }
            await BindMediaAsync(info, ct).ConfigureAwait(false);
            if (rejoin != null)
                foreach (var ch in rejoin) await JoinChannelAsync(ch, ct).ConfigureAwait(false);
            Post(() => OnRecovered?.Invoke(info));
        }

        private void HandleMessage(ControlMessage m)
        {
            OnControlMessage?.Invoke(m);
            switch (m.Type)
            {
                case "ChannelJoinAck":
                {
                    var channelId = m.Id("channel_id");
                    var roster = new List<Participant>();
                    lock (_channels)
                    {
                        var map = new Dictionary<Guid, Participant>();
                        foreach (var b in m.Participants())
                        {
                            var p = new Participant
                            {
                                UserId = b.UserId, DisplayName = b.DisplayName, Ssrc = b.Ssrc,
                                Role = b.Role, IsMuted = b.IsMuted, IsSpeaking = b.IsSpeaking,
                            };
                            map[p.UserId] = p;
                            _bySsrc[p.Ssrc] = p;
                            roster.Add(p);
                        }
                        _channels[channelId] = map;
                        _joinedChannels.Add(channelId);
                    }
                    OnChannelJoined?.Invoke(channelId, roster);
                    break;
                }
                case "ParticipantJoined":
                {
                    var channelId = m.Id("channel_id");
                    var p = new Participant
                    {
                        UserId = m.Id("user_id"), DisplayName = m.Str("display_name") ?? string.Empty,
                        Ssrc = m.U32("ssrc"), Role = ChannelRole.Speaker,
                    };
                    lock (_channels)
                    {
                        if (!_channels.TryGetValue(channelId, out var map)) _channels[channelId] = map = new Dictionary<Guid, Participant>();
                        map[p.UserId] = p;
                        _bySsrc[p.Ssrc] = p;
                    }
                    OnParticipantJoined?.Invoke(channelId, p);
                    break;
                }
                case "ParticipantLeft":
                {
                    var channelId = m.Id("channel_id");
                    Participant p = null;
                    lock (_channels)
                    {
                        if (_channels.TryGetValue(channelId, out var map) && map.TryGetValue(m.Id("user_id"), out p))
                        {
                            map.Remove(p.UserId);
                            _bySsrc.Remove(p.Ssrc);
                            _media?.ForgetSender(p.Ssrc);
                        }
                    }
                    if (p != null) OnParticipantLeft?.Invoke(channelId, p);
                    break;
                }
                case "MuteStateChanged":
                {
                    var p = Lookup(m.Id("channel_id"), m.Id("user_id"));
                    if (p == null) break;
                    p.IsMuted = m.Bool("muted");
                    p.IsServerMuted = m.Bool("server_muted");
                    OnParticipantUpdated?.Invoke(m.Id("channel_id"), p);
                    break;
                }
                case "SpeakingStateChanged":
                {
                    var p = Lookup(m.Id("channel_id"), m.Id("user_id"));
                    if (p == null) break;
                    p.IsSpeaking = m.Bool("speaking");
                    OnSpeaking?.Invoke(m.Id("channel_id"), p, p.IsSpeaking);
                    break;
                }
                case "PositionUpdate":
                    OnPositions?.Invoke(m.Id("channel_id"), m.Positions());
                    break;
                case "RecordingNotification":
                    OnRecording?.Invoke(new RecordingNotice
                    {
                        ChannelId = m.Id("channel_id"), RecordingId = m.Id("recording_id"),
                        Active = m.Bool("active"), InitiatedBy = m.Id("initiated_by"),
                    });
                    break;
                case "BitrateCommand":
                    OnBitrateCommand?.Invoke(m.U32("target_bitrate_kbps"), m.Str("reason") ?? string.Empty);
                    break;
                case "Kick":
                {
                    var channelId = m.Id("channel_id");
                    lock (_channels) { _channels.Remove(channelId); _joinedChannels.Remove(channelId); }
                    OnKicked?.Invoke(channelId, m.Str("reason") ?? string.Empty);
                    break;
                }
                case "UserBlockChanged":
                {
                    var userId = m.Id("user_id");
                    var blocked = m.Bool("blocked");
                    lock (_blockedUsers) { if (blocked) _blockedUsers.Add(userId); else _blockedUsers.Remove(userId); }
                    OnUserBlockChanged?.Invoke(userId, blocked);
                    break;
                }
                case "ReceiverPreferences":
                {
                    var prefs = m.ReceiverPreferences();
                    lock (_blockedUsers) { _blockedUsers.Clear(); foreach (var u in prefs.BlockedUsers) _blockedUsers.Add(u); }
                    // A resumed session still holds our mutes/volumes; merge what the server reports.
                    lock (_localMutes)
                        foreach (var lm in prefs.LocalMutes)
                        {
                            if (!_localMutes.TryGetValue(lm.UserId, out var scopes)) _localMutes[lm.UserId] = scopes = new HashSet<Guid>();
                            scopes.Add(lm.ChannelId ?? Guid.Empty);
                        }
                    lock (_volumes)
                        foreach (var v in prefs.Volumes)
                        {
                            if (v.Volume == 1f) _volumes.Remove(v.UserId);
                            else _volumes[v.UserId] = v.Volume;
                        }
                    OnReceiverPreferences?.Invoke(prefs);
                    break;
                }
                case "SessionClose":
                {
                    var reason = m.Str("reason") ?? "session closed";
                    OnSessionClosed?.Invoke(reason);
                    _ = DisconnectAsync(reason);
                    break;
                }
                case "Error":
                    OnServerError?.Invoke(m.Str("code") ?? "error", m.Str("message") ?? string.Empty);
                    break;
                case "Pong":
                {
                    var sent = (long)m.Num("nonce");
                    ControlRttMs = Math.Max(0, DateTimeOffset.UtcNow.ToUnixTimeMilliseconds() - sent);
                    _lastPong = DateTime.UtcNow;
                    break;
                }
                default:
                    break; // ChannelLeave acks, MediaBound, occlusion/reverb hints etc. are informational
            }
        }

        /// <summary>
        /// Runs on the network thread so <see cref="JoinChannelAsync"/> completes even when the host is
        /// not yet pumping <see cref="Update"/>. Roster state is still applied in <see cref="HandleMessage"/>.
        /// </summary>
        private void CompletePendingJoin(ControlMessage m)
        {
            var tcs = _joinTcs;
            var pending = _pendingChannelJoin;
            if (tcs == null || pending == null) return;
            if (m.Type == "ChannelJoinAck" && m.Id("channel_id") == pending.Value)
            {
                var roster = new List<Participant>();
                foreach (var b in m.Participants())
                    roster.Add(new Participant
                    {
                        UserId = b.UserId, DisplayName = b.DisplayName, Ssrc = b.Ssrc,
                        Role = b.Role, IsMuted = b.IsMuted, IsSpeaking = b.IsSpeaking,
                    });
                tcs.TrySetResult(roster);
            }
            else if (m.Type == "Error")
            {
                tcs.TrySetException(new InvalidOperationException($"{m.Str("code")}: {m.Str("message")}"));
            }
        }

        private Participant Lookup(Guid channelId, Guid userId)
        {
            lock (_channels)
                return _channels.TryGetValue(channelId, out var map) && map.TryGetValue(userId, out var p) ? p : null;
        }
    }
}
