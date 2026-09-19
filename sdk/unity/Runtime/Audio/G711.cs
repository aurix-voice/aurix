using System;

namespace Aurix.Audio
{
    /// <summary>Codec of one native AURX audio frame.</summary>
    public enum AudioCodec
    {
        /// <summary>Opus, 48 kHz — what every channel speaks internally.</summary>
        Opus = 0,
        /// <summary>
        /// G.711 μ-law, 8 kHz mono, 64 kbit/s — the fallback negotiated per session with
        /// <c>AurixVoiceClient.SetAudioCodecAsync</c>. The server transcodes to/from Opus at the
        /// edge, so peers never notice.
        /// </summary>
        Pcmu = 1,
    }

    /// <summary>ITU-T G.711 μ-law: 8 kHz, one byte per sample (bit-exact with the server).</summary>
    public static class G711
    {
        public const int SampleRate = 8000;
        /// <summary>Bytes in a 20 ms PCMU frame.</summary>
        public const int FrameSamples = 160;
        /// <summary>Frame sizes (10/20/40/60 ms) the server accepts.</summary>
        public static readonly int[] FrameSizes = { 80, 160, 320, 480 };

        private const int Bias = 0x84;
        private const int Clip = 32635;

        public static bool IsValidFrame(int bytes) => Array.IndexOf(FrameSizes, bytes) >= 0;

        public static byte UlawEncode(short sample)
        {
            int pcm = sample;
            int sign = 0;
            if (pcm < 0) { pcm = -pcm; sign = 0x80; }
            if (pcm > Clip) pcm = Clip;
            pcm += Bias;
            int exponent = 0;
            for (int mask = 0x4000; exponent < 7 && (pcm & mask) == 0; mask >>= 1) exponent++;
            exponent = 7 - exponent;
            int mantissa = (pcm >> (exponent + 3)) & 0x0F;
            return (byte)~(sign | (exponent << 4) | mantissa);
        }

        public static short UlawDecode(byte b)
        {
            int u = ~b & 0xFF;
            int sign = u & 0x80;
            int exponent = (u >> 4) & 0x07;
            int mantissa = u & 0x0F;
            int magnitude = (((mantissa << 3) + Bias) << exponent) - Bias;
            return (short)(sign != 0 ? -magnitude : magnitude);
        }

        /// <summary>Float PCM (-1..1) at 8 kHz → μ-law bytes.</summary>
        public static void Encode(ReadOnlySpan<float> pcm, Span<byte> ulaw)
        {
            for (int i = 0; i < pcm.Length; i++)
            {
                float s = pcm[i];
                if (s > 1f) s = 1f; else if (s < -1f) s = -1f;
                ulaw[i] = UlawEncode((short)Math.Round(s * 32767f));
            }
        }

        /// <summary>μ-law bytes → float PCM (-1..1) at 8 kHz.</summary>
        public static void Decode(ReadOnlySpan<byte> ulaw, Span<float> pcm)
        {
            for (int i = 0; i < ulaw.Length; i++) pcm[i] = UlawDecode(ulaw[i]) / 32768f;
        }
    }

    /// <summary>
    /// Windowed-sinc low-pass run at 48 kHz on both sides of the PCMU path (anti-aliasing before
    /// decimation, anti-imaging after zero-stuffing). Telephone-band cut-off.
    /// </summary>
    internal sealed class NarrowbandFir
    {
        private const int Taps = 63;
        private const float CutoffHz = 3600f;
        private readonly float[] _taps = new float[Taps];
        private readonly float[] _history = new float[Taps];
        private int _pos;

        public NarrowbandFir(float gain)
        {
            float fc = CutoffHz / AudioFormat.SampleRate;
            float m = Taps - 1;
            float sum = 0f;
            for (int n = 0; n < Taps; n++)
            {
                float x = n - m / 2f;
                float sinc = x == 0f ? 2f * fc : MathF.Sin(2f * MathF.PI * fc * x) / (MathF.PI * x);
                float hamming = 0.54f - 0.46f * MathF.Cos(2f * MathF.PI * n / m);
                _taps[n] = sinc * hamming;
                sum += _taps[n];
            }
            for (int n = 0; n < Taps; n++) _taps[n] *= gain / sum;
        }

        public float Process(float sample)
        {
            _history[_pos] = sample;
            float acc = 0f;
            int idx = _pos;
            for (int n = 0; n < Taps; n++)
            {
                acc += _taps[n] * _history[idx];
                idx = idx == 0 ? Taps - 1 : idx - 1;
            }
            _pos = (_pos + 1) % Taps;
            return acc;
        }

        public void Reset()
        {
            Array.Clear(_history, 0, _history.Length);
            _pos = 0;
        }
    }

    /// <summary>
    /// G.711 μ-law behind the frame-codec interface the capture path and <see cref="RemoteMixer"/>
    /// already use for Opus: 48 kHz mono float in/out, 8 kHz μ-law on the wire (a 20 ms frame of
    /// 960 samples becomes 160 bytes). Stateful FIR decimation/interpolation on both sides,
    /// fade-to-silence concealment for lost frames. No bitrate or policy controls apply.
    /// </summary>
    public sealed class PcmuCodec : IOpusCodec
    {
        public const int Decimation = AudioFormat.SampleRate / G711.SampleRate; // 6

        private readonly NarrowbandFir _down = new NarrowbandFir(1f);
        private readonly NarrowbandFir _up = new NarrowbandFir(Decimation);
        private float _last;

        public int SampleRate => AudioFormat.SampleRate;
        public int Channels => 1;

        /// <summary>Encode 48 kHz mono PCM; writes <c>frameSamplesPerChannel / 6</c> μ-law bytes.</summary>
        public int Encode(ReadOnlySpan<float> pcm, int frameSamplesPerChannel, Span<byte> output)
        {
            int n = Math.Min(frameSamplesPerChannel, pcm.Length);
            int written = 0;
            for (int i = 0; i < n; i++)
            {
                float y = _down.Process(pcm[i]);
                if (i % Decimation == Decimation - 1)
                {
                    if (written >= output.Length) break;
                    if (y > 1f) y = 1f; else if (y < -1f) y = -1f;
                    output[written++] = G711.UlawEncode((short)Math.Round(y * 32767f));
                }
            }
            return written;
        }

        /// <summary>Decode one μ-law frame (10/20/40/60 ms) to 48 kHz; returns samples written, 0 for a bad size.</summary>
        public int Decode(ReadOnlySpan<byte> ulaw, Span<float> pcm, int maxFrameSamplesPerChannel)
        {
            if (!G711.IsValidFrame(ulaw.Length)) return 0;
            int total = ulaw.Length * Decimation;
            if (total > maxFrameSamplesPerChannel || total > pcm.Length) return 0;
            int n = 0;
            for (int i = 0; i < ulaw.Length; i++)
            {
                float s = G711.UlawDecode(ulaw[i]) / 32768f;
                _last = s;
                for (int k = 0; k < Decimation; k++) pcm[n++] = _up.Process(k == 0 ? s : 0f);
            }
            return n;
        }

        /// <summary>Concealment: a fast fade from the last sample to silence.</summary>
        public int DecodeLost(Span<float> pcm, int frameSamplesPerChannel)
        {
            int n = Math.Min(frameSamplesPerChannel, pcm.Length);
            for (int i = 0; i < n; i++) pcm[i] = _last * (1f - (float)i / n);
            _last = 0f;
            return n;
        }

        /// <summary>G.711 has a fixed rate; ignored.</summary>
        public void SetBitrate(int bitsPerSecond) { }

        public void Reset()
        {
            _down.Reset();
            _up.Reset();
            _last = 0f;
        }

        public void Dispose() { }
    }
}
