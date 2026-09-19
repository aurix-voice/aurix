using System;

namespace Aurix.Audio
{
    /// <summary>
    /// Plays extra audio into the outgoing voice frames: a sound test in an <c>echo</c> channel,
    /// a bot voice, an in-game radio. Sources are either a whole PCM clip (<see cref="Play"/>,
    /// optionally looped) or a live stream fed with <see cref="Push"/> (e.g. TTS output).
    /// Everything is converted to 48 kHz mono once so <see cref="Fill"/> is cheap on the game
    /// loop. Not thread-safe: drive it from the thread that produces microphone frames.
    /// </summary>
    public sealed class AudioInjector
    {
        /// <summary>Most streamed audio kept ahead of playback (10 s); older samples are dropped.</summary>
        public const int MaxQueuedSamples = AudioFormat.SampleRate * 10;

        private float[] _clip;
        private int _clipPos;
        private bool _loop;

        private float[] _ring = new float[AudioFormat.SampleRate];
        private int _ringRead, _ringCount;
        private bool _streaming;

        private float _gain = 1f;

        /// <summary><c>true</c> while a clip plays or a stream is open.</summary>
        public bool Active => _clip != null || _streaming;

        /// <summary>Linear gain of the injected signal (<c>0..AudioLevel.MaxInputGain</c>).</summary>
        public float Gain
        {
            get => _gain;
            set => _gain = AudioLevel.ClampGain(value, AudioLevel.MaxInputGain);
        }

        /// <summary>
        /// Keep the microphone audible underneath the injected audio (default). <c>false</c>
        /// replaces the microphone for as long as the injection is active.
        /// </summary>
        public bool MixWithMicrophone { get; set; } = true;

        /// <summary>A non-looping clip reached its end, or <see cref="Stop"/> was called while active.</summary>
        public event Action Ended;

        /// <summary>
        /// Start playing <paramref name="pcm"/> (interleaved float, <paramref name="channels"/>
        /// channels at <paramref name="sampleRate"/> Hz) from the beginning, replacing any
        /// current clip or stream.
        /// </summary>
        public void Play(float[] pcm, int channels, int sampleRate, bool loop = false)
        {
            if (pcm == null) throw new ArgumentNullException(nameof(pcm));
            if (channels <= 0) throw new ArgumentOutOfRangeException(nameof(channels));
            if (sampleRate <= 0) throw new ArgumentOutOfRangeException(nameof(sampleRate));
            bool wasActive = Active;
            ResetSources();
            _clip = ToMono48k(pcm, channels, sampleRate);
            _clipPos = 0;
            _loop = loop;
            if (_clip.Length == 0)
            {
                _clip = null;
                if (wasActive) Ended?.Invoke();
            }
        }

        /// <summary>
        /// Open a live stream (replacing any current clip or stream); feed it with
        /// <see cref="Push"/>. It stays active — emitting silence when starved — until
        /// <see cref="Stop"/>.
        /// </summary>
        public void OpenStream()
        {
            ResetSources();
            _streaming = true;
        }

        /// <summary>
        /// Queue interleaved PCM for the open stream. Ignored when no stream is open; the oldest
        /// samples are dropped beyond <see cref="MaxQueuedSamples"/>.
        /// </summary>
        public void Push(float[] pcm, int channels, int sampleRate)
        {
            if (!_streaming || pcm == null) return;
            if (channels <= 0) throw new ArgumentOutOfRangeException(nameof(channels));
            if (sampleRate <= 0) throw new ArgumentOutOfRangeException(nameof(sampleRate));
            var mono = ToMono48k(pcm, channels, sampleRate);
            foreach (var s in mono) RingWrite(s);
        }

        /// <summary>Samples queued for the stream and not yet played.</summary>
        public int QueuedSamples => _ringCount;

        /// <summary>Stop playing; the next <see cref="Fill"/> leaves the microphone untouched.</summary>
        public bool Stop()
        {
            if (!Active) return false;
            ResetSources();
            Ended?.Invoke();
            return true;
        }

        /// <summary>
        /// Apply the injection to one outgoing 48 kHz mono frame (<paramref name="frame"/>, first
        /// <paramref name="count"/> samples hold the microphone): adds the injected signal, or
        /// replaces the microphone when <see cref="MixWithMicrophone"/> is <c>false</c>, hard
        /// clipping to ±1. Returns <c>false</c> (frame untouched) when nothing is active.
        /// </summary>
        public bool Fill(float[] frame, int count)
        {
            if (frame == null) return false;
            if (!Active) return false;
            count = Math.Min(count, frame.Length);
            if (!MixWithMicrophone) Array.Clear(frame, 0, count);
            for (int i = 0; i < count; i++)
            {
                float v = frame[i] + Next() * _gain;
                frame[i] = v > 1f ? 1f : (v < -1f ? -1f : v);
            }
            if (_clip != null && !_loop && _clipPos >= _clip.Length)
            {
                _clip = null;
                Ended?.Invoke();
            }
            return true;
        }

        private float Next()
        {
            if (_clip != null)
            {
                if (_clipPos >= _clip.Length)
                {
                    if (!_loop) return 0f;
                    _clipPos = 0;
                }
                return _clip[_clipPos++];
            }
            if (_ringCount == 0) return 0f;
            float s = _ring[_ringRead];
            _ringRead = (_ringRead + 1) % _ring.Length;
            _ringCount--;
            return s;
        }

        private void RingWrite(float s)
        {
            if (_ringCount == _ring.Length)
            {
                if (_ring.Length < MaxQueuedSamples)
                {
                    var grown = new float[Math.Min(MaxQueuedSamples, _ring.Length * 2)];
                    for (int i = 0; i < _ringCount; i++) grown[i] = _ring[(_ringRead + i) % _ring.Length];
                    _ring = grown;
                    _ringRead = 0;
                }
                else
                {
                    _ringRead = (_ringRead + 1) % _ring.Length; // drop the oldest sample
                    _ringCount--;
                }
            }
            _ring[(_ringRead + _ringCount) % _ring.Length] = s;
            _ringCount++;
        }

        private void ResetSources()
        {
            _clip = null;
            _clipPos = 0;
            _loop = false;
            _streaming = false;
            _ringRead = 0;
            _ringCount = 0;
        }

        /// <summary>Mono downmix and linear resample to <see cref="AudioFormat.SampleRate"/>.</summary>
        public static float[] ToMono48k(float[] pcm, int channels, int sampleRate)
        {
            int srcFrames = pcm.Length / channels;
            if (srcFrames == 0) return Array.Empty<float>();
            if (sampleRate == AudioFormat.SampleRate && channels == 1)
            {
                var copy = new float[srcFrames];
                Array.Copy(pcm, copy, srcFrames);
                return copy;
            }
            int dstFrames = (int)((long)srcFrames * AudioFormat.SampleRate / sampleRate);
            var dst = new float[dstFrames];
            for (int i = 0; i < dstFrames; i++)
            {
                double pos = (double)i * srcFrames / dstFrames;
                int i0 = (int)pos;
                int i1 = Math.Min(i0 + 1, srcFrames - 1);
                float t = (float)(pos - i0);
                float a = 0, b = 0;
                for (int c = 0; c < channels; c++) { a += pcm[i0 * channels + c]; b += pcm[i1 * channels + c]; }
                dst[i] = (a + (b - a) * t) / channels;
            }
            return dst;
        }
    }
}
