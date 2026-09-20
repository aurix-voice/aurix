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
