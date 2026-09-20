using System;
using System.Collections.Generic;

namespace Aurix.WebGL
{
    /// <summary>
    /// Browser-side settings of an <see cref="AurixWebGLVoiceClient"/>; read once when the browser client is
    /// created (the first <see cref="AurixWebGLVoiceClient.ConnectAsync"/>). Mirrors the Web SDK's
    /// <c>AurixClientOptions</c> minus what the bridge provides itself (token callbacks, audio output).
    /// </summary>
    public sealed class WebGLClientOptions
    {
        /// <summary>Force TURN relay for WebRTC (<c>iceTransportPolicy: "relay"</c>).</summary>
        public bool UseTurn;
        /// <summary>
        /// Extra ICE servers as the JSON array the browser expects (<c>[{"urls":"turn:…","username":"…","credential":"…"}]</c>);
        /// null = only what the Aurix node advertises.
        /// </summary>
        public string IceServersJson;
        public bool EchoCancellation = true;
        public bool NoiseSuppression = true;
        public bool AutoGainControl = true;
        /// <summary>Capture two channels and let the encoder send stereo where the channel policy allows it (music channels).</summary>
        public bool Stereo;
        /// <summary>Microphone <c>deviceId</c> (from <see cref="AurixWebGLVoiceClient.EnumerateDevicesAsync"/>); null = browser default.</summary>
        public string InputDeviceId;
        /// <summary>Software microphone gain (1 = unity).</summary>
        public float InputGain = 1f;
        /// <summary>Run the local voice-activity meter (<see cref="AurixWebGLVoiceClient.OnLocalSpeaking"/>).</summary>
        public bool LocalVoiceActivity = true;
        public TimeSpan PingInterval = TimeSpan.FromSeconds(15);
        public TimeSpan QualityReportInterval = TimeSpan.FromSeconds(5);
        public TimeSpan RequestTimeout = TimeSpan.FromSeconds(10);
        public bool AutoReconnect = true;
        public ReconnectPolicy Reconnect = new ReconnectPolicy();
        /// <summary>Also surface the raw server control messages through <see cref="AurixWebGLVoiceClient.OnControlMessage"/>.</summary>
        public bool RawMessages;
        /// <summary>
        /// Per-participant WebRTC downlink tracks to negotiate on top of the mixed one (browser-side gain/HRTF per
        /// speaker). null = as many as the node allows (<c>webrtc_participant_streams</c>), 0 = mixed only.
        /// </summary>
        public int? ParticipantStreams;
        /// <summary>How the browser renders the dedicated tracks; <see cref="WebGLSpatialAudio.Hrtf"/> by default.</summary>
        public WebGLSpatialAudio SpatialAudio = WebGLSpatialAudio.Hrtf;

        internal Dictionary<string, object> ToBridge(string apiUrl, string wsUrl, string token, bool refreshToken, bool joinToken)
        {
            var o = new Dictionary<string, object>
            {
                { "apiUrl", apiUrl },
                { "wsUrl", wsUrl },
                { "token", token },
                { "refreshToken", refreshToken },
                { "joinToken", joinToken },
                { "useTurn", UseTurn },
                { "inputGain", (double)InputGain },
                { "localVoiceActivity", LocalVoiceActivity },
                { "pingIntervalMs", PingInterval.TotalMilliseconds },
                { "qualityReportIntervalMs", QualityReportInterval.TotalMilliseconds },
                { "requestTimeoutMs", RequestTimeout.TotalMilliseconds },
                { "autoReconnect", AutoReconnect },
                { "rawMessages", RawMessages },
                {
                    "audioConstraints", new Dictionary<string, object>
                    {
                        { "echoCancellation", EchoCancellation },
                        { "noiseSuppression", NoiseSuppression },
                        { "autoGainControl", AutoGainControl },
                        { "channelCount", Stereo ? 2 : 1 },
                    }
                },
            };
            if (Stereo) o["opus"] = new Dictionary<string, object> { { "stereo", true } };
            if (ParticipantStreams.HasValue) o["participantStreams"] = Math.Max(0, ParticipantStreams.Value);
            switch (SpatialAudio)
            {
                case WebGLSpatialAudio.EqualPower: o["spatialAudio"] = "equalpower"; break;
                case WebGLSpatialAudio.None: o["spatialAudio"] = false; break;
            }
            if (!string.IsNullOrEmpty(IceServersJson)) o["iceServers"] = Protocol.MiniJson.Parse(IceServersJson);
            if (!string.IsNullOrEmpty(InputDeviceId)) o["inputDeviceId"] = InputDeviceId;
            if (Reconnect != null)
                o["reconnect"] = new Dictionary<string, object>
                {
                    { "initialDelayMs", Reconnect.InitialDelay.TotalMilliseconds },
                    { "maxDelayMs", Reconnect.MaxDelay.TotalMilliseconds },
                    { "factor", Reconnect.Factor },
                    { "jitter", Reconnect.Jitter },
                    { "maxAttempts", Reconnect.MaxAttempts },
                };
            return o;
        }
    }

    /// <summary>Rendering of per-participant downlink tracks in the browser (Web Audio <c>PannerNode</c>).</summary>
    public enum WebGLSpatialAudio
    {
        /// <summary>Binaural HRTF panning for positional channels, plain gain elsewhere.</summary>
        Hrtf,
        /// <summary>Cheaper stereo panning (<c>panningModel: "equalpower"</c>).</summary>
        EqualPower,
        /// <summary>
        /// Negotiate the tracks but do not play them from the SDK: the host page renders the
        /// <c>MediaStream</c>s itself (only the mixed track is played). Without a host-side renderer
        /// the dedicated speakers stay silent — prefer <see cref="Hrtf"/> unless you know why.
        /// </summary>
        None,
    }

    /// <summary>One negotiated per-participant WebRTC track (<c>ParticipantStreams</c> snapshot).</summary>
    public struct WebGLParticipantStream
    {
        /// <summary>SDP media id of the track.</summary>
        public string Mid;
        /// <summary>Who the node currently forwards on this track; null while the slot is idle.</summary>
        public Guid? UserId;
        /// <summary>True once the browser received the track (a <c>MediaStream</c> exists on the page).</summary>
        public bool Live;
    }

    /// <summary>Browser media statistics (WebRTC <c>getStats</c> plus the control-plane RTT); see <see cref="AurixWebGLVoiceClient.GetStatsAsync"/>.</summary>
    public struct WebGLStats
    {
        public float RttMs;
        public float RttMinMs;
        public float RttAvgMs;
        public float RttMaxMs;
        public float IceRttMs;
        public float JitterMs;
        public float LossPercent;
        public long PacketsReceived;
        public long PacketsLost;
        public long BytesReceived;
        public long ConcealedSamples;
        public long PacketsDiscarded;
        public float JitterBufferDelayMs;
        public long PacketsSent;
        public long BytesSent;
        public float RemoteLossPercent;
        public float RemoteJitterMs;
        public float RFactor;
        public float Mos;
        public int Bars;
    }

    /// <summary>A microphone or speaker the browser exposes (labels are empty until the user granted microphone access).</summary>
    public sealed class WebGLAudioDevice
    {
        public string DeviceId;
        public string GroupId;
        public string Label;
        public bool IsOutput;
    }

    public sealed class WebGLAudioDevices
    {
        public List<WebGLAudioDevice> Inputs = new List<WebGLAudioDevice>();
        public List<WebGLAudioDevice> Outputs = new List<WebGLAudioDevice>();
    }

    /// <summary>A failure reported by the browser side of the bridge (<c>{"ok":false,"error":{…}}</c> or a rejected promise).</summary>
    public sealed class WebGLBridgeException : Exception
    {
        /// <summary>JavaScript error name (<c>Error</c>, <c>NotAllowedError</c>, …).</summary>
        public string Name { get; }
        /// <summary>Aurix error code when the server rejected the request (<c>AUTH_DENIED</c>, <c>CHANNEL_FULL</c>, …), else null.</summary>
        public string Code { get; }

        public WebGLBridgeException(string message, string name = null, string code = null) : base(message)
        {
            Name = name;
            Code = code;
        }
    }
}
