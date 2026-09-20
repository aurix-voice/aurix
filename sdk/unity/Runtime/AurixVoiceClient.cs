using System;
using System.Collections.Generic;
using System.Security.Cryptography;
using System.Threading;
using System.Threading.Tasks;
using Aurix.Audio;
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
        /// <summary>Last reported audio energy 0..1 (from <c>ChannelEnergy</c>; decays to 0 when silent).</summary>
        public float Energy;
        /// <summary>
        /// Priority speaker: while they talk the node ducks every non-priority voice in the channel
        /// (<see cref="ChannelInfo.Ducking"/>) and <see cref="AurixVoiceClient.OnDuckingChanged"/> fires for your game audio.
        /// </summary>
        public bool IsPriority;
    }

    /// <summary>
    /// How far presence and text reach in a positional channel (<c>PositionalConfig.roster_radius</c> /
    /// <c>text_radius</c>, from <c>ChannelJoinAck</c>). <c>null</c> = the whole channel.
    /// </summary>
    public struct ChannelScope
    {
        /// <summary>
        /// With a roster radius the roster only lists members within this distance of you (once both
        /// positions are known); <see cref="AurixVoiceClient.OnParticipantJoined"/> / <see cref="AurixVoiceClient.OnParticipantLeft"/>
        /// also fire when someone moves in or out of range (leaving uses a 10 % wider radius so the edge does not flicker).
        /// </summary>
        public float? RosterRadius;
        /// <summary>Channel chat, typing and transcripts reach only members within this distance.</summary>
        public float? TextRadius;
    }

    /// <summary>What the server told us about a joined channel and our place in it (<c>ChannelJoinAck</c>).</summary>
    public struct ChannelInfo
    {
        /// <summary>
        /// Our role; <see cref="ChannelRole.Listener"/> means receive-only (the grant had <c>speak: false</c>),
        /// whatever the channel type — the microphone is not sent there.
        /// </summary>
        public ChannelRole Role;
        /// <summary>
        /// Members across all nodes, including listeners hidden from <see cref="AurixVoiceClient.GetParticipants"/>
        /// (<see cref="HiddenListeners"/>). A snapshot taken at join time.
        /// </summary>
        public uint ParticipantCount;
        /// <summary>
        /// Receive-only listeners are absent from the roster and never announced by
        /// <see cref="AurixVoiceClient.OnParticipantJoined"/> / <see cref="AurixVoiceClient.OnParticipantLeft"/>
        /// (<c>ChannelConfig.audience.hide_listeners</c>); show <see cref="ParticipantCount"/> for the audience size.
        /// </summary>
        public bool HiddenListeners;
        /// <summary>Speech in the channel is transcribed server-side (<see cref="AurixVoiceClient.OnTranscript"/>).</summary>
        public bool Transcription;
        /// <summary>Speech in the channel is analysed by the operator's content-safety pipeline.</summary>
        public bool SafetyVoice;
        /// <summary>
        /// Priority-speaker ducking the node applies in this channel; <c>null</c> when the channel has none
        /// (then <see cref="AurixVoiceClient.SetPriorityAsync"/> is rejected and nobody is ever a priority speaker).
        /// </summary>
        public DuckingConfig? Ducking;
        /// <summary>Whether we are a priority speaker here (the grant's <c>priority</c> or a later <c>SetPriority</c>).</summary>
        public bool IsPriority;

        /// <summary>Whether this session may transmit in the channel.</summary>
        public bool CanSpeak => Role != ChannelRole.Listener;
    }

    /// <summary>A queued text-to-speech request; see <see cref="AurixVoiceClient.SpeakAsync"/>.</summary>
    public sealed class SpeechRequest
    {
        public Guid RequestId;
        public string ClientRef;
        /// <summary>Completes with the terminal status (finished, cancelled or failed).</summary>
        public Task<TtsStatus> Done;
    }

    public sealed class SessionInfo
    {
        public Guid SessionId;
        public uint Ssrc;
        /// <summary>Primary native media endpoint (<c>host:port</c>, IPv6 hosts bracketed).</summary>
        public string MediaAddr;
        /// <summary>
        /// Every public media endpoint of the node, IPv4 first then IPv6 (contains at least <see cref="MediaAddr"/>).
        /// The SDK tries them in order when binding UDP and sticks with the family that answered.
        /// </summary>
        public IReadOnlyList<string> MediaAddrs = Array.Empty<string>();
        /// <summary>True when this is the same session as before a connection loss (same SSRC, channels kept).</summary>
        public bool Resumed;
        /// <summary>
        /// The resume landed on a different node than the one that created the session (the previous node
        /// stopped answering): same session id and SSRC, new media key and endpoint — media is rebound and
        /// the playout buffers re-synchronised by the SDK.
        /// </summary>
        public bool Migrated;
        /// <summary>WebSocket URL of the node serving this session.</summary>
        public string Endpoint;
        /// <summary>Other nodes the server advertised for failover, tried in order after <see cref="Endpoint"/> on a reconnect.</summary>
        public IReadOnlyList<string> Failover = Array.Empty<string>();
        /// <summary>The node accepts AURX media as binary frames on the control WebSocket (UDP fallback).</summary>
        public bool MediaTunnel;
        /// <summary>
        /// The node can deliver one server-mixed stream per channel
        /// (<see cref="AurixVoiceClient.SetDownlinkModeAsync"/> with <see cref="DownlinkMode.Mixed"/>).
        /// </summary>
        public bool DownlinkMix;
        /// <summary>
        /// The node translates transcripts on request (<see cref="AurixVoiceClient.SetTranslationAsync"/>);
        /// null when the operator has not configured translation.
        /// </summary>
        public TranslationInfo Translation;
    }

    public sealed class RecordingNotice
    {
        public Guid ChannelId;
        public Guid RecordingId;
        public bool Active;
        public Guid InitiatedBy;
        /// <summary>Real-time stream to an operator service rather than a stored file (same consent flow).</summary>
        public bool Live;
    }

    /// <summary>Server request to change the uplink bitrate (adaptive bitrate over the quality reports).</summary>
    public struct BitrateCommand
    {
        public uint TargetBitrateKbps;
        public string Reason;
        /// <summary>Loss the server observed, for tuning the encoder's FEC (<c>0..100</c>).</summary>
        public int ExpectedLossPercent;
        public int TargetBitrateBps => (int)Math.Min(TargetBitrateKbps * 1000UL, int.MaxValue);
    }

    /// <summary>
    /// High-level Aurix client for .NET / Unity: WebSocket control plane + native AURX/UDP media.
    /// Thread model: network I/O runs on background tasks; all events are raised from
    /// <see cref="Update"/>, which the host must call periodically (e.g. from a MonoBehaviour's
    /// Update). Audio frames are exposed through <see cref="TryDequeueAudio"/> / <see cref="OnAudio"/>.
    /// Not available in Unity WebGL players (no UDP, no raw WebSocket): use
    /// <c>Aurix.WebGL.AurixWebGLVoiceClient</c> there — both implement <see cref="IAurixVoiceClient"/>.
    /// </summary>
    public sealed class AurixVoiceClient : IAurixVoiceClient
    {
        public const string SdkVersion = "1.2.0";

        private readonly string _wsUrl;
        private string _token;
        private Guid _localUserId;
        private readonly Dictionary<Guid, Dictionary<Guid, Participant>> _channels = new Dictionary<Guid, Dictionary<Guid, Participant>>();
        private readonly Dictionary<uint, Participant> _bySsrc = new Dictionary<uint, Participant>();
        private readonly Queue<Action> _mainThreadQueue = new Queue<Action>();
        private readonly HashSet<Guid> _joinedChannels = new HashSet<Guid>();
        /// <summary>Receiver-local mutes held by this client: user → channels (Guid.Empty = every channel).</summary>
        private readonly Dictionary<Guid, HashSet<Guid>> _localMutes = new Dictionary<Guid, HashSet<Guid>>();
        private readonly Dictionary<Guid, float> _volumes = new Dictionary<Guid, float>();
        private readonly HashSet<Guid> _blockedUsers = new HashSet<Guid>();
        private TransmissionMode _transmission = TransmissionMode.All;
        private Guid? _focusChannel;
        private readonly Random _random = new Random();
        private ControlChannel _control;
        private MediaTransport _media;
        private CancellationTokenSource _cts;
        private DateTime _lastPing = DateTime.MinValue;
        private DateTime _lastPong = DateTime.MinValue;
        private DateTime _probeDeadline = DateTime.MaxValue;
        private ulong _probeNonce;
        private uint _rtpTimestamp;
        private bool _muted;
        private Guid? _pendingChannelJoin;
        private TaskCompletionSource<List<Participant>> _joinTcs;
        private string _pendingModeration;
        private TaskCompletionSource<bool> _moderationTcs;
        /// <summary>client_ref → pending <see cref="SendMessageAsync"/>/<see cref="SendDirectMessageAsync"/>.</summary>
        private readonly Dictionary<string, TaskCompletionSource<ChatMessage>> _pendingChat = new Dictionary<string, TaskCompletionSource<ChatMessage>>();
        private int _chatRefCounter;
        /// <summary>client_ref → pending <see cref="HistoryAsync"/>.</summary>
        private readonly Dictionary<string, TaskCompletionSource<ChatHistoryPage>> _pendingHistory = new Dictionary<string, TaskCompletionSource<ChatHistoryPage>>();
        /// <summary>scope key → pending <see cref="ReadMarkersAsync"/> calls, settled in request order.</summary>
        private readonly Dictionary<string, Queue<TaskCompletionSource<ChatReadMarkers>>> _pendingReadMarkers = new Dictionary<string, Queue<TaskCompletionSource<ChatReadMarkers>>>();
        /// <summary>channel → time of the last <c>ChatTyping {typing: true}</c> sent.</summary>
        private readonly Dictionary<Guid, DateTime> _typingSentAt = new Dictionary<Guid, DateTime>();
        /// <summary>client_ref → pending <see cref="SpeakAsync"/> (settled by the <c>queued</c> status or an <c>Error</c>).</summary>
        private readonly Dictionary<string, TaskCompletionSource<SpeechRequest>> _pendingSpeak = new Dictionary<string, TaskCompletionSource<SpeechRequest>>();
        /// <summary>client_ref → completion of <see cref="SpeechRequest.Done"/> for requests still in flight.</summary>
        private readonly Dictionary<string, TaskCompletionSource<TtsStatus>> _speechDone = new Dictionary<string, TaskCompletionSource<TtsStatus>>();
        private int _speakRefCounter;
        private bool _wantTranscripts = true;
        private TranslationPrefs _translation = new TranslationPrefs();
        private AudioCodec _preferredCodec = AudioCodec.Opus;
        private AudioCodec _activeCodec = AudioCodec.Opus;
        private DownlinkMode _preferredDownlink = DownlinkMode.Streams;
        private DownlinkMode _activeDownlink = DownlinkMode.Streams;
        private readonly HashSet<Guid> _transcribedChannels = new HashSet<Guid>();
        private readonly HashSet<Guid> _monitoredChannels = new HashSet<Guid>();
        private readonly Dictionary<Guid, ChannelScope> _channelScopes = new Dictionary<Guid, ChannelScope>();
        private readonly Dictionary<Guid, ChannelInfo> _channelInfos = new Dictionary<Guid, ChannelInfo>();
        /// <summary>Channels another member's priority speech is ducking right now (see <see cref="RefreshDucking"/>).</summary>
        private readonly HashSet<Guid> _duckedChannels = new HashSet<Guid>();
        /// <summary>Audio policy of every joined channel (from <c>ChannelJoinAck</c> / <c>ChannelAudioPolicy</c>).</summary>
        private readonly Dictionary<Guid, AudioPolicy> _channelPolicies = new Dictionary<Guid, AudioPolicy>();
        private AudioPolicy? _audioPolicy;
        private OpusEncoderSettings _encoderSettings = OpusEncoderSettings.Default;
        private int? _complexityPin;
        private bool _followChannelPolicy = true;
        private BitrateCommand? _bitrateCommand;
        private IOpusCodec _encoder;
        private OpusEncoderSettings? _appliedEncoderSettings;
        private byte[] _mediaKey;
        private string _resumeToken;
        private uint _lastMediaSequence;
        /// <summary>UDP failed to bind or its heartbeats died at least until here: Auto sessions opened before go straight to the tunnel.</summary>
        private DateTime? _udpBlockedUntil;
        /// <summary>Address family of the last UDP candidate that answered a <c>SessionBind</c>; tried first next time.</summary>
        private System.Net.Sockets.AddressFamily? _udpFamilyHint;
        private DateTime _nextUdpProbe = DateTime.MaxValue;
        private int _pathSwitchActive;
        private bool _closedByUser;
        private CancellationTokenSource _skipBackoff;
        private int _reconnectLoopActive;
        private readonly LossWindow _lossWindow = new LossWindow();
        private DateTime _lastQualityReport = DateTime.MinValue;
        private NetworkQuality? _serverQuality;
        private E2eeGroup _e2ee = new E2eeGroup(E2eeIdentityKey.Generate());
        private DateTime? _e2eeRotateDue;
        private long _framesE2ee;
        private long _e2eeUndecryptable;
        private static readonly TimeSpan E2eeRotateDebounce = TimeSpan.FromMilliseconds(200);

        public VoiceConnectionState State { get; private set; } = VoiceConnectionState.Disconnected;
        public SessionInfo Session { get; private set; }
        public bool IsMuted => _muted;
        public MediaTransport Media => _media;
        /// <summary>
        /// How the native media reaches the node: <see cref="MediaPathPolicy.Auto"/> (default) uses UDP and
        /// falls back to the same sealed packets as binary frames on the control WebSocket when UDP does not
        /// bind (2 × 1 s) or <see cref="UdpFallbackLostHeartbeats"/> heartbeats go unanswered, re-probing UDP
        /// every <see cref="UdpReprobeInterval"/> while tunnelled. The tunnel is TCP: expect more latency under
        /// loss (head-of-line blocking), but voice keeps working where only 443 gets through. Applies to the
        /// next bind (connect / reconnect / fallback).
        /// </summary>
        public MediaPathPolicy MediaPathPolicy { get; set; } = MediaPathPolicy.Auto;
        /// <summary>Consecutive unanswered UDP heartbeats (every <see cref="MediaHeartbeatInterval"/>) before Auto moves to the tunnel; 0 disables.</summary>
        public int UdpFallbackLostHeartbeats { get; set; } = 3;
        /// <summary>How often a tunnelled Auto session tries UDP again (and moves back when it answers); zero disables.</summary>
        public TimeSpan UdpReprobeInterval { get; set; } = TimeSpan.FromSeconds(30);
        /// <summary>Media heartbeat cadence (RTT samples, liveness, fallback detection). Applies to the next bind.</summary>
        public TimeSpan MediaHeartbeatInterval { get; set; } = TimeSpan.FromSeconds(5);
        /// <summary>Link the media currently uses (<see cref="MediaPath.None"/> before the first bind).</summary>
        public MediaPath ActiveMediaPath => _media?.Path ?? MediaPath.None;
        /// <summary>
        /// The playout mixer whose jitter-buffer counters feed <see cref="GetStats"/> and the periodic
        /// quality report (set by <c>AurixVoiceBehaviour</c>; assign it yourself when driving the mixer manually).
        /// </summary>
        public RemoteMixer Mixer
        {
            get => _mixer;
            set
            {
                _mixer = value;
                if (value != null) lock (_capture) value.VisemesEnabled = _visemesEnabled;
            }
        }
        private RemoteMixer _mixer;
        private readonly object _capture = new object();
        private bool _visemesEnabled;
        private NativeVisemeAnalyzer _localVisemes;
        private NativeVoiceEffects _voiceEffects;
        private VoiceEffectParams _voiceEffectParams;
        /// <summary>Wall-clock RTT of the last WebSocket ping, in ms.</summary>
        public float ControlRttMs { get; private set; }
        public TimeSpan PingInterval { get; set; } = TimeSpan.FromSeconds(15);
        /// <summary>
        /// How often <see cref="Update"/> samples statistics (raising <see cref="OnStats"/>) and sends a
        /// <c>QualityReport</c> that drives the server's adaptive bitrate and <see cref="LastNetworkQuality"/>.
        /// Default 5 s; <see cref="TimeSpan.Zero"/> disables (call <see cref="ReportQualityAsync()"/> yourself).
        /// </summary>
        public TimeSpan QualityReportInterval { get; set; } = TimeSpan.FromSeconds(5);
        /// <summary>Last server-side quality report (both directions, <c>Bars</c> 1–5), or null until one arrived.</summary>
        public NetworkQuality? LastNetworkQuality => _serverQuality;
        public TimeSpan RequestTimeout { get; set; } = TimeSpan.FromSeconds(10);
        /// <summary>
        /// Reconnect automatically after an unexpected connection loss, resuming the same session when
        /// the server still holds it (see <see cref="ResumeGrace"/>) and re-joining channels otherwise.
        /// Never triggers after <see cref="DisconnectAsync"/> or a server-initiated <c>SessionClose</c>.
        /// </summary>
        public bool AutoReconnect { get; set; } = true;
        public ReconnectPolicy Reconnect { get; } = new ReconnectPolicy();
        /// <summary>
        /// Called before every reconnect attempt to obtain a fresh credential. Required when the client
        /// was constructed with a one-time <c>login</c> action token: it is spent by the first connection
        /// (and expires within its short TTL), so a fresh session after the resume window needs a new one.
        /// </summary>
        public Func<CancellationToken, Task<string>> TokenRefresher { get; set; }
        /// <summary>
        /// Called by <see cref="JoinChannelAsync(Guid, CancellationToken)"/> to obtain a one-time <c>join</c>
        /// action token for the channel. Required when the server enforces <c>auth.require_action_tokens</c>;
        /// without it the session credential itself authorises the join.
        /// </summary>
        public Func<Guid, CancellationToken, Task<string>> JoinTokenProvider { get; set; }
        /// <summary>How long the server keeps a dropped session resumable (zero = resume disabled).</summary>
        public TimeSpan ResumeGrace { get; private set; }
        /// <summary>Channels currently joined (restored across reconnects).</summary>
        public IReadOnlyCollection<Guid> JoinedChannels { get { lock (_channels) return new List<Guid>(_joinedChannels); } }

        // ---- end-to-end encryption ---------------------------------------------------------------

        /// <summary>
        /// Announce end-to-end encryption support and take part in the sender-key exchange of <c>e2ee</c>
        /// channels (<see cref="E2eeGroup"/>): frames sent there are sealed with our sender key before they
        /// enter the AURX packet, and members' frames are opened with theirs; the server relays keys and
        /// frames opaque and cannot mix, record or transcribe them. Off, the server refuses to join such a
        /// channel (<c>E2EE_REQUIRED</c>) — there is no plaintext fallback. Set before <see cref="ConnectAsync"/>.
        /// </summary>
        public bool E2ee { get; set; } = true;
        /// <summary>Fingerprint of our X25519 identity, as peers see it in <see cref="OnE2eePeerKey"/>.</summary>
        public string E2eeFingerprint { get { lock (_e2ee) return _e2ee.Identity.Fingerprint; } }
        /// <summary>Generation of our current sender key (bumps on every rotation).</summary>
        public int E2eeGeneration { get { lock (_e2ee) return _e2ee.Generation; } }

        /// <summary>
        /// The 32-byte identity secret to persist and pass to <see cref="SetE2eeIdentity"/> next run: a
        /// stable fingerprint lets peers pin it (a fresh one per run shows up as <c>KeyChanged</c>).
        /// </summary>
        public byte[] ExportE2eeIdentity() { lock (_e2ee) return _e2ee.Identity.ExportSecret(); }

        /// <summary>Use a previously exported identity secret; only while disconnected.</summary>
        public void SetE2eeIdentity(byte[] secret)
        {
            if (State != VoiceConnectionState.Disconnected && State != VoiceConnectionState.Failed)
                throw new InvalidOperationException("the E2EE identity can only be set while disconnected");
            var identity = E2eeIdentityKey.FromBytes(secret);
            lock (_e2ee) _e2ee = new E2eeGroup(identity);
        }

        /// <summary>Fingerprint of <paramref name="userId"/>'s identity key, or null before it announced itself to us.</summary>
        public string E2eePeerFingerprint(Guid userId) { lock (_e2ee) return _e2ee.PeerFingerprint(userId); }
        /// <summary>Whether we hold a sender key of <paramref name="userId"/>, i.e. can open their frames.</summary>
        public bool IsE2eePeerDecryptable(Guid userId) { lock (_e2ee) return _e2ee.HasKeyFor(userId); }
        /// <summary>Peers whose frames we can currently open.</summary>
        public IReadOnlyList<Guid> GetE2eeDecryptablePeers() { lock (_e2ee) return _e2ee.DecryptablePeers(); }
        /// <summary>Whether <paramref name="channelId"/> is a joined end-to-end encrypted channel.</summary>
        public bool IsChannelEncrypted(Guid channelId)
        {
            lock (_channels) return _channelPolicies.TryGetValue(channelId, out var p) && p.E2ee;
        }

        /// <summary>
        /// Rotate our sender key now (join/leave rotate automatically) and send it to every peer. Returns
        /// the new generation, or null when the control connection is down.
        /// </summary>
        public int? RotateE2eeKey()
        {
            var control = _control;
            if (control == null || !control.IsOpen) return null;
            List<E2eeOutgoing> outgoing;
            int generation;
            lock (_e2ee)
            {
                outgoing = _e2ee.Rotate(true);
                generation = _e2ee.Generation;
            }
            _e2eeRotateDue = null;
            E2eeSend(control, outgoing);
            OnE2eeKeyRotated?.Invoke(generation);
            return generation;
        }

        private void E2eeSend(ControlChannel control, List<E2eeOutgoing> outgoing)
        {
            if (outgoing.Count == 0) return;
            byte[] pk;
            lock (_e2ee) pk = _e2ee.Identity.PublicKey;
            foreach (var o in outgoing)
            {
                var json = o.Kind == E2eeOutgoingKind.Hello
                    ? ControlMessage.E2eeHello(o.ChannelId, pk)
                    : ControlMessage.E2eeSenderKey(o.ChannelId, o.To, pk, o.Generation, o.Wrapped);
                _ = control.SendAsync(json);
            }
        }

        /// <summary>
        /// A <c>ChannelJoinAck</c> for an encrypted channel: a first join announces us, a re-ack (session
        /// resume replays them) forgets members who left meanwhile and re-announces.
        /// </summary>
        private void E2eeJoined(Guid channelId, List<Guid> members)
        {
            var control = _control;
            if (control == null) return;
            List<E2eeOutgoing> outgoing;
            var gone = new List<Guid>();
            lock (_e2ee) outgoing = _e2ee.Rejoined(channelId, members, gone);
            E2eeSend(control, outgoing);
            foreach (var user in gone) { var u = user; Post(() => OnE2eePeerDecryptable?.Invoke(u, false)); }
        }

        private void E2eeLeft(Guid channelId)
        {
            List<Guid> before, after;
            lock (_e2ee)
            {
                if (!_e2ee.IsEncrypted(channelId)) return;
                before = _e2ee.DecryptablePeers();
                _e2ee.Left(channelId);
                after = _e2ee.DecryptablePeers();
            }
            E2eeReportLost(before, after);
        }

        private void E2eePeerLeft(Guid channelId, Guid userId)
        {
            bool lost;
            lock (_e2ee)
            {
                bool had = _e2ee.HasKeyFor(userId);
                _e2ee.PeerLeft(channelId, userId);
                lost = had && !_e2ee.HasKeyFor(userId);
            }
            if (lost) Post(() => OnE2eePeerDecryptable?.Invoke(userId, false));
        }

        private void E2eeReset()
        {
            List<Guid> before;
            lock (_e2ee)
            {
                before = _e2ee.DecryptablePeers();
                _e2ee.Reset();
            }
            _e2eeRotateDue = null;
            E2eeReportLost(before, new List<Guid>());
        }

        private void E2eeReportLost(List<Guid> before, List<Guid> after)
        {
            foreach (var user in before)
                if (!after.Contains(user)) { var u = user; Post(() => OnE2eePeerDecryptable?.Invoke(u, false)); }
        }

        private void E2eeOnHello(Guid channelId, Guid userId, byte[] publicKey)
        {
            var control = _control;
            if (control == null) return;
            List<E2eeOutgoing> outgoing;
            E2eePeerChange change;
            string fingerprint;
            bool decryptableBefore;
            lock (_e2ee)
            {
                if (!_e2ee.IsEncrypted(channelId)) return;
                decryptableBefore = _e2ee.HasKeyFor(userId);
                outgoing = _e2ee.OnHello(channelId, userId, publicKey, out change);
                fingerprint = _e2ee.PeerFingerprint(userId);
            }
            E2eeSend(control, outgoing);
            E2eeEmitChange(userId, fingerprint, change, decryptableBefore);
        }

        private void E2eeOnSenderKey(Guid channelId, Guid userId, byte[] publicKey, byte generation, byte[] wrapped)
        {
            var control = _control;
            if (control == null) return;
            List<E2eeOutgoing> outgoing;
            E2eePeerChange change;
            string fingerprint;
            bool decryptableBefore;
            lock (_e2ee)
            {
                if (!_e2ee.IsEncrypted(channelId)) return;
                decryptableBefore = _e2ee.HasKeyFor(userId);
                try { outgoing = _e2ee.OnSenderKey(channelId, userId, publicKey, generation, wrapped, out change); }
                catch (CryptographicException) { return; } // not wrapped for us / forged: keep what we had
                fingerprint = _e2ee.PeerFingerprint(userId);
            }
            E2eeSend(control, outgoing);
            E2eeEmitChange(userId, fingerprint, change, decryptableBefore);
        }

        private void E2eeEmitChange(Guid userId, string fingerprint, E2eePeerChange change, bool decryptableBefore)
        {
            if (change.Kind == E2eePeerChangeKind.New) OnE2eePeerKey?.Invoke(userId, fingerprint, null);
            else if (change.Kind == E2eePeerChangeKind.KeyChanged) OnE2eePeerKey?.Invoke(userId, fingerprint, change.PreviousFingerprint);
            bool decryptable;
            lock (_e2ee) decryptable = _e2ee.HasKeyFor(userId);
            if (decryptable != decryptableBefore) OnE2eePeerDecryptable?.Invoke(userId, decryptable);
        }

        /// <summary>Performs a due rotation a little after it was requested, so a join wave costs one rotation.</summary>
        private void E2eeRotateTick()
        {
            var control = _control;
            if (control == null || !control.IsOpen) { _e2eeRotateDue = null; return; }
            bool pending, active;
            lock (_e2ee) { pending = _e2ee.RotationPending; active = _e2ee.Active; }
            if (!pending || !active) { _e2eeRotateDue = null; return; }
            var now = DateTime.UtcNow;
            if (!_e2eeRotateDue.HasValue) { _e2eeRotateDue = now + E2eeRotateDebounce; return; }
            if (now < _e2eeRotateDue.Value) return;
            List<E2eeOutgoing> outgoing;
            int generation;
            lock (_e2ee)
            {
                outgoing = _e2ee.Rotate(false);
                generation = _e2ee.Generation;
            }
            _e2eeRotateDue = null;
            E2eeSend(control, outgoing);
            OnE2eeKeyRotated?.Invoke(generation);
        }

        /// <summary>Merged audio policy of the joined channels (kept after leaving the last one); <c>null</c> before the first join.</summary>
        public AudioPolicy? AudioPolicy { get { lock (_channels) return _audioPolicy; } }

        /// <summary>
        /// Retune the encoder from the channels' audio policy (bitrate, bandwidth, FEC, DTX, signal and
        /// complexity hint). Off = the baseline from <see cref="SetEncoderSettings"/> is used verbatim.
        /// </summary>
        public bool FollowChannelPolicy
        {
            get { lock (_channels) return _followChannelPolicy; }
            set
            {
                lock (_channels) { if (_followChannelPolicy == value) return; _followChannelPolicy = value; }
                ReapplyEncoder();
            }
        }

        /// <summary>Baseline encoder settings (what the policy and bitrate commands are layered over).</summary>
        public OpusEncoderSettings EncoderSettings { get { lock (_channels) return _encoderSettings; } }

        /// <summary>
        /// Settings the uplink encoder should be running with right now: the baseline with the channel
        /// policy applied (if followed), the complexity pin, and the last server bitrate command.
        /// </summary>
        public OpusEncoderSettings EffectiveEncoderSettings { get { lock (_channels) return EffectiveEncoderSettingsLocked(); } }

        /// <summary>
        /// Replace the baseline (mirrors <c>aurix_client_set_encoder_settings</c>). Clears any transient
        /// bitrate command; the policy of joined channels is re-applied on top when followed.
        /// </summary>
        public void SetEncoderSettings(OpusEncoderSettings settings)
        {
            lock (_channels) { _encoderSettings = settings.Clamped(); _bitrateCommand = null; }
            ReapplyEncoder();
        }

        /// <summary>
        /// Pin the Opus complexity (0..10) regardless of channel policy hints — the CPU budget is the
        /// game's call, not the operator's. <c>null</c> un-pins.
        /// </summary>
        public void SetComplexity(int? complexity)
        {
            lock (_channels) _complexityPin = complexity.HasValue ? (int?)Math.Max(0, Math.Min(OpusEncoderSettings.MaxComplexity, complexity.Value)) : null;
            ReapplyEncoder();
        }

        /// <summary>
        /// The uplink codec this client keeps tuned. Set it once after constructing the codec; every
        /// policy/bitrate change is pushed through <see cref="IOpusEncoderControls.Apply"/> (or
        /// <see cref="IOpusCodec.SetBitrate"/> for codecs without full controls).
        /// </summary>
        public IOpusCodec Encoder
        {
            get { lock (_channels) return _encoder; }
            set
            {
                lock (_channels) { _encoder = value; _appliedEncoderSettings = null; }
                ReapplyEncoder();
            }
        }

        private OpusEncoderSettings EffectiveEncoderSettingsLocked()
        {
            bool followed = _followChannelPolicy && _audioPolicy.HasValue;
            var s = followed ? _encoderSettings.WithPolicy(_audioPolicy.Value, _complexityPin) : _encoderSettings;
            if (!followed && _complexityPin.HasValue) s.Complexity = _complexityPin.Value;
            if (_bitrateCommand.HasValue)
            {
                var cmd = _bitrateCommand.Value;
                s.BitrateBps = cmd.TargetBitrateBps;
                s.ExpectedLossPercent = Math.Max(s.ExpectedLossPercent, cmd.ExpectedLossPercent);
            }
            return s.Clamped();
        }

        /// <summary>Push the effective settings to the bound codec and notify, if they changed.</summary>
        private void ReapplyEncoder()
        {
            OpusEncoderSettings s;
            IOpusCodec codec;
            lock (_channels)
            {
                s = EffectiveEncoderSettingsLocked();
                if (_appliedEncoderSettings.HasValue && _appliedEncoderSettings.Value.Equals(s)) return;
                _appliedEncoderSettings = s;
                codec = _encoder;
            }
            if (codec is IOpusEncoderControls full) full.Apply(s);
            else codec?.SetBitrate(s.BitrateBps);
            OnEncoderSettingsChanged?.Invoke(s);
        }

        /// <summary>Recompute the merged policy; on change retune the encoder and raise <see cref="OnAudioPolicyChanged"/>.</summary>
        private void RefreshAudioPolicy()
        {
            AudioPolicy merged;
            lock (_channels)
            {
                if (_channelPolicies.Count == 0) return;
                merged = Audio.AudioPolicy.MergeAll(_channelPolicies.Values);
                if (_audioPolicy.HasValue && _audioPolicy.Value.Equals(merged)) return;
                _audioPolicy = merged;
            }
            ReapplyEncoder();
            OnAudioPolicyChanged?.Invoke(merged);
        }

        public event Action<VoiceConnectionState> OnStateChanged;
        public event Action<SessionInfo> OnSessionReady;
        public event Action<Guid, IReadOnlyList<Participant>> OnChannelJoined;
        public event Action<Guid> OnChannelLeft;
        public event Action<Guid, Participant> OnParticipantJoined;
        public event Action<Guid, Participant> OnParticipantLeft;
        public event Action<Guid, Participant> OnParticipantUpdated;
        public event Action<Guid, Participant, bool> OnSpeaking;
        /// <summary>
        /// (channel, user, priority): a member's priority-speaker state changed — including yourself, as the ack of
        /// <see cref="SetPriorityAsync"/> or a moderator's grant. <see cref="Participant.IsPriority"/> is already updated.
        /// </summary>
        public event Action<Guid, Guid, bool> OnParticipantPriorityChanged;
        /// <summary>
        /// Another member's priority speech started (<c>true</c>) or stopped (<c>false</c>) ducking the channel —
        /// the hook to attenuate game audio with the same envelope (<see cref="DuckingConfig"/>). Fires on
        /// transitions only, never per speaking update; your own speech never ducks what you hear.
        /// </summary>
        public event Action<Guid, bool, DuckingConfig> OnDuckingChanged;
        public event Action<Guid, IReadOnlyList<UserPosition>> OnPositions;
        public event Action<RecordingNotice> OnRecording;
        /// <summary>
        /// Adaptive-bitrate request from the server. Already applied to <see cref="Encoder"/> when one is
        /// bound; subscribe only to observe or to drive a codec the client does not own.
        /// </summary>
        public event Action<BitrateCommand> OnBitrateCommand;
        /// <summary>
        /// The merged audio policy of the joined channels changed (join, leave, or an operator edited a
        /// channel). With <see cref="FollowChannelPolicy"/> the encoder is already retuned.
        /// </summary>
        public event Action<AudioPolicy> OnAudioPolicyChanged;
        /// <summary>
        /// The settings the uplink encoder should run with changed (baseline, policy, complexity pin or
        /// bitrate command). Applied to <see cref="Encoder"/> automatically when one is bound.
        /// </summary>
        public event Action<OpusEncoderSettings> OnEncoderSettingsChanged;
        /// <summary>
        /// Server-side view of this connection (downlink from our reports + uplink as measured by the SFU),
        /// sent when the 1–5 bars change and periodically as a summary.
        /// </summary>
        public event Action<NetworkQuality> OnNetworkQuality;
        /// <summary>Statistics snapshot taken before each periodic quality report (see <see cref="QualityReportInterval"/>).</summary>
        public event Action<VoiceStats> OnStats;
        public event Action<Guid, string> OnKicked;
        /// <summary>Snapshot of persistent cross-mutes (and, on a resumed session, local mutes/volumes) from the server.</summary>
        public event Action<ReceiverPreferences> OnReceiverPreferences;
        /// <summary>A cross-mute placed or lifted by this user, from this or any other device / the REST API.</summary>
        public event Action<Guid, bool> OnUserBlockChanged;
        /// <summary>Transmission mode confirmed by the server (also raised when a left channel resets it to <see cref="TransmissionMode.None"/>).</summary>
        public event Action<TransmissionMode> OnTransmissionChanged;
        /// <summary>Channel focus confirmed by the server; null when cleared (explicitly or by leaving the focused channel).</summary>
        public event Action<Guid?> OnChannelFocusChanged;
        /// <summary>
        /// The server switched this session's media codec (acknowledging <see cref="SetAudioCodecAsync"/>, or
        /// back to Opus on a fresh session). From this point capture must encode with the new codec and every
        /// downlink frame arrives in it (<see cref="IncomingAudio.Codec"/>).
        /// </summary>
        public event Action<AudioCodec> OnAudioCodecChanged;
        /// <summary>
        /// The server switched how channel audio reaches this session (acknowledging
        /// <see cref="SetDownlinkModeAsync"/>, or back to <see cref="DownlinkMode.Streams"/> on a fresh session).
        /// Frames already in flight may still be of the previous kind.
        /// </summary>
        public event Action<DownlinkMode> OnDownlinkModeChanged;
        /// <summary>
        /// The media moved to another link (path, reason): on every bind, when UDP fell back to the
        /// WebSocket tunnel, and when a UDP re-probe brought it back. Purely informational — audio,
        /// sequence numbers and the session continue.
        /// </summary>
        public event Action<MediaPath, string> OnMediaPathChanged;
        /// <summary>
        /// A text message for this client: channel message of a joined channel, directed message addressed to
        /// this user, or the echo of a message this client sent (<see cref="ChatMessage.IsOwn"/>).
        /// </summary>
        public event Action<ChatMessage> OnChatMessage;
        /// <summary>
        /// A read marker moved: this user's own (from any device, this one included) or, when the server has
        /// read receipts on, another participant's in a shared conversation.
        /// </summary>
        public event Action<ChatReadMarker> OnChatReadMarker;
        /// <summary>
        /// Once per connection, after the directed messages that arrived while this user was offline were
        /// replayed through <see cref="OnChatMessage"/> with <see cref="ChatMessage.Offline"/>. <c>(delivered,
        /// truncated)</c>: <c>truncated</c> means older unread ones exist beyond the server's replay limit —
        /// page them with <see cref="HistoryAsync"/>.
        /// </summary>
        public event Action<int, bool> OnChatInboxSynced;
        /// <summary>Another member of the channel started/stopped typing (channel, user, typing).</summary>
        public event Action<Guid, Guid, bool> OnParticipantTyping;
        /// <summary>
        /// Speech-to-text of a member (or this user) in a channel the server transcribes. Ephemeral: nothing is
        /// stored server-side. Suppressed after <see cref="SetTranscriptsAsync"/>(false); participants this client
        /// blocked or locally muted are not transcribed for it either.
        /// </summary>
        public event Action<Transcript> OnTranscript;
        /// <summary>The server applied (or a resumed session restored) this client's translation preference.</summary>
        public event Action<TranslationPrefs> OnTranslationChanged;
        /// <summary>Progress of a <see cref="SpeakAsync"/> request (queued → playing → finished/cancelled/failed).</summary>
        public event Action<TtsStatus> OnTtsStatus;
        /// <summary>
        /// Periodic audio levels of channel members whose energy changed (channel, levels). Participants'
        /// <see cref="Participant.Energy"/> is updated before the event fires. Level meters should decay
        /// on their own between reports; a participant that went silent is reported once with 0.
        /// </summary>
        public event Action<Guid, IReadOnlyList<ParticipantEnergy>> OnChannelEnergy;
        public event Action<string, string> OnServerError;
        /// <summary>Connection closed for good: after <see cref="DisconnectAsync"/>, a server <c>SessionClose</c>, or when reconnecting gave up.</summary>
        public event Action<string> OnDisconnected;
        /// <summary>Connection lost; reconnect attempt N will run after the given delay. (attempt, delay, cause)</summary>
        public event Action<int, TimeSpan, string> OnRecovering;
        /// <summary>Reconnected. <see cref="SessionInfo.Resumed"/> tells whether the old session (and SSRC) survived.</summary>
        public event Action<SessionInfo> OnRecovered;
        /// <summary>
        /// A failover node answered while the previous one did not; the client now talks to the given URL
        /// (<see cref="Endpoint"/>). Raised before that connection's <see cref="OnRecovered"/>.
        /// </summary>
        public event Action<string> OnEndpointChanged;
        /// <summary>All reconnect attempts failed; the client is now <see cref="VoiceConnectionState.Failed"/>.</summary>
        public event Action<Exception> OnFailedToRecover;
        /// <summary>The server ended the session (kick, ban, shutdown, replaced by another login); no reconnect follows.</summary>
        public event Action<string> OnSessionClosed;
        /// <summary>Verified downlink frame: (participant or null if unknown SSRC, frame).</summary>
        public event Action<Participant, IncomingAudio> OnAudio;
        /// <summary>
        /// A peer's identity key was learnt (<c>(userId, fingerprint, null)</c>) or changed
        /// (<c>(userId, fingerprint, previousFingerprint)</c>) — show it for out-of-band verification.
        /// </summary>
        public event Action<Guid, string, string> OnE2eePeerKey;
        /// <summary>Whether we can open <c>userId</c>'s frames changed (their sender key arrived / they left every shared channel).</summary>
        public event Action<Guid, bool> OnE2eePeerDecryptable;
        /// <summary>Our sender key rotated to the given generation.</summary>
        public event Action<int> OnE2eeKeyRotated;
        /// <summary>Every raw control message, for diagnostics/extensions.</summary>
        public event Action<ControlMessage> OnControlMessage;

        /// <param name="wsUrl">e.g. <c>ws://host:8081/ws</c> (or <c>wss://</c>).</param>
        /// <param name="token">Per-user credential issued by your backend: a session JWT (<c>POST /v1/tokens</c>) or a one-time <c>login</c> action token (<c>POST /v1/tokens/action</c>).</param>
        public AurixVoiceClient(string wsUrl, string token)
        {
#if UNITY_WEBGL && !UNITY_EDITOR
            throw new PlatformNotSupportedException("AurixVoiceClient needs UDP/WebSocket sockets; in WebGL players use Aurix.WebGL.AurixWebGLVoiceClient");
#else
            _wsUrl = wsUrl ?? throw new ArgumentNullException(nameof(wsUrl));
            _token = token ?? throw new ArgumentNullException(nameof(token));
            _localUserId = TokenUserId(_token);
            Endpoint = _wsUrl;
#endif
        }

        /// <summary>WebSocket URL of the node the client talks to (the constructor URL until a failover moved it).</summary>
        public string Endpoint { get; private set; }
        /// <summary>Alternate nodes advertised by the server for this session (see <see cref="OnEndpointChanged"/>).</summary>
        public IReadOnlyList<string> FailoverEndpoints { get; private set; } = Array.Empty<string>();

        /// <summary>
        /// Reconnect target for <paramref name="attempt"/> (1-based): the node that holds the session first — a
        /// same-node resume is the cheapest recovery — then each advertised alternate, round-robin.
        /// </summary>
        public static string ReconnectEndpoint(string active, IReadOnlyList<string> failover, int attempt)
        {
            int n = 1 + (failover?.Count ?? 0);
            int i = (Math.Max(1, attempt) - 1) % n;
            return i == 0 ? active : failover[i - 1];
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
            Endpoint = _wsUrl;
            FailoverEndpoints = Array.Empty<string>();
            SetState(VoiceConnectionState.Connecting);
            try
            {
                var info = await OpenSessionAsync(_wsUrl, null, _cts.Token).ConfigureAwait(false);
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

        /// <summary>
        /// Drop the current connection and reconnect right away (resume + UDP rebind), as if it had been
        /// lost. Use it when the network path is known to have changed — Wi-Fi ↔ cellular, VPN up/down:
        /// the TCP socket may take a minute to notice and the media session is bound to the old source
        /// address, so nothing is heard until the rebind. While already reconnecting it skips the current
        /// backoff delay; disconnected clients are left alone. Call from the <see cref="Update"/> thread.
        /// </summary>
        public void ForceReconnect(string reason = "network changed")
        {
            if (_closedByUser) return;
            if (State == VoiceConnectionState.Reconnecting) { ReconnectNow(); return; }
            var control = _control;
            if (control == null || State == VoiceConnectionState.Disconnected || State == VoiceConnectionState.Failed) return;
            control.Dispose();
            HandleClosed(control, reason);
        }

        /// <summary>
        /// Check whether the control connection is still alive after the app was suspended (mobile
        /// background, laptop sleep): sends a ping now and, unless a pong arrives within
        /// <paramref name="timeout"/> (default 2 s), treats the connection as lost so the reconnect
        /// starts immediately instead of after the regular ping timeout (3 × <see cref="PingInterval"/>).
        /// A live connection is left untouched.
        /// </summary>
        public void ProbeConnection(TimeSpan? timeout = null)
        {
            var control = _control;
            if (control == null || !control.IsOpen) return;
            var now = DateTime.UtcNow;
            _lastPing = now;
            _probeDeadline = now + (timeout ?? TimeSpan.FromSeconds(2));
            var nonce = (ulong)(DateTimeOffset.UtcNow.ToUnixTimeMilliseconds() & 0x1F_FFFF_FFFF_FFFF);
            _probeNonce = nonce; // only a pong to this ping (or a later one) counts, not one already in flight
            _ = control.SendAsync(ControlMessage.Ping(nonce));
        }

        /// <summary>Open a control channel (optionally resuming) and wait for <c>SessionInitAck</c>.</summary>
        private async Task<SessionInfo> OpenSessionAsync(string url, string resume, CancellationToken ct)
        {
            var control = new ControlChannel();
            control.Closed += reason => Post(() => HandleClosed(control, reason));
            control.Received += CompletePendingJoin;
            control.Received += CompletePendingModeration;
            control.Received += CompletePendingChat;
            control.Received += CompletePendingHistory;
            control.Received += CompletePendingSpeak;
            try
            {
                await control.ConnectAsync(new Uri(url), _token, ct, resume).ConfigureAwait(false);
                ControlMessage ack;
                while (true)
                {
                    ack = await control.NextAsync(RequestTimeout, ct).ConfigureAwait(false);
                    if (ack.Type == "SessionInitAck") break;
                    if (ack.Type == "Error") throw new InvalidOperationException($"{ack.Str("code")}: {ack.Str("message")}");
                }
                var failover = new List<string>();
                var advertised = MiniJson.AsArray(ack.Data != null && ack.Data.TryGetValue("failover", out var f) ? f : null);
                if (advertised != null)
                    foreach (var item in advertised)
                        if (item is string s && s != url) failover.Add(s);
                var mediaAddr = ack.Str("media_addr");
                var mediaAddrs = new List<string>();
                var advertisedMedia = MiniJson.AsArray(ack.Data != null && ack.Data.TryGetValue("media_addrs", out var ma) ? ma : null);
                if (advertisedMedia != null)
                    foreach (var item in advertisedMedia)
                        if (item is string s && !string.IsNullOrEmpty(s) && !mediaAddrs.Contains(s)) mediaAddrs.Add(s);
                if (mediaAddrs.Count == 0 && !string.IsNullOrEmpty(mediaAddr)) mediaAddrs.Add(mediaAddr);
                var info = new SessionInfo
                {
                    SessionId = ack.Id("session_id"),
                    Ssrc = ack.U32("ssrc"),
                    MediaAddr = mediaAddr,
                    MediaAddrs = mediaAddrs,
                    Resumed = ack.Bool("resumed"),
                    Migrated = ack.Bool("migrated"),
                    Endpoint = url,
                    Failover = failover,
                    MediaTunnel = ack.Bool("media_tunnel"),
                    DownlinkMix = ack.Bool("downlink_mix"),
                    Translation = ack.Translation(),
                };
                _mediaKey = Convert.FromBase64String(ack.Str("media_key") ?? throw new InvalidOperationException("SessionInitAck without media_key"));
                var token = ack.Str("resume_token");
                _resumeToken = string.IsNullOrEmpty(token) ? null : token;
                ResumeGrace = TimeSpan.FromMilliseconds(ack.Num("resume_grace_ms"));
                Session = info;
                FailoverEndpoints = failover;
                if (Endpoint != url)
                {
                    Endpoint = url;
                    Post(() => OnEndpointChanged?.Invoke(url));
                }
                // Publish only now so Update() cannot drain the handshake messages from under us.
                _control = control;
                _lastPing = DateTime.UtcNow;
                _lastPong = _lastPing;
                _probeDeadline = DateTime.MaxValue;
                SetState(VoiceConnectionState.Connected);
                if (E2ee)
                {
                    byte[] pk;
                    lock (_e2ee) pk = _e2ee.Identity.PublicKey;
                    await control.SendAsync(ControlMessage.E2eeHello(null, pk), ct).ConfigureAwait(false);
                }
                return info;
            }
            catch
            {
                control.Dispose();
                throw;
            }
        }

        /// <summary>Bind (or rebind, from a fresh UDP port / the new control socket) the native media path for the current session.</summary>
        private async Task BindMediaAsync(SessionInfo info, CancellationToken ct)
        {
            var old = _media;
            _media = null;
            if (old != null)
            {
                _lastMediaSequence = old.CurrentSequence;
                old.Dispose();
            }
            var seq = new SequenceCounter(info.Resumed ? _lastMediaSequence : 0);
            var control = _control;
            MediaTransport media;
            string reason;
            switch (MediaPathPolicy)
            {
                case MediaPathPolicy.UdpOnly:
                    media = await BindUdpAsync(info, seq, 5, 500, ct).ConfigureAwait(false);
                    reason = "UDP bound";
                    break;
                case MediaPathPolicy.TunnelOnly:
                    if (!info.MediaTunnel) throw new InvalidOperationException("media path is tunnel-only but the node offers no media tunnel");
                    media = await BindTunnelAsync(control, info, seq, ct).ConfigureAwait(false);
                    reason = "tunnel-only policy";
                    break;
                default:
                    if (!info.MediaTunnel)
                    {
                        media = await BindUdpAsync(info, seq, 5, 500, ct).ConfigureAwait(false);
                        reason = "UDP bound";
                    }
                    else if (_udpBlockedUntil.HasValue && _udpBlockedUntil.Value > DateTime.UtcNow)
                    {
                        media = await BindTunnelAsync(control, info, seq, ct).ConfigureAwait(false);
                        reason = "UDP was blocked before this session";
                    }
                    else
                    {
                        try
                        {
                            media = await BindUdpAsync(info, seq, 2, 1000, ct).ConfigureAwait(false);
                            reason = "UDP bound";
                        }
                        catch (Exception e) when (!(e is OperationCanceledException))
                        {
                            MarkUdpBlocked();
                            media = await BindTunnelAsync(control, info, seq, ct).ConfigureAwait(false);
                            reason = $"UDP bind failed: {e.Message}";
                        }
                    }
                    break;
            }
            _media = media;
            _lossWindow.Reset();
            if (_muted) media.SendMuteState(true);
            if (media.Path == MediaPath.Tunnel) _nextUdpProbe = DateTime.UtcNow + UdpReprobeInterval;
            SetState(VoiceConnectionState.MediaBound);
            var path = media.Path;
            Post(() => OnMediaPathChanged?.Invoke(path, reason));
        }

        /// <summary>
        /// Bind UDP against the node's media candidates one after another (the address family that answered last
        /// time first). Candidates are not raced in parallel: the server binds the session to the last
        /// <c>SessionBind</c> it sees, so two winners would fight over the downlink endpoint.
        /// </summary>
        private async Task<MediaTransport> BindUdpAsync(SessionInfo info, SequenceCounter seq, int attempts, int timeoutMs, CancellationToken ct)
        {
            var candidates = await MediaTransport.ResolveCandidatesAsync(info.MediaAddr, info.MediaAddrs).ConfigureAwait(false);
            if (_udpFamilyHint is System.Net.Sockets.AddressFamily preferred)
                candidates.Sort((a, b) => (a.AddressFamily == preferred ? 0 : 1).CompareTo(b.AddressFamily == preferred ? 0 : 1));
            Exception last = null;
            foreach (var endpoint in candidates)
            {
                ct.ThrowIfCancellationRequested();
                var media = new MediaTransport(endpoint, info.SessionId, info.Ssrc, _mediaKey, seq) { HeartbeatInterval = MediaHeartbeatInterval };
                try
                {
                    await media.BindAsync(ct, attempts, timeoutMs).ConfigureAwait(false);
                    _udpFamilyHint = endpoint.AddressFamily;
                    return media;
                }
                catch (Exception e) when (!(e is OperationCanceledException))
                {
                    media.Dispose();
                    last = e;
                }
            }
            throw last ?? new InvalidOperationException("no media endpoint advertised");
        }

        private async Task<MediaTransport> BindTunnelAsync(ControlChannel control, SessionInfo info, SequenceCounter seq, CancellationToken ct)
        {
            if (control == null) throw new InvalidOperationException("not connected");
            var media = MediaTransport.OverTunnel(control, info.SessionId, info.Ssrc, _mediaKey, seq);
            media.HeartbeatInterval = MediaHeartbeatInterval;
            try
            {
                await media.BindAsync(ct, 2, 3000).ConfigureAwait(false);
                return media;
            }
            catch
            {
                media.Dispose();
                throw;
            }
        }

        /// <summary>Remember that UDP is unusable for as long as a re-probe cycle (or a minute when re-probing is off).</summary>
        private void MarkUdpBlocked()
        {
            var memory = UdpReprobeInterval > TimeSpan.Zero ? UdpReprobeInterval : TimeSpan.FromSeconds(60);
            _udpBlockedUntil = DateTime.UtcNow + memory;
        }

        /// <summary>
        /// Called from <see cref="Update"/>: move a UDP session whose heartbeats died onto the tunnel, or
        /// try UDP again from a tunnelled one. At most one switch runs at a time.
        /// </summary>
        private void DriveMediaPath(MediaTransport media, ControlChannel control)
        {
            var session = Session;
            if (session == null || !session.MediaTunnel || MediaPathPolicy != MediaPathPolicy.Auto || _pathSwitchActive != 0) return;
            var ct = _cts;
            if (ct == null || ct.IsCancellationRequested) return;
            if (media.Path == MediaPath.Udp)
            {
                int lost = media.HeartbeatsLostConsecutive;
                if (UdpFallbackLostHeartbeats <= 0 || lost < UdpFallbackLostHeartbeats) return;
                if (Interlocked.CompareExchange(ref _pathSwitchActive, 1, 0) != 0) return;
                _ = FallBackToTunnelAsync(media, control, session, $"{lost} UDP heartbeats unanswered", ct.Token);
            }
            else if (UdpReprobeInterval > TimeSpan.Zero && DateTime.UtcNow >= _nextUdpProbe)
            {
                if (Interlocked.CompareExchange(ref _pathSwitchActive, 1, 0) != 0) return;
                _ = ReprobeUdpAsync(media, session, ct.Token);
            }
        }

        private async Task FallBackToTunnelAsync(MediaTransport udp, ControlChannel control, SessionInfo session, string reason, CancellationToken ct)
        {
            MediaTransport tunnel = null;
            try
            {
                tunnel = await BindTunnelAsync(control, session, udp.Sequence, ct).ConfigureAwait(false);
                var bound = tunnel;
                Post(() =>
                {
                    if (!ReferenceEquals(_media, udp) || !ReferenceEquals(_control, control)) { bound.Dispose(); return; }
                    _media = bound;
                    udp.Dispose();
                    MarkUdpBlocked();
                    _nextUdpProbe = DateTime.UtcNow + UdpReprobeInterval;
                    OnMediaPathChanged?.Invoke(MediaPath.Tunnel, reason);
                });
            }
            catch (OperationCanceledException) when (ct.IsCancellationRequested) { }
            catch (Exception e)
            {
                // Neither link works: treat the connection as lost so the regular reconnect takes over.
                Post(() => { if (ReferenceEquals(_media, udp)) ForceReconnect($"{reason}; tunnel bind failed: {e.Message}"); });
            }
            finally
            {
                Interlocked.Exchange(ref _pathSwitchActive, 0);
            }
        }

        private async Task ReprobeUdpAsync(MediaTransport tunnel, SessionInfo session, CancellationToken ct)
        {
            try
            {
                MediaTransport udp;
                try
                {
                    udp = await BindUdpAsync(session, tunnel.Sequence, 1, 1500, ct).ConfigureAwait(false);
                }
                catch (Exception e) when (!(e is OperationCanceledException))
                {
                    _nextUdpProbe = DateTime.UtcNow + UdpReprobeInterval;
                    // The probe may have reached the server (only its ack got lost), in which case the
                    // session's media now points at a dead socket: make the tunnel the endpoint again.
                    try { await tunnel.ReclaimAsync(ct).ConfigureAwait(false); }
                    catch (Exception r) when (!(r is OperationCanceledException))
                    {
                        Post(() => { if (ReferenceEquals(_media, tunnel)) ForceReconnect($"media tunnel lost after a UDP probe: {r.Message}"); });
                    }
                    return;
                }
                Post(() =>
                {
                    if (!ReferenceEquals(_media, tunnel)) { udp.Dispose(); return; }
                    _media = udp;
                    tunnel.Dispose();
                    _udpBlockedUntil = null;
                    OnMediaPathChanged?.Invoke(MediaPath.Udp, "UDP re-probe answered");
                });
            }
            catch (OperationCanceledException) { }
            finally
            {
                Interlocked.Exchange(ref _pathSwitchActive, 0);
            }
        }

        /// <summary>
        /// Join a channel. Returns the current roster. The join is authorised by <see cref="JoinTokenProvider"/>
        /// when set, otherwise by the session credential.
        /// </summary>
        public async Task<IReadOnlyList<Participant>> JoinChannelAsync(Guid channelId, CancellationToken ct = default)
        {
            EnsureConnected();
            var provider = JoinTokenProvider;
            var token = provider != null ? await provider(channelId, ct).ConfigureAwait(false) : _token;
            return await JoinChannelAsync(channelId, token, ct).ConfigureAwait(false);
        }

        /// <summary>Join a channel with an explicit one-time <c>join</c> action token minted for this user and channel.</summary>
        public async Task<IReadOnlyList<Participant>> JoinChannelAsync(Guid channelId, string joinToken, CancellationToken ct = default)
        {
            if (joinToken == null) throw new ArgumentNullException(nameof(joinToken));
            EnsureConnected();
            if (_joinTcs != null) throw new InvalidOperationException("a join is already in progress");
            var tcs = new TaskCompletionSource<List<Participant>>(TaskCreationOptions.RunContinuationsAsynchronously);
            _joinTcs = tcs;
            _pendingChannelJoin = channelId;
            await _control.SendAsync(ControlMessage.ChannelJoin(channelId, joinToken), ct).ConfigureAwait(false);
            using (var timeout = new CancellationTokenSource(RequestTimeout))
            using (timeout.Token.Register(() => tcs.TrySetException(new TimeoutException("ChannelJoinAck timeout"))))
            using (ct.Register(() => tcs.TrySetCanceled()))
            {
                try
                {
                    var roster = await tcs.Task.ConfigureAwait(false);
                    lock (_channels) _joinedChannels.Add(channelId);
                    await ReplayChannelPrefsAsync(channelId, ct).ConfigureAwait(false);
                    return roster;
                }
                finally { _joinTcs = null; _pendingChannelJoin = null; }
            }
        }

        /// <summary>
        /// Kick or server-mute/unmute <paramref name="userId"/> in <paramref name="channelId"/> with a one-time
        /// <c>kick</c>/<c>mute</c>/<c>unmute</c> action token minted for this user (<c>POST /v1/tokens/action</c>).
        /// A mismatch (other actor, target, channel or action) leaves the token unused and fails with <c>AUTH_DENIED</c>;
        /// a replay fails with <c>TOKEN_REUSED</c>.
        /// </summary>
        public async Task ModerateAsync(Guid channelId, Guid userId, ModerationAction action, string token, string reason = null, CancellationToken ct = default)
        {
            if (token == null) throw new ArgumentNullException(nameof(token));
            EnsureConnected();
            if (_moderationTcs != null) throw new InvalidOperationException("a moderation request is already in progress");
            var tcs = new TaskCompletionSource<bool>(TaskCreationOptions.RunContinuationsAsynchronously);
            _moderationTcs = tcs;
            _pendingModeration = ModerationKey(channelId, userId, action);
            try
            {
                await _control.SendAsync(ControlMessage.ModerateParticipant(channelId, userId, action, token, reason), ct).ConfigureAwait(false);
                using (var timeout = new CancellationTokenSource(RequestTimeout))
                using (timeout.Token.Register(() => tcs.TrySetException(new TimeoutException("ModerateParticipantAck timeout"))))
                using (ct.Register(() => tcs.TrySetCanceled()))
                    await tcs.Task.ConfigureAwait(false);
            }
            finally { _moderationTcs = null; _pendingModeration = null; }
        }

        private static string ModerationKey(Guid channelId, Guid userId, ModerationAction action) =>
            $"{channelId:D}/{userId:D}/{ControlMessage.ModerationActionToWire(action)}";

        /// <summary>Leave a channel. The server does not acknowledge; membership is dropped locally at once.</summary>
        public async Task LeaveChannelAsync(Guid channelId, CancellationToken ct = default)
        {
            EnsureConnected();
            await _control.SendAsync(ControlMessage.ChannelLeave(channelId), ct).ConfigureAwait(false);
            DropDucking(channelId);
            lock (_channels)
            {
                _joinedChannels.Remove(channelId);
                _transcribedChannels.Remove(channelId);
                _monitoredChannels.Remove(channelId);
                _channelScopes.Remove(channelId);
                _channelInfos.Remove(channelId);
                _channelPolicies.Remove(channelId);
                if (_channels.TryGetValue(channelId, out var map))
                {
                    foreach (var p in map.Values) _bySsrc.Remove(p.Ssrc);
                    _channels.Remove(channelId);
                }
            }
            E2eeLeft(channelId);
            RefreshAudioPolicy();
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

        // ---- priority speaker + ducking ---------------------------------------------------------

        /// <summary>
        /// Make <paramref name="userId"/> (<c>null</c> = yourself) a priority speaker of <paramref name="channelId"/>, or
        /// revoke it. Others need a moderator role; your own flag can be set with a <c>priority</c> grant and always
        /// revoked. Rejected in channels without <see cref="ChannelInfo.Ducking"/>. Acked to everyone by
        /// <see cref="OnParticipantPriorityChanged"/> (the <c>ChannelInfo</c> and <see cref="Participant.IsPriority"/> follow).
        /// </summary>
        public Task SetPriorityAsync(Guid channelId, Guid? userId, bool priority, CancellationToken ct = default)
        {
            EnsureConnected();
            return _control.SendAsync(ControlMessage.SetPriority(channelId, userId, priority), ct);
        }

        /// <summary>Whether we are a priority speaker in <paramref name="channelId"/> (false for channels not joined).</summary>
        public bool IsPriority(Guid channelId)
        {
            lock (_channels) return _channelInfos.TryGetValue(channelId, out var i) && i.IsPriority;
        }

        /// <summary>
        /// Whether another member's priority speech is ducking <paramref name="channelId"/> right now — the state
        /// <see cref="OnDuckingChanged"/> last reported.
        /// </summary>
        public bool IsDuckingActive(Guid channelId)
        {
            lock (_channels) return _duckedChannels.Contains(channelId);
        }

        /// <summary>
        /// This client's user id as claimed by the connect token (an unverified JWT claim, <see cref="Guid.Empty"/> when the
        /// token carries none). Used to keep your own speech from ducking what you hear.
        /// </summary>
        public Guid LocalUserId
        {
            get { lock (_channels) return _localUserId; }
        }

        private static Guid TokenUserId(string token)
        {
            if (string.IsNullOrEmpty(token)) return Guid.Empty;
            var parts = token.Split('.');
            if (parts.Length < 2) return Guid.Empty;
            var payload = parts[1].Replace('-', '+').Replace('_', '/');
            switch (payload.Length % 4) { case 2: payload += "=="; break; case 3: payload += "="; break; case 1: return Guid.Empty; }
            try
            {
                var claims = MiniJson.AsObject(MiniJson.Parse(System.Text.Encoding.UTF8.GetString(Convert.FromBase64String(payload))));
                return MiniJson.GetGuid(claims, "user_id") ?? Guid.Empty;
            }
            catch (Exception) { return Guid.Empty; }
        }

        /// <summary>
        /// Whether <paramref name="p"/>'s speech ducks the others in a channel with <paramref name="ducking"/>: the explicit
        /// flag, or a moderator role when <see cref="DuckingConfig.Moderators"/> is on. Never our own speech.
        /// </summary>
        private bool Ducks(Participant p, DuckingConfig ducking) =>
            p.IsSpeaking && p.UserId != _localUserId &&
            (p.IsPriority || (ducking.Moderators && (p.Role == ChannelRole.Moderator || p.Role == ChannelRole.Administrator)));

        /// <summary>Re-evaluates the game-audio ducking state of <paramref name="channelId"/>; fires <see cref="OnDuckingChanged"/> on a transition.</summary>
        private void RefreshDucking(Guid channelId)
        {
            bool active;
            DuckingConfig ducking;
            lock (_channels)
            {
                active = false;
                ducking = default;
                if (_channelInfos.TryGetValue(channelId, out var info) && info.Ducking.HasValue && _channels.TryGetValue(channelId, out var map))
                {
                    ducking = info.Ducking.Value;
                    foreach (var p in map.Values)
                        if (Ducks(p, ducking)) { active = true; break; }
                }
                if (active == _duckedChannels.Contains(channelId)) return;
                if (active) _duckedChannels.Add(channelId); else _duckedChannels.Remove(channelId);
            }
            OnDuckingChanged?.Invoke(channelId, active, ducking);
        }

        /// <summary>The channel is gone (left, kicked, disconnected): release its game-audio ducking, if any.</summary>
        private void DropDucking(Guid channelId)
        {
            DuckingConfig ducking;
            lock (_channels)
            {
                if (!_duckedChannels.Remove(channelId)) return;
                ducking = _channelInfos.TryGetValue(channelId, out var info) && info.Ducking.HasValue ? info.Ducking.Value : DuckingConfig.Default;
            }
            OnDuckingChanged?.Invoke(channelId, false, ducking);
        }

        private void DropAllDucking()
        {
            List<Guid> ducked;
            lock (_channels) ducked = _duckedChannels.Count == 0 ? null : new List<Guid>(_duckedChannels);
            if (ducked == null) return;
            foreach (var ch in ducked) DropDucking(ch);
        }

        // ---- lip-sync (visemes) + voice effects ---------------------------------------------------

        /// <summary>
        /// Whether lip-sync analysis is available here: the <c>aurix_client</c> native library with its
        /// viseme analyser is loadable (the same binary <see cref="NativeOpusCodec"/> uses).
        /// </summary>
        public bool SupportsVisemes => NativeVisemeAnalyzer.IsAvailable;

        /// <summary>Whether <see cref="SetVisemesAsync"/> turned lip-sync analysis on.</summary>
        public bool VisemesEnabled { get { lock (_capture) return _visemesEnabled; } }

        /// <summary>
        /// Turn lip-sync analysis on or off for every participant heard through <see cref="Mixer"/> and for
        /// the local microphone (frames the behaviour hands to <see cref="AnalyzeLocalVoice"/>). Purely local:
        /// audio is analysed after decoding / E2EE decryption and nothing is sent to the server. Costs one
        /// FFT per stream per 20 ms. Throws <see cref="PlatformNotSupportedException"/> without the native library.
        /// </summary>
        public Task SetVisemesAsync(bool enabled, CancellationToken ct = default)
        {
            if (enabled && !NativeVisemeAnalyzer.IsAvailable)
                throw new PlatformNotSupportedException("lip-sync needs the aurix_client native library");
            lock (_capture)
            {
                _visemesEnabled = enabled;
                if (enabled) { if (_localVisemes == null) _localVisemes = new NativeVisemeAnalyzer(); }
                else if (_localVisemes != null) { _localVisemes.Dispose(); _localVisemes = null; }
                var mixer = _mixer;
                if (mixer != null) mixer.VisemesEnabled = enabled;
            }
            return Task.CompletedTask;
        }

        /// <summary>
        /// Mouth state of <paramref name="userId"/> — the last analysed 20 ms of its voice (or its TTS voice,
        /// whichever spoke last) in any joined channel; null when unknown, not heard through <see cref="Mixer"/>
        /// or with lip-sync off. Silence (mouth closed) between talk spurts.
        /// </summary>
        public VisemeFrame? GetParticipantVisemes(Guid userId)
        {
            var mixer = _mixer;
            if (mixer == null || !VisemesEnabled) return null;
            uint ssrc = 0;
            lock (_channels)
            {
                foreach (var map in _channels.Values)
                    if (map.TryGetValue(userId, out var p) && p.Ssrc != 0) { ssrc = p.Ssrc; break; }
            }
            if (ssrc == 0) return null;
            return mixer.TryGetParticipantVisemes(ssrc, out var frame) ? frame : (VisemeFrame?)null;
        }

        /// <summary>Mouth state of the local microphone as encoded (effects included); null with lip-sync off.</summary>
        public VisemeFrame? GetLocalVisemes()
        {
            lock (_capture) return _localVisemes?.Frame;
        }

        /// <summary>
        /// Feed the interleaved 48 kHz frame about to be encoded (call from the capture thread, one 20 ms frame
        /// at a time); a no-op with lip-sync off. <c>AurixVoiceBehaviour</c> does this for you.
        /// </summary>
        public void AnalyzeLocalVoice(float[] pcm, int frames, int channels)
        {
            lock (_capture) _localVisemes?.Push(pcm, 0, frames, channels);
        }

        /// <summary>The microphone stopped (device switch, mute, disconnect): the local mouth closes.</summary>
        public void ResetLocalVoice()
        {
            lock (_capture)
            {
                _localVisemes?.Reset();
                _voiceEffects?.Reset();
            }
        }

        /// <summary>Whether voice effects can run here (the <c>aurix_client</c> native library is loadable).</summary>
        public bool SupportsVoiceEffects => NativeVoiceEffects.IsAvailable;

        /// <summary>The active voice-effect parameters (clamped), <see cref="VoiceEffectParams.Bypass"/> by default.</summary>
        public VoiceEffectParams VoiceEffects { get { lock (_capture) return _voiceEffectParams; } }

        /// <summary>
        /// Apply a voice-effect chain to the local microphone — presets via <see cref="VoiceEffectParams.Preset"/>,
        /// <see cref="VoiceEffectParams.Bypass"/> to switch off. Runs on the captured frames the behaviour hands to
        /// <see cref="ApplyVoiceEffects"/> before encoding: injected audio and what you hear are untouched, and
        /// listeners get the processed voice (encrypted channels included, since it is applied before sealing).
        /// Throws <see cref="PlatformNotSupportedException"/> for a non-bypass chain without the native library.
        /// </summary>
        public Task SetVoiceEffectsAsync(VoiceEffectParams effects, CancellationToken ct = default)
        {
            var sanitized = effects.Sanitized();
            lock (_capture)
            {
                if (sanitized.IsBypass)
                {
                    _voiceEffects?.Dispose();
                    _voiceEffects = null;
                    _voiceEffectParams = sanitized;
                    return Task.CompletedTask;
                }
                if (!NativeVoiceEffects.IsAvailable)
                    throw new PlatformNotSupportedException("voice effects need the aurix_client native library");
                if (_voiceEffects == null) _voiceEffects = new NativeVoiceEffects(sanitized);
                else _voiceEffects.Apply(sanitized);
                _voiceEffectParams = _voiceEffects.Params;
            }
            return Task.CompletedTask;
        }

        /// <summary><see cref="SetVoiceEffectsAsync(VoiceEffectParams, CancellationToken)"/> with a built-in preset.</summary>
        public Task SetVoiceEffectsAsync(VoiceEffectPreset preset, CancellationToken ct = default) =>
            SetVoiceEffectsAsync(VoiceEffectParams.Preset(preset), ct);

        /// <summary>
        /// Run the active effect chain in place over one interleaved 48 kHz microphone frame (1 or 2 channels);
        /// a no-op in bypass. Call from the capture thread, before mixing in injected audio and before encoding.
        /// </summary>
        public void ApplyVoiceEffects(float[] pcm, int frames, int channels)
        {
            lock (_capture) _voiceEffects?.Process(pcm, frames, channels);
        }

        private void DisposeCaptureProcessors()
        {
            lock (_capture)
            {
                _localVisemes?.Dispose();
                _localVisemes = null;
                _voiceEffects?.Dispose();
                _voiceEffects = null;
            }
        }

        // ---- multi-channel: transmission policy (sender side) and focus (receiver side) ---------

        /// <summary>Where this session's audio is delivered; <see cref="TransmissionMode.All"/> by default.</summary>
        public TransmissionMode Transmission { get { lock (_channels) return _transmission; } }

        /// <summary>Channel heard at full volume while the other joined channels are attenuated; null when unfocused.</summary>
        public Guid? FocusChannel { get { lock (_channels) return _focusChannel; } }

        /// <summary>
        /// Choose which joined channels receive this session's voice: <see cref="TransmissionMode.All"/>,
        /// <see cref="TransmissionMode.None"/> (server-side push-to-talk release) or
        /// <see cref="TransmissionMode.Single"/>. Enforced by the server; frames for other channels are
        /// dropped there and <see cref="SendOpusFrame"/> skips them locally too. A <c>Single</c> target that is
        /// not joined yet is kept and sent once the channel is joined; leaving the target resets the mode to
        /// <c>None</c> (reported through <see cref="OnTransmissionChanged"/>). Re-applied after a reconnect.
        /// </summary>
        public Task SetTransmissionAsync(TransmissionMode mode, CancellationToken ct = default)
        {
            bool send;
            lock (_channels)
            {
                _transmission = mode;
                send = mode.Kind != TransmissionKind.Single || _joinedChannels.Contains(mode.ChannelId);
            }
            var control = _control;
            if (control == null || !send) return Task.CompletedTask;
            return control.SendAsync(ControlMessage.SetTransmission(mode), ct);
        }

        /// <summary>Shortcut for <see cref="SetTransmissionAsync"/> with <see cref="TransmissionMode.Single"/>.</summary>
        public Task TransmitToChannelAsync(Guid channelId, CancellationToken ct = default) =>
            SetTransmissionAsync(TransmissionMode.Single(channelId), ct);

        /// <summary>Would a frame addressed to <paramref name="channelId"/> be forwarded under the current mode?</summary>
        public bool TransmitsTo(Guid channelId) => Transmission.Allows(channelId);

        /// <summary>
        /// Hear <paramref name="channelId"/> at full volume and every other joined channel attenuated by the
        /// server's <c>media.unfocused_channel_gain</c> (0.5 by default); null restores full volume everywhere.
        /// Receiver-local: multiplies per-participant volume, never overrides local mutes or blocks. A focus on a
        /// channel not joined yet is sent once it is joined; leaving the focused channel clears it.
        /// </summary>
        public Task SetChannelFocusAsync(Guid? channelId, CancellationToken ct = default)
        {
            if (channelId == Guid.Empty) channelId = null;
            bool send;
            lock (_channels)
            {
                _focusChannel = channelId;
                send = channelId == null || _joinedChannels.Contains(channelId.Value);
            }
            var control = _control;
            if (control == null || !send) return Task.CompletedTask;
            return control.SendAsync(ControlMessage.SetChannelFocus(channelId), ct);
        }

        /// <summary>
        /// Send a text message to every member of a joined channel. Completes with the server's copy (id,
        /// timestamp) once accepted — also raised through <see cref="OnChatMessage"/> — or faults with
        /// <c>CODE: message</c> (<c>AUTH_DENIED</c> not a member, <c>USER_MUTED</c>, <c>RATE_LIMIT_EXCEEDED</c>,
        /// <c>MESSAGE_BLOCKED</c> by the content filter, <c>VALIDATION_ERROR</c>).
        /// <paramref name="metadata"/> is an optional game payload (dictionary/list/string/number/bool) sent verbatim.
        /// </summary>
        public Task<ChatMessage> SendMessageAsync(Guid channelId, string text, object metadata = null, string clientRef = null, CancellationToken ct = default) =>
            SendChatAsync(r => ControlMessage.ChatSend(channelId, text, metadata, r), clientRef, ct);

        /// <summary>
        /// Send a text message to one user of the same app. Neither side may have blocked the other. When the
        /// server stores chat with offline delivery (<c>chat.persist</c> + <c>chat.offline_delivery</c>), an
        /// offline recipient gets the message on their next connect and the echo carries
        /// <see cref="ChatMessage.Offline"/>; otherwise the target must currently have a session (<c>USER_OFFLINE</c>).
        /// </summary>
        public Task<ChatMessage> SendDirectMessageAsync(Guid userId, string text, object metadata = null, string clientRef = null, CancellationToken ct = default) =>
            SendChatAsync(r => ControlMessage.ChatSendDirect(userId, text, metadata, r), clientRef, ct);

        /// <summary>
        /// One page of stored history of a joined channel, newest first. Page into the past with
        /// <paramref name="before"/> = <see cref="ChatHistoryPage.NextBefore"/>, catch up after a gap with
        /// <paramref name="after"/> = <see cref="ChatMessage.Cursor"/> of the last message seen. Faults with
        /// <c>AUTH_DENIED</c> for a channel this client has not joined, <c>NOT_FOUND</c> when the server does not store chat.
        /// </summary>
        public Task<ChatHistoryPage> HistoryAsync(Guid channelId, string before = null, string after = null, int? limit = null, CancellationToken ct = default) =>
            HistoryAsync(channelId, null, before, after, limit, ct);

        /// <summary>One page of the stored direct conversation with <paramref name="userId"/>; see <see cref="HistoryAsync(Guid, string, string, int?, CancellationToken)"/>.</summary>
        public Task<ChatHistoryPage> DirectHistoryAsync(Guid userId, string before = null, string after = null, int? limit = null, CancellationToken ct = default) =>
            HistoryAsync(null, userId, before, after, limit, ct);

        private async Task<ChatHistoryPage> HistoryAsync(Guid? channelId, Guid? userId, string before, string after, int? limit, CancellationToken ct)
        {
            EnsureConnected();
            var reference = $"h{Interlocked.Increment(ref _chatRefCounter)}-{DateTimeOffset.UtcNow.ToUnixTimeMilliseconds():x}";
            var tcs = new TaskCompletionSource<ChatHistoryPage>(TaskCreationOptions.RunContinuationsAsynchronously);
            lock (_pendingHistory) _pendingHistory[reference] = tcs;
            try
            {
                await _control.SendAsync(ControlMessage.ChatHistory(channelId, userId, before, after, limit, reference), ct).ConfigureAwait(false);
                using (var timeout = new CancellationTokenSource(RequestTimeout))
                using (timeout.Token.Register(() => tcs.TrySetException(new TimeoutException("history request timeout"))))
                using (ct.Register(() => tcs.TrySetCanceled()))
                    return await tcs.Task.ConfigureAwait(false);
            }
            finally { lock (_pendingHistory) _pendingHistory.Remove(reference); }
        }

        /// <summary>
        /// Move this user's read marker in a channel to <paramref name="messageId"/> (the newest message the
        /// user has seen). Idempotent and never moves backwards; every device of the user receives the new
        /// position through <see cref="OnChatReadMarker"/>.
        /// </summary>
        public Task MarkReadAsync(Guid channelId, Guid messageId, CancellationToken ct = default)
        {
            EnsureConnected();
            return _control.SendAsync(ControlMessage.ChatMarkRead(channelId, null, messageId), ct);
        }

        /// <summary>Move this user's read marker in the direct conversation with <paramref name="userId"/>; see <see cref="MarkReadAsync"/>.</summary>
        public Task MarkDirectReadAsync(Guid userId, Guid messageId, CancellationToken ct = default)
        {
            EnsureConnected();
            return _control.SendAsync(ControlMessage.ChatMarkRead(null, userId, messageId), ct);
        }

        /// <summary>Read markers and this user's unread count of a joined channel.</summary>
        public Task<ChatReadMarkers> ReadMarkersAsync(Guid channelId, CancellationToken ct = default) => ReadMarkersAsync(channelId, null, ct);

        /// <summary>Read markers and this user's unread count of the direct conversation with <paramref name="userId"/>.</summary>
        public Task<ChatReadMarkers> DirectReadMarkersAsync(Guid userId, CancellationToken ct = default) => ReadMarkersAsync(null, userId, ct);

        private static string ScopeKey(Guid? channelId, Guid? userId) =>
            channelId.HasValue ? "c:" + channelId.Value.ToString("D") : "u:" + (userId?.ToString("D") ?? string.Empty);

        private async Task<ChatReadMarkers> ReadMarkersAsync(Guid? channelId, Guid? userId, CancellationToken ct)
        {
            EnsureConnected();
            var key = ScopeKey(channelId, userId);
            var tcs = new TaskCompletionSource<ChatReadMarkers>(TaskCreationOptions.RunContinuationsAsynchronously);
            lock (_pendingReadMarkers)
            {
                if (!_pendingReadMarkers.TryGetValue(key, out var queue)) _pendingReadMarkers[key] = queue = new Queue<TaskCompletionSource<ChatReadMarkers>>();
                queue.Enqueue(tcs);
            }
            try
            {
                await _control.SendAsync(ControlMessage.ChatReadMarkers(channelId, userId), ct).ConfigureAwait(false);
                using (var timeout = new CancellationTokenSource(RequestTimeout))
                using (timeout.Token.Register(() => tcs.TrySetException(new TimeoutException("read markers request timeout"))))
                using (ct.Register(() => tcs.TrySetCanceled()))
                    return await tcs.Task.ConfigureAwait(false);
            }
            finally
            {
                lock (_pendingReadMarkers)
                {
                    if (_pendingReadMarkers.TryGetValue(key, out var queue))
                    {
                        // Normally already dequeued by the result; on timeout/cancel drop this entry only.
                        if (queue.Contains(tcs))
                        {
                            var rest = new Queue<TaskCompletionSource<ChatReadMarkers>>();
                            foreach (var t in queue) if (t != tcs) rest.Enqueue(t);
                            if (rest.Count == 0) _pendingReadMarkers.Remove(key); else _pendingReadMarkers[key] = rest;
                        }
                        else if (queue.Count == 0) _pendingReadMarkers.Remove(key);
                    }
                }
            }
        }

        /// <summary>
        /// Announce that this user is (not) typing in a channel. Best-effort: <c>typing = true</c> is coalesced
        /// to at most one message per <paramref name="interval"/> (default 1.5 s) so it can be called on every
        /// keystroke; <c>typing = false</c> is always sent and clears the throttle.
        /// </summary>
        public Task SetTypingAsync(Guid channelId, bool typing, TimeSpan? interval = null, CancellationToken ct = default)
        {
            var control = _control;
            if (control == null || !control.IsOpen) return Task.CompletedTask;
            var now = DateTime.UtcNow;
            lock (_typingSentAt)
            {
                if (typing)
                {
                    if (_typingSentAt.TryGetValue(channelId, out var last) && now - last < (interval ?? TimeSpan.FromMilliseconds(1500)))
                        return Task.CompletedTask;
                    _typingSentAt[channelId] = now;
                }
                else _typingSentAt.Remove(channelId);
            }
            return control.SendAsync(ControlMessage.ChatTyping(channelId, typing), ct);
        }

        /// <summary>Whether the server transcribes <paramref name="channelId"/> (per its <c>ChannelJoinAck</c>).</summary>
        public bool IsChannelTranscribed(Guid channelId)
        {
            lock (_channels) return _transcribedChannels.Contains(channelId);
        }

        /// <summary>
        /// Whether speech in <paramref name="channelId"/> is monitored by the operator's content-safety
        /// pipeline (transcribed and classified server-side, <c>ChannelConfig.safety_voice</c>). Disclose
        /// this to players, e.g. with a "voice chat is moderated" badge.
        /// </summary>
        public bool IsChannelMonitored(Guid channelId)
        {
            lock (_channels) return _monitoredChannels.Contains(channelId);
        }

        /// <summary>
        /// Presence / text range of a joined positional channel (see <see cref="ChannelScope"/>); both radii are
        /// <c>null</c> for unscoped channels, and the method returns <c>null</c> before the join is acknowledged.
        /// </summary>
        public ChannelScope? GetChannelScope(Guid channelId)
        {
            lock (_channels) return _channelScopes.TryGetValue(channelId, out var s) ? s : (ChannelScope?)null;
        }

        /// <summary>
        /// Our role, the member count (all nodes, hidden listeners included) and the roster policy of a joined
        /// channel (see <see cref="ChannelInfo"/>); <c>null</c> before the join is acknowledged.
        /// </summary>
        public ChannelInfo? GetChannelInfo(Guid channelId)
        {
            lock (_channels) return _channelInfos.TryGetValue(channelId, out var i) ? i : (ChannelInfo?)null;
        }

        /// <summary>
        /// Whether this session may transmit in <paramref name="channelId"/>: false for listeners (a grant with
        /// <c>speak: false</c>) and for channels not joined. Capture sent to a listener-only channel is dropped
        /// by the server, so use this to hide the push-to-talk / mute controls.
        /// </summary>
        public bool CanSpeakIn(Guid channelId)
        {
            lock (_channels) return _channelInfos.TryGetValue(channelId, out var i) && i.CanSpeak;
        }

        /// <summary>Whether this client receives <see cref="OnTranscript"/> (default true).</summary>
        public bool TranscriptsEnabled { get { lock (_channels) return _wantTranscripts; } }

        /// <summary>
        /// Opt out of (or back into) transcript delivery. Client-held: survives reconnects. Does not change
        /// whether the channel is transcribed for others.
        /// </summary>
        public Task SetTranscriptsAsync(bool enabled, CancellationToken ct = default)
        {
            lock (_channels) _wantTranscripts = enabled;
            var control = _control;
            if (control == null || !control.IsOpen) return Task.CompletedTask;
            return control.SendAsync(ControlMessage.SetTranscripts(enabled), ct);
        }

        /// <summary>Translation preference this client asked for (client-held; the server acks with <see cref="OnTranslationChanged"/>).</summary>
        public TranslationPrefs TranslationPrefs { get { lock (_channels) return _translation.Clone(); } }

        /// <summary>
        /// Receive transcripts translated into <paramref name="language"/> (BCP-47; null = originals only).
        /// Segments already in that language arrive untranslated; translated ones carry
        /// <see cref="Transcript.OriginalText"/>. <paramref name="spokenLanguage"/> tells peers' translators which
        /// language this participant speaks when the recogniser cannot tell; <paramref name="speech"/> also has the
        /// translation spoken privately to this client (needs <see cref="SessionInfo.Translation"/>.Speech). Requires
        /// transcripts to be enabled. Client-held: replayed after a fresh session.
        /// </summary>
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
            var control = _control;
            if (control == null || !control.IsOpen) return Task.CompletedTask;
            return control.SendAsync(ControlMessage.SetTranslation(prefs), ct);
        }

        /// <summary>
        /// Codec currently negotiated for this session's native media path (what to encode with and what
        /// <see cref="TryDequeueAudio"/> delivers). Opus until the server acknowledges a PCMU request.
        /// </summary>
        public AudioCodec AudioCodec { get { lock (_channels) return _activeCodec; } }

        /// <summary>Codec requested with <see cref="SetAudioCodecAsync"/>; re-negotiated after a fresh reconnect.</summary>
        public AudioCodec PreferredAudioCodec { get { lock (_channels) return _preferredCodec; } }

        /// <summary>
        /// Negotiate the session codec: <see cref="AudioCodec.Pcmu"/> (G.711 μ-law, 8 kHz, 64 kbit/s, no Opus
        /// needed on this device) or back to <see cref="AudioCodec.Opus"/>. Session-wide, not per channel: the
        /// server transcodes at the edge, so other participants keep hearing Opus. Keep encoding with
        /// <see cref="AudioCodec"/> until <see cref="OnAudioCodecChanged"/> confirms the switch; the request fails
        /// with <see cref="OnServerError"/> <c>CODEC_NOT_AVAILABLE</c> when the node disables the fallback
        /// (<c>media.pcmu_fallback = false</c>). Client-held: survives reconnects.
        /// </summary>
        public Task SetAudioCodecAsync(AudioCodec codec, CancellationToken ct = default)
        {
            bool send;
            lock (_channels)
            {
                _preferredCodec = codec;
                send = codec != _activeCodec;
            }
            var control = _control;
            if (control == null || !control.IsOpen || !send) return Task.CompletedTask;
            return control.SendAsync(ControlMessage.SetAudioCodec(codec), ct);
        }

        /// <summary>How channel audio currently reaches this session (server-acknowledged).</summary>
        public DownlinkMode DownlinkMode { get { lock (_channels) return _activeDownlink; } }

        /// <summary>Mode requested with <see cref="SetDownlinkModeAsync"/>; re-requested after a fresh reconnect.</summary>
        public DownlinkMode PreferredDownlinkMode { get { lock (_channels) return _preferredDownlink; } }

        /// <summary>
        /// <see cref="DownlinkMode.Mixed"/>: the node decodes every speaker we may hear, applies our mutes,
        /// volumes, focus and positional gains and sends one stereo Opus stream per channel
        /// (<see cref="IncomingAudio.Mixed"/>, under the channel's mix SSRC) instead of one stream per speaker —
        /// constant downlink bandwidth and decode cost in large channels. End-to-end encrypted speakers still
        /// arrive as separate streams. Needs <see cref="SessionInfo.DownlinkMix"/>; otherwise the request fails
        /// with <see cref="OnServerError"/> <c>DOWNLINK_MIX_NOT_AVAILABLE</c>. Takes effect on
        /// <see cref="OnDownlinkModeChanged"/>. Client-held: survives reconnects.
        /// </summary>
        public Task SetDownlinkModeAsync(DownlinkMode mode, CancellationToken ct = default)
        {
            bool send;
            lock (_channels)
            {
                _preferredDownlink = mode;
                send = mode != _activeDownlink;
            }
            var control = _control;
            if (control == null || !control.IsOpen || !send) return Task.CompletedTask;
            return control.SendAsync(ControlMessage.SetDownlinkMode(mode), ct);
        }

        /// <summary>
        /// Have the server synthesize <paramref name="text"/> and play it as this user's voice. Completes once
        /// the request is queued; <see cref="SpeechRequest.Done"/> and <see cref="OnTtsStatus"/> follow playback.
        /// <paramref name="channelId"/> may be null when the session transmits to exactly one channel. Faults with
        /// <c>CODE: message</c> on refusal (<c>FEATURE_DISABLED</c>, <c>AUTH_DENIED</c> not a member, <c>USER_MUTED</c>
        /// when server-muted, <c>VALIDATION_ERROR</c> text too long / unknown voice, <c>RATE_LIMIT_EXCEEDED</c>,
        /// <c>MESSAGE_BLOCKED</c> by the content filter).
        /// </summary>
        public async Task<SpeechRequest> SpeakAsync(string text, Guid? channelId = null, TtsDestination destination = TtsDestination.Channel,
            string voice = null, string clientRef = null, CancellationToken ct = default)
        {
            EnsureConnected();
            if (channelId == Guid.Empty) channelId = null;
            var reference = clientRef ?? $"t{Interlocked.Increment(ref _speakRefCounter)}-{DateTimeOffset.UtcNow.ToUnixTimeMilliseconds():x}";
            var tcs = new TaskCompletionSource<SpeechRequest>(TaskCreationOptions.RunContinuationsAsynchronously);
            lock (_pendingSpeak)
            {
                if (_pendingSpeak.ContainsKey(reference) || _speechDone.ContainsKey(reference))
                    throw new InvalidOperationException($"clientRef {reference} already pending");
                _pendingSpeak[reference] = tcs;
            }
            try
            {
                await _control.SendAsync(ControlMessage.TtsSpeak(channelId, text, voice, destination, reference), ct).ConfigureAwait(false);
                using (var timeout = new CancellationTokenSource(RequestTimeout))
                using (timeout.Token.Register(() => tcs.TrySetException(new TimeoutException("speak request timeout"))))
                using (ct.Register(() => tcs.TrySetCanceled()))
                    return await tcs.Task.ConfigureAwait(false);
            }
            finally { lock (_pendingSpeak) _pendingSpeak.Remove(reference); }
        }

        /// <summary>Cancel every queued or playing <see cref="SpeakAsync"/> request of this session.</summary>
        public Task CancelSpeechAsync(CancellationToken ct = default)
        {
            var control = _control;
            if (control == null || !control.IsOpen) return Task.CompletedTask;
            return control.SendAsync(ControlMessage.TtsCancel(), ct);
        }

        private async Task<ChatMessage> SendChatAsync(Func<string, string> build, string clientRef, CancellationToken ct)
        {
            EnsureConnected();
            var reference = clientRef ?? $"m{Interlocked.Increment(ref _chatRefCounter)}-{DateTimeOffset.UtcNow.ToUnixTimeMilliseconds():x}";
            var tcs = new TaskCompletionSource<ChatMessage>(TaskCreationOptions.RunContinuationsAsynchronously);
            lock (_pendingChat)
            {
                if (_pendingChat.ContainsKey(reference)) throw new InvalidOperationException($"clientRef {reference} already pending");
                _pendingChat[reference] = tcs;
            }
            try
            {
                await _control.SendAsync(build(reference), ct).ConfigureAwait(false);
                using (var timeout = new CancellationTokenSource(RequestTimeout))
                using (timeout.Token.Register(() => tcs.TrySetException(new TimeoutException("message send timeout"))))
                using (ct.Register(() => tcs.TrySetCanceled()))
                    return await tcs.Task.ConfigureAwait(false);
            }
            finally { lock (_pendingChat) _pendingChat.Remove(reference); }
        }

        private void FailPendingChat(Exception e)
        {
            List<TaskCompletionSource<ChatMessage>> pending;
            lock (_pendingChat) { pending = new List<TaskCompletionSource<ChatMessage>>(_pendingChat.Values); _pendingChat.Clear(); }
            foreach (var tcs in pending) tcs.TrySetException(e);
            List<TaskCompletionSource<ChatHistoryPage>> history;
            lock (_pendingHistory) { history = new List<TaskCompletionSource<ChatHistoryPage>>(_pendingHistory.Values); _pendingHistory.Clear(); }
            foreach (var tcs in history) tcs.TrySetException(e);
            var markers = new List<TaskCompletionSource<ChatReadMarkers>>();
            lock (_pendingReadMarkers)
            {
                foreach (var q in _pendingReadMarkers.Values) markers.AddRange(q);
                _pendingReadMarkers.Clear();
            }
            foreach (var tcs in markers) tcs.TrySetException(e);
            lock (_typingSentAt) _typingSentAt.Clear();
            List<TaskCompletionSource<SpeechRequest>> speak;
            List<TaskCompletionSource<TtsStatus>> done;
            lock (_pendingSpeak)
            {
                speak = new List<TaskCompletionSource<SpeechRequest>>(_pendingSpeak.Values); _pendingSpeak.Clear();
                done = new List<TaskCompletionSource<TtsStatus>>(_speechDone.Values); _speechDone.Clear();
            }
            foreach (var tcs in speak) tcs.TrySetException(e);
            foreach (var tcs in done) tcs.TrySetException(e);
            lock (_channels)
            {
                _transcribedChannels.Clear();
                _monitoredChannels.Clear();
                _channelScopes.Clear();
                _channelInfos.Clear();
            }
        }

        /// <summary>A fresh (non-resumed) session forgot our local mutes/volumes/transmission: send them again.</summary>
        private async Task ReplayReceiverPrefsAsync(CancellationToken ct)
        {
            var control = _control;
            if (control == null) return;
            List<Guid> everywhere;
            List<KeyValuePair<Guid, float>> volumes;
            TransmissionMode transmission;
            lock (_localMutes)
            {
                everywhere = new List<Guid>();
                foreach (var kv in _localMutes) if (kv.Value.Contains(Guid.Empty)) everywhere.Add(kv.Key);
            }
            lock (_volumes) volumes = new List<KeyValuePair<Guid, float>>(_volumes);
            lock (_channels) transmission = _transmission;
            foreach (var user in everywhere)
                await control.SendAsync(ControlMessage.SetParticipantMute(user, null, true), ct).ConfigureAwait(false);
            foreach (var kv in volumes)
                await control.SendAsync(ControlMessage.SetParticipantVolume(kv.Key, kv.Value), ct).ConfigureAwait(false);
            // `All` is the server default; `Single`/focus need the channel and follow its ChannelJoinAck.
            if (transmission.Kind == TransmissionKind.None)
                await control.SendAsync(ControlMessage.SetTransmission(transmission), ct).ConfigureAwait(false);
            bool wantTranscripts;
            AudioCodec codec;
            DownlinkMode downlink;
            TranslationPrefs translation;
            lock (_channels) { wantTranscripts = _wantTranscripts; codec = _preferredCodec; downlink = _preferredDownlink; translation = _translation; }
            if (!wantTranscripts)
                await control.SendAsync(ControlMessage.SetTranscripts(false), ct).ConfigureAwait(false);
            if (translation.Language != null || translation.SpokenLanguage != null)
                await control.SendAsync(ControlMessage.SetTranslation(translation), ct).ConfigureAwait(false);
            // A fresh session is Opus; the server confirms the switch with AudioCodecChanged.
            if (codec != AudioCodec.Opus)
                await control.SendAsync(ControlMessage.SetAudioCodec(codec), ct).ConfigureAwait(false);
            // Likewise per-speaker streams; the server confirms with DownlinkModeChanged.
            if (downlink != DownlinkMode.Streams)
                await control.SendAsync(ControlMessage.SetDownlinkMode(downlink), ct).ConfigureAwait(false);
        }

        /// <summary>Channel-scoped mutes, a <c>Single</c> target and the focus need membership, so they are (re-)sent after each successful join.</summary>
        private async Task ReplayChannelPrefsAsync(Guid channelId, CancellationToken ct)
        {
            var control = _control;
            if (control == null) return;
            var users = new List<Guid>();
            lock (_localMutes)
                foreach (var kv in _localMutes)
                    if (kv.Value.Contains(channelId) && !kv.Value.Contains(Guid.Empty)) users.Add(kv.Key);
            foreach (var user in users)
                await control.SendAsync(ControlMessage.SetParticipantMute(user, channelId, true), ct).ConfigureAwait(false);
            TransmissionMode transmission;
            Guid? focus;
            lock (_channels) { transmission = _transmission; focus = _focusChannel; }
            if (transmission.Kind == TransmissionKind.Single && transmission.ChannelId == channelId)
                await control.SendAsync(ControlMessage.SetTransmission(transmission), ct).ConfigureAwait(false);
            if (focus == channelId)
                await control.SendAsync(ControlMessage.SetChannelFocus(channelId), ct).ConfigureAwait(false);
        }

        /// <summary>
        /// Send one encoded Opus frame (20 ms @ 48 kHz recommended) to a channel. No-op while muted or when
        /// the current <see cref="Transmission"/> mode excludes that channel (the server would drop it anyway).
        /// <paramref name="channelHash"/> comes from <see cref="ChannelHash"/>.
        /// <paramref name="level"/> is the frame's measured <see cref="Audio.AudioLevel"/> (e.g. from a
        /// <see cref="Audio.VoiceActivityDetector"/>); when given, the server uses it for speaking detection and
        /// energy reports instead of mere packet arrival.
        /// </summary>
        public void SendOpusFrame(uint channelHash, byte[] opus, int length = -1, int samplesPerChannel = Audio.AudioFormat.FrameSamples, byte? level = null) =>
            SendAudioFrame(channelHash, AudioCodec.Opus, opus, length, samplesPerChannel, level);

        /// <summary>
        /// <see cref="SendOpusFrame"/> for a frame of <paramref name="codec"/>. Encode with whatever
        /// <see cref="AudioCodec"/> reports: a μ-law frame on an Opus session (or the reverse) is dropped by the
        /// server. <paramref name="samplesPerChannel"/> is still the 48 kHz duration (960 for 20 ms) so the RTP
        /// clock is codec-independent.
        /// </summary>
        public void SendAudioFrame(uint channelHash, AudioCodec codec, byte[] frame, int length = -1, int samplesPerChannel = Audio.AudioFormat.FrameSamples, byte? level = null)
        {
            if (_media == null || State != VoiceConnectionState.MediaBound) return;
            _rtpTimestamp = unchecked(_rtpTimestamp + (uint)samplesPerChannel);
            if (_muted || !AllowsHash(channelHash)) return;
            byte[] sealedFrame = null;
            SendToTarget(_media, channelHash, IsHashEncrypted(channelHash), _rtpTimestamp, codec, frame, length, level, ref sealedFrame);
        }

        /// <summary>
        /// Send one encoded Opus frame to every joined channel the current <see cref="Transmission"/> mode allows
        /// (all of them by default, one for <see cref="TransmissionMode.Single"/>, nothing for
        /// <see cref="TransmissionMode.None"/>). The RTP clock advances once per call. Returns the number of
        /// channels the frame was sent to.
        /// </summary>
        public int TransmitOpusFrame(byte[] opus, int length = -1, int samplesPerChannel = Audio.AudioFormat.FrameSamples, byte? level = null) =>
            TransmitAudioFrame(AudioCodec.Opus, opus, length, samplesPerChannel, level);

        /// <summary><see cref="TransmitOpusFrame"/> for a frame of <paramref name="codec"/> (see <see cref="SendAudioFrame"/>).</summary>
        public int TransmitAudioFrame(AudioCodec codec, byte[] frame, int length = -1, int samplesPerChannel = Audio.AudioFormat.FrameSamples, byte? level = null)
        {
            if (_media == null || State != VoiceConnectionState.MediaBound) return 0;
            _rtpTimestamp = unchecked(_rtpTimestamp + (uint)samplesPerChannel);
            if (_muted) return 0;
            var targets = new List<KeyValuePair<uint, bool>>();
            lock (_channels)
                foreach (var ch in _joinedChannels)
                    if (_transmission.Allows(ch))
                        targets.Add(new KeyValuePair<uint, bool>(ChannelHash(ch), _channelPolicies.TryGetValue(ch, out var p) && p.E2ee));
            int sent = 0;
            byte[] sealedFrame = null;
            foreach (var t in targets)
                if (SendToTarget(_media, t.Key, t.Value, _rtpTimestamp, codec, frame, length, level, ref sealedFrame)) sent++;
            return sent;
        }

        /// <summary>
        /// Hand one frame to the transport for a channel, sealing it once (<paramref name="sealedFrame"/> is
        /// reused across encrypted targets) when the channel is end-to-end encrypted. Only Opus goes to
        /// encrypted channels: a μ-law frame would need the server to transcode it, so it is not sent.
        /// </summary>
        private bool SendToTarget(MediaTransport media, uint channelHash, bool encrypted, uint ts, AudioCodec codec, byte[] frame, int length, byte? level, ref byte[] sealedFrame)
        {
            if (!encrypted)
            {
                media.SendAudio(channelHash, ts, codec, frame, length, level);
                return true;
            }
            if (codec != AudioCodec.Opus) return false;
            if (sealedFrame == null)
                lock (_e2ee) sealedFrame = _e2ee.Encrypt(frame, 0, length < 0 ? frame.Length : length);
            media.SendAudioE2ee(channelHash, ts, sealedFrame, level);
            Interlocked.Increment(ref _framesE2ee);
            return true;
        }

        private bool IsHashEncrypted(uint channelHash)
        {
            lock (_channels)
                foreach (var kv in _channelPolicies)
                    if (kv.Value.E2ee && ChannelHash(kv.Key) == channelHash) return true;
            return false;
        }

        private bool AllowsHash(uint channelHash)
        {
            TransmissionMode mode;
            lock (_channels) mode = _transmission;
            switch (mode.Kind)
            {
                case TransmissionKind.None: return false;
                case TransmissionKind.Single: return ChannelHash(mode.ChannelId) == channelHash;
                default: return true;
            }
        }

        /// <summary>
        /// Account for a captured frame that is deliberately not sent (client-side VAD gating / DTX) so the
        /// RTP clock of the next frame still reflects wall time.
        /// </summary>
        public void SkipFrame(int samplesPerChannel = Audio.AudioFormat.FrameSamples)
        {
            _rtpTimestamp = unchecked(_rtpTimestamp + (uint)samplesPerChannel);
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

        /// <summary>Sample <see cref="GetStats"/>, raise <see cref="OnStats"/> and report the measured RTT/jitter/loss to the server.</summary>
        public Task ReportQualityAsync(CancellationToken ct = default)
        {
            var s = GetStats();
            OnStats?.Invoke(s);
            return ReportQualityAsync(s.RttMs > 0f ? s.RttMs : s.ControlRttMs, s.JitterMs, s.LossPercent, ct);
        }

        /// <summary>
        /// Current statistics: transport counters, RTT min/avg/max, downlink jitter, jitter-buffer
        /// lost/late/underruns from <see cref="Mixer"/>, and the derived R-factor / MOS / 1–5 bars.
        /// Advances the loss period, so call it at a steady cadence (the periodic report does).
        /// </summary>
        public VoiceStats GetStats()
        {
            var media = _media;
            var mixer = Mixer;
            var s = new VoiceStats
            {
                State = State, ControlRttMs = ControlRttMs, Server = _serverQuality,
                FramesE2ee = Interlocked.Read(ref _framesE2ee), E2eeUndecryptable = Interlocked.Read(ref _e2eeUndecryptable),
            };
            if (media != null)
            {
                var rtt = media.Rtt;
                s.MediaPath = media.Path;
                s.PacketsSent = media.PacketsSent;
                s.BytesSent = media.BytesSent;
                s.PacketsReceived = media.PacketsReceived;
                s.BytesReceived = media.BytesReceived;
                s.BadAuth = media.PacketsBadAuth;
                s.Replayed = media.PacketsReplayed;
                s.HeartbeatsLost = media.HeartbeatsLost;
                s.HeartbeatsLostConsecutive = media.HeartbeatsLostConsecutive;
                s.UplinkDropped = media.UplinkDropped;
                s.RttMs = rtt.LastMs;
                s.RttMinMs = rtt.MinMs;
                s.RttAvgMs = rtt.AvgMs;
                s.RttMaxMs = rtt.MaxMs;
                s.JitterMs = media.DownlinkJitterMs;
            }
            if (mixer != null)
            {
                var t = mixer.Totals;
                s.FramesLost = t.Lost;
                s.FramesFecRecovered = t.FecRecovered;
                s.FramesLate = t.Late;
                s.Underruns = t.Underruns;
                s.ActiveStreams = mixer.ActiveStreams;
            }
            s.LossPercent = _lossWindow.Advance(s.FramesLost, s.PacketsReceived);
            s.RFactor = QualityModel.RFactor(s.RttMs > 0f ? s.RttMs : s.ControlRttMs, s.JitterMs, s.LossPercent);
            s.Mos = QualityModel.MosFromR(s.RFactor);
            s.Bars = QualityModel.BarsFromR(s.RFactor);
            return s;
        }

        public IReadOnlyList<Participant> GetParticipants(Guid channelId)
        {
            lock (_channels)
                return _channels.TryGetValue(channelId, out var m) ? new List<Participant>(m.Values) : new List<Participant>();
        }

        public Participant FindBySsrc(uint ssrc)
        {
            lock (_channels)
            {
                if (_bySsrc.TryGetValue(ssrc, out var p)) return p;
                // A participant's synthesized (TTS) voice shares its SSRC with the flag bit set.
                return (ssrc & AurxPacket.SynthSsrcFlag) != 0 && _bySsrc.TryGetValue(ssrc & ~AurxPacket.SynthSsrcFlag, out p) ? p : null;
            }
        }

        /// <summary>First roster entry of <paramref name="userId"/> across the joined channels, or null.</summary>
        public Participant FindByUser(Guid userId)
        {
            lock (_channels)
            {
                foreach (var m in _channels.Values)
                    if (m.TryGetValue(userId, out var p)) return p;
                return null;
            }
        }

        /// <summary>True for SSRCs of server-synthesized speech (a participant's TTS voice or a channel announcement).</summary>
        public static bool IsSynthesizedSsrc(uint ssrc) => (ssrc & AurxPacket.SynthSsrcFlag) != 0;

        /// <summary>
        /// Non-event alternative to <see cref="OnAudio"/> for audio-thread consumers. Frames of end-to-end
        /// encrypted channels come out opened (<see cref="IncomingAudio.E2ee"/> stays set); ones that cannot
        /// be opened — sender unknown, key not yet received, replay, tamper — and plaintext frames on an
        /// encrypted channel are dropped (<see cref="VoiceStats.E2eeUndecryptable"/>).
        /// </summary>
        public bool TryDequeueAudio(out IncomingAudio audio)
        {
            var media = _media;
            if (media != null) return DequeueAudio(media, out audio);
            audio = default;
            return false;
        }

        private bool DequeueAudio(MediaTransport media, out IncomingAudio audio)
        {
            while (media.TryDequeueAudio(out audio))
            {
                if (audio.E2ee)
                {
                    if (audio.Mixed) { Interlocked.Increment(ref _e2eeUndecryptable); continue; }
                    var sender = FindBySsrc(audio.SenderSsrc);
                    byte[] plain = null;
                    if (sender != null) lock (_e2ee) plain = _e2ee.Decrypt(sender.UserId, audio.Payload);
                    if (plain == null) { Interlocked.Increment(ref _e2eeUndecryptable); continue; }
                    audio.Payload = plain;
                    return true;
                }
                // Plaintext on an encrypted channel is never played: only the members' keys speak there.
                if (IsHashEncrypted(audio.ChannelHash)) { Interlocked.Increment(ref _e2eeUndecryptable); continue; }
                return true;
            }
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
                while (DequeueAudio(_media, out var a)) OnAudio?.Invoke(FindBySsrc(a.SenderSsrc), a);
            }

            E2eeRotateTick();

            if (_media != null && _control != null && _control.IsOpen && State == VoiceConnectionState.MediaBound)
                DriveMediaPath(_media, _control);

            if (_media != null && _control != null && _control.IsOpen && State == VoiceConnectionState.MediaBound
                && QualityReportInterval > TimeSpan.Zero && DateTime.UtcNow - _lastQualityReport > QualityReportInterval)
            {
                _lastQualityReport = DateTime.UtcNow;
                try { _ = ReportQualityAsync(); } catch (InvalidOperationException) { }
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
                bool probeExpired = _probeDeadline != DateTime.MaxValue && now > _probeDeadline;
                // Two unanswered pings (or an unanswered probe): the socket is half-open, treat it as lost
                // so a reconnect can start.
                if (probeExpired || (_lastPong < _lastPing && now - _lastPong > PingInterval + PingInterval + PingInterval))
                {
                    _probeDeadline = DateTime.MaxValue;
                    control.Dispose();
                    HandleClosed(control, probeExpired ? "probe timeout" : "ping timeout");
                }
            }
        }

        void IAurixVoiceClient.Update() => Update();

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
            DisposeCaptureProcessors();
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
            _serverQuality = null;
            _control?.Dispose();
            _control = null;
            _resumeToken = null;
            _joinTcs?.TrySetException(new OperationCanceledException("disconnected"));
            FailPendingChat(new OperationCanceledException("disconnected"));
            DropAllDucking();
            ResetLocalVoice();
            lock (_channels) { _channels.Clear(); _bySsrc.Clear(); _joinedChannels.Clear(); _activeCodec = AudioCodec.Opus; _activeDownlink = DownlinkMode.Streams; }
            E2eeReset();
        }

        /// <summary>Runs on the Update thread when a control channel closes or fails.</summary>
        private void HandleClosed(ControlChannel control, string reason)
        {
            if (!ReferenceEquals(control, _control)) return; // stale channel from before a reconnect
            _control = null;
            control.Dispose();
            _joinTcs?.TrySetException(new System.IO.IOException("connection lost"));
            FailPendingChat(new System.IO.IOException("connection lost"));
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
                    await ReattachAsync(attempt, ct).ConfigureAwait(false);
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
        /// since the UDP path may have changed with the network. The node that holds the session is
        /// tried first; later attempts rotate through the nodes it advertised for failover, where a
        /// takeover hands back the same session (<see cref="SessionInfo.Migrated"/>).
        /// </summary>
        private async Task ReattachAsync(int attempt, CancellationToken ct)
        {
            var previous = Session;
            string resume = previous != null && _resumeToken != null ? $"{previous.SessionId:D}.{_resumeToken}" : null;
            var url = ReconnectEndpoint(Endpoint, FailoverEndpoints, attempt);
            var refresher = TokenRefresher;
            if (refresher != null)
            {
                var fresh = await refresher(ct).ConfigureAwait(false);
                if (string.IsNullOrEmpty(fresh)) throw new InvalidOperationException("TokenRefresher returned no token");
                _token = fresh;
                lock (_channels) _localUserId = TokenUserId(fresh);
            }
            var info = await OpenSessionAsync(url, resume, ct).ConfigureAwait(false);
            List<Guid> rejoin = null;
            if (!info.Resumed)
            {
                // New session: the old memberships are gone on the server; drop the stale rosters and re-join.
                DropAllDucking();
                lock (_channels)
                {
                    rejoin = new List<Guid>(_joinedChannels);
                    _joinedChannels.Clear();
                    _channels.Clear();
                    _bySsrc.Clear();
                    _channelPolicies.Clear();
                }
                E2eeReset();
                foreach (var ch in rejoin) { var id = ch; Post(() => OnChannelLeft?.Invoke(id)); }
                await ReplayReceiverPrefsAsync(ct).ConfigureAwait(false);
            }
            else if (info.Migrated)
            {
                // Same speakers, new node: downlink numbering restarts and the old node's frames
                // still in flight must not be played.
                Mixer?.Resync();
            }
            await BindMediaAsync(info, ct).ConfigureAwait(false);
            if (rejoin != null)
                foreach (var ch in rejoin) await JoinChannelAsync(ch, ct).ConfigureAwait(false);
            Post(() => OnRecovered?.Invoke(info));
        }

        internal void HandleMessage(ControlMessage m)
        {
            OnControlMessage?.Invoke(m);
            switch (m.Type)
            {
                case "ChannelJoinAck":
                {
                    var channelId = m.Id("channel_id");
                    var roster = new List<Participant>();
                    bool encrypted;
                    lock (_channels)
                    {
                        var map = new Dictionary<Guid, Participant>();
                        foreach (var b in m.Participants())
                        {
                            var p = new Participant
                            {
                                UserId = b.UserId, DisplayName = b.DisplayName, Ssrc = b.Ssrc,
                                Role = b.Role, IsMuted = b.IsMuted, IsSpeaking = b.IsSpeaking, IsPriority = b.IsPriority,
                            };
                            map[p.UserId] = p;
                            _bySsrc[p.Ssrc] = p;
                            roster.Add(p);
                        }
                        _channels[channelId] = map;
                        _joinedChannels.Add(channelId);
                        if (m.Bool("transcription")) _transcribedChannels.Add(channelId); else _transcribedChannels.Remove(channelId);
                        if (m.Bool("safety_voice")) _monitoredChannels.Add(channelId); else _monitoredChannels.Remove(channelId);
                        _channelScopes[channelId] = new ChannelScope
                        {
                            RosterRadius = m.Has("roster_radius") ? (float?)m.Num("roster_radius") : null,
                            TextRadius = m.Has("text_radius") ? (float?)m.Num("text_radius") : null,
                        };
                        _channelInfos[channelId] = new ChannelInfo
                        {
                            Role = m.Has("role") ? ControlMessage.ParseRole(m.Str("role")) : ChannelRole.Speaker,
                            ParticipantCount = m.Has("participant_count") ? m.U32("participant_count") : (uint)roster.Count,
                            HiddenListeners = m.Bool("hidden_listeners"),
                            Transcription = m.Bool("transcription"),
                            SafetyVoice = m.Bool("safety_voice"),
                            Ducking = DuckingConfig.FromMessage(m),
                            IsPriority = m.Bool("priority"),
                        };
                        var policy = Audio.AudioPolicy.FromMessage(m);
                        if (policy.HasValue) _channelPolicies[channelId] = policy.Value;
                        encrypted = policy.HasValue && policy.Value.E2ee;
                    }
                    RefreshAudioPolicy();
                    if (encrypted)
                    {
                        var members = new List<Guid>(roster.Count);
                        foreach (var p in roster) members.Add(p.UserId);
                        E2eeJoined(channelId, members);
                    }
                    else E2eeLeft(channelId);
                    OnChannelJoined?.Invoke(channelId, roster);
                    RefreshDucking(channelId);
                    break;
                }
                case "ChannelAudioPolicy":
                {
                    var channelId = m.Id("channel_id");
                    var policy = Audio.AudioPolicy.FromMessage(m);
                    var ducking = DuckingConfig.FromMessage(m);
                    lock (_channels)
                    {
                        if (!_joinedChannels.Contains(channelId)) break;
                        if (policy.HasValue) _channelPolicies[channelId] = policy.Value;
                        if (_channelInfos.TryGetValue(channelId, out var info))
                        {
                            info.Ducking = ducking;
                            _channelInfos[channelId] = info;
                        }
                    }
                    if (policy.HasValue) RefreshAudioPolicy();
                    RefreshDucking(channelId);
                    break;
                }
                case "ParticipantJoined":
                {
                    var channelId = m.Id("channel_id");
                    var p = new Participant
                    {
                        UserId = m.Id("user_id"), DisplayName = m.Str("display_name") ?? string.Empty,
                        Ssrc = m.U32("ssrc"),
                        Role = m.Has("role") ? ControlMessage.ParseRole(m.Str("role")) : ChannelRole.Speaker,
                        IsMuted = m.Bool("is_muted"),
                        IsPriority = m.Bool("is_priority"),
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
                    if (p != null)
                    {
                        E2eePeerLeft(channelId, p.UserId);
                        OnParticipantLeft?.Invoke(channelId, p);
                        RefreshDucking(channelId);
                    }
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
                    RefreshDucking(m.Id("channel_id"));
                    break;
                }
                case "PriorityChanged":
                {
                    var channelId = m.Id("channel_id");
                    var userId = m.Id("user_id");
                    var priority = m.Bool("priority");
                    var p = Lookup(channelId, userId);
                    lock (_channels)
                    {
                        if (userId == _localUserId && _channelInfos.TryGetValue(channelId, out var info))
                        {
                            info.IsPriority = priority;
                            _channelInfos[channelId] = info;
                        }
                    }
                    if (p != null) p.IsPriority = priority;
                    OnParticipantPriorityChanged?.Invoke(channelId, userId, priority);
                    RefreshDucking(channelId);
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
                        Live = m.Bool("live"),
                    });
                    break;
                case "BitrateCommand":
                {
                    var cmd = new BitrateCommand
                    {
                        TargetBitrateKbps = m.U32("target_bitrate_kbps"),
                        Reason = m.Str("reason") ?? string.Empty,
                        ExpectedLossPercent = (int)m.U32("expected_loss_percent"),
                    };
                    if (cmd.TargetBitrateKbps > 0)
                    {
                        lock (_channels) _bitrateCommand = cmd;
                        ReapplyEncoder();
                    }
                    OnBitrateCommand?.Invoke(cmd);
                    break;
                }
                case "NetworkQuality":
                {
                    var q = NetworkQuality.FromMessage(m);
                    if (q.HasValue)
                    {
                        _serverQuality = q;
                        OnNetworkQuality?.Invoke(q.Value);
                    }
                    break;
                }
                case "Kick":
                {
                    var channelId = m.Id("channel_id");
                    DropDucking(channelId);
                    lock (_channels) { _channels.Remove(channelId); _joinedChannels.Remove(channelId); _channelPolicies.Remove(channelId); _channelInfos.Remove(channelId); }
                    E2eeLeft(channelId);
                    RefreshAudioPolicy();
                    OnKicked?.Invoke(channelId, m.Str("reason") ?? string.Empty);
                    break;
                }
                case "E2eeHello":
                {
                    // Only trustworthy with the server-authenticated sender and a channel scope.
                    var channelId = MiniJson.GetGuid(m.Data, "channel_id");
                    var userId = MiniJson.GetGuid(m.Data, "user_id");
                    var pk = Protocol.E2ee.ParsePublicKey(m.Str("public_key"));
                    if (!channelId.HasValue || !userId.HasValue || pk == null) break;
                    E2eeOnHello(channelId.Value, userId.Value, pk);
                    break;
                }
                case "E2eeSenderKey":
                {
                    var channelId = MiniJson.GetGuid(m.Data, "channel_id");
                    var from = MiniJson.GetGuid(m.Data, "from");
                    var pk = Protocol.E2ee.ParsePublicKey(m.Str("public_key"));
                    var wrapped = Protocol.E2ee.DecodeBytes(m.Str("key"));
                    var generation = m.Data != null && m.Data.ContainsKey("generation") ? m.Num("generation") : -1;
                    if (!channelId.HasValue || !from.HasValue || pk == null || wrapped == null || generation < 0 || generation > 255) break;
                    E2eeOnSenderKey(channelId.Value, from.Value, pk, (byte)generation, wrapped);
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
                    // On a resumed session the server's transmission/focus are authoritative (fresh sessions
                    // report the defaults and are followed by our replay).
                    var session = Session;
                    if (session != null && session.Resumed)
                        lock (_channels) { _transmission = prefs.Transmission; _focusChannel = prefs.FocusChannel; }
                    // The codec is authoritative either way: a fresh session reports Opus and our replay of a
                    // PCMU preference is answered by AudioCodecChanged afterwards.
                    bool codecChanged, downlinkChanged;
                    lock (_channels)
                    {
                        codecChanged = _activeCodec != prefs.Codec; _activeCodec = prefs.Codec;
                        downlinkChanged = _activeDownlink != prefs.Downlink; _activeDownlink = prefs.Downlink;
                    }
                    OnReceiverPreferences?.Invoke(prefs);
                    if (codecChanged) OnAudioCodecChanged?.Invoke(prefs.Codec);
                    if (downlinkChanged) OnDownlinkModeChanged?.Invoke(prefs.Downlink);
                    break;
                }
                case "AudioCodecChanged":
                {
                    var codec = m.AudioCodec();
                    lock (_channels) _activeCodec = codec;
                    OnAudioCodecChanged?.Invoke(codec);
                    break;
                }
                case "DownlinkModeChanged":
                {
                    var mode = m.DownlinkMode();
                    lock (_channels) _activeDownlink = mode;
                    OnDownlinkModeChanged?.Invoke(mode);
                    break;
                }
                case "TransmissionChanged":
                {
                    var mode = m.Transmission();
                    lock (_channels) _transmission = mode;
                    OnTransmissionChanged?.Invoke(mode);
                    break;
                }
                case "ChannelFocusChanged":
                {
                    var focus = m.FocusChannel();
                    lock (_channels) _focusChannel = focus;
                    OnChannelFocusChanged?.Invoke(focus);
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
                case "ChatMessageReceived":
                {
                    var chat = m.ChatMessage();
                    if (chat != null) OnChatMessage?.Invoke(chat);
                    break;
                }
                case "ParticipantTyping":
                    OnParticipantTyping?.Invoke(m.Id("channel_id"), m.Id("user_id"), m.Bool("typing"));
                    break;
                case "ChatReadMarker":
                {
                    var marker = m.ReadMarker();
                    if (marker != null) OnChatReadMarker?.Invoke(marker);
                    break;
                }
                case "ChatInboxSynced":
                    OnChatInboxSynced?.Invoke((int)m.Num("delivered"), m.Bool("truncated"));
                    break;
                case "Transcript":
                {
                    var t = m.Transcript();
                    if (t != null) OnTranscript?.Invoke(t);
                    break;
                }
                case "TranslationChanged":
                {
                    var prefs = m.TranslationPrefs();
                    lock (_channels) _translation = prefs.Clone();
                    OnTranslationChanged?.Invoke(prefs);
                    break;
                }
                case "TtsStatus":
                {
                    var s = m.TtsStatus();
                    if (s != null) OnTtsStatus?.Invoke(s);
                    break;
                }
                case "ChannelEnergy":
                {
                    var channelId = m.Id("channel_id");
                    var levels = m.Levels();
                    foreach (var l in levels)
                    {
                        var p = Lookup(channelId, l.UserId);
                        if (p != null) p.Energy = l.Energy;
                    }
                    OnChannelEnergy?.Invoke(channelId, levels);
                    break;
                }
                case "Pong":
                {
                    var sent = (long)m.Num("nonce");
                    ControlRttMs = Math.Max(0, DateTimeOffset.UtcNow.ToUnixTimeMilliseconds() - sent);
                    _lastPong = DateTime.UtcNow;
                    if ((ulong)sent >= _probeNonce) _probeDeadline = DateTime.MaxValue;
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
            else if (m.Type == "Error" && m.Str("client_ref") == null)
            {
                tcs.TrySetException(new InvalidOperationException($"{m.Str("code")}: {m.Str("message")}"));
            }
        }

        private void CompletePendingModeration(ControlMessage m)
        {
            var tcs = _moderationTcs;
            var pending = _pendingModeration;
            if (tcs == null || pending == null) return;
            if (m.Type == "ModerateParticipantAck")
            {
                var action = ControlMessage.ParseModerationAction(m.Str("action"));
                if (action.HasValue && ModerationKey(m.Id("channel_id"), m.Id("user_id"), action.Value) == pending)
                    tcs.TrySetResult(true);
            }
            else if (m.Type == "Error" && _joinTcs == null && m.Str("client_ref") == null)
            {
                tcs.TrySetException(new InvalidOperationException($"{m.Str("code")}: {m.Str("message")}"));
            }
        }

        /// <summary>
        /// Runs on the network thread: the sender's echo (which alone carries <c>client_ref</c>) or an
        /// <c>Error</c> tagged with the same <c>client_ref</c> settles the matching send.
        /// </summary>
        private void CompletePendingChat(ControlMessage m)
        {
            string reference;
            if (m.Type == "ChatMessageReceived") reference = m.ChatMessage()?.ClientRef;
            else if (m.Type == "Error") reference = m.Str("client_ref");
            else return;
            if (reference == null) return;
            TaskCompletionSource<ChatMessage> tcs;
            lock (_pendingChat) if (!_pendingChat.TryGetValue(reference, out tcs)) return;
            if (m.Type == "Error") tcs.TrySetException(new InvalidOperationException($"{m.Str("code")}: {m.Str("message")}"));
            else tcs.TrySetResult(m.ChatMessage());
        }

        /// <summary>
        /// Runs on the network thread: a <c>ChatHistoryResult</c> (or an <c>Error</c>) tagged with our
        /// <c>client_ref</c> settles the matching history request; a <c>ChatReadMarkersResult</c> settles the
        /// oldest read-marker query of the same conversation (the server answers them in order).
        /// </summary>
        private void CompletePendingHistory(ControlMessage m)
        {
            if (m.Type == "ChatReadMarkersResult")
            {
                var key = ScopeKey(MiniJson.GetGuid(m.Data, "channel_id"), MiniJson.GetGuid(m.Data, "user_id"));
                TaskCompletionSource<ChatReadMarkers> waiter = null;
                lock (_pendingReadMarkers)
                    if (_pendingReadMarkers.TryGetValue(key, out var queue) && queue.Count > 0)
                    {
                        waiter = queue.Dequeue();
                        if (queue.Count == 0) _pendingReadMarkers.Remove(key);
                    }
                waiter?.TrySetResult(m.ReadMarkers());
                return;
            }
            if (m.Type != "ChatHistoryResult" && m.Type != "Error") return;
            var reference = m.Str("client_ref");
            if (reference == null) return;
            TaskCompletionSource<ChatHistoryPage> tcs;
            lock (_pendingHistory) if (!_pendingHistory.TryGetValue(reference, out tcs)) return;
            if (m.Type == "Error") tcs.TrySetException(new InvalidOperationException($"{m.Str("code")}: {m.Str("message")}"));
            else tcs.TrySetResult(m.ChatHistory());
        }

        /// <summary>
        /// Runs on the network thread: the <c>queued</c> status settles the matching <see cref="SpeakAsync"/>,
        /// a terminal status completes its <see cref="SpeechRequest.Done"/>, an <c>Error</c> with the same
        /// <c>client_ref</c> faults the send.
        /// </summary>
        private void CompletePendingSpeak(ControlMessage m)
        {
            if (m.Type == "Error")
            {
                var reference = m.Str("client_ref");
                if (reference == null) return;
                TaskCompletionSource<SpeechRequest> pending;
                lock (_pendingSpeak) if (!_pendingSpeak.TryGetValue(reference, out pending)) return;
                pending.TrySetException(new InvalidOperationException($"{m.Str("code")}: {m.Str("message")}"));
                return;
            }
            if (m.Type != "TtsStatus") return;
            var status = m.TtsStatus();
            if (status?.ClientRef == null) return;
            TaskCompletionSource<SpeechRequest> speak;
            TaskCompletionSource<TtsStatus> done = null;
            lock (_pendingSpeak)
            {
                if (_pendingSpeak.TryGetValue(status.ClientRef, out speak) && !_speechDone.ContainsKey(status.ClientRef))
                {
                    done = new TaskCompletionSource<TtsStatus>(TaskCreationOptions.RunContinuationsAsynchronously);
                    _speechDone[status.ClientRef] = done;
                }
                else speak = null;
                if (status.IsTerminal && _speechDone.TryGetValue(status.ClientRef, out var finished))
                {
                    _speechDone.Remove(status.ClientRef);
                    finished.TrySetResult(status);
                }
            }
            speak?.TrySetResult(new SpeechRequest { RequestId = status.RequestId, ClientRef = status.ClientRef, Done = done.Task });
        }

        private Participant Lookup(Guid channelId, Guid userId)
        {
            lock (_channels)
                return _channels.TryGetValue(channelId, out var map) && map.TryGetValue(userId, out var p) ? p : null;
        }
    }
}
