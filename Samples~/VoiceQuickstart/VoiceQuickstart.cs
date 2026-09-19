#if UNITY_5_3_OR_NEWER
using System;
using System.Collections.Generic;
using System.Threading.Tasks;
using Aurix.Audio;
using Aurix.Protocol;
using Aurix.Unity;
using UnityEngine;

namespace Aurix.Samples
{
    /// <summary>
    /// Self-contained voice quick start: connects <see cref="AurixVoiceBehaviour"/> with the Concentus codec,
    /// joins a channel, and draws an IMGUI panel with the roster (speaking / muted / energy), microphone and
    /// speaker mute, push-to-talk, network quality bars, client statistics, reconnect and a chat line.
    /// Add it to any GameObject (the required <see cref="AudioSource"/> and <see cref="AurixVoiceBehaviour"/>
    /// are created on demand) or open the bundled <c>VoiceQuickstart.unity</c> scene.
    /// </summary>
    public sealed class VoiceQuickstart : MonoBehaviour
    {
        [Header("Connection")]
        [Tooltip("ws://host:8081/ws or wss://... of the Aurix node.")]
        public string WebSocketUrl = "ws://127.0.0.1:8081/ws";
        [Tooltip("Per-user JWT from your backend (POST /v1/tokens). Editable at runtime in the panel; never ship API keys.")]
        public string Token = "";
        [Tooltip("Channel UUID(s) the token grants, comma-separated.")]
        public string ChannelId = "";
        public bool ConnectOnStart = false;

        [Header("Input")]
        [Tooltip("Hold this key to transmit; None = open microphone.")]
        public KeyCode PushToTalkKey = KeyCode.None;

        [Header("Panel")]
        public bool ShowPanel = true;
        [Range(240, 800)] public int PanelWidth = 380;
        [Range(5, 60)] public int LogLines = 12;

        public AurixVoiceBehaviour Voice { get; private set; }

        private readonly List<string> _log = new List<string>();
        private readonly Dictionary<Guid, List<Participant>> _rosters = new Dictionary<Guid, List<Participant>>();
        private NetworkQuality? _quality;
        private VoiceStats _stats;
        private float _nextStatsAt;
        private string _chatInput = "";
        private string _lastError;
        private bool _busy;
        private bool _pttHeld;
        private Vector2 _scroll;
        private Guid? _primaryChannel;

        private void Awake()
        {
            Voice = GetComponent<AurixVoiceBehaviour>();
            if (Voice == null) Voice = gameObject.AddComponent<AurixVoiceBehaviour>();
            // libopus through the native core when its binary is in Plugins/, pure C# Concentus otherwise.
            Voice.CodecFactory = NativeOpusCodec.IsAvailable
                ? (Func<IOpusCodec>)(() => new NativeOpusCodec(AudioFormat.SampleRate, 1, Voice.EncoderSettingsFromInspector()))
                : () => new ConcentusOpusCodec();
            Voice.AutoConnectOnStart = false;
            Voice.OnLocalSpeaking += speaking => Log(speaking ? "you started speaking" : "you stopped speaking");
            Voice.OnMicrophonePermissionDenied += () => Log("microphone permission denied — listening only");
            Voice.OnInputDeviceChanged += device => Log($"microphone: {device ?? "system default"}");
        }

        private void Start()
        {
            if (ConnectOnStart) _ = ConnectAsync();
        }

        private void Update()
        {
            if (PushToTalkKey != KeyCode.None && Voice.IsConnected)
            {
                bool held = Input.GetKey(PushToTalkKey);
                if (held != _pttHeld)
                {
                    _pttHeld = held;
                    Voice.SetMuted(!held);
                }
            }

            var client = Voice.Client;
            if (client != null && Time.unscaledTime >= _nextStatsAt)
            {
                _nextStatsAt = Time.unscaledTime + 1f;
                _stats = client.GetStats();
            }
        }

        /// <summary>Connect with the current URL/token and join the configured channel(s).</summary>
        public async Task ConnectAsync()
        {
            if (_busy) return;
            if (string.IsNullOrWhiteSpace(Token)) { _lastError = "token is empty"; return; }
            _busy = true;
            _lastError = null;
            _rosters.Clear();
            _quality = null;
            _primaryChannel = null;
            try
            {
                Voice.WebSocketUrl = WebSocketUrl.Trim();
                Voice.Token = Token.Trim();
                Voice.ChannelId = ChannelId.Trim();
                await Voice.Connect();
                Subscribe(Voice.Client);
                if (PushToTalkKey != KeyCode.None)
                {
                    _pttHeld = false;
                    Voice.SetMuted(true);
                }
                Log($"connected: session {Voice.Client.Session?.SessionId}, media {Voice.Client.Session?.MediaAddr}");
            }
            catch (Exception e)
            {
                _lastError = e.Message;
                Log($"connect failed: {e.Message}");
            }
            finally { _busy = false; }
        }

        public async Task DisconnectAsync()
        {
            if (_busy) return;
            _busy = true;
            try
            {
                await Voice.Disconnect();
                _rosters.Clear();
                _quality = null;
                Log("disconnected");
            }
            finally { _busy = false; }
        }

        private void Subscribe(AurixVoiceClient client)
        {
            client.OnStateChanged += state => Log($"state: {state}");
            client.OnChannelJoined += (channel, members) =>
            {
                _rosters[channel] = new List<Participant>(members);
                if (_primaryChannel == null) _primaryChannel = channel;
                Log($"joined {Short(channel)} with {members.Count} other(s)");
            };
            client.OnChannelLeft += channel =>
            {
                _rosters.Remove(channel);
                if (_primaryChannel == channel) _primaryChannel = null;
                Log($"left {Short(channel)}");
            };
            client.OnParticipantJoined += (channel, p) =>
            {
                Roster(channel).Add(p);
                Log($"{p.DisplayName} joined {Short(channel)}");
            };
            client.OnParticipantLeft += (channel, p) =>
            {
                Roster(channel).RemoveAll(x => x.UserId == p.UserId);
                Log($"{p.DisplayName} left {Short(channel)}");
            };
            client.OnParticipantUpdated += (channel, p) =>
            {
                var roster = Roster(channel);
                int i = roster.FindIndex(x => x.UserId == p.UserId);
                if (i >= 0) roster[i] = p; else roster.Add(p);
            };
            client.OnNetworkQuality += q => _quality = q;
            client.OnStats += s => _stats = s;
            client.OnChatMessage += m =>
            {
                string from = m.FromUserId == ChatMessage.SystemUserId ? "server" : m.DisplayName;
                Log($"[chat] {from}: {m.Text}");
            };
            client.OnKicked += (channel, reason) => Log($"kicked from {Short(channel)}: {reason}");
            client.OnRecording += r => Log($"recording {(r.Active ? "started" : "stopped")} in {Short(r.ChannelId)}");
            client.OnServerError += (code, message) => { _lastError = $"{code}: {message}"; Log($"server error {code}: {message}"); };
            client.OnRecovering += (attempt, delay, reason) => Log($"reconnecting (attempt {attempt}, in {delay.TotalMilliseconds:F0} ms): {reason}");
            client.OnRecovered += s => Log(s.Resumed ? "session resumed (same SSRC, channels kept)" : "reconnected with a fresh session");
            client.OnFailedToRecover += e => { _lastError = e.Message; Log($"gave up reconnecting: {e.Message}"); };
            client.OnSessionClosed += reason => Log($"session closed by server: {reason}");
            client.OnDisconnected += reason => Log($"connection lost: {reason}");
        }

        private List<Participant> Roster(Guid channel)
        {
            if (!_rosters.TryGetValue(channel, out var list))
            {
                list = new List<Participant>();
                _rosters[channel] = list;
            }
            return list;
        }

        private async Task SendChatAsync()
        {
            var client = Voice.Client;
            var text = _chatInput.Trim();
            if (client == null || _primaryChannel == null || text.Length == 0) return;
            _chatInput = "";
            try { await client.SendMessageAsync(_primaryChannel.Value, text); }
            catch (Exception e) { _lastError = e.Message; Log($"chat rejected: {e.Message}"); }
        }

        private void Log(string line)
        {
            _log.Add($"{DateTime.Now:HH:mm:ss} {line}");
            while (_log.Count > LogLines) _log.RemoveAt(0);
        }

        private static string Short(Guid id) => id.ToString("N").Substring(0, 8);

        private static string BarsText(int bars) => bars <= 0 ? "—" : new string('▮', bars) + new string('▯', 5 - bars);

        private void OnGUI()
        {
            if (!ShowPanel) return;
            var client = Voice.Client;
            bool connected = Voice.IsConnected;

            GUILayout.BeginArea(new Rect(10, 10, PanelWidth, Screen.height - 20), GUI.skin.box);
            _scroll = GUILayout.BeginScrollView(_scroll);
            GUILayout.Label("<b>Aurix voice — quick start</b>", Rich());

            GUI.enabled = client == null && !_busy;
            GUILayout.Label("WebSocket URL");
            WebSocketUrl = GUILayout.TextField(WebSocketUrl);
            GUILayout.Label("Player token (JWT)");
            Token = GUILayout.TextField(Token);
            GUILayout.Label("Channel id(s)");
            ChannelId = GUILayout.TextField(ChannelId);
            GUI.enabled = true;

            GUILayout.BeginHorizontal();
            if (client == null)
            {
                GUI.enabled = !_busy;
                if (GUILayout.Button(_busy ? "Connecting…" : "Connect")) _ = ConnectAsync();
            }
            else
            {
                GUI.enabled = !_busy;
                if (GUILayout.Button("Disconnect")) _ = DisconnectAsync();
                if (GUILayout.Button("Reconnect")) client.ForceReconnect("user requested");
            }
            GUI.enabled = true;
            GUILayout.EndHorizontal();

            GUILayout.Label($"State: {(client == null ? VoiceConnectionState.Disconnected : client.State)}" +
                            (client?.Session != null ? $"  ·  session {Short(client.Session.SessionId)}" : ""));
            GUILayout.Label($"Microphone: {(Voice.IsCapturing ? Voice.ActiveInputDevice ?? "system default" : "off")}  ·  permission {Voice.PermissionState}");
            if (_lastError != null) GUILayout.Label($"<color=#ff8080>{_lastError}</color>", Rich());

            if (connected)
            {
                GUILayout.Space(6);
                GUILayout.BeginHorizontal();
                if (PushToTalkKey == KeyCode.None)
                {
                    bool muted = GUILayout.Toggle(client.IsMuted, " Mute microphone");
                    if (muted != client.IsMuted) Voice.SetMuted(muted);
                }
                else
                {
                    GUILayout.Label($"Push-to-talk: hold {PushToTalkKey} ({(client.IsMuted ? "idle" : "transmitting")})");
                }
                bool speakerMuted = GUILayout.Toggle(Voice.OutputMuted, " Mute speakers");
                if (speakerMuted != Voice.OutputMuted) Voice.SetOutputMuted(speakerMuted);
                GUILayout.EndHorizontal();

                GUILayout.Label($"Mic level {Meter(Voice.Vad.Energy)}  {(Voice.Vad.Speaking ? "speaking" : "")}");
                GUILayout.Label("Speaker volume");
                float volume = GUILayout.HorizontalSlider(Voice.OutputVolume, 0f, 2f);
                if (Math.Abs(volume - Voice.OutputVolume) > 0.001f) Voice.SetOutputVolume(volume);

                GUILayout.Space(6);
                GUILayout.Label("<b>Network</b>", Rich());
                int bars = _quality?.Bars ?? _stats.Bars;
                GUILayout.Label($"Quality {BarsText(bars)}  R {(_quality?.RFactor ?? _stats.RFactor):F0}  MOS {(_quality?.Mos ?? _stats.Mos):F1}");
                GUILayout.Label($"RTT {_stats.RttMs:F0} ms (min {_stats.RttMinMs:F0} / avg {_stats.RttAvgMs:F0} / max {_stats.RttMaxMs:F0})  ·  WS {_stats.ControlRttMs:F0} ms");
                GUILayout.Label($"Downlink loss {_stats.LossPercent:F1}%  jitter {_stats.JitterMs:F1} ms  ·  streams {_stats.ActiveStreams}");
                if (_quality.HasValue)
                    GUILayout.Label($"Uplink (server view) loss {_quality.Value.UplinkLossPercent:F1}%  jitter {_quality.Value.UplinkJitterMs:F1} ms  {_quality.Value.UplinkBitrateKbps} kbps");
                GUILayout.Label($"Packets ↑{_stats.PacketsSent} ↓{_stats.PacketsReceived}  lost {_stats.FramesLost} late {_stats.FramesLate} underruns {_stats.Underruns}  bad auth {_stats.BadAuth}");

                GUILayout.Space(6);
                GUILayout.Label("<b>Participants</b>", Rich());
                foreach (var entry in _rosters)
                {
                    GUILayout.Label($"channel {Short(entry.Key)}{(entry.Key == _primaryChannel ? " (chat target)" : "")}");
                    if (entry.Value.Count == 0) GUILayout.Label("   nobody else here");
                    foreach (var p in entry.Value)
                    {
                        string flags = (p.IsSpeaking ? " 🔊" : "") + (p.IsMuted ? " muted" : "") + (p.IsServerMuted ? " server-muted" : "");
                        GUILayout.Label($"   {p.DisplayName} {Meter(p.Energy)}{flags}  [{p.Role}]");
                    }
                }

                GUILayout.Space(6);
                GUILayout.Label("<b>Chat</b>", Rich());
                GUILayout.BeginHorizontal();
                GUI.enabled = _primaryChannel != null;
                _chatInput = GUILayout.TextField(_chatInput);
                bool enter = Event.current.type == EventType.KeyDown && Event.current.keyCode == KeyCode.Return;
                if (GUILayout.Button("Send", GUILayout.Width(60)) || enter) _ = SendChatAsync();
                GUI.enabled = true;
                GUILayout.EndHorizontal();
            }

            GUILayout.Space(6);
            GUILayout.Label("<b>Log</b>", Rich());
            foreach (var line in _log) GUILayout.Label(line);

            GUILayout.EndScrollView();
            GUILayout.EndArea();
        }

        private static string Meter(float energy)
        {
            int filled = Mathf.Clamp(Mathf.RoundToInt(energy * 8f), 0, 8);
            return "[" + new string('#', filled) + new string('.', 8 - filled) + "]";
        }

        private static GUIStyle _rich;
        private static GUIStyle Rich()
        {
            if (_rich == null) _rich = new GUIStyle(GUI.skin.label) { richText = true };
            return _rich;
        }
    }
}
#endif
