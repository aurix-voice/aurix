#if UNITY_5_3_OR_NEWER
using System;
using System.Collections.Generic;
using Aurix.Audio;
using Aurix.Protocol;
using UnityEngine;
using UnityEngine.Audio;

namespace Aurix.Unity
{
    /// <summary>
    /// Ducks the game's audio while a priority speaker talks, with the channel's own envelope
    /// (<see cref="DuckingConfig"/>: attack, hold, release, gain) — the same curve the node applies to
    /// the other voices. Listens to <see cref="IAurixVoiceClient.OnDuckingChanged"/> of the voice
    /// component and drives either an exposed <see cref="AudioMixer"/> parameter (in dB, relative to
    /// its value when ducking starts) or the volume of a set of <see cref="AudioSource"/>s. Your own
    /// priority speech never ducks your game audio. Read <see cref="CurrentGain"/> / <see cref="OnGainChanged"/>
    /// for a custom target.
    /// </summary>
    public sealed class AurixGameAudioDucker : MonoBehaviour
    {
        [Tooltip("The voice component (AurixVoiceBehaviour / AurixWebGLVoiceBehaviour) whose ducking state is followed; empty = one on this GameObject or the first in the scene.")]
        public MonoBehaviour Voice;
        [Tooltip("Only react to this channel (GUID); empty = any joined channel with ducking configured.")]
        public string ChannelId;

        [Header("Target")]
        [Tooltip("Optional: an AudioMixer with an exposed volume parameter (dB) for the game's music / SFX groups.")]
        public AudioMixer Mixer;
        [Tooltip("Name of the exposed AudioMixer parameter to attenuate.")]
        public string MixerParameter = "GameVolume";
        [Tooltip("Optional: AudioSources whose volume is scaled while ducked (restored afterwards).")]
        public AudioSource[] Sources = Array.Empty<AudioSource>();

        [Header("Envelope")]
        [Tooltip("Ignore the channel's envelope and use the values below.")]
        public bool OverrideEnvelope;
        [Range(0f, 1f)] public float Gain = 0.25f;
        public int AttackMs = 60;
        public int ReleaseMs = 400;
        public int HoldMs = 250;

        /// <summary>Current game-audio gain (1 = untouched).</summary>
        public float CurrentGain => _envelope.Gain;
        /// <summary>Game audio is attenuated right now — engaged, holding, or still releasing.</summary>
        public bool IsDucked => _envelope.IsDucking;
        /// <summary>A priority speaker is holding some channel ducked (engaged or within the hold).</summary>
        public bool IsActive => _envelope.Active;
        /// <summary>Channels currently ducked by a priority speaker.</summary>
        public IReadOnlyCollection<Guid> DuckedChannels => _ducked;

        /// <summary>Fired on the main thread whenever the applied gain changes.</summary>
        public event Action<float> OnGainChanged;

        private readonly DuckingEnvelope _envelope = new DuckingEnvelope();
        private readonly HashSet<Guid> _ducked = new HashSet<Guid>();
        private IAurixVoiceHost _host;
        private IAurixVoiceClient _client;
        private Guid? _channelFilter;
        private float _applied = 1f;
        private float _mixerBaseDb = float.NaN;
        private float[] _sourceBase;

        private void OnEnable()
        {
            _channelFilter = !string.IsNullOrEmpty(ChannelId) && Guid.TryParse(ChannelId, out var id) ? id : (Guid?)null;
        }

        private void OnDisable()
        {
            Detach();
            _ducked.Clear();
            _envelope.Reset();
            Apply(1f);
        }

        private void Update()
        {
            if (_host == null) _host = AurixVoiceHost.Resolve(Voice, this);
            var client = _host?.VoiceClient;
            if (!ReferenceEquals(client, _client))
            {
                Detach();
                _client = client;
                if (client != null)
                {
                    client.OnDuckingChanged += OnDuckingChanged;
                    client.OnDisconnected += OnDisconnected;
                    client.OnChannelLeft += OnChannelLeft;
                }
                else
                {
                    _ducked.Clear();
                    _envelope.Set(false, _envelope.Config);
                }
            }
            Apply(_envelope.Advance(Time.deltaTime));
        }

        private void Detach()
        {
            if (_client == null) return;
            _client.OnDuckingChanged -= OnDuckingChanged;
            _client.OnDisconnected -= OnDisconnected;
            _client.OnChannelLeft -= OnChannelLeft;
            _client = null;
        }

        private void OnDuckingChanged(Guid channelId, bool active, DuckingConfig config)
        {
            if (_channelFilter.HasValue && _channelFilter.Value != channelId) return;
            if (active) _ducked.Add(channelId);
            else _ducked.Remove(channelId);
            _envelope.Set(_ducked.Count > 0, Envelope(config));
        }

        private void OnChannelLeft(Guid channelId)
        {
            if (_ducked.Remove(channelId)) _envelope.Set(_ducked.Count > 0, _envelope.Config);
        }

        private void OnDisconnected(string reason)
        {
            _ducked.Clear();
            _envelope.Set(false, _envelope.Config);
        }

        private DuckingConfig Envelope(DuckingConfig fromChannel) => OverrideEnvelope
            ? new DuckingConfig { Gain = Math.Max(0f, Math.Min(1f, Gain)), AttackMs = AttackMs, ReleaseMs = ReleaseMs, HoldMs = HoldMs, Moderators = fromChannel.Moderators }
            : fromChannel;

        private void Apply(float gain)
        {
            if (gain == _applied) return;
            bool wasFlat = _applied >= 1f;
            _applied = gain;
            if (Mixer != null && !string.IsNullOrEmpty(MixerParameter))
            {
                if (wasFlat && Mixer.GetFloat(MixerParameter, out var baseDb)) _mixerBaseDb = baseDb;
                if (!float.IsNaN(_mixerBaseDb))
                {
                    float db = gain <= 0.0001f ? -80f : 20f * (float)Math.Log10(gain);
                    Mixer.SetFloat(MixerParameter, gain >= 1f ? _mixerBaseDb : Math.Max(-80f, _mixerBaseDb + db));
                }
            }
            if (Sources != null && Sources.Length > 0)
            {
                if (wasFlat || _sourceBase == null || _sourceBase.Length != Sources.Length)
                {
                    _sourceBase = new float[Sources.Length];
                    for (int i = 0; i < Sources.Length; i++) _sourceBase[i] = Sources[i] != null ? Sources[i].volume : 1f;
                }
                for (int i = 0; i < Sources.Length; i++)
                    if (Sources[i] != null) Sources[i].volume = _sourceBase[i] * gain;
            }
            OnGainChanged?.Invoke(gain);
        }
    }
}
#endif
