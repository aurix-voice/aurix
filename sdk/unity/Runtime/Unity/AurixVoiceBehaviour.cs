#if UNITY_5_3_OR_NEWER
using System;
using System.Threading.Tasks;
using Aurix.Audio;
using UnityEngine;

namespace Aurix.Unity
{
    /// <summary>
    /// Drop-in Unity component: captures the microphone, encodes with the codec you provide,
    /// sends AURX/UDP audio, and plays the mixed remote participants through an AudioSource.
    /// Assign <see cref="CodecFactory"/> before calling <see cref="Connect"/> (see the Concentus sample).
    /// </summary>
    [RequireComponent(typeof(AudioSource))]
    public sealed class AurixVoiceBehaviour : MonoBehaviour
    {
        [Header("Connection")]
        [Tooltip("ws://host:8081/ws or wss://...")]
        public string WebSocketUrl = "ws://127.0.0.1:8081/ws";
        [Tooltip("Per-user JWT from your game backend (POST /v1/tokens). Never embed API keys in builds.")]
        public string Token;
        public string ChannelId;

        [Header("Audio")]
        public string MicrophoneDevice = null;
        [Range(6000, 128000)] public int BitrateBps = 32000;
        public bool AutoConnectOnStart = false;

        [Header("Voice activity")]
        [Tooltip("RMS level (0..1) above which a frame counts as speech. 0.01 ≈ -40 dBov.")]
        [Range(0f, 0.2f)] public float VadThreshold = 0.01f;
        [Tooltip("Quiet frames (20 ms each) before local speech is considered over.")]
        [Range(1, 100)] public int VadHangoverFrames = 15;
        [Tooltip("Do not send frames while the local VAD says silence (saves uplink; DTX-like). " +
                 "Off: every frame is sent, tagged with its level, and the server decides who is speaking.")]
        public bool GateOnVad = false;

        /// <summary>Creates encoder/decoder instances. Must be set by your code (platform/licensing choice).</summary>
        public Func<IOpusCodec> CodecFactory;

        public AurixVoiceClient Client { get; private set; }
        public bool IsConnected => Client != null && Client.State == VoiceConnectionState.MediaBound;

        /// <summary>Local microphone meter/VAD; read <see cref="VoiceActivityDetector.Energy"/> for a level bar.</summary>
        public VoiceActivityDetector Vad { get; } = new VoiceActivityDetector();
        /// <summary>Local VAD edge (true = started speaking). Fired from the Unity main thread.</summary>
        public event Action<bool> OnLocalSpeaking;

        private AudioClip _micClip;
        private int _micReadPos;
        private float[] _micScratch;
        private float[] _frame;
        private float[] _mono;
        private byte[] _opusOut = new byte[1275];
        private IOpusCodec _encoder;
        private RemoteMixer _mixer;
        private uint _channelHash;
        private int _micRate;
        private int _micChannels;

        private void Start()
        {
            if (AutoConnectOnStart) _ = Connect();
        }

        public async Task Connect()
        {
            if (CodecFactory == null) throw new InvalidOperationException("Assign CodecFactory (e.g. () => new ConcentusOpusCodec()) first");
            if (Client != null) await Disconnect();

            _encoder = CodecFactory();
            _encoder.SetBitrate(BitrateBps);
            _mixer = new RemoteMixer(CodecFactory);

            Client = new AurixVoiceClient(WebSocketUrl, Token);
            Client.OnBitrateCommand += (kbps, _) => _encoder?.SetBitrate((int)kbps * 1000);
            Client.OnParticipantLeft += (_, p) => _mixer?.Remove(p.Ssrc);
            Client.OnDisconnected += _ => StopMic();
            await Client.ConnectAsync();

            var channel = Guid.Parse(ChannelId);
            _channelHash = AurixVoiceClient.ChannelHash(channel);
            await Client.JoinChannelAsync(channel);
            StartMic();

            var src = GetComponent<AudioSource>();
            src.clip = null;
            src.loop = true;
            src.spatialBlend = 0f;
            if (!src.isPlaying) src.Play();
        }

        public async Task Disconnect()
        {
            StopMic();
            var c = Client;
            Client = null;
            if (c != null) await c.DisconnectAsync();
            _encoder?.Dispose(); _encoder = null;
            _mixer?.Dispose(); _mixer = null;
        }

        public void SetMuted(bool muted) => Client?.SetMuted(muted);

        private void StartMic()
        {
            _micRate = AudioFormat.SampleRate;
            Microphone.GetDeviceCaps(MicrophoneDevice, out var minFreq, out var maxFreq);
            if (maxFreq != 0 && (_micRate < minFreq || _micRate > maxFreq)) _micRate = maxFreq;
            _micClip = Microphone.Start(MicrophoneDevice, true, 1, _micRate);
            _micChannels = _micClip.channels;
            _micReadPos = 0;
        }

        private void StopMic()
        {
            if (_micClip != null)
            {
                Microphone.End(MicrophoneDevice);
                _micClip = null;
            }
        }

        private void Update()
        {
            Client?.Update();
            PumpMicrophone();
            PumpDownlink();
        }

        private void PumpMicrophone()
        {
            if (_micClip == null || Client == null || !IsConnected) return;
            int writePos = Microphone.GetPosition(MicrophoneDevice);
            int frameAtMicRate = _micRate * AudioFormat.FrameMs / 1000;
            int available = (writePos - _micReadPos + _micClip.samples) % _micClip.samples;
            while (available >= frameAtMicRate)
            {
                int needed = frameAtMicRate * _micChannels;
                if (_micScratch == null || _micScratch.Length != needed) _micScratch = new float[needed];
                _micClip.GetData(_micScratch, _micReadPos);
                _micReadPos = (_micReadPos + frameAtMicRate) % _micClip.samples;
                available -= frameAtMicRate;

                if (_mono == null || _mono.Length != AudioFormat.FrameSamples) _mono = new float[AudioFormat.FrameSamples];
                Downmix(_micScratch, _micChannels, frameAtMicRate, _mono, AudioFormat.FrameSamples);
                Vad.Threshold = VadThreshold;
                Vad.HangoverFrames = VadHangoverFrames;
                if (Vad.Process(_mono, AudioFormat.FrameSamples)) OnLocalSpeaking?.Invoke(Vad.Speaking);
                if (GateOnVad && !Vad.Speaking)
                {
                    Client.SkipFrame(AudioFormat.FrameSamples);
                    continue;
                }
                int n = _encoder.Encode(_mono, AudioFormat.FrameSamples, _opusOut);
                if (n > 0) Client.SendOpusFrame(_channelHash, _opusOut, n, AudioFormat.FrameSamples, Vad.Level);
            }
        }

        /// <summary>Mono downmix plus naive linear resample to 48 kHz (only used when the mic cannot run at 48 kHz).</summary>
        private static void Downmix(float[] src, int srcCh, int srcFrames, float[] dst, int dstFrames)
        {
            for (int i = 0; i < dstFrames; i++)
            {
                float pos = (float)i * srcFrames / dstFrames;
                int i0 = (int)pos;
                int i1 = Math.Min(i0 + 1, srcFrames - 1);
                float t = pos - i0;
                float a = 0, b = 0;
                for (int c = 0; c < srcCh; c++) { a += src[i0 * srcCh + c]; b += src[i1 * srcCh + c]; }
                dst[i] = (a + (b - a) * t) / srcCh;
            }
        }

        private void PumpDownlink()
        {
            if (Client == null || _mixer == null) return;
            while (Client.TryDequeueAudio(out var a)) _mixer.Push(a.SenderSsrc, a.Sequence, a.Volume, a.Opus);
        }

        // Runs on Unity's audio thread; the AudioSource plays silence which we fill with the mix.
        private void OnAudioFilterRead(float[] data, int channels)
        {
            var mixer = _mixer;
            if (mixer == null) return;
            if (AudioSettings.outputSampleRate != AudioFormat.SampleRate) return; // set Project Settings > Audio > 48000 Hz
            mixer.Mix(data, channels);
        }

        private void OnDestroy()
        {
            _ = Disconnect();
        }
    }
}
#endif
