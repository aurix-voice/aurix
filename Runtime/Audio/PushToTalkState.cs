using System;

namespace Aurix.Audio
{
    /// <summary>
    /// Engine-independent push-to-talk state machine shared by the Unity behaviour (and usable from
    /// custom pipelines): hold or toggle, with a release delay so the last syllable is not clipped.
    /// Feed <see cref="Update"/> once per frame with the input state and the clock; it reports
    /// when transmission should start or stop, and the caller flips the real mute flag.
    /// </summary>
    public sealed class PushToTalkState
    {
        public bool Toggle;
        public float ReleaseDelay;
        private float _releaseAt = -1f;
        private bool _latched;

        public bool Transmitting { get; private set; }

        public PushToTalkState(bool toggle = false, float releaseDelay = 0.15f)
        {
            Toggle = toggle;
            ReleaseDelay = releaseDelay;
        }

        /// <summary>Hold mode: <paramref name="pressed"/> is the current key state. Toggle mode: pass the "pressed this frame" edge.</summary>
        /// <returns>true when transmission started, false when it stopped, null when unchanged.</returns>
        public bool? Update(bool pressed, float now)
        {
            if (Toggle) { if (pressed) _latched = !_latched; }
            else _latched = pressed;
            bool want = _latched;
            if (want)
            {
                _releaseAt = -1f;
                if (Transmitting) return null;
                Transmitting = true;
                return true;
            }
            if (!Transmitting) return null;
            if (ReleaseDelay <= 0f)
            {
                Transmitting = false;
                return false;
            }
            if (_releaseAt < 0f) _releaseAt = now + ReleaseDelay;
            if (now < _releaseAt) return null;
            Transmitting = false;
            _releaseAt = -1f;
            return false;
        }

        /// <summary>Forget any pending release and stop transmitting (focus loss, disconnect, disable).</summary>
        /// <returns>true when it was transmitting.</returns>
        public bool Reset()
        {
            _releaseAt = -1f;
            _latched = false;
            bool was = Transmitting;
            Transmitting = false;
            return was;
        }
    }
}
