using System;
using System.Collections.Generic;
using System.Threading;
using System.Threading.Tasks;
using Aurix.Protocol;
using Aurix.Transport;

namespace Aurix
{
    public enum VoiceConnectionState { Disconnected, Connecting, Connected, MediaBound, Failed }

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
        private ControlChannel _control;
        private MediaTransport _media;
        private CancellationTokenSource _cts;
        private DateTime _lastPing = DateTime.MinValue;
        private uint _rtpTimestamp;
        private bool _muted;
        private Guid? _pendingChannelJoin;
        private TaskCompletionSource<List<Participant>> _joinTcs;

        public VoiceConnectionState State { get; private set; } = VoiceConnectionState.Disconnected;
        public SessionInfo Session { get; private set; }
        public bool IsMuted => _muted;
        public MediaTransport Media => _media;
        /// <summary>Wall-clock RTT of the last WebSocket ping, in ms.</summary>
        public float ControlRttMs { get; private set; }
        public TimeSpan PingInterval { get; set; } = TimeSpan.FromSeconds(15);
        public TimeSpan RequestTimeout { get; set; } = TimeSpan.FromSeconds(10);

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
        public event Action<string, string> OnServerError;
        public event Action<string> OnDisconnected;
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
            SetState(VoiceConnectionState.Connecting);
            try
            {
                _control = new ControlChannel();
                _control.Closed += reason => Post(() => HandleClosed(reason));
                _control.Received += CompletePendingJoin;
                await _control.ConnectAsync(new Uri(_wsUrl), _token, _cts.Token).ConfigureAwait(false);

                ControlMessage ack;
                while (true)
                {
                    ack = await _control.NextAsync(RequestTimeout, _cts.Token).ConfigureAwait(false);
                    if (ack.Type == "SessionInitAck") break;
                    if (ack.Type == "Error") throw new InvalidOperationException($"{ack.Str("code")}: {ack.Str("message")}");
                }
                var info = new SessionInfo
                {
                    SessionId = ack.Id("session_id"),
                    Ssrc = ack.U32("ssrc"),
                    MediaAddr = ack.Str("media_addr"),
                };
                var mediaKey = Convert.FromBase64String(ack.Str("media_key") ?? throw new InvalidOperationException("SessionInitAck without media_key"));
                Session = info;
                SetState(VoiceConnectionState.Connected);
                Post(() => OnSessionReady?.Invoke(info));

                var endpoint = await MediaTransport.ResolveAsync(info.MediaAddr).ConfigureAwait(false);
                _media = new MediaTransport(endpoint, info.SessionId, info.Ssrc, mediaKey);
                await _media.BindAsync(_cts.Token).ConfigureAwait(false);
                SetState(VoiceConnectionState.MediaBound);
                return info;
            }
            catch
            {
                SetState(VoiceConnectionState.Failed);
                Teardown();
                throw;
            }
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
                try { return await tcs.Task.ConfigureAwait(false); }
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

            if (_control != null && _control.IsOpen && DateTime.UtcNow - _lastPing > PingInterval)
            {
                _lastPing = DateTime.UtcNow;
                var nonce = (ulong)(DateTimeOffset.UtcNow.ToUnixTimeMilliseconds() & 0x1F_FFFF_FFFF_FFFF);
                _ = _control.SendAsync(ControlMessage.Ping(nonce));
            }
        }

        public async Task DisconnectAsync(string reason = "client disconnect")
        {
            var control = _control;
            Teardown();
            if (control != null) await control.CloseAsync(reason).ConfigureAwait(false);
            SetState(VoiceConnectionState.Disconnected);
            OnDisconnected?.Invoke(reason);
        }

        public void Dispose() => Teardown();

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
            _media?.Dispose();
            _media = null;
            _control?.Dispose();
            _control = null;
            _joinTcs?.TrySetException(new OperationCanceledException("disconnected"));
            lock (_channels) { _channels.Clear(); _bySsrc.Clear(); }
        }

        private void HandleClosed(string reason)
        {
            if (State == VoiceConnectionState.Disconnected) return;
            Teardown();
            SetState(VoiceConnectionState.Disconnected);
            OnDisconnected?.Invoke(reason);
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
                    lock (_channels) _channels.Remove(channelId);
                    OnKicked?.Invoke(channelId, m.Str("reason") ?? string.Empty);
                    break;
                }
                case "SessionClose":
                    _ = DisconnectAsync(m.Str("reason") ?? "session closed");
                    break;
                case "Error":
                    OnServerError?.Invoke(m.Str("code") ?? "error", m.Str("message") ?? string.Empty);
                    break;
                case "Pong":
                {
                    var sent = (long)m.Num("nonce");
                    ControlRttMs = Math.Max(0, DateTimeOffset.UtcNow.ToUnixTimeMilliseconds() - sent);
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
