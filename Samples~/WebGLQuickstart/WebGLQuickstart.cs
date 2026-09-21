#if UNITY_5_3_OR_NEWER
using System;
using System.Collections.Generic;
using System.Threading.Tasks;
using Aurix.Protocol;
using Aurix.Unity;
using Aurix.WebGL;
using UnityEngine;

namespace Aurix.Samples
{
    /// <summary>
    /// Voice lobby for Unity WebGL players: the browser flavour of <c>VoiceQuickstart</c>. Wires up
    /// <see cref="AurixWebGLVoiceBehaviour"/> (browser WebRTC through <c>aurix-web-sdk.js</c>), joins a channel
    /// and draws an IMGUI panel with the roster (speaking / muted), microphone and speaker mute, network quality,
    /// WebRTC statistics, per-participant track layout, an "Enable audio" button for the autoplay policy, reconnect
    /// and a chat line. Connection fields can be pre-filled from the page URL:
    /// <c>index.html?ws=wss://…/ws&amp;api=https://…&amp;token=…&amp;channel=…</c>.
    /// Outside a WebGL player (Editor Play mode, desktop) the panel explains that this sample needs a WebGL build —
    /// use <c>VoiceQuickstart</c> there.
    /// </summary>
    public sealed class WebGLQuickstart : MonoBehaviour
    {
        [Header("Connection")]
        [Tooltip("REST base URL of the Aurix API (https://host:8080): TURN credentials, chat history.")]
        public string ApiUrl = "http://127.0.0.1:8080";
        [Tooltip("wss://host:8081/ws of the Aurix node (ws:// only on http:// pages).")]
        public string WebSocketUrl = "ws://127.0.0.1:8081/ws";
        [Tooltip("Per-user JWT from your backend (POST /v1/tokens). Editable at runtime in the panel; never ship API keys.")]
        public string Token = "";
        [Tooltip("Channel UUID(s) the token grants, comma-separated.")]
        public string ChannelId = "";
        [Tooltip("Read ws / api / token / channel from the page query string on start.")]
        public bool ReadPageQuery = true;
        public bool ConnectOnStart = false;

        [Header("Panel")]
        public bool ShowPanel = true;
        [Range(240, 800)] public int PanelWidth = 400;
        [Range(5, 60)] public int LogLines = 12;

        public AurixWebGLVoiceBehaviour Voice { get; private set; }

        private readonly List<string> _log = new List<string>();
        private readonly Dictionary<Guid, List<Participant>> _rosters = new Dictionary<Guid, List<Participant>>();
        private NetworkQuality? _quality;
        private WebGLStats _stats;
        private IReadOnlyList<WebGLParticipantStream> _streams = Array.Empty<WebGLParticipantStream>();
        private float _nextStatsAt;
        private string _chatInput = "";
        private string _lastError;
        private bool _busy;
        private Vector2 _scroll;
        private Guid? _primaryChannel;

        /// <summary>True in a WebGL player, where the jslib plugin is linked.</summary>
        public static bool IsWebGLPlayer => NativeWebGLBridge.IsSupported;

        private void Awake()
        {
            Voice = GetComponent<AurixWebGLVoiceBehaviour>();
            if (Voice == null) Voice = gameObject.AddComponent<AurixWebGLVoiceBehaviour>();
            Voice.AutoConnectOnStart = false;
            if (ReadPageQuery) ApplyPageQuery(Application.absoluteURL);
        }

        private void Start()
        {
            if (ConnectOnStart && IsWebGLPlayer) _ = ConnectAsync();
        }

        private void Update()
        {
            var client = Voice.Client;
            if (client != null && client.IsCreated && client.State == VoiceConnectionState.Connected &&
                Time.unscaledTime >= _nextStatsAt)
            {
                _nextStatsAt = Time.unscaledTime + 1f;
                _ = RefreshStatsAsync(client);
            }
        }

        /// <summary>Apply <c>?ws=&amp;api=&amp;token=&amp;channel=</c> from a page URL (public for tests / custom loaders).</summary>
        public void ApplyPageQuery(string url)
        {
            foreach (var pair in Query(url))
            {
                switch (pair.Key)
                {
                    case "ws": WebSocketUrl = pair.Value; break;
                    case "api": ApiUrl = pair.Value; break;
                    case "token": Token = pair.Value; break;
                    case "channel": ChannelId = pair.Value; break;
                }
            }
        }

        internal static IEnumerable<KeyValuePair<string, string>> Query(string url)
        {
            if (string.IsNullOrEmpty(url)) yield break;
            int q = url.IndexOf('?');
            if (q < 0) yield break;
            string query = url.Substring(q + 1);
            int hash = query.IndexOf('#');
            if (hash >= 0) query = query.Substring(0, hash);
            foreach (var part in query.Split('&'))
            {
                if (part.Length == 0) continue;
                int eq = part.IndexOf('=');
                string key = eq < 0 ? part : part.Substring(0, eq);
                string value = eq < 0 ? "" : part.Substring(eq + 1);
                yield return new KeyValuePair<string, string>(Uri.UnescapeDataString(key), Uri.UnescapeDataString(value.Replace('+', ' ')));
            }
        }

        /// <summary>Connect with the current URLs/token and join the configured channel(s).</summary>
        public async Task ConnectAsync()
        {
            if (_busy) return;
            if (!IsWebGLPlayer) { _lastError = "this sample runs in a Unity WebGL build (use VoiceQuickstart elsewhere)"; return; }
            if (string.IsNullOrWhiteSpace(Token)) { _lastError = "token is empty"; return; }
            _busy = true;
            _lastError = null;
            _rosters.Clear();
            _quality = null;
            _stats = default;
            _streams = Array.Empty<WebGLParticipantStream>();
            _primaryChannel = null;
            try
            {
                Voice.ApiUrl = ApiUrl.Trim();
                Voice.WebSocketUrl = WebSocketUrl.Trim();
                Voice.Token = Token.Trim();
                Voice.ChannelId = ChannelId.Trim();
                var connect = Voice.Connect();
                // The client exists as soon as Connect() starts; subscribe before the first events arrive.
                if (Voice.Client != null) Subscribe(Voice.Client);
                await connect;
                var s = Voice.Client?.Session;
                if (s != null) Log($"connected: session {Short(s.SessionId)}{(s.Resumed ? " (resumed)" : "")}, media WebRTC");
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

        private void Subscribe(AurixWebGLVoiceClient client)
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
            client.OnSpeaking += (channel, p, speaking) =>
            {
                var roster = Roster(channel);
                int i = roster.FindIndex(x => x.UserId == p.UserId);
                if (i >= 0) roster[i] = p;
            };
            client.OnNetworkQuality += q => _quality = q;
            client.OnStats += s => _stats = s;
            client.OnParticipantStreams += layout => _streams = layout;
            client.OnRemoteAudio += (playing, reason) => Log(playing ? "remote audio playing" : $"remote audio blocked: {reason} — press Enable audio");
            client.OnChatMessage += m =>
            {
                string from = m.FromUserId == ChatMessage.SystemUserId ? "server" : m.DisplayName;
                Log($"[chat] {from}: {m.Text}");
            };
            client.OnKicked += (channel, reason) => Log($"kicked from {Short(channel)}: {reason}");
            client.OnRecording += r => Log($"recording {(r.Active ? "started" : "stopped")} in {Short(r.ChannelId)}");
            client.OnServerError += (code, message) => { _lastError = $"{code}: {message}"; Log($"server error {code}: {message}"); };
            client.OnError += e => { _lastError = e.Message; Log($"browser error: {e.Message}"); };
            client.OnEventsDropped += n => Log($"{n} event(s) dropped (frame stall) — roster re-synced");
            client.OnRecovering += (attempt, delay, reason) => Log($"reconnecting (attempt {attempt}, in {delay.TotalMilliseconds:F0} ms): {reason}");
            client.OnRecovered += s => Log(s.Resumed ? "session resumed (channels kept)" : "reconnected with a fresh session");
            client.OnEndpointChanged += url => Log($"moved to node {url}");
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

        private async Task RefreshStatsAsync(AurixWebGLVoiceClient client)
        {
            try { _stats = await client.GetStatsAsync(); }
            catch (Exception) { /* disconnected meanwhile */ }
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

        private async Task ResumeAudioAsync()
        {
            try { await Voice.ResumeAudio(); }
            catch (Exception e) { _lastError = e.Message; }
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
            bool connected = client != null && client.State == VoiceConnectionState.Connected;

            GUILayout.BeginArea(new Rect(10, 10, PanelWidth, Screen.height - 20), GUI.skin.box);
            _scroll = GUILayout.BeginScrollView(_scroll);
            GUILayout.Label("<b>Aurix voice — WebGL quick start</b>", Rich());
            if (!IsWebGLPlayer)
                GUILayout.Label("<color=#ffd080>Not a WebGL player: build for WebGL (File ▸ Build Settings ▸ WebGL) and open the page over http(s). Use the Voice quick start sample in the Editor.</color>", Rich());

            GUI.enabled = client == null && !_busy;
            GUILayout.Label("API URL");
            ApiUrl = GUILayout.TextField(ApiUrl);
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
                GUI.enabled = !_busy && IsWebGLPlayer;
                if (GUILayout.Button(_busy ? "Connecting…" : "Connect")) _ = ConnectAsync();
            }
            else
            {
                GUI.enabled = !_busy;
                if (GUILayout.Button("Disconnect")) _ = DisconnectAsync();
                if (GUILayout.Button("Reconnect")) client.ReconnectNow();
            }
            GUI.enabled = true;
            GUILayout.EndHorizontal();

            GUILayout.Label($"State: {(client == null ? VoiceConnectionState.Disconnected : client.State)}" +
                            (client?.Session != null ? $"  ·  session {Short(client.Session.SessionId)}" : "") +
                            (client?.Endpoint != null ? $"  ·  {client.Endpoint}" : ""));
            if (_lastError != null) GUILayout.Label($"<color=#ff8080>{_lastError}</color>", Rich());

            if (connected)
            {
                GUILayout.Space(6);
                if (!Voice.RemoteAudioPlaying)
                {
                    GUILayout.Label($"<color=#ffd080>Remote audio is blocked by the browser{(Voice.RemoteAudioBlockedReason != null ? ": " + Voice.RemoteAudioBlockedReason : "")}</color>", Rich());
                    if (GUILayout.Button("Enable audio")) _ = ResumeAudioAsync();
                }

                GUILayout.BeginHorizontal();
                bool muted = GUILayout.Toggle(client.IsMuted, " Mute microphone");
                if (muted != client.IsMuted) client.SetMuted(muted);
                bool speakerMuted = GUILayout.Toggle(Voice.OutputMuted, " Mute speakers");
                if (speakerMuted != Voice.OutputMuted) Voice.OutputMuted = speakerMuted; // applied in the behaviour's Update
                GUILayout.EndHorizontal();

                GUILayout.Label("Speaker volume");
                float volume = GUILayout.HorizontalSlider(Voice.OutputVolume, 0f, 1f);
                if (Math.Abs(volume - Voice.OutputVolume) > 0.001f) Voice.OutputVolume = volume;

                GUILayout.Space(6);
                GUILayout.Label("<b>Network (WebRTC)</b>", Rich());
                int bars = _quality?.Bars ?? _stats.Bars;
                GUILayout.Label($"Quality {BarsText(bars)}  R {(_quality?.RFactor ?? _stats.RFactor):F0}  MOS {(_quality?.Mos ?? _stats.Mos):F1}");
                GUILayout.Label($"RTT {_stats.RttMs:F0} ms (min {_stats.RttMinMs:F0} / avg {_stats.RttAvgMs:F0} / max {_stats.RttMaxMs:F0})  ·  ICE {_stats.IceRttMs:F0} ms");
                GUILayout.Label($"Downlink loss {_stats.LossPercent:F1}%  jitter {_stats.JitterMs:F1} ms  buffer {_stats.JitterBufferDelayMs:F0} ms  concealed {_stats.ConcealedSamples}");
                GUILayout.Label($"Packets ↑{_stats.PacketsSent} ↓{_stats.PacketsReceived}  lost {_stats.PacketsLost}  ·  uplink (remote view) loss {_stats.RemoteLossPercent:F1}% jitter {_stats.RemoteJitterMs:F1} ms");
                if (_quality.HasValue)
                    GUILayout.Label($"Server view: uplink loss {_quality.Value.UplinkLossPercent:F1}%  jitter {_quality.Value.UplinkJitterMs:F1} ms  {_quality.Value.UplinkBitrateKbps} kbps");

                GUILayout.Space(6);
                GUILayout.Label("<b>Participants</b>", Rich());
                foreach (var entry in _rosters)
                {
                    GUILayout.Label($"channel {Short(entry.Key)}{(entry.Key == _primaryChannel ? " (chat target)" : "")}");
                    if (entry.Value.Count == 0) GUILayout.Label("   nobody else here");
                    foreach (var p in entry.Value)
                    {
                        string flags = (p.IsSpeaking ? " 🔊" : "") + (p.IsMuted ? " muted" : "") + (p.IsServerMuted ? " server-muted" : "") +
                                       (client.IsParticipantSpatialized(p.UserId) ? " · own track (HRTF)" : " · mix");
                        GUILayout.Label($"   {p.DisplayName}{flags}  [{p.Role}]");
                    }
                }
                if (_streams.Count > 0)
                {
                    int live = 0;
                    foreach (var s in _streams) if (s.UserId != null) live++;
                    GUILayout.Label($"Per-participant tracks: {live}/{_streams.Count} in use");
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

        private static GUIStyle _rich;
        private static GUIStyle Rich()
        {
            if (_rich == null) _rich = new GUIStyle(GUI.skin.label) { richText = true };
            return _rich;
        }
    }
}
#endif
