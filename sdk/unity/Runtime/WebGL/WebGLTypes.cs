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
        /// <summary>
        /// Fetch the node's TURN credentials (<c>GET /v1/me/turn-credentials</c>) and add its relay to the ICE servers, like
        /// the Web SDK does by default; failures are non-fatal. False = host/STUN candidates only.
        /// </summary>
        public bool UseTurn = true;
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
        /// <summary>
        /// Group end-to-end encryption. true (default): announce the capability when the browser has
        /// WebCrypto and an encoded-frame API so channels created with <c>e2ee: true</c> can be joined
        /// (without them the join fails with <c>E2EE_REQUIRED</c>; there is no plaintext fallback).
        /// false: never join encrypted channels.
        /// </summary>
        public bool E2ee = true;
        /// <summary>
        /// 32-byte X25519 identity secret from an earlier <see cref="AurixWebGLVoiceClient.ExportE2eeIdentity"/>;
        /// keeps this player's E2EE fingerprint stable across sessions. null = a fresh random identity.
        /// </summary>
        public byte[] E2eeIdentity;
        /// <summary>Which encoded-frame API the browser transform uses; <see cref="WebGLE2eeTransform.Auto"/> by default.</summary>
        public WebGLE2eeTransform E2eeTransform = WebGLE2eeTransform.Auto;
        /// <summary>Serve the E2EE transform worker from this URL instead of a <c>blob:</c> URL (CSP without <c>worker-src blob:</c>).</summary>
        public string E2eeWorkerUrl;
        /// <summary>
        /// Push <see cref="AurixWebGLVoiceClient.OnParticipantVisemes"/> / <see cref="AurixWebGLVoiceClient.OnLocalVisemes"/>
        /// (50 events/s per analysed voice) while lip-sync is on. Off by default — poll
        /// <see cref="AurixWebGLVoiceClient.GetParticipantVisemes"/> once per rendered frame instead.
        /// </summary>
        public bool VisemeEvents;
        /// <summary>Microphone effect chain from the start (<see cref="Audio.VoiceEffectParams.Bypass"/> = none).</summary>
        public Audio.VoiceEffectParams VoiceEffects = Audio.VoiceEffectParams.Bypass;

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
                { "visemeEvents", VisemeEvents },
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
            var effects = VoiceEffects.Sanitized();
            if (!effects.IsBypass) o["voiceEffects"] = BridgeJson.VoiceEffectsToBridge(effects);
            if (ParticipantStreams.HasValue) o["participantStreams"] = Math.Max(0, ParticipantStreams.Value);
            switch (SpatialAudio)
            {
                case WebGLSpatialAudio.EqualPower: o["spatialAudio"] = "equalpower"; break;
                case WebGLSpatialAudio.None: o["spatialAudio"] = false; break;
            }
            if (!E2ee)
            {
                o["e2ee"] = false;
            }
            else if (E2eeIdentity != null || E2eeTransform != WebGLE2eeTransform.Auto || !string.IsNullOrEmpty(E2eeWorkerUrl))
            {
                var e2ee = new Dictionary<string, object>();
                if (E2eeIdentity != null)
                {
                    if (E2eeIdentity.Length != 32) throw new ArgumentException("E2eeIdentity must be 32 bytes", nameof(E2eeIdentity));
                    e2ee["identity"] = Convert.ToBase64String(E2eeIdentity);
                }
                switch (E2eeTransform)
                {
                    case WebGLE2eeTransform.ScriptTransform: e2ee["transform"] = "script"; break;
                    case WebGLE2eeTransform.EncodedStreams: e2ee["transform"] = "streams"; break;
                }
                if (!string.IsNullOrEmpty(E2eeWorkerUrl)) e2ee["workerUrl"] = E2eeWorkerUrl;
                o["e2ee"] = e2ee;
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

    /// <summary>Browser API that encrypts/decrypts encoded Opus frames for E2EE channels.</summary>
    public enum WebGLE2eeTransform
    {
        /// <summary>Prefer <c>RTCRtpScriptTransform</c> (worker), fall back to <c>createEncodedStreams()</c>.</summary>
        Auto,
        /// <summary>Only <c>RTCRtpScriptTransform</c>.</summary>
        ScriptTransform,
        /// <summary>Only Chromium's <c>createEncodedStreams()</c>.</summary>
        EncodedStreams,
    }

    /// <summary>Counters of the browser's encrypted-frame path (see <see cref="AurixWebGLVoiceClient.GetE2eeStatsAsync"/>).</summary>
    public struct WebGLE2eeStats
    {
        /// <summary>Frames sealed on the uplink plus frames opened on the downlink.</summary>
        public long FramesE2ee;
        /// <summary>Received frames dropped because no key of their sender could open them.</summary>
        public long Undecryptable;
        /// <summary>Uplink frames dropped while the channel's encryption state was still unknown.</summary>
        public long Held;
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
