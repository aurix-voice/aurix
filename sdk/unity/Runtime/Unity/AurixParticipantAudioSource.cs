#if UNITY_5_3_OR_NEWER && !(UNITY_WEBGL && !UNITY_EDITOR)
using System;
using Aurix.Audio;
using UnityEngine;

namespace Aurix.Unity
{
    /// <summary>
    /// Plays one participant's voice (microphone plus its TTS voice) through the AudioSource on this
    /// GameObject, so Unity's 3D audio does the spatialization: distance attenuation and rolloff,
    /// the project's spatializer plugin (HRTF — Steam Audio, Resonance, Oculus…), occlusion via
    /// low-pass filters, reverb zones and mixer groups. Put it on the avatar of the remote player,
    /// call <see cref="Bind"/> with their user id (e.g. from <c>Client.OnParticipantJoined</c>) and set
    /// <see cref="AurixVoiceBehaviour.Playback"/> to <see cref="VoicePlaybackMode.PerParticipant"/>.
    /// The audio is delivered centred and unpanned (the server's directions are ignored); the
    /// AudioSource's own spatialBlend / rolloff / spatialize settings decide how it is heard.
    /// The voice component's OutputVolume / OutputMuted and per-participant volumes still apply.
    /// For the echo canceller, add an <see cref="AurixListenerTap"/> to the AudioListener.
    /// </summary>
    [RequireComponent(typeof(AudioSource))]
    public sealed class AurixParticipantAudioSource : MonoBehaviour
    {
        [Tooltip("The voice component whose downlink this source plays; empty = the first one in the scene.")]
        public AurixVoiceBehaviour Voice;
        [Tooltip("Participant to play (user id GUID). Usually set from code with Bind().")]
        public string UserId;

        /// <summary>Participant bound with <see cref="Bind"/> (or parsed from <see cref="UserId"/>); <see cref="Guid.Empty"/> = none.</summary>
        public Guid BoundUserId { get; private set; }
        public bool IsBound => BoundUserId != Guid.Empty;

        /// <summary>True while decoded audio is flowing for this participant (updated on the main thread).</summary>
        public bool IsActive { get; private set; }
        /// <summary>Fired on the main thread when <see cref="IsActive"/> flips: drive a speaking indicator or lip sync.</summary>
        public event Action<bool> OnActiveChanged;

        /// <summary>SSRC currently resolved for the bound user, 0 while they are not in a joined channel.</summary>
        public uint ClaimedSsrc => _ssrc;

        private readonly OutputResampler _resampler = new OutputResampler();
        private OutputResampler.FillSource _fill;
        private volatile int _outputRate = AudioFormat.SampleRate;
        private volatile uint _ssrc;
        private volatile int _lastPulled;

        /// <summary>Play <paramref name="userId"/>'s voice through this source from now on.</summary>
        public void Bind(Guid userId)
        {
            BoundUserId = userId;
            UserId = userId == Guid.Empty ? null : userId.ToString();
            Resolve();
        }

        /// <summary>Stop playing anyone; the participant returns to the voice component's mix (in PerParticipant mode).</summary>
        public void Unbind() => Bind(Guid.Empty);

        private void Awake()
        {
            _fill = (buf, off, frames, ch) =>
            {
                var mixer = Voice != null ? Voice.Mixer : null;
                uint ssrc = _ssrc;
                if (mixer == null || ssrc == 0)
                {
                    Array.Clear(buf, off, frames * ch);
                    _lastPulled = 0;
                    return;
                }
                _lastPulled = mixer.PullParticipant(ssrc, buf, off, frames, ch);
            };
            _outputRate = AudioSettings.outputSampleRate;
        }

        private void OnEnable()
        {
            if (Voice == null) Voice = FindObjectOfType<AurixVoiceBehaviour>();
            if (BoundUserId == Guid.Empty && !string.IsNullOrEmpty(UserId) && Guid.TryParse(UserId, out var parsed)) BoundUserId = parsed;
            AudioSettings.OnAudioConfigurationChanged += OnAudioConfigurationChanged;
            var src = GetComponent<AudioSource>();
            src.clip = null;
            src.loop = true;
            if (!src.isPlaying) src.Play();
            Resolve();
            if (Voice != null) Voice.RegisterParticipantSource(this);
        }

        private void OnDisable()
        {
            AudioSettings.OnAudioConfigurationChanged -= OnAudioConfigurationChanged;
            if (Voice != null) Voice.UnregisterParticipantSource(this);
            _ssrc = 0;
            _lastPulled = 0;
            if (IsActive) { IsActive = false; OnActiveChanged?.Invoke(false); }
        }

        private void OnAudioConfigurationChanged(bool deviceWasChanged)
        {
            _outputRate = AudioSettings.outputSampleRate;
            _resampler.Reset();
            var src = GetComponent<AudioSource>();
            if (src != null && !src.isPlaying) src.Play();
        }

        private void Update()
        {
            Resolve();
            bool active = _lastPulled > 0;
            if (active != IsActive)
            {
                IsActive = active;
                OnActiveChanged?.Invoke(active);
            }
        }

        /// <summary>Look the bound user up in the roster; their SSRC changes when they rejoin.</summary>
        private void Resolve()
        {
            uint ssrc = 0;
            var client = Voice != null ? Voice.Client : null;
            if (client != null && BoundUserId != Guid.Empty)
            {
                var p = client.FindByUser(BoundUserId);
                if (p != null) ssrc = p.Ssrc;
            }
            if (ssrc == _ssrc) return;
            _ssrc = ssrc;
            _resampler.Reset();
            if (Voice != null) Voice.RefreshClaims();
        }

        // Unity's audio thread: the AudioSource plays silence which we overwrite with this participant,
        // resampled from 48 kHz when the device runs at another rate. The source's 3D settings and
        // spatializer are applied by Unity after this filter.
        private void OnAudioFilterRead(float[] data, int channels)
        {
            if (_fill == null) return;
            _resampler.Process(data, channels, _outputRate, _fill);
        }
    }

    /// <summary>
    /// Put on the AudioListener: forwards the final output (voices spatialized by Unity, game audio,
    /// music) to the voice component's echo canceller, so per-participant playback and game sounds
    /// are cancelled from the microphone. Without it only the voice component's own mix is cancelled.
    /// </summary>
    [RequireComponent(typeof(AudioListener))]
    public sealed class AurixListenerTap : MonoBehaviour
    {
        [Tooltip("The voice component whose echo canceller receives the output; empty = the first one in the scene.")]
        public AurixVoiceBehaviour Voice;

        private readonly RenderRateConverter _toVoiceRate = new RenderRateConverter();
        private float[] _scratch = Array.Empty<float>();
        private volatile int _outputRate = AudioFormat.SampleRate;

        private void OnEnable()
        {
            if (Voice == null) Voice = FindObjectOfType<AurixVoiceBehaviour>();
            _outputRate = AudioSettings.outputSampleRate;
            AudioSettings.OnAudioConfigurationChanged += OnAudioConfigurationChanged;
            if (Voice != null) Voice.RenderFedExternally = true;
        }

        private void OnDisable()
        {
            AudioSettings.OnAudioConfigurationChanged -= OnAudioConfigurationChanged;
            if (Voice != null) Voice.RenderFedExternally = false;
        }

        private void OnAudioConfigurationChanged(bool deviceWasChanged)
        {
            _outputRate = AudioSettings.outputSampleRate;
            _toVoiceRate.Reset();
        }

        private void OnAudioFilterRead(float[] data, int channels)
        {
            var voice = Voice;
            var dsp = voice != null ? voice.Dsp : null;
            if (dsp == null) return;
            int rate = _outputRate;
            if (rate == AudioFormat.SampleRate)
            {
                dsp.PushRender(data, 0, data.Length, channels);
                return;
            }
            int need = RenderRateConverter.MaxOutputSamples(data.Length / channels, channels, rate);
            if (_scratch.Length < need) _scratch = new float[need];
            int produced = _toVoiceRate.Convert(data, channels, rate, _scratch);
            dsp.PushRender(_scratch, 0, produced, channels);
        }
    }
}
#endif
