using System;

namespace Aurix.Audio
{
    /// <summary>
    /// Pulls 48 kHz frames from a source (the <see cref="RemoteMixer"/>) and linearly resamples them to
    /// the device output rate, carrying the fractional read position across callbacks so block edges
    /// stay seamless. Needed on mobile, where Unity follows the hardware rate (44.1 kHz on many Android
    /// devices, 24 kHz Bluetooth HFP routes on iOS) regardless of the project audio settings.
    /// Single consumer (the audio thread); no allocations after the first block of a given size.
    /// </summary>
    public sealed class OutputResampler
    {
        /// <summary>Add <paramref name="frames"/> interleaved frames at <paramref name="offsetSamples"/> (the region is zeroed beforehand).</summary>
        public delegate void FillSource(float[] buffer, int offsetSamples, int frames, int channels);

        private float[] _src = Array.Empty<float>();
        private int _srcFrames;
        private double _phase;
        private int _channels;

        /// <summary>Source frames requested so far (diagnostics/tests).</summary>
        public long SourceFramesPulled { get; private set; }

        public void Reset()
        {
            _srcFrames = 0;
            _phase = 0;
        }

        /// <summary>
        /// Fill <paramref name="output"/> (interleaved, adds to its contents) at <paramref name="outputRate"/>
        /// from source audio at <see cref="AudioFormat.SampleRate"/>. Equal rates pass straight through.
        /// </summary>
        public void Process(float[] output, int channels, int outputRate, FillSource fill)
        {
            if (channels <= 0 || outputRate <= 0) throw new ArgumentOutOfRangeException(channels <= 0 ? nameof(channels) : nameof(outputRate));
            int outFrames = output.Length / channels;
            if (outputRate == AudioFormat.SampleRate)
            {
                fill(output, 0, outFrames, channels);
                SourceFramesPulled += outFrames;
                return;
            }
            if (channels != _channels)
            {
                _channels = channels;
                Reset();
            }

            double ratio = (double)AudioFormat.SampleRate / outputRate;
            double endPhase = _phase + outFrames * ratio;
            int consumed = (int)Math.Floor(endPhase);
            int needed = consumed + 2; // interpolation reads frame floor(pos) and floor(pos) + 1
            if (_src.Length < needed * channels)
            {
                var grown = new float[needed * channels];
                Array.Copy(_src, grown, _srcFrames * channels);
                _src = grown;
            }
            if (needed > _srcFrames)
            {
                int fresh = needed - _srcFrames;
                Array.Clear(_src, _srcFrames * channels, fresh * channels);
                fill(_src, _srcFrames * channels, fresh, channels);
                SourceFramesPulled += fresh;
                _srcFrames = needed;
            }

            for (int i = 0; i < outFrames; i++)
            {
                double pos = _phase + i * ratio;
                int i0 = (int)pos;
                float t = (float)(pos - i0);
                int a = i0 * channels, b = a + channels, o = i * channels;
                for (int c = 0; c < channels; c++)
                    output[o + c] += _src[a + c] + (_src[b + c] - _src[a + c]) * t;
            }

            int keep = _srcFrames - consumed;
            Array.Copy(_src, consumed * channels, _src, 0, keep * channels);
            _srcFrames = keep;
            _phase = endPhase - consumed;
        }
    }

    /// <summary>
    /// The opposite direction of <see cref="OutputResampler"/>: converts blocks the device produced at its
    /// own rate into 48 kHz for the echo canceller's render reference (<c>AurixListenerTap</c>). Linear
    /// interpolation with the phase and last frame carried across blocks; single consumer (audio thread).
    /// </summary>
    public sealed class RenderRateConverter
    {
        private float[] _last = Array.Empty<float>();
        private bool _hasLast;
        private double _phase;
        private int _channels;
        private int _inputRate;

        public void Reset()
        {
            _hasLast = false;
            _phase = 0;
        }

        /// <summary>Output samples (interleaved) produced from <paramref name="frames"/> frames at <paramref name="inputRate"/>; size <paramref name="output"/> with this.</summary>
        public static int MaxOutputSamples(int frames, int channels, int inputRate)
            => ((int)Math.Ceiling((frames + 1) * (double)AudioFormat.SampleRate / inputRate) + 1) * channels;

        /// <summary>Convert <paramref name="input"/> (interleaved, <paramref name="inputRate"/>) to 48 kHz into <paramref name="output"/>; returns samples written.</summary>
        public int Convert(float[] input, int channels, int inputRate, float[] output)
        {
            if (channels <= 0 || inputRate <= 0) throw new ArgumentOutOfRangeException(channels <= 0 ? nameof(channels) : nameof(inputRate));
            int inFrames = input.Length / channels;
            if (inFrames == 0) return 0;
            if (inputRate == AudioFormat.SampleRate)
            {
                Array.Copy(input, output, inFrames * channels);
                return inFrames * channels;
            }
            if (channels != _channels || inputRate != _inputRate)
            {
                _channels = channels;
                _inputRate = inputRate;
                if (_last.Length < channels) _last = new float[channels];
                Reset();
            }
            if (!_hasLast)
            {
                Array.Copy(input, _last, channels);
                _hasLast = true;
                _phase = 1; // first block: start at its own frame 0
            }

            // Source timeline: frame -1 = _last, frames 0..inFrames-1 = input. Emit every output position
            // strictly before the last input frame; the remainder waits for the next block.
            double step = (double)inputRate / AudioFormat.SampleRate;
            int written = 0;
            double pos = _phase - 1; // relative to input frame 0
            while (pos < inFrames - 1 && written + channels <= output.Length)
            {
                int i0 = (int)Math.Floor(pos);
                float t = (float)(pos - i0);
                for (int c = 0; c < channels; c++)
                {
                    float a = i0 < 0 ? _last[c] : input[i0 * channels + c];
                    float b = input[(i0 + 1) * channels + c];
                    output[written + c] = a + (b - a) * t;
                }
                written += channels;
                pos += step;
            }
            _phase = pos - (inFrames - 1);
            Array.Copy(input, (inFrames - 1) * channels, _last, 0, channels);
            return written;
        }
    }
}
