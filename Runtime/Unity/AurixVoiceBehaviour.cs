#if UNITY_5_3_OR_NEWER
using System;
using System.Collections;
using System.Threading.Tasks;
using Aurix.Audio;
using Aurix.Protocol;
using UnityEngine;
#if UNITY_ANDROID
using UnityEngine.Android;
#endif

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
        [Tooltip("Channel id(s) to join on connect, comma-separated. The microphone goes to every joined channel " +
                 "unless Client.SetTransmissionAsync narrows it; Client.SetChannelFocusAsync picks the one heard at full volume.")]
        public string ChannelId;

        [Header("Audio")]
        [Tooltip("Microphone name from Microphone.devices (see InputDevices); empty = system default. " +
                 "Change at runtime with SetInputDevice.")]
        public string MicrophoneDevice = null;
        [Tooltip("Software microphone gain before VAD and encoding (1 = unity, 2 ≈ +6 dB).")]
        [Range(0f, 4f)] public float InputGain = 1f;
        [Tooltip("Master volume of all remote voices (1 = unity), on top of per-participant volumes.")]
        [Range(0f, 2f)] public float OutputVolume = 1f;
        [Tooltip("Speaker mute: hear nobody, without telling the server or affecting your microphone.")]
        public bool OutputMuted = false;
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

        [Header("Mobile")]
        [Tooltip("Ask for the microphone permission (Android RECORD_AUDIO / iOS NSMicrophoneUsageDescription) before " +
                 "capturing. Off: connect as a listener when the permission is missing and let your UI request it.")]
        public bool RequestMicrophonePermission = true;
        [Tooltip("Stop the microphone while the app is in the background and restart it on return " +
                 "(iOS suspends audio anyway; on Android this avoids recording behind the user's back).")]
        public bool StopMicrophoneInBackground = true;
        [Tooltip("After being suspended longer than this many seconds, ping the server and reconnect at once if " +
                 "it does not answer within 2 s (instead of waiting for the regular 3 × PingInterval timeout). 0 = off.")]
        public float ProbeAfterBackgroundSeconds = 2f;
        [Tooltip("Reconnect (resume + UDP rebind) when Application.internetReachability changes, e.g. Wi-Fi ↔ cellular: " +
                 "the media session is bound to the old source address, so nothing is heard until the rebind.")]
        public bool ReconnectOnNetworkChange = true;

        /// <summary>Creates encoder/decoder instances. Must be set by your code (platform/licensing choice).</summary>
        public Func<IOpusCodec> CodecFactory;

        public AurixVoiceClient Client { get; private set; }
        public bool IsConnected => Client != null && Client.State == VoiceConnectionState.MediaBound;

        /// <summary>Local microphone meter/VAD; read <see cref="VoiceActivityDetector.Energy"/> for a level bar.</summary>
        public VoiceActivityDetector Vad { get; } = new VoiceActivityDetector();

        /// <summary>
        /// Extra audio mixed into (or replacing) the microphone before VAD and encoding — see
        /// <see cref="InjectClip"/>. Keeps transmitting on a 20 ms clock while no microphone is
        /// capturing, so a sound test works on a machine without one.
        /// </summary>
        public AudioInjector Injector { get; } = new AudioInjector();
        public bool IsInjecting => Injector.Active;
        /// <summary>Local VAD edge (true = started speaking). Fired from the Unity main thread.</summary>
        public event Action<bool> OnLocalSpeaking;
        /// <summary>
        /// The capture device changed: a <see cref="SetInputDevice"/> call, or the active microphone
        /// disappeared and capture fell back to the system default (<c>null</c>).
        /// </summary>
        public event Action<string> OnInputDeviceChanged;
        /// <summary>
        /// The user denied the microphone permission: the client stays connected as a listener. Call
        /// <see cref="RetryMicrophonePermission"/> after explaining why the game needs it (Android may
        /// require the user to grant it from the system settings after a second refusal).
        /// </summary>
        public event Action OnMicrophonePermissionDenied;

        public enum MicrophonePermission { Unknown, Requesting, Granted, Denied }
        /// <summary>Current microphone permission state (always <see cref="MicrophonePermission.Granted"/> on desktop).</summary>
        public MicrophonePermission PermissionState { get; private set; } = MicrophonePermission.Unknown;

        /// <summary>Microphones currently known to Unity (names usable with <see cref="SetInputDevice"/>).</summary>
        public static string[] InputDevices => Microphone.devices;

        /// <summary>Device actually being captured (<c>null</c> = system default); differs from
        /// <see cref="MicrophoneDevice"/> after a fallback.</summary>
        public string ActiveInputDevice => _activeDevice;
        public bool IsCapturing => _micClip != null;

        private AudioClip _micClip;
        private string _activeDevice;
        private float _micRetryAt;
        private int _micReadPos;
        private float[] _micScratch;
        private float[] _mono;
        private byte[] _opusOut = new byte[1275];
        private IOpusCodec _encoder;
        private RemoteMixer _mixer;
        private int _micRate;
        private int _micChannels;
        private float _injectClock;
        private readonly OutputResampler _outputResampler = new OutputResampler();
        private OutputResampler.FillSource _fillFromMixer;
        private volatile int _outputRate = AudioFormat.SampleRate;
        private float _pausedAt = -1f;
        private NetworkReachability _reachability;
        private float _nextReachabilityCheck;

        private void Awake()
        {
            _fillFromMixer = (buf, off, frames, ch) => _mixer?.Mix(buf, off, frames, ch);
            _outputRate = AudioSettings.outputSampleRate;
        }

        private void OnEnable() => AudioSettings.OnAudioConfigurationChanged += OnAudioConfigurationChanged;

        private void OnDisable() => AudioSettings.OnAudioConfigurationChanged -= OnAudioConfigurationChanged;

        /// <summary>
        /// Headphones/Bluetooth route changes on mobile restart the audio engine, possibly at another sample
        /// rate and with every AudioSource stopped: pick up the new rate and resume the playback source.
        /// </summary>
        private void OnAudioConfigurationChanged(bool deviceWasChanged)
        {
            _outputRate = AudioSettings.outputSampleRate;
            _outputResampler.Reset();
            if (Client == null) return;
            var src = GetComponent<AudioSource>();
            if (src != null && !src.isPlaying) src.Play();
        }

        private void Start()
        {
            _reachability = Application.internetReachability;
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
            Client.Mixer = _mixer;
            Client.OnBitrateCommand += (kbps, _) => _encoder?.SetBitrate((int)kbps * 1000);
            Client.OnParticipantLeft += (_, p) => { _mixer?.Remove(p.Ssrc); _mixer?.Remove(p.Ssrc | AurxPacket.SynthSsrcFlag); };
            Client.OnDisconnected += _ => StopMic();
            await Client.ConnectAsync();

            foreach (var id in (ChannelId ?? string.Empty).Split(','))
            {
                var trimmed = id.Trim();
                if (trimmed.Length > 0) await Client.JoinChannelAsync(Guid.Parse(trimmed));
            }
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
            Injector.Stop();
            var c = Client;
            Client = null;
            if (c != null) await c.DisconnectAsync();
            _encoder?.Dispose(); _encoder = null;
            _mixer?.Dispose(); _mixer = null;
        }

        public void SetMuted(bool muted) => Client?.SetMuted(muted);

        /// <summary>
        /// Switch the microphone (<c>null</c>/empty = system default). Restarts capture when it is
        /// running; returns <c>false</c> (and keeps the current device) when the name is unknown.
        /// </summary>
        public bool SetInputDevice(string device)
        {
            if (string.IsNullOrEmpty(device)) device = null;
            if (device != null && Array.IndexOf(Microphone.devices, device) < 0) return false;
            MicrophoneDevice = device;
            if (_micClip == null) return true;
            StopMic();
            StartMic();
            return true;
        }

        /// <summary>Software microphone gain, clamped to <c>0..4</c>.</summary>
        public void SetInputGain(float gain) => InputGain = AudioLevel.ClampGain(gain, AudioLevel.MaxInputGain);

        /// <summary>Master volume of remote voices, clamped to <c>0..2</c>.</summary>
        public void SetOutputVolume(float volume) => OutputVolume = AudioLevel.ClampGain(volume, AudioLevel.MaxOutputVolume);

        /// <summary>Speaker mute (local only). Your own microphone keeps sending.</summary>
        public void SetOutputMuted(bool muted) => OutputMuted = muted;

        /// <summary>Ask for the microphone permission again after <see cref="OnMicrophonePermissionDenied"/>.</summary>
        public void RetryMicrophonePermission()
        {
            if (PermissionState == MicrophonePermission.Requesting) return;
            PermissionState = MicrophonePermission.Unknown;
            _micRetryAt = 0f;
        }

        /// <summary>
        /// Play <paramref name="clip"/> into every channel the microphone goes to (an <c>echo</c>
        /// channel for a sound test, a bot voice, an in-game radio). Mixed over the microphone
        /// unless <paramref name="mixWithMicrophone"/> is <c>false</c>; <see cref="SetMuted"/>
        /// silences both. Replaces a previous injection; <see cref="AudioInjector.Ended"/> fires
        /// when a non-looping clip finishes.
        /// </summary>
        public void InjectClip(AudioClip clip, bool loop = false, float gain = 1f, bool mixWithMicrophone = true)
        {
            if (clip == null) throw new ArgumentNullException(nameof(clip));
            var pcm = new float[clip.samples * clip.channels];
            clip.GetData(pcm, 0);
            Injector.Gain = gain;
            Injector.MixWithMicrophone = mixWithMicrophone;
            Injector.Play(pcm, clip.channels, clip.frequency, loop);
            _injectClock = 0f;
        }

        /// <summary>Stop <see cref="InjectClip"/> (or a stream opened on <see cref="Injector"/>).</summary>
        public void StopInjection() => Injector.Stop();

        private void StartMic()
        {
            if (!EnsureMicrophonePermission()) return;
            string device = string.IsNullOrEmpty(MicrophoneDevice) ? null : MicrophoneDevice;
            if (device != null && Array.IndexOf(Microphone.devices, device) < 0)
            {
                Debug.LogWarning($"Aurix: microphone '{device}' not found, using the system default");
                device = null;
            }
            _micRate = AudioFormat.SampleRate;
            Microphone.GetDeviceCaps(device, out var minFreq, out var maxFreq);
            if (maxFreq != 0 && (_micRate < minFreq || _micRate > maxFreq)) _micRate = maxFreq;
            _micClip = Microphone.Start(device, true, 1, _micRate);
            if (_micClip == null)
            {
                Debug.LogWarning("Aurix: Microphone.Start failed; will retry");
                _micRetryAt = Time.unscaledTime + 1f;
                return;
            }
            bool changed = _activeDevice != device;
            _activeDevice = device;
            _micChannels = _micClip.channels;
            _micReadPos = 0;
            Vad.Reset();
            if (changed) OnInputDeviceChanged?.Invoke(device);
        }

        /// <summary>
        /// True when capture may start now. Otherwise the permission request is in flight (or was denied)
        /// and <see cref="WatchMicrophone"/> will call <see cref="StartMic"/> again when it resolves.
        /// </summary>
        private bool EnsureMicrophonePermission()
        {
            switch (PermissionState)
            {
                case MicrophonePermission.Granted: return true;
                case MicrophonePermission.Requesting: return false;
                case MicrophonePermission.Denied: _micRetryAt = float.PositiveInfinity; return false;
            }
            if (HasMicrophonePermission())
            {
                PermissionState = MicrophonePermission.Granted;
                return true;
            }
            if (!RequestMicrophonePermission)
            {
                PermissionDenied();
                return false;
            }
            PermissionState = MicrophonePermission.Requesting;
#if UNITY_ANDROID && !UNITY_EDITOR
            var callbacks = new PermissionCallbacks();
            callbacks.PermissionGranted += _ => PermissionGranted();
            callbacks.PermissionDenied += _ => PermissionDenied();
            callbacks.PermissionDeniedAndDontAskAgain += _ => PermissionDenied();
            Permission.RequestUserPermission(Permission.Microphone, callbacks);
#else
            StartCoroutine(RequestPermissionCoroutine());
#endif
            return false;
        }

        private static bool HasMicrophonePermission()
        {
#if UNITY_ANDROID && !UNITY_EDITOR
            return Permission.HasUserAuthorizedPermission(Permission.Microphone);
#else
            return Application.HasUserAuthorization(UserAuthorization.Microphone);
#endif
        }

        private IEnumerator RequestPermissionCoroutine()
        {
            yield return Application.RequestUserAuthorization(UserAuthorization.Microphone);
            if (Application.HasUserAuthorization(UserAuthorization.Microphone)) PermissionGranted();
            else PermissionDenied();
        }

        private void PermissionGranted()
        {
            PermissionState = MicrophonePermission.Granted;
            _micRetryAt = 0f;
        }

        private void PermissionDenied()
        {
            PermissionState = MicrophonePermission.Denied;
            _micRetryAt = float.PositiveInfinity;
            Debug.LogWarning("Aurix: microphone permission denied; staying connected as a listener");
            OnMicrophonePermissionDenied?.Invoke();
        }

        private void StopMic()
        {
            if (_micClip != null)
            {
                Microphone.End(_activeDevice);
                _micClip = null;
            }
        }

        private void Update()
        {
            Client?.Update();
            _outputRate = AudioSettings.outputSampleRate;
            var mixer = _mixer;
            if (mixer != null)
            {
                mixer.OutputVolume = OutputVolume;
                mixer.OutputMuted = OutputMuted;
            }
            WatchMicrophone();
            PumpMicrophone();
            PumpInjectionWithoutMic();
            PumpDownlink();
            WatchNetwork();
        }

        /// <summary>
        /// Background/foreground: drop the microphone while suspended and, on return, probe the control
        /// connection so a socket the OS silently killed is replaced now rather than after the ping timeout.
        /// </summary>
        private void OnApplicationPause(bool paused)
        {
            if (paused)
            {
                _pausedAt = Time.realtimeSinceStartup;
                if (StopMicrophoneInBackground) StopMic();
                return;
            }
            if (_pausedAt < 0f) return;
            float away = Time.realtimeSinceStartup - _pausedAt;
            _pausedAt = -1f;
            _micRetryAt = 0f;
            if (ProbeAfterBackgroundSeconds > 0f && away >= ProbeAfterBackgroundSeconds) Client?.ProbeConnection();
        }

        private void WatchNetwork()
        {
            if (!ReconnectOnNetworkChange || Time.unscaledTime < _nextReachabilityCheck) return;
            _nextReachabilityCheck = Time.unscaledTime + 1f;
            var now = Application.internetReachability;
            if (now == _reachability) return;
            var before = _reachability;
            _reachability = now;
            if (Client == null || now == NetworkReachability.NotReachable) return;
            Client.ForceReconnect($"network changed ({before} -> {now})");
        }

        /// <summary>Recover from an unplugged/failed microphone: fall back to the default device.</summary>
        private void WatchMicrophone()
        {
            if (Client == null || !IsConnected) return;
            if (_pausedAt >= 0f && StopMicrophoneInBackground) return; // suspended: Update may still run with "Run In Background"
            if (_micClip != null)
            {
                if (Microphone.IsRecording(_activeDevice)) return;
                Debug.LogWarning($"Aurix: microphone '{_activeDevice ?? "default"}' stopped; switching to the system default");
                StopMic();
                if (_activeDevice != null) MicrophoneDevice = null;
                _micRetryAt = Time.unscaledTime + 1f;
                return;
            }
            if (Time.unscaledTime < _micRetryAt) return;
            StartMic();
        }

        private void PumpMicrophone()
        {
            if (_micClip == null || Client == null || !IsConnected) return;
            int writePos = Microphone.GetPosition(_activeDevice);
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
                AudioLevel.ApplyGain(_mono, AudioFormat.FrameSamples, InputGain);
                Injector.Fill(_mono, AudioFormat.FrameSamples);
                EncodeAndSend();
            }
        }

        /// <summary>Without a microphone the injector alone paces the uplink at one frame per 20 ms.</summary>
        private void PumpInjectionWithoutMic()
        {
            if (_micClip != null || Client == null || !IsConnected || !Injector.Active)
            {
                _injectClock = 0f;
                return;
            }
            _injectClock += Time.unscaledDeltaTime;
            const float frameSeconds = AudioFormat.FrameMs / 1000f;
            int frames = 0;
            while (_injectClock >= frameSeconds && frames++ < 5 && Injector.Active)
            {
                _injectClock -= frameSeconds;
                if (_mono == null || _mono.Length != AudioFormat.FrameSamples) _mono = new float[AudioFormat.FrameSamples];
                Array.Clear(_mono, 0, _mono.Length);
                Injector.Fill(_mono, AudioFormat.FrameSamples);
                EncodeAndSend();
            }
            if (_injectClock > frameSeconds) _injectClock = 0f; // fell behind (hitch): don't burst
        }

        /// <summary>VAD, gating and Opus for the frame in <see cref="_mono"/>.</summary>
        private void EncodeAndSend()
        {
            Vad.Threshold = VadThreshold;
            Vad.HangoverFrames = VadHangoverFrames;
            if (Vad.Process(_mono, AudioFormat.FrameSamples)) OnLocalSpeaking?.Invoke(Vad.Speaking);
            if (GateOnVad && !Vad.Speaking)
            {
                Client.SkipFrame(AudioFormat.FrameSamples);
                return;
            }
            int n = _encoder.Encode(_mono, AudioFormat.FrameSamples, _opusOut);
            if (n > 0) Client.TransmitOpusFrame(_opusOut, n, AudioFormat.FrameSamples, Vad.Level);
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
            while (Client.TryDequeueAudio(out var a)) _mixer.Push(a.SenderSsrc, a.Sequence, a.Volume, a.Direction, a.Opus);
        }

        // Runs on Unity's audio thread; the AudioSource plays silence which we fill with the mix,
        // resampled from 48 kHz when the device runs at another rate (common on mobile).
        private void OnAudioFilterRead(float[] data, int channels)
        {
            var mixer = _mixer;
            if (mixer == null || _fillFromMixer == null) return;
            _outputResampler.Process(data, channels, _outputRate, _fillFromMixer);
        }

        private void OnDestroy()
        {
            _ = Disconnect();
        }
    }
}
#endif
