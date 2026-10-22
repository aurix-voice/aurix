#if UNITY_EDITOR
using System;
using System.Collections.Generic;
using System.Text;
using System.Threading.Tasks;
using Aurix.Audio;
using UnityEditor;
using UnityEngine;

namespace Aurix.Editor
{
    /// <summary>
    /// <c>Aurix Voice ▸ Voice test window</c>: connect to a node, join a channel, watch the roster, quality and
    /// every event — from the editor, without entering Play mode. Also a microphone level test (local only).
    /// Audio playback needs an AudioSource, so what you hear is Play-mode only; everything else (auth, join,
    /// roster, mute state, stats, errors) works here, which is what support tickets are usually about.
    /// Token / URL / channel are kept in EditorPrefs (per user, never in the project).
    /// </summary>
    public sealed class AurixVoiceWindow : EditorWindow
    {
        private const string PrefUrl = "Aurix.Voice.Editor.Url";
        private const string PrefToken = "Aurix.Voice.Editor.Token";
        private const string PrefChannel = "Aurix.Voice.Editor.Channel";

        private string _url, _token, _channel;
        private AurixVoiceClient _client;
        private Task _busy;
        private readonly List<string> _log = new List<string>();
        private readonly Dictionary<Guid, List<Participant>> _rosters = new Dictionary<Guid, List<Participant>>();
        private NetworkQuality _quality;
        private VoiceStats _stats;
        private bool _hasStats;
        private Vector2 _logScroll, _rosterScroll;
        private bool _muted;

        private AudioClip _micClip;
        private string _micDevice;
        private int _micRate, _micReadPos;
        private float[] _micScratch;
        private readonly VoiceActivityDetector _vad = new VoiceActivityDetector();

        [MenuItem("Aurix Voice/Voice test window", false, 3)]
        public static void Open()
        {
            GetWindow<AurixVoiceWindow>("Aurix Voice");
        }

        private void OnEnable()
        {
            _url = EditorPrefs.GetString(PrefUrl, "wss://");
            _token = EditorPrefs.GetString(PrefToken, "");
            _channel = EditorPrefs.GetString(PrefChannel, "");
            EditorApplication.update += Pump;
        }

        private void OnDisable()
        {
            EditorApplication.update -= Pump;
            StopMicTest();
            if (_client != null) _ = Disconnect();
        }

        private void Pump()
        {
            _client?.Update();
            PumpMicTest();
            if (_client != null || _micClip != null) Repaint();
        }

        // ---- UI --------------------------------------------------------------------------------------------

        private void OnGUI()
        {
            bool connected = _client != null && _client.State != VoiceConnectionState.Disconnected;
            bool working = _busy != null && !_busy.IsCompleted;

            EditorGUILayout.LabelField("Connection", EditorStyles.boldLabel);
            using (new EditorGUI.DisabledScope(connected))
            {
                string url = EditorGUILayout.TextField("WebSocket URL", _url);
                string token = EditorGUILayout.PasswordField("Session token", _token);
                if (url != _url) { _url = url; EditorPrefs.SetString(PrefUrl, url); }
                if (token != _token) { _token = token; EditorPrefs.SetString(PrefToken, token); }
            }
            string channel = EditorGUILayout.TextField("Channel UUID", _channel);
            if (channel != _channel) { _channel = channel; EditorPrefs.SetString(PrefChannel, channel); }

            EditorGUILayout.BeginHorizontal();
            using (new EditorGUI.DisabledScope(working))
            {
                if (!connected)
                {
                    if (GUILayout.Button("Connect")) _busy = Connect();
                }
                else
                {
                    if (GUILayout.Button("Disconnect")) _busy = Disconnect();
                    if (GUILayout.Button("Join")) _busy = Join();
                    if (GUILayout.Button("Leave all")) _busy = LeaveAll();
                    bool muted = GUILayout.Toggle(_muted, "Muted");
                    if (muted != _muted) { _muted = muted; _client.SetMuted(muted); Log("muted = " + muted); }
                    if (GUILayout.Button("Reconnect now")) { _client.ReconnectNow(); Log("reconnect requested"); }
                }
            }
            EditorGUILayout.EndHorizontal();

            EditorGUILayout.Space();
            EditorGUILayout.LabelField("Status", EditorStyles.boldLabel);
            if (_client == null)
            {
                EditorGUILayout.HelpBox("Not connected. Mint a session token with your token server (or the aurix CLI) and paste it above; it is stored only in your EditorPrefs.", MessageType.Info);
            }
            else
            {
                var sb = new StringBuilder();
                sb.Append("state: ").Append(_client.State);
                if (_hasStats) sb.Append("   media: ").Append(_stats.MediaPath).Append("   rtt: ").Append(_stats.RttMs.ToString("0")).Append(" ms");
                if (_quality.Bars > 0) sb.Append("\nquality: ").Append(_quality.Bars).Append("/5 bars   MOS ").Append(_quality.Mos.ToString("0.00"))
                    .Append("   loss ↓").Append(_quality.DownlinkLossPercent.ToString("0.0")).Append("% ↑").Append(_quality.UplinkLossPercent.ToString("0.0"))
                    .Append("%   jitter ↓").Append(_quality.DownlinkJitterMs.ToString("0")).Append(" ms");
                if (_hasStats) sb.Append("\nsent ").Append(_stats.PacketsSent).Append(" pkts   received ").Append(_stats.PacketsReceived).Append(" pkts   heartbeats lost ").Append(_stats.HeartbeatsLost);
                EditorGUILayout.HelpBox(sb.ToString(), MessageType.None);
                EditorGUILayout.HelpBox("Remote voices are not played in edit mode (no AudioSource) — enter Play mode or use the VoiceQuickstart sample to listen.", MessageType.Info);
            }

            EditorGUILayout.Space();
            EditorGUILayout.LabelField("Participants", EditorStyles.boldLabel);
            _rosterScroll = EditorGUILayout.BeginScrollView(_rosterScroll, GUILayout.MinHeight(90));
            if (_rosters.Count == 0) EditorGUILayout.LabelField("—", EditorStyles.miniLabel);
            foreach (var entry in _rosters)
            {
                EditorGUILayout.LabelField("channel " + Short(entry.Key), EditorStyles.miniLabel);
                foreach (var p in entry.Value)
                {
                    string flags = (p.IsSpeaking ? " speaking" : "") + (p.IsMuted ? " muted" : "") + (p.IsServerMuted ? " server-muted" : "")
                                   + (p.IsPriority ? " priority" : "") + (p.CaptureUnavailable ? " no-mic" : "");
                    EditorGUILayout.LabelField("   " + p.DisplayName + "  [" + p.Role + "]" + flags);
                }
            }
            EditorGUILayout.EndScrollView();

            EditorGUILayout.Space();
            EditorGUILayout.LabelField("Microphone test (local, no network)", EditorStyles.boldLabel);
            EditorGUILayout.BeginHorizontal();
            if (_micClip == null)
            {
                if (GUILayout.Button("Start mic test")) StartMicTest();
            }
            else
            {
                if (GUILayout.Button("Stop mic test")) StopMicTest();
                GUILayout.Label((_micDevice ?? "default device") + "  level " + Meter(_vad.Energy) + (_vad.Speaking ? "  speaking" : ""));
            }
            EditorGUILayout.EndHorizontal();
            if (Microphone.devices.Length == 0) EditorGUILayout.HelpBox("No microphone devices visible to the editor.", MessageType.Warning);

            EditorGUILayout.Space();
            EditorGUILayout.BeginHorizontal();
            EditorGUILayout.LabelField("Events", EditorStyles.boldLabel);
            if (GUILayout.Button("Clear", GUILayout.Width(60))) _log.Clear();
            if (GUILayout.Button("Copy", GUILayout.Width(60))) GUIUtility.systemCopyBuffer = string.Join("\n", _log);
            if (GUILayout.Button("Check project setup", GUILayout.Width(140))) AurixVoiceMenu.CheckProjectSetup();
            EditorGUILayout.EndHorizontal();
            _logScroll = EditorGUILayout.BeginScrollView(_logScroll);
            foreach (var line in _log) EditorGUILayout.LabelField(line, EditorStyles.miniLabel);
            EditorGUILayout.EndScrollView();
        }

        // ---- actions ---------------------------------------------------------------------------------------

        private async Task Connect()
        {
            if (string.IsNullOrEmpty(_url) || string.IsNullOrEmpty(_token)) { Log("URL and token are required"); return; }
            var client = new AurixVoiceClient(_url, _token);
            client.OnStateChanged += s => Log("state → " + s);
            client.OnSessionReady += s => Log("session " + Short(s.SessionId) + " ready (ssrc " + s.Ssrc + ", media " + s.MediaAddr + ")");
            client.OnChannelJoined += (ch, members) => { _rosters[ch] = new List<Participant>(members); Log("joined " + Short(ch) + " with " + members.Count + " other(s)"); };
            client.OnChannelLeft += ch => { _rosters.Remove(ch); Log("left " + Short(ch)); };
            client.OnParticipantJoined += (ch, p) => { Roster(ch).Add(p); Log(p.DisplayName + " joined " + Short(ch)); };
            client.OnParticipantRemoved += (ch, p, reason) => { Roster(ch).RemoveAll(x => x.UserId == p.UserId); Log(p.DisplayName + " left " + Short(ch) + " (" + reason + ")"); };
            client.OnParticipantUpdated += (ch, p) => { var r = Roster(ch); int i = r.FindIndex(x => x.UserId == p.UserId); if (i >= 0) r[i] = p; else r.Add(p); };
            client.OnParticipantCaptureChanged += (ch, p, available) => Log(p.DisplayName + (available ? " has a microphone again" : " has no microphone"));
            client.OnNetworkQuality += q => _quality = q;
            client.OnStats += s => { _stats = s; _hasStats = true; };
            client.OnServerError += (code, message) => Log("server error " + code + ": " + message + " — " + AurixErrors.Describe(code));
            client.OnKicked += (ch, reason) => Log("kicked from " + Short(ch) + ": " + reason);
            client.OnRecovering += (attempt, delay, cause) => Log("recovering (attempt " + attempt + ", " + cause + ")");
            client.OnRecovered += _ => Log("recovered");
            client.OnFailedToRecover += e => Log("failed to recover: " + e.Message);
            client.OnDisconnected += reason => Log("disconnected: " + reason);
            _client = client;
            try
            {
                await client.ConnectAsync();
            }
            catch (Exception e)
            {
                Log("connect failed: " + e.Message);
                _client = null;
            }
        }

        private async Task Disconnect()
        {
            var client = _client;
            _client = null;
            _rosters.Clear();
            _hasStats = false;
            _quality = default;
            if (client == null) return;
            try { await client.DisconnectAsync("editor window"); }
            catch (Exception e) { Log("disconnect: " + e.Message); }
            client.Dispose();
        }

        private async Task Join()
        {
            if (_client == null) return;
            if (!Guid.TryParse(_channel, out var id)) { Log("channel must be a UUID"); return; }
            try { await _client.JoinChannelAsync(id); }
            catch (Exception e) { Log("join failed: " + e.Message); }
        }

        private async Task LeaveAll()
        {
            if (_client == null) return;
            foreach (var ch in _client.JoinedChannels)
            {
                try { await _client.LeaveChannelAsync(ch); }
                catch (Exception e) { Log("leave failed: " + e.Message); }
            }
        }

        // ---- microphone test -------------------------------------------------------------------------------

        private void StartMicTest()
        {
            _micDevice = Microphone.devices.Length > 0 ? Microphone.devices[0] : null;
            _micRate = AudioFormat.SampleRate;
            Microphone.GetDeviceCaps(_micDevice, out var minFreq, out var maxFreq);
            if (maxFreq != 0 && (_micRate < minFreq || _micRate > maxFreq)) _micRate = maxFreq;
            _micClip = Microphone.Start(_micDevice, true, 1, _micRate);
            _micReadPos = 0;
            _vad.Reset();
            if (_micClip == null) Log("Microphone.Start failed (no device or permission)");
        }

        private void StopMicTest()
        {
            if (_micClip == null) return;
            Microphone.End(_micDevice);
            _micClip = null;
        }

        private void PumpMicTest()
        {
            if (_micClip == null) return;
            int writePos = Microphone.GetPosition(_micDevice);
            int frame = _micRate * AudioFormat.FrameMs / 1000;
            int available = (writePos - _micReadPos + _micClip.samples) % _micClip.samples;
            while (available >= frame)
            {
                int needed = frame * _micClip.channels;
                if (_micScratch == null || _micScratch.Length != needed) _micScratch = new float[needed];
                _micClip.GetData(_micScratch, _micReadPos);
                _micReadPos = (_micReadPos + frame) % _micClip.samples;
                available -= frame;
                if (_micClip.channels > 1)
                {
                    for (int i = 0; i < frame; i++)
                    {
                        float sum = 0f;
                        for (int c = 0; c < _micClip.channels; c++) sum += _micScratch[i * _micClip.channels + c];
                        _micScratch[i] = sum / _micClip.channels;
                    }
                }
                _vad.Process(_micScratch, frame);
            }
        }

        // ---- helpers ---------------------------------------------------------------------------------------

        private List<Participant> Roster(Guid channel)
        {
            if (!_rosters.TryGetValue(channel, out var list)) _rosters[channel] = list = new List<Participant>();
            return list;
        }

        private void Log(string line)
        {
            _log.Add(DateTime.Now.ToString("HH:mm:ss.fff") + "  " + line);
            if (_log.Count > 500) _log.RemoveAt(0);
            Repaint();
        }

        private static string Short(Guid id) => id.ToString("N").Substring(0, 8);

        private static string Meter(float energy)
        {
            double e = energy < 0f ? 0.0 : energy > 1f ? 1.0 : energy;
            int n = Math.Min(10, Math.Max(0, (int)Math.Round(Math.Sqrt(e) * 10.0)));
            return new string('█', n) + new string('·', 10 - n);
        }
    }
}
#endif
