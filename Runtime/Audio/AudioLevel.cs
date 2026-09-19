using System;

namespace Aurix.Audio
{
    /// <summary>
    /// Audio level as carried on the wire (RFC 6464 style): <c>0</c> is a full-scale signal,
    /// <see cref="Silence"/> (127) is no signal, in between the value is <c>-dBov</c>.
    /// Mirrors <c>encode_audio_level</c> / <c>decode_audio_level</c> on the server.
    /// </summary>
    public static class AudioLevel
    {
        public const byte Silence = 127;

        /// <summary>Linear energy (RMS, 0..1) → wire level.</summary>
        public static byte Encode(float energy)
        {
            if (float.IsNaN(energy) || energy <= 0f) return Silence;
            double dbov = -(20.0 * Math.Log10(energy));
            if (dbov < 0) dbov = 0;
            if (dbov > Silence) dbov = Silence;
            return (byte)Math.Round(dbov);
        }

        /// <summary>Wire level → linear energy (0..1); <see cref="Silence"/> maps to 0.</summary>
        public static float Decode(byte level)
        {
            if (level >= Silence) return 0f;
            return (float)Math.Pow(10.0, -level / 20.0);
        }

        /// <summary>RMS of an interleaved/mono PCM float frame, 0..1.</summary>
        public static float Rms(float[] pcm, int count)
        {
            if (pcm == null || count <= 0) return 0f;
            count = Math.Min(count, pcm.Length);
            double sum = 0;
            for (int i = 0; i < count; i++) sum += (double)pcm[i] * pcm[i];
            return (float)Math.Sqrt(sum / count);
        }
    }

    /// <summary>
    /// Frame-by-frame level meter with a simple energy VAD: speech starts as soon as one frame
    /// crosses <see cref="Threshold"/>, and ends once <see cref="HangoverFrames"/> consecutive
    /// frames stay below it (so short pauses inside a sentence do not flap the state).
    /// </summary>
    public sealed class VoiceActivityDetector
    {
        /// <summary>Linear RMS threshold (0.01 ≈ -40 dBov by default).</summary>
        public float Threshold = 0.01f;
        /// <summary>Number of quiet 20 ms frames before speech is considered over (default 300 ms).</summary>
        public int HangoverFrames = 15;
        /// <summary>Smoothing factor for <see cref="Energy"/> (0 = raw, 1 = frozen).</summary>
        public float Smoothing = 0.5f;

        private int _quiet;

        /// <summary>Smoothed linear energy of the most recent frames.</summary>
        public float Energy { get; private set; }
        /// <summary>Wire level for the most recent frame (unsmoothed).</summary>
        public byte Level { get; private set; } = AudioLevel.Silence;
        public bool Speaking { get; private set; }

        /// <summary>Feeds one PCM frame; returns true when <see cref="Speaking"/> changed.</summary>
        public bool Process(float[] pcm, int count)
        {
            float rms = AudioLevel.Rms(pcm, count);
            Level = AudioLevel.Encode(rms);
            float s = Math.Max(0f, Math.Min(1f, Smoothing));
            Energy = Energy * s + rms * (1f - s);

            bool was = Speaking;
            if (rms >= Threshold)
            {
                _quiet = 0;
                Speaking = true;
            }
            else if (Speaking && ++_quiet >= Math.Max(1, HangoverFrames))
            {
                Speaking = false;
            }
            return was != Speaking;
        }

        public void Reset()
        {
            _quiet = 0;
            Energy = 0f;
            Level = AudioLevel.Silence;
            Speaking = false;
        }
    }
}
