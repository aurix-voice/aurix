using System;
using System.Collections.Concurrent;
using System.Net;
using System.Net.Sockets;
using System.Security.Cryptography;
using System.Threading;
using System.Threading.Tasks;
using Aurix.Protocol;

namespace Aurix.Transport
{
    /// <summary>A verified downlink audio packet (one Opus frame from one remote participant).</summary>
    public struct IncomingAudio
    {
        public uint SenderSsrc;
        public uint Sequence;
        public uint Timestamp;
        public float Volume;
        public byte[] Opus;
    }

    /// <summary>
    /// Native AURX/UDP media transport (protocol v2). Every uplink packet is sealed with the
    /// per-session <see cref="MediaKeys"/> (AES-256-CTR payload + HMAC tag) and carries one
    /// monotonically increasing sequence number (the server keeps one anti-replay window per
    /// session, shared by audio, heartbeat and control packets). Downlink packets are sealed by
    /// the server with the same keys, opened here and replay-checked per remote SSRC before
    /// being exposed. Only the initial <c>SessionBind</c> is signed without encryption.
    /// </summary>
    public sealed class MediaTransport : IDisposable
    {
        private static readonly TimeSpan HeartbeatInterval = TimeSpan.FromSeconds(5);

        private readonly UdpClient _udp;
        private readonly IPEndPoint _server;
        private readonly MediaKeys _keys;
        private readonly uint _ssrc;
        private readonly Guid _sessionId;
        private readonly ConcurrentQueue<IncomingAudio> _audioInbox = new ConcurrentQueue<IncomingAudio>();
        private readonly ConcurrentDictionary<uint, ReplayWindow> _replayBySender = new ConcurrentDictionary<uint, ReplayWindow>();
        private readonly object _seqLock = new object();
        private uint _seq;
        private CancellationTokenSource _cts;
        private Task _recvLoop;
        private Task _heartbeatLoop;
        private long _packetsSent, _packetsReceived, _packetsBadAuth, _packetsReplayed;
        private volatile int _lastHeartbeatAckTs;
        private long _lastAckUnixMs;

        public long PacketsSent => Interlocked.Read(ref _packetsSent);
        public long PacketsReceived => Interlocked.Read(ref _packetsReceived);
        public long PacketsBadAuth => Interlocked.Read(ref _packetsBadAuth);
        public long PacketsReplayed => Interlocked.Read(ref _packetsReplayed);
        /// <summary>Round-trip time of the last heartbeat, in milliseconds (0 until the first ack).</summary>
        public float LastRttMs { get; private set; }
        public long HeartbeatAcks => Interlocked.Read(ref _heartbeatAcks);
        private long _heartbeatAcks;
        /// <summary>True when a HeartbeatAck arrived within the last 3 intervals.</summary>
        public bool IsAlive => DateTimeOffset.UtcNow.ToUnixTimeMilliseconds() - Interlocked.Read(ref _lastAckUnixMs) < HeartbeatInterval.TotalMilliseconds * 3;

        public MediaTransport(IPEndPoint server, Guid sessionId, uint ssrc, byte[] mediaKey)
        {
            _server = server ?? throw new ArgumentNullException(nameof(server));
            _sessionId = sessionId;
            _ssrc = ssrc;
            if (mediaKey == null) throw new ArgumentNullException(nameof(mediaKey));
            _keys = MediaKeys.Derive(mediaKey);
            _udp = new UdpClient(server.AddressFamily);
            _udp.Client.Bind(new IPEndPoint(server.AddressFamily == AddressFamily.InterNetworkV6 ? IPAddress.IPv6Any : IPAddress.Any, 0));
            _udp.Client.ReceiveBufferSize = 1 << 20;
        }

        /// <summary>Resolve <c>host:port</c> as delivered in <c>SessionInitAck.media_addr</c>.</summary>
        public static async Task<IPEndPoint> ResolveAsync(string mediaAddr)
        {
            int colon = mediaAddr.LastIndexOf(':');
            if (colon <= 0) throw new FormatException("media_addr must be host:port");
            var host = mediaAddr.Substring(0, colon).Trim('[', ']');
            var port = int.Parse(mediaAddr.Substring(colon + 1));
            if (IPAddress.TryParse(host, out var ip)) return new IPEndPoint(ip, port);
            var addrs = await Dns.GetHostAddressesAsync(host).ConfigureAwait(false);
            foreach (var a in addrs) if (a.AddressFamily == AddressFamily.InterNetwork) return new IPEndPoint(a, port);
            if (addrs.Length == 0) throw new SocketException((int)SocketError.HostNotFound);
            return new IPEndPoint(addrs[0], port);
        }

        /// <summary>
        /// Authenticate this UDP source with the server (<c>SessionBind</c> → <c>SessionBindAck</c>),
        /// retrying a few times. Starts the receive and heartbeat loops on success.
        /// </summary>
        public async Task BindAsync(CancellationToken ct, int attempts = 5, int timeoutMs = 500)
        {
            var nonce = new byte[8];
            using (var rng = RandomNumberGenerator.Create()) rng.GetBytes(nonce);
            var bind = AurxPacket.SessionBind(_sessionId, _ssrc, DateTimeOffset.UtcNow.ToUnixTimeMilliseconds(), BitConverter.ToUInt64(nonce, 0));
            var wire = bind.EncodeAuthenticated(_keys);
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

        private uint NextSeq()
        {
            lock (_seqLock) return ++_seq;
        }

        /// <summary>Send one encoded Opus frame. <paramref name="rtpTimestamp"/> advances by 960 per 20 ms at 48 kHz.</summary>
        public void SendAudio(uint channelHash, uint rtpTimestamp, byte[] opusFrame, int length = -1)
        {
            if (length < 0) length = opusFrame.Length;
            var payload = length == opusFrame.Length ? opusFrame : Slice(opusFrame, length);
            if (AurxPacket.HeaderSize + payload.Length + AurxPacket.AuthTagSize > AurxPacket.MaxPacketSize)
                throw new ArgumentException("Opus frame too large for one AURX packet");
            Send(AurxPacket.Audio(NextSeq(), rtpTimestamp, _ssrc, channelHash, payload));
        }

        public void SendMuteState(bool muted) => Send(AurxPacket.MuteState(NextSeq(), _ssrc, muted));

        public void SendQualityReport(float rttMs, float jitterMs, float lossPercent) =>
            Send(AurxPacket.QualityReport(NextSeq(), _ssrc, rttMs, jitterMs, lossPercent));

        private void Send(AurxPacket packet)
        {
            var wire = packet.Seal(_keys);
            try
            {
                _udp.Send(wire, wire.Length, _server);
                Interlocked.Increment(ref _packetsSent);
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
                    Send(AurxPacket.Heartbeat(NextSeq(), _ssrc, ts));
                }
            }
            catch (OperationCanceledException) { }
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
                if (!pkt.IsEncrypted || !pkt.Open(_keys))
                {
                    Interlocked.Increment(ref _packetsBadAuth);
                    continue;
                }
                switch (pkt.Header.Type)
                {
                    case PacketType.Audio:
                    case PacketType.AudioFec:
                        var window = _replayBySender.GetOrAdd(pkt.Header.Ssrc, _ => new ReplayWindow());
                        bool fresh;
                        lock (window) fresh = window.CheckAndUpdate(pkt.Header.Sequence);
                        if (!fresh) { Interlocked.Increment(ref _packetsReplayed); continue; }
                        Interlocked.Increment(ref _packetsReceived);
                        _audioInbox.Enqueue(new IncomingAudio
                        {
                            SenderSsrc = pkt.Header.Ssrc,
                            Sequence = pkt.Header.Sequence,
                            Timestamp = pkt.Header.Timestamp,
                            Volume = pkt.Volume,
                            Opus = pkt.AudioPayload.ToArray(),
                        });
                        break;
                    case PacketType.HeartbeatAck:
                        Interlocked.Exchange(ref _lastAckUnixMs, DateTimeOffset.UtcNow.ToUnixTimeMilliseconds());
                        Interlocked.Increment(ref _heartbeatAcks);
                        if ((int)pkt.Header.Timestamp == _lastHeartbeatAckTs)
                            LastRttMs = Math.Max(0, unchecked((int)((uint)Environment.TickCount - pkt.Header.Timestamp)));
                        break;
                    default:
                        break;
                }
            }
        }

        /// <summary>Forget the replay window of a participant that left, so a rejoin with a reset sequence is accepted.</summary>
        public void ForgetSender(uint ssrc) => _replayBySender.TryRemove(ssrc, out _);

        private static byte[] Slice(byte[] src, int len)
        {
            var dst = new byte[len];
            Buffer.BlockCopy(src, 0, dst, 0, len);
            return dst;
        }

        public void Dispose()
        {
            _cts?.Cancel();
            _udp.Dispose();
            _cts?.Dispose();
            _keys.Dispose();
        }
    }
}
