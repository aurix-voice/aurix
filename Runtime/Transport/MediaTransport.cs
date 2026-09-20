using System;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.IO;
using System.Net;
using System.Net.Sockets;
using System.Security.Cryptography;
using System.Threading;
using System.Threading.Tasks;
using Aurix.Audio;
using Aurix.Protocol;

namespace Aurix.Transport
{
    /// <summary>Which link a <see cref="MediaTransport"/> moves AURX packets over.</summary>
    public enum MediaPath
    {
        /// <summary>No media transport bound yet.</summary>
        None,
        /// <summary>Native AURX datagrams to the node's media port (the low-latency default).</summary>
        Udp,
        /// <summary>The same sealed packets as binary frames on the control WebSocket (TCP; head-of-line blocking under loss).</summary>
        Tunnel,
    }

    /// <summary>How <see cref="AurixVoiceClient"/> picks the media path.</summary>
    public enum MediaPathPolicy
    {
        /// <summary>UDP first; fall back to the tunnel when UDP does not bind or its heartbeats die, re-probe UDP periodically.</summary>
        Auto,
        /// <summary>UDP or nothing (the pre-1.3 behaviour).</summary>
        UdpOnly,
        /// <summary>Always tunnel through the control WebSocket (testing, or networks known to drop UDP).</summary>
        TunnelOnly,
    }

    /// <summary>
    /// Uplink sequence counter shared by every <see cref="MediaTransport"/> of one session, so the
    /// server's single anti-replay window keeps accepting packets when the media moves between UDP
    /// and the tunnel (or a resumed session rebinds).
    /// </summary>
    public sealed class SequenceCounter
    {
        private readonly object _lock = new object();
        private uint _seq;

        public SequenceCounter(uint initial = 0) { _seq = initial; }

        /// <summary>The last sequence number handed out.</summary>
        public uint Current { get { lock (_lock) return _seq; } }

        public uint Next()
        {
            lock (_lock) return ++_seq;
        }
    }

    /// <summary>Heartbeat round-trip statistics of a <see cref="MediaTransport"/>.</summary>
    public struct RttStats
    {
        public float LastMs;
        public float MinMs;
        public float MaxMs;
        public float AvgMs;
        public long Samples;
    }

    /// <summary>A verified downlink audio packet (one Opus frame from one remote participant).</summary>
    public struct IncomingAudio
    {
        public uint SenderSsrc;
        public uint Sequence;
        public uint Timestamp;
        /// <summary>Channel the frame was forwarded through (<see cref="AurxPacket.ChannelIdHash(Guid)"/> of its id).</summary>
        public uint ChannelHash;
        public float Volume;
        /// <summary>Speaker direction in this listener's frame (directional positional channels), or null.</summary>
        public Direction? Direction;
        /// <summary>Codec of <see cref="Payload"/>: Opus unless this session negotiated PCMU.</summary>
        public AudioCodec Codec;
        /// <summary>
        /// The frame is the server's mix of the whole channel for this receiver (<see cref="PacketFlags.Mixed"/>):
        /// stereo Opus (mono μ-law on a PCMU session) under the channel's mix SSRC rather than one speaker's voice.
        /// </summary>
        public bool Mixed;
        /// <summary>The encoded frame (Opus, or μ-law when <see cref="Codec"/> is <see cref="AudioCodec.Pcmu"/>).</summary>
        public byte[] Payload;
        [Obsolete("Use Payload and check Codec; the frame is not Opus on a PCMU session.")]
        public byte[] Opus => Payload;
    }

    /// <summary>
    /// Native AURX media transport (protocol v2). Every uplink packet is sealed with the
    /// per-session <see cref="MediaKeys"/> (AES-256-CTR payload + HMAC tag) and carries one
    /// monotonically increasing sequence number (the server keeps one anti-replay window per
    /// session, shared by audio, heartbeat and control packets). Downlink packets are sealed by
    /// the server with the same keys, opened here and replay-checked per remote SSRC before
    /// being exposed. Only the initial <c>SessionBind</c> is signed without encryption.
    /// The packets travel either as UDP datagrams (<see cref="MediaTransport(IPEndPoint, Guid, uint, byte[], uint)"/>)
    /// or, byte for byte the same, as binary frames on an <see cref="IMediaTunnel"/>
    /// (<see cref="OverTunnel"/>) when UDP is blocked.
    /// </summary>
    public sealed class MediaTransport : IDisposable
    {
        /// <summary>Heartbeat cadence; set before <see cref="BindAsync"/>. Also the RTT sampling rate.</summary>
        public TimeSpan HeartbeatInterval { get; set; } = TimeSpan.FromSeconds(5);

        private readonly UdpClient _udp;
        private readonly IPEndPoint _server;
        private readonly IMediaTunnel _tunnel;
        private readonly MediaKeys _keys;
        private readonly uint _ssrc;
        private readonly Guid _sessionId;
        private readonly SequenceCounter _seq;
        private readonly ConcurrentQueue<IncomingAudio> _audioInbox = new ConcurrentQueue<IncomingAudio>();
        private readonly ConcurrentDictionary<uint, ReplayWindow> _replayBySender = new ConcurrentDictionary<uint, ReplayWindow>();
        private CancellationTokenSource _cts;
        private Task _recvLoop;
        private Task _heartbeatLoop;
        private volatile TaskCompletionSource<bool> _bindAck;
        private volatile bool _disposed;
        private long _packetsSent, _packetsReceived, _packetsBadAuth, _packetsReplayed;
        private long _bytesSent, _bytesReceived, _heartbeatsSent, _heartbeatsLost, _uplinkDropped;
        private int _heartbeatsLostConsecutive;
        private volatile int _lastHeartbeatAckTs;
        private volatile bool _heartbeatAcked;
        private long _lastAckUnixMs;
        private readonly object _statsLock = new object();
        private float _rttMin, _rttMax, _rttAvg;
        private long _rttSamples;
        private uint _jitterSsrc, _jitterLastTs;
        private long _jitterLastArrivalTicks;
        private float _jitterMs;

        public long PacketsSent => Interlocked.Read(ref _packetsSent);
        public long PacketsReceived => Interlocked.Read(ref _packetsReceived);
        public long PacketsBadAuth => Interlocked.Read(ref _packetsBadAuth);
        public long PacketsReplayed => Interlocked.Read(ref _packetsReplayed);
        public long BytesSent => Interlocked.Read(ref _bytesSent);
        /// <summary>Payload bytes of verified downlink audio.</summary>
        public long BytesReceived => Interlocked.Read(ref _bytesReceived);
        /// <summary>Heartbeats sent without an ack arriving before the next one.</summary>
        public long HeartbeatsLost => Interlocked.Read(ref _heartbeatsLost);
        /// <summary>Heartbeats lost in a row; reset by every ack. Drives the UDP → tunnel fallback.</summary>
        public int HeartbeatsLostConsecutive => Volatile.Read(ref _heartbeatsLostConsecutive);
        /// <summary>Uplink packets dropped because the tunnel's send queue was full (always 0 on UDP).</summary>
        public long UplinkDropped => Interlocked.Read(ref _uplinkDropped);
        /// <summary>The link this transport uses.</summary>
        public MediaPath Path => _tunnel != null ? MediaPath.Tunnel : MediaPath.Udp;
        /// <summary>Local UDP socket address (null on the tunnel, which has no socket of its own).</summary>
        public IPEndPoint LocalEndPoint => _udp?.Client?.LocalEndPoint as IPEndPoint;
        /// <summary>Round-trip time of the last heartbeat, in milliseconds (0 until the first ack).</summary>
        public float LastRttMs { get; private set; }
        /// <summary>Heartbeat RTT statistics over the life of this transport (all 0 until the first ack).</summary>
        public RttStats Rtt
        {
            get { lock (_statsLock) return new RttStats { LastMs = LastRttMs, MinMs = _rttMin, MaxMs = _rttMax, AvgMs = _rttAvg, Samples = _rttSamples }; }
        }
        /// <summary>RFC 3550 inter-arrival jitter of downlink audio, in ms (one estimator over all senders).</summary>
        public float DownlinkJitterMs { get { lock (_statsLock) return _jitterMs; } }
        public long HeartbeatAcks => Interlocked.Read(ref _heartbeatAcks);
        private long _heartbeatAcks;
        /// <summary>True when a HeartbeatAck arrived within the last 3 intervals.</summary>
        public bool IsAlive => DateTimeOffset.UtcNow.ToUnixTimeMilliseconds() - Interlocked.Read(ref _lastAckUnixMs) < HeartbeatInterval.TotalMilliseconds * 3;

        /// <summary>Last uplink sequence number used; pass it as <c>initialSequence</c> when rebinding a resumed session.</summary>
        public uint CurrentSequence => _seq.Current;
        /// <summary>The counter behind <see cref="CurrentSequence"/>; hand it to the next transport of the same session.</summary>
        public SequenceCounter Sequence => _seq;

        /// <param name="initialSequence">
        /// Sequence counter to continue from. A resumed session keeps its media key and the server's
        /// replay window, so a fresh transport for it must not restart at zero.
        /// </param>
        public MediaTransport(IPEndPoint server, Guid sessionId, uint ssrc, byte[] mediaKey, uint initialSequence = 0)
            : this(server, sessionId, ssrc, mediaKey, new SequenceCounter(initialSequence)) { }

        /// <summary>UDP transport continuing <paramref name="sequence"/> (shared with the transport it replaces).</summary>
        public MediaTransport(IPEndPoint server, Guid sessionId, uint ssrc, byte[] mediaKey, SequenceCounter sequence)
        {
            _server = server ?? throw new ArgumentNullException(nameof(server));
            _sessionId = sessionId;
            _ssrc = ssrc;
            _seq = sequence ?? throw new ArgumentNullException(nameof(sequence));
            if (mediaKey == null) throw new ArgumentNullException(nameof(mediaKey));
            _keys = MediaKeys.Derive(mediaKey);
            _udp = new UdpClient(server.AddressFamily);
            _udp.Client.Bind(new IPEndPoint(server.AddressFamily == AddressFamily.InterNetworkV6 ? IPAddress.IPv6Any : IPAddress.Any, 0));
            _udp.Client.ReceiveBufferSize = 1 << 20;
        }

        private MediaTransport(IMediaTunnel tunnel, Guid sessionId, uint ssrc, byte[] mediaKey, SequenceCounter sequence)
        {
            _tunnel = tunnel ?? throw new ArgumentNullException(nameof(tunnel));
            _sessionId = sessionId;
            _ssrc = ssrc;
            _seq = sequence ?? throw new ArgumentNullException(nameof(sequence));
            if (mediaKey == null) throw new ArgumentNullException(nameof(mediaKey));
            _keys = MediaKeys.Derive(mediaKey);
            _tunnel.MediaReceived += OnTunnelFrame;
        }

        /// <summary>
        /// Transport that moves the same sealed packets over <paramref name="tunnel"/> (the control
        /// WebSocket) instead of UDP. The node must advertise <c>SessionInitAck.media_tunnel</c>.
        /// </summary>
        public static MediaTransport OverTunnel(IMediaTunnel tunnel, Guid sessionId, uint ssrc, byte[] mediaKey, SequenceCounter sequence = null) =>
            new MediaTransport(tunnel, sessionId, ssrc, mediaKey, sequence ?? new SequenceCounter());

        /// <summary>Resolve <c>host:port</c> as delivered in <c>SessionInitAck.media_addr</c> (IPv4 preferred for host names).</summary>
        public static async Task<IPEndPoint> ResolveAsync(string mediaAddr)
        {
            var all = await ResolveAllAsync(mediaAddr).ConfigureAwait(false);
            if (all.Count == 0) throw new SocketException((int)SocketError.HostNotFound);
            return all[0];
        }

        /// <summary>
        /// Resolve every media endpoint the node advertised (<c>SessionInitAck.media_addrs</c>: IPv4 first, then
        /// IPv6; older nodes send only <c>media_addr</c>) into distinct UDP candidates in bind order. Entries that
        /// fail to parse or resolve are skipped as long as at least one candidate remains.
        /// </summary>
        public static async Task<List<IPEndPoint>> ResolveCandidatesAsync(string mediaAddr, IReadOnlyList<string> mediaAddrs)
        {
            var endpoints = mediaAddrs != null && mediaAddrs.Count > 0 ? mediaAddrs : new[] { mediaAddr };
            var result = new List<IPEndPoint>();
            Exception last = null;
            foreach (var ep in endpoints)
            {
                if (string.IsNullOrEmpty(ep)) continue;
                try
                {
                    foreach (var candidate in await ResolveAllAsync(ep).ConfigureAwait(false))
                        if (!result.Contains(candidate)) result.Add(candidate);
                }
                catch (Exception e) { last = e; }
            }
            if (result.Count == 0) throw last ?? new SocketException((int)SocketError.HostNotFound);
            return result;
        }

        private static async Task<List<IPEndPoint>> ResolveAllAsync(string mediaAddr)
        {
            int colon = mediaAddr.LastIndexOf(':');
            if (colon <= 0) throw new FormatException("media_addr must be host:port");
            var host = mediaAddr.Substring(0, colon).Trim('[', ']');
            var port = int.Parse(mediaAddr.Substring(colon + 1));
            var result = new List<IPEndPoint>();
            if (IPAddress.TryParse(host, out var ip)) { result.Add(new IPEndPoint(ip, port)); return result; }
            var addrs = await Dns.GetHostAddressesAsync(host).ConfigureAwait(false);
            foreach (var a in addrs) if (a.AddressFamily == AddressFamily.InterNetwork) result.Add(new IPEndPoint(a, port));
            foreach (var a in addrs) if (a.AddressFamily == AddressFamily.InterNetworkV6) result.Add(new IPEndPoint(a, port));
            return result;
        }

        /// <summary>
        /// Authenticate this link with the server (<c>SessionBind</c> → <c>SessionBindAck</c>),
        /// retrying a few times. Starts the receive and heartbeat loops on success.
        /// </summary>
        public async Task BindAsync(CancellationToken ct, int attempts = 5, int timeoutMs = 500)
        {
            if (_tunnel != null)
            {
                await BindTunnelAsync(ct, attempts, timeoutMs).ConfigureAwait(false);
                return;
            }
            var wire = BindPacket();
            for (int i = 0; i < attempts; i++)
            {
                ct.ThrowIfCancellationRequested();
                await _udp.SendAsync(wire, wire.Length, _server).ConfigureAwait(false);
                var recv = _udp.ReceiveAsync();
                var done = await Task.WhenAny(recv, Task.Delay(timeoutMs, ct)).ConfigureAwait(false);
                if (done != recv) continue;
                var res = recv.Result;
                if (!AurxPacket.TryDecode(res.Buffer, out var pkt, out _)) continue;
                if (pkt.Header.Type == PacketType.SessionBindAck && pkt.IsEncrypted && pkt.Open(_keys))
                {
                    Interlocked.Exchange(ref _lastAckUnixMs, DateTimeOffset.UtcNow.ToUnixTimeMilliseconds());
                    _cts = new CancellationTokenSource();
                    _recvLoop = Task.Run(() => ReceiveLoop(_cts.Token));
                    _heartbeatLoop = Task.Run(() => HeartbeatLoop(_cts.Token));
                    return;
                }
            }
            throw new TimeoutException("no SessionBindAck from media server");
        }

        private byte[] BindPacket()
        {
            var nonce = new byte[8];
            using (var rng = RandomNumberGenerator.Create()) rng.GetBytes(nonce);
            var bind = AurxPacket.SessionBind(_sessionId, _ssrc, DateTimeOffset.UtcNow.ToUnixTimeMilliseconds(), BitConverter.ToUInt64(nonce, 0));
            return bind.EncodeAuthenticated(_keys);
        }

        private async Task BindTunnelAsync(CancellationToken ct, int attempts, int timeoutMs)
        {
            bool first = _cts == null;
            for (int i = 0; i < attempts; i++)
            {
                ct.ThrowIfCancellationRequested();
                var ack = new TaskCompletionSource<bool>(TaskCreationOptions.RunContinuationsAsynchronously);
                _bindAck = ack;
                // A bind's timestamp must be newer than the previous one the server accepted.
                if (i > 0) await Task.Delay(2, ct).ConfigureAwait(false);
                if (!_tunnel.TrySendMedia(BindPacket())) throw new IOException("media tunnel is closed");
                var done = await Task.WhenAny(ack.Task, Task.Delay(timeoutMs, ct)).ConfigureAwait(false);
                if (done != ack.Task) continue;
                _bindAck = null;
                if (first)
                {
                    _cts = new CancellationTokenSource();
                    _heartbeatLoop = Task.Run(() => HeartbeatLoop(_cts.Token));
                }
                return;
            }
            _bindAck = null;
            throw new TimeoutException("no SessionBindAck from media server");
        }

        /// <summary>
        /// Re-send <c>SessionBind</c> over an already bound tunnel: after a UDP probe that the server
        /// may have answered (moving the session's media to the probe socket) while the client never
        /// saw the ack, this makes the tunnel the session's media path again. UDP transports rebind
        /// by construction (a new socket); calling this on one is an error.
        /// </summary>
        public Task ReclaimAsync(CancellationToken ct, int attempts = 2, int timeoutMs = 3000)
        {
            if (_tunnel == null) throw new InvalidOperationException("only a tunnelled transport can reclaim its bind");
            if (_cts == null) throw new InvalidOperationException("transport not bound");
            return BindTunnelAsync(ct, attempts, timeoutMs);
        }

        private uint NextSeq() => _seq.Next();

        /// <summary>
        /// Send one encoded Opus frame. <paramref name="rtpTimestamp"/> advances by 960 per 20 ms at 48 kHz.
        /// <paramref name="level"/> is the sender's measured <see cref="Aurix.Audio.AudioLevel"/> of the
        /// frame (null = not measured; the server then infers speaking from packet arrival only).
        /// </summary>
        public void SendAudio(uint channelHash, uint rtpTimestamp, byte[] opusFrame, int length = -1, byte? level = null) =>
            SendAudio(channelHash, rtpTimestamp, AudioCodec.Opus, opusFrame, length, level);

        /// <summary>
        /// Send one encoded frame of <paramref name="codec"/>. The RTP clock stays at 48 kHz for both
        /// codecs; a μ-law frame is flagged <see cref="PacketFlags.Pcmu"/> and must be 10/20/40/60 ms
        /// (80/160/320/480 bytes) — anything else is dropped by the server.
        /// </summary>
        public void SendAudio(uint channelHash, uint rtpTimestamp, AudioCodec codec, byte[] frame, int length = -1, byte? level = null)
        {
            if (length < 0) length = frame.Length;
            var payload = length == frame.Length ? frame : Slice(frame, length);
            int extra = level.HasValue ? 1 : 0;
            if (AurxPacket.HeaderSize + payload.Length + extra + AurxPacket.AuthTagSize > AurxPacket.MaxPacketSize)
                throw new ArgumentException("audio frame too large for one AURX packet");
            var pkt = level.HasValue
                ? AurxPacket.AudioWithLevel(NextSeq(), rtpTimestamp, _ssrc, channelHash, level.Value, payload)
                : AurxPacket.Audio(NextSeq(), rtpTimestamp, _ssrc, channelHash, payload);
            if (codec == AudioCodec.Pcmu) pkt.Header.Flags |= PacketFlags.Pcmu;
            Send(pkt);
        }

        public void SendMuteState(bool muted) => Send(AurxPacket.MuteState(NextSeq(), _ssrc, muted));

        public void SendQualityReport(float rttMs, float jitterMs, float lossPercent) =>
            Send(AurxPacket.QualityReport(NextSeq(), _ssrc, rttMs, jitterMs, lossPercent));

        private void Send(AurxPacket packet)
        {
            var wire = packet.Seal(_keys);
            if (_tunnel != null)
            {
                if (_disposed) return;
                if (_tunnel.TrySendMedia(wire))
                {
                    Interlocked.Increment(ref _packetsSent);
                    Interlocked.Add(ref _bytesSent, wire.Length);
                }
                else Interlocked.Increment(ref _uplinkDropped);
                return;
            }
            try
            {
                _udp.Send(wire, wire.Length, _server);
                Interlocked.Increment(ref _packetsSent);
                Interlocked.Add(ref _bytesSent, wire.Length);
            }
            catch (SocketException) { /* transient; the caller's next frame retries */ }
            catch (ObjectDisposedException) { }
        }

        /// <summary>Dequeue verified downlink audio (call from your audio/update thread).</summary>
        public bool TryDequeueAudio(out IncomingAudio audio) => _audioInbox.TryDequeue(out audio);

        private async Task HeartbeatLoop(CancellationToken ct)
        {
            try
            {
                while (!ct.IsCancellationRequested)
                {
                    await Task.Delay(HeartbeatInterval, ct).ConfigureAwait(false);
                    uint ts = (uint)Environment.TickCount;
                    _lastHeartbeatAckTs = (int)ts;
                    if (!_heartbeatAcked && Interlocked.Read(ref _heartbeatsSent) > 0)
                    {
                        Interlocked.Increment(ref _heartbeatsLost);
                        Interlocked.Increment(ref _heartbeatsLostConsecutive);
                    }
                    _heartbeatAcked = false;
                    Interlocked.Increment(ref _heartbeatsSent);
                    Send(AurxPacket.Heartbeat(NextSeq(), _ssrc, ts));
                }
            }
            catch (OperationCanceledException) { }
        }

        private void OnTunnelFrame(byte[] frame)
        {
            if (_disposed) return;
            if (!AurxPacket.TryDecode(frame, out var pkt, out _)) return;
            try
            {
                if (pkt.Header.Type == PacketType.SessionBindAck)
                {
                    if (!pkt.IsEncrypted || !pkt.Open(_keys)) { Interlocked.Increment(ref _packetsBadAuth); return; }
                    Interlocked.Exchange(ref _lastAckUnixMs, DateTimeOffset.UtcNow.ToUnixTimeMilliseconds());
                    _bindAck?.TrySetResult(true);
                    return;
                }
                if (_cts == null) return; // not bound yet: nothing but the ack is expected
                Process(pkt, frame.Length);
            }
            catch (ObjectDisposedException) { /* disposed while a frame was in flight */ }
        }

        private async Task ReceiveLoop(CancellationToken ct)
        {
            while (!ct.IsCancellationRequested)
            {
                UdpReceiveResult r;
                try { r = await _udp.ReceiveAsync().ConfigureAwait(false); }
                catch (ObjectDisposedException) { return; }
                catch (SocketException) { if (ct.IsCancellationRequested) return; continue; }

                if (!r.RemoteEndPoint.Equals(_server)) continue;
                if (!AurxPacket.TryDecode(r.Buffer, out var pkt, out _)) continue;
                Process(pkt, r.Buffer.Length);
            }
        }

        /// <summary>Open, replay-check and dispatch one downlink packet — identical for both links.</summary>
        private void Process(AurxPacket pkt, int wireLength)
        {
            if (!pkt.IsEncrypted || !pkt.Open(_keys))
            {
                Interlocked.Increment(ref _packetsBadAuth);
                return;
            }
            switch (pkt.Header.Type)
            {
                case PacketType.Audio:
                case PacketType.AudioFec:
                    var window = _replayBySender.GetOrAdd(pkt.Header.Ssrc, _ => new ReplayWindow());
                    bool fresh;
                    lock (window) fresh = window.CheckAndUpdate(pkt.Header.Sequence);
                    if (!fresh) { Interlocked.Increment(ref _packetsReplayed); return; }
                    Interlocked.Increment(ref _packetsReceived);
                    Interlocked.Add(ref _bytesReceived, wireLength);
                    ObserveJitter(pkt.Header.Ssrc, pkt.Header.Timestamp);
                    var (volume, direction) = pkt.TakeDownlinkMeta();
                    pkt.TakeAudioLevel();
                    _audioInbox.Enqueue(new IncomingAudio
                    {
                        SenderSsrc = pkt.Header.Ssrc,
                        Sequence = pkt.Header.Sequence,
                        Timestamp = pkt.Header.Timestamp,
                        ChannelHash = pkt.Header.ChannelIdHash,
                        Volume = volume,
                        Direction = direction,
                        Codec = (pkt.Header.Flags & PacketFlags.Pcmu) != 0 ? AudioCodec.Pcmu : AudioCodec.Opus,
                        Mixed = (pkt.Header.Flags & PacketFlags.Mixed) != 0,
                        Payload = pkt.Payload,
                    });
                    break;
                case PacketType.HeartbeatAck:
                    Interlocked.Exchange(ref _lastAckUnixMs, DateTimeOffset.UtcNow.ToUnixTimeMilliseconds());
                    Interlocked.Increment(ref _heartbeatAcks);
                    Volatile.Write(ref _heartbeatsLostConsecutive, 0);
                    _heartbeatAcked = true;
                    if ((int)pkt.Header.Timestamp == _lastHeartbeatAckTs)
                        RecordRtt(Math.Max(0, unchecked((int)((uint)Environment.TickCount - pkt.Header.Timestamp))));
                    break;
                case PacketType.SessionBindAck:
                    // A late ack for a UDP re-bind (or a tunnel reclaim) — the bind loop already returned.
                    break;
                default:
                    break;
            }
        }

        /// <summary>Forget the replay window of a participant that left, so a rejoin with a reset sequence is accepted.</summary>
        public void ForgetSender(uint ssrc) => _replayBySender.TryRemove(ssrc, out _);

        private void RecordRtt(float rttMs)
        {
            lock (_statsLock)
            {
                LastRttMs = rttMs;
                if (_rttSamples == 0) { _rttMin = _rttMax = _rttAvg = rttMs; }
                else
                {
                    _rttMin = Math.Min(_rttMin, rttMs);
                    _rttMax = Math.Max(_rttMax, rttMs);
                    _rttAvg += (rttMs - _rttAvg) / (_rttSamples + 1);
                }
                _rttSamples++;
            }
        }

        private void ObserveJitter(uint ssrc, uint rtpTimestamp)
        {
            long now = DateTime.UtcNow.Ticks;
            lock (_statsLock)
            {
                if (_jitterLastArrivalTicks != 0 && _jitterSsrc == ssrc)
                {
                    float expectedMs = unchecked(rtpTimestamp - _jitterLastTs) / (float)(AudioFormat.SampleRate / 1000);
                    float actualMs = (now - _jitterLastArrivalTicks) / (float)TimeSpan.TicksPerMillisecond;
                    if (expectedMs >= 0f && expectedMs < 1000f)
                    {
                        float d = Math.Abs(actualMs - expectedMs);
                        _jitterMs += (d - _jitterMs) / 16f;
                    }
                }
                _jitterSsrc = ssrc;
                _jitterLastTs = rtpTimestamp;
                _jitterLastArrivalTicks = now;
            }
        }

        private static byte[] Slice(byte[] src, int len)
        {
            var dst = new byte[len];
            Buffer.BlockCopy(src, 0, dst, 0, len);
            return dst;
        }

        public void Dispose()
        {
            _disposed = true;
            _cts?.Cancel();
            if (_tunnel != null) _tunnel.MediaReceived -= OnTunnelFrame;
            _udp?.Dispose();
            _cts?.Dispose();
            _keys.Dispose();
        }
    }
}
