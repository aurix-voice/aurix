#if UNITY_5_3_OR_NEWER
using System;
using System.Threading.Tasks;
using Aurix.WebGL;
using UnityEngine;

namespace Aurix.Unity
{
    /// <summary>
    /// Drop-in component for Unity WebGL players: connects an <see cref="AurixWebGLVoiceClient"/> (browser
    /// WebRTC through the Web SDK), joins channels and pumps its events every frame. The browser captures the
    /// microphone and plays the remote voices itself — there is no AudioSource, mixer or spatializer path
    /// (see the WebGL section of the SDK README). Ship <c>aurix-web-sdk.js</c> in <c>StreamingAssets/</c>
    /// or point <see cref="SdkUrl"/> at it.
    /// </summary>
    public sealed class AurixWebGLVoiceBehaviour : MonoBehaviour
    {
        [Header("Connection")]
        [Tooltip("REST base URL of the Aurix API (https://host:8080), used for TURN credentials and history.")]
        public string ApiUrl = "http://127.0.0.1:8080";
        [Tooltip("ws://host:8081/ws or wss://... (browsers require wss:// on https pages).")]
        public string WebSocketUrl = "ws://127.0.0.1:8081/ws";
        [Tooltip("Per-user JWT from your game backend (POST /v1/tokens). Never embed API keys in builds.")]
        public string Token;
        [Tooltip("Channel id(s) to join on connect, comma-separated.")]
        public string ChannelId;
        [Tooltip("URL of the standalone Web SDK bundle relative to index.html (or absolute). Ignored when the page " +
                 "already loaded it with a <script> tag.")]
        public string SdkUrl = AurixWebGLVoiceClient.DefaultSdkPath;
        public bool AutoConnectOnStart = false;

        [Header("Browser audio")]
        [Tooltip("Browser echo cancellation on the microphone track.")]
        public bool EchoCancellation = true;
        [Tooltip("Browser noise suppression on the microphone track.")]
        public bool NoiseSuppression = true;
        [Tooltip("Browser automatic gain control on the microphone track.")]
        public bool AutoGainControl = true;
        [Tooltip("Software microphone gain before encoding (1 = unity, 2 ≈ +6 dB).")]
        [Range(0f, 4f)] public float InputGain = 1f;
        [Tooltip("Master volume of all remote voices (1 = unity), on top of per-participant volumes.")]
        [Range(0f, 1f)] public float OutputVolume = 1f;
        [Tooltip("Speaker mute: hear nobody, without telling the server or affecting your microphone.")]
        public bool OutputMuted = false;
        [Tooltip("Force the WebRTC media through the node's TURN relay (restrictive networks).")]
        public bool UseTurn = false;
        [Tooltip("Per-participant downlink tracks to negotiate next to the server mix (-1 = as many as the node allows, 0 = mix only). Each is spatialized by the browser from UpdatePositionAsync positions.")]
        public int ParticipantStreams = -1;
        [Tooltip("How the browser renders per-participant tracks: HRTF (binaural), equal-power panning, or none (tracks negotiated, playback left to the page).")]
        public WebGLSpatialAudio SpatialAudio = WebGLSpatialAudio.Hrtf;

        /// <summary>The live client, or null before <see cref="Connect"/> / after <see cref="Disconnect"/>.</summary>
        public AurixWebGLVoiceClient Client { get; private set; }
        /// <summary>Set before <see cref="Connect"/> to supply a fresh JWT when the server reports the current one expired.</summary>
        public Func<System.Threading.CancellationToken, Task<string>> TokenRefresher;
        /// <summary>Set before <see cref="Connect"/> to supply per-channel join tokens on demand.</summary>
        public Func<Guid, System.Threading.CancellationToken, Task<string>> JoinTokenProvider;
        /// <summary>Test seam: a bridge other than the jslib plugin (null = <see cref="NativeWebGLBridge"/>).</summary>
        public IWebGLBridge Bridge;

        /// <summary>Last remote-audio playback state reported by the browser (false until the autoplay policy let it start).</summary>
        public bool RemoteAudioPlaying { get; private set; }
        /// <summary>Why remote audio is not playing (autoplay policy message), or null.</summary>
        public string RemoteAudioBlockedReason { get; private set; }

        private float _appliedInputGain = float.NaN;
        private float _appliedOutputVolume = float.NaN;
        private bool? _appliedOutputMuted;

        private void Start()
        {
            if (AutoConnectOnStart) _ = Connect();
        }

        public async Task Connect()
        {
            if (Client != null) await Disconnect();

            var client = new AurixWebGLVoiceClient(ApiUrl, WebSocketUrl, Token, Bridge);
            client.SdkUrl = SdkUrl;
            client.TokenRefresher = TokenRefresher;
            client.JoinTokenProvider = JoinTokenProvider;
            client.Options.EchoCancellation = EchoCancellation;
            client.Options.NoiseSuppression = NoiseSuppression;
            client.Options.AutoGainControl = AutoGainControl;
            client.Options.InputGain = InputGain;
            client.Options.UseTurn = UseTurn;
            client.Options.ParticipantStreams = ParticipantStreams < 0 ? (int?)null : ParticipantStreams;
            client.Options.SpatialAudio = SpatialAudio;
            client.OnRemoteAudio += (playing, reason) =>
            {
                RemoteAudioPlaying = playing;
                RemoteAudioBlockedReason = playing ? null : reason;
            };
            Client = client;
            _appliedInputGain = InputGain;
            _appliedOutputVolume = float.NaN;
            _appliedOutputMuted = null;

            await client.ConnectAsync();
            if (Client != client) return;

            foreach (var id in (ChannelId ?? string.Empty).Split(','))
            {
                var trimmed = id.Trim();
                if (trimmed.Length > 0) await client.JoinChannelAsync(Guid.Parse(trimmed));
            }
        }

        public async Task Disconnect()
        {
            var c = Client;
            Client = null;
            if (c == null) return;
            await c.DisconnectAsync();
            c.Dispose();
        }

        /// <summary>Retry remote playback after the browser blocked autoplay; call from a button/click handler.</summary>
        public Task ResumeAudio() => Client?.ResumeAudioAsync() ?? Task.CompletedTask;

        private void Update()
        {
            var c = Client;
            if (c == null) return;
            c.Update();
            if (!c.IsCreated) return;
            if (InputGain != _appliedInputGain)
            {
                _appliedInputGain = InputGain;
                c.SetInputGain(InputGain);
            }
            if (OutputVolume != _appliedOutputVolume)
            {
                _appliedOutputVolume = OutputVolume;
                c.SetOutputVolume(OutputVolume);
            }
            if (OutputMuted != _appliedOutputMuted)
            {
                _appliedOutputMuted = OutputMuted;
                c.SetOutputMuted(OutputMuted);
            }
        }

        private void OnDestroy()
        {
            _ = Disconnect();
        }
    }
}
#endif
