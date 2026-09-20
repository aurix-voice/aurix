using System;
using Aurix.Protocol;

namespace Aurix.Audio
{
    /// <summary>
    /// The priority-speaker ducking envelope (<see cref="DuckingConfig"/>) as a client-side gain for
    /// game audio: depth ramps to 1 over <c>AttackMs</c> while a priority speaker holds the channel,
    /// stays there for <c>HoldMs</c> after they stop and ramps back over <c>ReleaseMs</c>. Feed
    /// <see cref="Set"/> from <c>OnDuckingChanged</c> and <see cref="Advance"/> once per rendered
    /// frame; the node applies the same curve to the voices it mixes or forwards.
    /// </summary>
    public sealed class DuckingEnvelope
    {
        private float _depth;
        private float _holdLeft;
        private bool _active;

        /// <summary>The envelope in force (from the channel's config); <see cref="DuckingConfig.Default"/> until the first <see cref="Set"/>.</summary>
        public DuckingConfig Config { get; private set; } = DuckingConfig.Default;

        /// <summary>A priority speaker is holding the channel ducked (before the hold-off and release).</summary>
        public bool Active => _active || _holdLeft > 0f;

        /// <summary>Gain for game audio right now: 1 = untouched, <see cref="DuckingConfig.Gain"/> = fully ducked.</summary>
        public float Gain => 1f - _depth * (1f - Config.Gain);

        /// <summary>Depth of the duck, 0 (off) .. 1 (fully at <see cref="DuckingConfig.Gain"/>).</summary>
        public float Depth => _depth;

        /// <summary>Anything still attenuated (attack, hold or release in progress).</summary>
        public bool IsDucking => _depth > 0.0005f;

        /// <summary>Priority speech started (<paramref name="active"/>) or ended; <paramref name="config"/> is the channel's envelope.</summary>
        public void Set(bool active, DuckingConfig config)
        {
            Config = config;
            if (active) _holdLeft = 0f;
            else if (_active) _holdLeft = Math.Max(0, config.HoldMs) / 1000f;
            _active = active;
        }

        /// <summary>Move the envelope by <paramref name="dtSeconds"/> and return the new <see cref="Gain"/>.</summary>
        public float Advance(float dtSeconds)
        {
            if (dtSeconds < 0f || float.IsNaN(dtSeconds)) dtSeconds = 0f;
            if (_active)
            {
                Ramp(1f, dtSeconds);
            }
            else if (_holdLeft > 0f)
            {
                float held = Math.Min(_holdLeft, dtSeconds);
                _holdLeft -= held;
                Ramp(1f, held);
                if (_holdLeft <= 0f) Ramp(0f, dtSeconds - held);
            }
            else
            {
                Ramp(0f, dtSeconds);
            }
            return Gain;
        }

        /// <summary>Drop to "not ducked" at once (disconnect, channel left).</summary>
        public void Reset()
        {
            _depth = 0f;
            _holdLeft = 0f;
            _active = false;
        }

        private void Ramp(float target, float dtSeconds)
        {
            if (dtSeconds <= 0f) return;
            float rampMs = target > _depth ? Config.AttackMs : Config.ReleaseMs;
            if (rampMs <= 0f)
            {
                _depth = target;
                return;
            }
            float step = dtSeconds * 1000f / rampMs;
            _depth = target > _depth ? Math.Min(target, _depth + step) : Math.Max(target, _depth - step);
        }
    }
}
