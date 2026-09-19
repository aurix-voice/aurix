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
}
