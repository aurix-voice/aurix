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
        /// <summary>Lost slots for which the following packet was already here (FEC recovery possible).</summary>
        public int Recoverable { get; private set; }

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
        public bool Pop(out byte[] opus) => Pop(out opus, out _);

        /// <summary>
        /// <see cref="Pop(out byte[])"/> that, for a lost slot, also hands out the packet of the next slot
        /// when it is already buffered (<paramref name="fecFrom"/>), so an <see cref="IOpusFecDecoder"/>
        /// can rebuild the lost frame from its in-band FEC instead of running PLC.
        /// </summary>
        public bool Pop(out byte[] opus, out byte[] fecFrom)
        {
            opus = null;
            fecFrom = null;
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
                    if (_frames.TryGetValue(_nextSeq, out fecFrom)) Recoverable++;
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
    /// Lifetime downlink playout counters of a <see cref="RemoteMixer"/>, including streams that
    /// have since been removed.
    /// </summary>
    public struct MixerTotals
    {
        /// <summary>Frames declared lost by the jitter buffers (concealed with PLC).</summary>
        public long Lost;
        /// <summary>Lost frames rebuilt from the next packet's in-band FEC (a subset of <see cref="Lost"/>).</summary>
        public long FecRecovered;
        /// <summary>Frames that arrived after their playout slot and were dropped.</summary>
        public long Late;
        /// <summary>
        /// Times a stream ran dry mid-spurt: the buffer starved and the next frame arrived within
        /// <see cref="RemoteMixer.UnderrunResumeWindow"/>. The natural end of a talk spurt is not counted.
        /// </summary>
        public long Underruns;
    }

    /// <summary>
    /// Decodes and mixes all remote senders into one interleaved float buffer (called from the
    /// audio thread, e.g. <c>OnAudioFilterRead</c>). Each sender gets its own decoder instance
    /// because Opus decoder state is per-stream.
    /// </summary>
    public sealed class RemoteMixer : IDisposable
    {
        /// <summary>A stream that starves and resumes within this window counts as an underrun.</summary>
        public static readonly TimeSpan UnderrunResumeWindow = TimeSpan.FromMilliseconds(250);

        private sealed class Stream
        {
            public JitterBuffer Jitter = new JitterBuffer();
            public IOpusCodec Decoder;
            public IOpusFecDecoder Fec;
            public AudioCodec Codec;
            public long FecRecovered;
            public float Volume = 1f;
            public float LeftGain = 1f;
            public float RightGain = 1f;
            public float[] Frame;
            public int FramePos;
            public int FrameLen;
            public long LastActivityTicks;
            public bool Starved;
            public long StarvedAtTicks;
        }

        private readonly Func<IOpusCodec> _decoderFactory;
        private readonly Dictionary<uint, Stream> _streams = new Dictionary<uint, Stream>();
        private readonly List<uint> _stale = new List<uint>();
        private float _outputVolume = 1f;
        private volatile bool _outputMuted;
        private long _retiredLost, _retiredLate, _retiredFec, _underruns;

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
        public void Push(uint ssrc, uint seq, float volume, byte[] opus) => Push(ssrc, seq, volume, null, AudioCodec.Opus, opus);

        /// <summary>
        /// Queue a verified frame with the speaker's direction relative to this listener. A mono
        /// stream is panned across a stereo output by constant-power law (see
        /// <see cref="Protocol.Direction.StereoGains"/>); <c>null</c> keeps it centred.
        /// </summary>
        public void Push(uint ssrc, uint seq, float volume, Protocol.Direction? direction, byte[] opus) =>
            Push(ssrc, seq, volume, direction, AudioCodec.Opus, opus);

        /// <summary>
        /// Queue a verified frame of either codec (<see cref="Transport.IncomingAudio.Codec"/>). After
        /// this session negotiates PCMU every downlink frame arrives as μ-law and is decoded by a
        /// <see cref="PcmuCodec"/>; a codec change on a stream swaps its decoder and refills the jitter buffer.
        /// </summary>
        public void Push(uint ssrc, uint seq, float volume, Protocol.Direction? direction, AudioCodec codec, byte[] payload)
        {
            Stream s;
            lock (_streams)
            {
                if (!_streams.TryGetValue(ssrc, out s))
                {
                    s = new Stream();
                    Attach(s, codec);
                    _streams[ssrc] = s;
                }
                else if (s.Codec != codec)
                {
                    Retire(s);
                    s.Jitter = new JitterBuffer();
                    s.FecRecovered = 0;
                    s.FramePos = s.FrameLen = 0;
                    Attach(s, codec);
                }
                s.Volume = volume;
                if (direction.HasValue) (s.LeftGain, s.RightGain) = direction.Value.StereoGains();
                else { s.LeftGain = 1f; s.RightGain = 1f; }
                long now = DateTime.UtcNow.Ticks;
                s.LastActivityTicks = now;
                if (s.Starved)
                {
                    // A talk spurt after silence: refill the target depth before playing again.
                    s.Jitter.Reset();
                    s.Starved = false;
                    if (now - s.StarvedAtTicks < UnderrunResumeWindow.Ticks) _underruns++;
                }
            }
            s.Jitter.Push(seq, payload);
        }

        private void Attach(Stream s, AudioCodec codec)
        {
            var decoder = codec == AudioCodec.Pcmu ? new PcmuCodec() : _decoderFactory();
            s.Decoder = decoder;
            s.Fec = decoder as IOpusFecDecoder;
            s.Codec = codec;
        }

        public void Remove(uint ssrc)
        {
            lock (_streams)
            {
                if (_streams.TryGetValue(ssrc, out var s))
                {
                    Retire(s);
                    _streams.Remove(ssrc);
                }
            }
        }

        /// <summary>Lifetime lost/late/underrun counters across all streams, past and present.</summary>
        public MixerTotals Totals
        {
            get
            {
                lock (_streams)
                {
                    var t = new MixerTotals { Lost = _retiredLost, Late = _retiredLate, FecRecovered = _retiredFec, Underruns = _underruns };
                    foreach (var s in _streams.Values) { t.Lost += s.Jitter.Lost; t.Late += s.Jitter.Late; t.FecRecovered += s.FecRecovered; }
                    return t;
                }
            }
        }

        /// <summary>Streams currently held (heard within the last 30 s).</summary>
        public int ActiveStreams { get { lock (_streams) return _streams.Count; } }

        private void Retire(Stream s)
        {
            _retiredLost += s.Jitter.Lost;
            _retiredLate += s.Jitter.Late;
            _retiredFec += s.FecRecovered;
            s.Decoder.Dispose();
        }

        /// <summary>
        /// Mix into <paramref name="output"/> (interleaved, <c>outputChannels</c> wide). Adds to
        /// existing contents. Mono streams with a direction are panned between the first two output
        /// channels (left, right); further channels get the centred signal.
        /// </summary>
        public void Mix(float[] output, int outputChannels) => Mix(output, 0, output.Length / outputChannels, outputChannels);

        /// <summary>
        /// Mix <paramref name="frames"/> frames into <paramref name="output"/> starting at sample index
        /// <paramref name="offset"/> (see <see cref="Mix(float[], int)"/>).
        /// </summary>
        public void Mix(float[] output, int offset, int frames, int outputChannels)
        {
            int framesNeeded = frames;
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
                            if (!s.Jitter.Pop(out var opus, out var fecFrom))
                            {
                                if (s.Jitter.Count == 0 && !s.Starved) { s.Starved = true; s.StarvedAtTicks = now; }
                                break;
                            }
                            int dc = s.Decoder.Channels;
                            int cap = AudioFormat.FrameSamples * 3 * dc; // up to 60 ms frames
                            if (s.Frame == null || s.Frame.Length < cap) s.Frame = new float[cap];
                            int n;
                            if (opus != null)
                                n = s.Decoder.Decode(opus, s.Frame, AudioFormat.FrameSamples * 3);
                            else if (fecFrom != null && s.Fec != null)
                            {
                                n = s.Fec.DecodeFec(fecFrom, s.Frame, AudioFormat.FrameSamples);
                                if (n > 0) s.FecRecovered++;
                                else n = s.Decoder.DecodeLost(s.Frame, AudioFormat.FrameSamples);
                            }
                            else
                                n = s.Decoder.DecodeLost(s.Frame, AudioFormat.FrameSamples);
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
                            bool pan = dch == 1 && outputChannels >= 2;
                            for (int f = 0; f < take; f++)
                            {
                                for (int c = 0; c < outputChannels; c++)
                                {
                                    int srcC = dch == 1 ? 0 : Math.Min(c, dch - 1);
                                    float g = gain;
                                    if (pan) g *= c == 0 ? s.LeftGain : c == 1 ? s.RightGain : 1f;
                                    output[offset + (written + f) * outputChannels + c] += s.Frame[s.FramePos + f * dch + srcC] * g;
                                }
                            }
                        }
                        s.FramePos += take * dch;
                        written += take;
                    }
                }
                foreach (var k in _stale) { Retire(_streams[k]); _streams.Remove(k); }
            }
            // Soft clip to avoid wrap-around distortion when several loud talkers overlap.
            int end = offset + frames * outputChannels;
            for (int i = offset; i < end; i++)
            {
                float v = output[i];
                if (v > 1f) output[i] = 1f; else if (v < -1f) output[i] = -1f;
            }
        }

        public void Dispose()
        {
            lock (_streams)
            {
                foreach (var s in _streams.Values) Retire(s);
                _streams.Clear();
            }
        }
    }
}
