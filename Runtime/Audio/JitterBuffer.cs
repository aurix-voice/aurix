using System;
using System.Collections.Generic;

namespace Aurix.Audio
{
    /// <summary>
    /// Per-sender jitter buffer: reorders by sequence number, absorbs network jitter with a small
    /// target depth and hands frames (or "lost" markers) to the decoder at a steady 20 ms cadence.
    /// </summary>
    public sealed class JitterBuffer
    {
        private readonly SortedDictionary<uint, byte[]> _frames = new SortedDictionary<uint, byte[]>();
        private readonly int _targetDepth;
        private readonly int _maxDepth;
        private uint _nextSeq;
        private bool _started;

        public int Count => _frames.Count;
        public int Lost { get; private set; }
        public int Late { get; private set; }

        /// <param name="targetDepthFrames">Frames buffered before playout starts (2 ≈ 40 ms).</param>
        /// <param name="maxDepthFrames">Hard cap; older frames are skipped when exceeded.</param>
        public JitterBuffer(int targetDepthFrames = 2, int maxDepthFrames = 12)
        {
            _targetDepth = Math.Max(1, targetDepthFrames);
            _maxDepth = Math.Max(_targetDepth + 1, maxDepthFrames);
        }

        public void Push(uint seq, byte[] opus)
        {
            lock (_frames)
            {
                if (_started && SeqBefore(seq, _nextSeq)) { Late++; return; }
                _frames[seq] = opus;
                if (_frames.Count > _maxDepth)
                {
                    // Fast-forward: drop the oldest frames to bound latency.
                    while (_frames.Count > _targetDepth)
                    {
                        var e = _frames.GetEnumerator();
                        e.MoveNext();
                        _frames.Remove(e.Current.Key);
                    }
                    var first = _frames.GetEnumerator();
                    first.MoveNext();
                    _nextSeq = first.Current.Key;
                }
            }
        }

        /// <summary>
        /// Pop the frame for the next playout slot. Returns false when nothing should be played
        /// (buffer still filling). <paramref name="opus"/> is null when the slot's packet is lost
        /// and the decoder should run PLC.
        /// </summary>
        public bool Pop(out byte[] opus)
        {
            opus = null;
            lock (_frames)
            {
                if (_frames.Count == 0) return false;
                if (!_started)
                {
                    if (_frames.Count < _targetDepth) return false;
                    var e = _frames.GetEnumerator();
                    e.MoveNext();
                    _nextSeq = e.Current.Key;
                    _started = true;
                }
                if (_frames.TryGetValue(_nextSeq, out opus))
                {
                    _frames.Remove(_nextSeq);
                    _nextSeq++;
                    return true;
                }
                // Gap: only declare loss if newer frames are already waiting; otherwise keep waiting.
                var en = _frames.GetEnumerator();
                en.MoveNext();
                if (SeqBefore(_nextSeq, en.Current.Key))
                {
                    Lost++;
                    _nextSeq++;
                    return true; // opus == null → PLC
                }
                return false;
            }
        }

        public void Reset()
        {
            lock (_frames)
            {
                _frames.Clear();
                _started = false;
            }
        }

        private static bool SeqBefore(uint a, uint b) => (int)(a - b) < 0;
    }

    /// <summary>
    /// Decodes and mixes all remote senders into one interleaved float buffer (called from the
    /// audio thread, e.g. <c>OnAudioFilterRead</c>). Each sender gets its own decoder instance
    /// because Opus decoder state is per-stream.
    /// </summary>
    public sealed class RemoteMixer : IDisposable
    {
        private sealed class Stream
        {
            public JitterBuffer Jitter = new JitterBuffer();
            public IOpusCodec Decoder;
            public float Volume = 1f;
            public float[] Frame;
            public int FramePos;
            public int FrameLen;
            public long LastActivityTicks;
        }

        private readonly Func<IOpusCodec> _decoderFactory;
        private readonly Dictionary<uint, Stream> _streams = new Dictionary<uint, Stream>();
        private readonly List<uint> _stale = new List<uint>();
        private float _outputVolume = 1f;
        private volatile bool _outputMuted;

        public RemoteMixer(Func<IOpusCodec> decoderFactory)
        {
            _decoderFactory = decoderFactory ?? throw new ArgumentNullException(nameof(decoderFactory));
        }

        /// <summary>
        /// Master volume applied on top of per-participant volumes, <c>0..2</c> (1 = unity).
        /// Safe to set from any thread.
        /// </summary>
        public float OutputVolume
        {
            get => _outputVolume;
            set => _outputVolume = AudioLevel.ClampGain(value, AudioLevel.MaxOutputVolume);
        }

        /// <summary>
        /// Speaker mute: streams keep being decoded (jitter buffers stay in sync, unmute is
        /// instant) but nothing is written to the output.
        /// </summary>
        public bool OutputMuted
        {
            get => _outputMuted;
            set => _outputMuted = value;
        }

        /// <summary>Queue a verified frame from the transport (any thread).</summary>
        public void Push(uint ssrc, uint seq, float volume, byte[] opus)
        {
            Stream s;
            lock (_streams)
            {
                if (!_streams.TryGetValue(ssrc, out s))
                {
                    s = new Stream { Decoder = _decoderFactory() };
                    _streams[ssrc] = s;
                }
                s.Volume = volume;
                s.LastActivityTicks = DateTime.UtcNow.Ticks;
            }
            s.Jitter.Push(seq, opus);
        }

        public void Remove(uint ssrc)
        {
            lock (_streams)
            {
                if (_streams.TryGetValue(ssrc, out var s))
                {
                    s.Decoder.Dispose();
                    _streams.Remove(ssrc);
                }
            }
        }

        /// <summary>Mix into <paramref name="output"/> (interleaved, <c>outputChannels</c> wide). Adds to existing contents.</summary>
        public void Mix(float[] output, int outputChannels)
        {
            int framesNeeded = output.Length / outputChannels;
            float master = _outputMuted ? 0f : _outputVolume;
            lock (_streams)
            {
                long now = DateTime.UtcNow.Ticks;
                _stale.Clear();
                foreach (var kv in _streams)
                {
                    var s = kv.Value;
                    if (now - s.LastActivityTicks > TimeSpan.TicksPerSecond * 30) { _stale.Add(kv.Key); continue; }
                    int written = 0;
                    while (written < framesNeeded)
                    {
                        if (s.FramePos >= s.FrameLen)
                        {
                            if (!s.Jitter.Pop(out var opus)) break;
                            int dc = s.Decoder.Channels;
                            int cap = AudioFormat.FrameSamples * 3 * dc; // up to 60 ms frames
                            if (s.Frame == null || s.Frame.Length < cap) s.Frame = new float[cap];
                            int n = opus != null
                                ? s.Decoder.Decode(opus, s.Frame, AudioFormat.FrameSamples * 3)
                                : s.Decoder.DecodeLost(s.Frame, AudioFormat.FrameSamples);
                            s.FrameLen = Math.Max(0, n) * dc;
                            s.FramePos = 0;
                            if (s.FrameLen == 0) break;
                        }
                        int dch = s.Decoder.Channels;
                        int availFrames = (s.FrameLen - s.FramePos) / dch;
                        int take = Math.Min(availFrames, framesNeeded - written);
                        float gain = s.Volume * master;
                        if (gain != 0f)
                        {
                            for (int f = 0; f < take; f++)
                            {
                                for (int c = 0; c < outputChannels; c++)
                                {
                                    int srcC = dch == 1 ? 0 : Math.Min(c, dch - 1);
                                    output[(written + f) * outputChannels + c] += s.Frame[s.FramePos + f * dch + srcC] * gain;
                                }
                            }
                        }
                        s.FramePos += take * dch;
                        written += take;
                    }
                }
                foreach (var k in _stale) { _streams[k].Decoder.Dispose(); _streams.Remove(k); }
            }
            // Soft clip to avoid wrap-around distortion when several loud talkers overlap.
            for (int i = 0; i < output.Length; i++)
            {
                float v = output[i];
                if (v > 1f) output[i] = 1f; else if (v < -1f) output[i] = -1f;
            }
        }

        public void Dispose()
        {
            lock (_streams)
            {
                foreach (var s in _streams.Values) s.Decoder.Dispose();
                _streams.Clear();
            }
        }
    }
}
