using System;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using System.Net;
using System.Net.Security;
using System.Net.Sockets;
using System.Security.Authentication;
using System.Security.Cryptography;
using System.Security.Cryptography.X509Certificates;
using System.Threading;
using System.Threading.Tasks;
using Aurix.Protocol;
using Aurix.Transport;
using Xunit;

namespace Aurix.Voice.Tests
{
    /// <summary>
    /// A TLS listener that plays the node's media tunnel: self-signed certificate, ALPN <c>aurix-tunnel/1</c>,
    /// <c>u16 | packet</c> frames, answers <c>SessionBind</c> / <c>Heartbeat</c> like the SFU and lets tests
    /// push raw frames or sealed downlink packets.
    /// </summary>
    internal sealed class FakeTlsNode : IDisposable
    {
        private readonly TcpListener _listener;
        private readonly X509Certificate2 _cert;
        private readonly MediaKeys _keys;
        private readonly CancellationTokenSource _cts = new CancellationTokenSource();
        private readonly TaskCompletionSource<SslStream> _accepted = new TaskCompletionSource<SslStream>(TaskCreationOptions.RunContinuationsAsynchronously);
        public readonly ConcurrentQueue<AurxPacket> Uplink = new ConcurrentQueue<AurxPacket>();
        public volatile bool AnswerBinds = true;
        public volatile string AlpnOverride;
        public int BindsSeen;
        public volatile string ClientError;

        public FakeTlsNode(byte[] mediaKey)
        {
            _keys = MediaKeys.Derive(mediaKey);
            using (var ecdsa = ECDsa.Create(ECCurve.NamedCurves.nistP256))
            {
                var req = new CertificateRequest("CN=aurix-media", ecdsa, HashAlgorithmName.SHA256);
                var san = new SubjectAlternativeNameBuilder();
                san.AddDnsName("aurix-media");
                req.CertificateExtensions.Add(san.Build());
                using (var ephemeral = req.CreateSelfSigned(DateTimeOffset.UtcNow.AddDays(-1), DateTimeOffset.UtcNow.AddDays(1)))
                    _cert = new X509Certificate2(ephemeral.Export(X509ContentType.Pfx));
            }
            _listener = new TcpListener(IPAddress.Loopback, 0);
            _listener.Start();
            _ = Task.Run(AcceptLoop);
        }

        public IPEndPoint EndPoint => (IPEndPoint)_listener.LocalEndpoint;
        public byte[] Der => _cert.RawData;
        public string Fingerprint => TlsMediaTunnel.Fingerprint(_cert.RawData);
        public TlsTunnelInfo Info => new TlsTunnelInfo { Addrs = new[] { EndPoint.ToString() }, CertSha256 = Fingerprint, ServerName = "aurix-media" };
        public Task<SslStream> Accepted => _accepted.Task;

        private async Task AcceptLoop()
        {
            try
            {
                var tcp = await _listener.AcceptTcpClientAsync().ConfigureAwait(false);
                var ssl = new SslStream(tcp.GetStream(), false);
                await ssl.AuthenticateAsServerAsync(new SslServerAuthenticationOptions
                {
                    ServerCertificate = _cert,
                    ApplicationProtocols = new List<SslApplicationProtocol> { new SslApplicationProtocol(AlpnOverride ?? TlsMediaTunnel.Alpn) },
                    EnabledSslProtocols = SslProtocols.Tls13,
                    ClientCertificateRequired = false,
                }, _cts.Token).ConfigureAwait(false);
                _accepted.TrySetResult(ssl);
                await ServeAsync(ssl).ConfigureAwait(false);
            }
            catch (Exception e)
            {
                ClientError = e.Message;
                _accepted.TrySetException(e);
            }
        }

        private async Task ServeAsync(SslStream ssl)
        {
            var header = new byte[2];
            var body = new byte[AurxPacket.MaxPacketSize];
            while (!_cts.IsCancellationRequested)
            {
                if (!await ReadExact(ssl, header, 2).ConfigureAwait(false)) return;
                int len = (header[0] << 8) | header[1];
                if (len == 0 || len > AurxPacket.MaxPacketSize) { ssl.Dispose(); return; }
                if (!await ReadExact(ssl, body, len).ConfigureAwait(false)) return;
                var wire = new byte[len];
                Buffer.BlockCopy(body, 0, wire, 0, len);
                if (!AurxPacket.TryDecode(wire, out var pkt, out _)) continue;
                if (pkt.Header.Type == PacketType.SessionBind)
                {
                    if (!pkt.VerifyAuth(_keys)) continue;
                    Interlocked.Increment(ref BindsSeen);
                    if (AnswerBinds) await Deliver(new AurxPacket(PacketHeader.Create(PacketType.SessionBindAck, 0, 0, pkt.Header.Ssrc), Array.Empty<byte>())).ConfigureAwait(false);
                }
                else if (pkt.IsEncrypted && pkt.Open(_keys))
                {
                    if (pkt.Header.Type == PacketType.Heartbeat)
                        await Deliver(new AurxPacket(PacketHeader.Create(PacketType.HeartbeatAck, 0, pkt.Header.Timestamp, pkt.Header.Ssrc), Array.Empty<byte>())).ConfigureAwait(false);
                }
                Uplink.Enqueue(pkt);
            }
        }

        private async Task<bool> ReadExact(SslStream ssl, byte[] buf, int len)
        {
            int got = 0;
            while (got < len)
            {
                int n;
                try { n = await ssl.ReadAsync(buf, got, len - got, _cts.Token).ConfigureAwait(false); }
                catch (Exception) { return false; }
                if (n <= 0) return false;
                got += n;
            }
            return true;
        }

        private readonly SemaphoreSlim _write = new SemaphoreSlim(1, 1);

        public Task Deliver(AurxPacket pkt) => DeliverRaw(TlsMediaTunnel.EncodeFrame(pkt.Seal(_keys)));

        public async Task DeliverRaw(byte[] frame)
        {
            var ssl = await Accepted.ConfigureAwait(false);
            await _write.WaitAsync().ConfigureAwait(false);
            try
            {
                await ssl.WriteAsync(frame, 0, frame.Length).ConfigureAwait(false);
                await ssl.FlushAsync().ConfigureAwait(false);
            }
            finally { _write.Release(); }
        }

        public async Task CloseClient()
        {
            var ssl = await Accepted.ConfigureAwait(false);
            ssl.Dispose();
        }

        public void Dispose()
        {
            _cts.Cancel();
            _listener.Stop();
            _cert.Dispose();
        }
    }

    public class TlsTunnelTests
    {
        private static readonly byte[] Key = { 9, 8, 7, 6, 5, 4, 3, 2, 1, 0, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32 };
        private static readonly Guid Session = Guid.Parse("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
        private const uint Ssrc = 0x0badf00d;
        private static readonly TimeSpan Timeout = TimeSpan.FromSeconds(5);

        private static async Task<T> Eventually<T>(Func<T> probe, Func<T, bool> ok, int timeoutMs = 3000)
        {
            var deadline = DateTime.UtcNow.AddMilliseconds(timeoutMs);
            T last = probe();
            while (!ok(last) && DateTime.UtcNow < deadline) { await Task.Delay(10); last = probe(); }
            return last;
        }

        [Fact]
        public void FramesAreBoundedAndBigEndian()
        {
            Assert.Null(TlsMediaTunnel.EncodeFrame(Array.Empty<byte>()));
            Assert.Null(TlsMediaTunnel.EncodeFrame(new byte[AurxPacket.MaxPacketSize + 1]));
            var max = TlsMediaTunnel.EncodeFrame(new byte[AurxPacket.MaxPacketSize]);
            Assert.Equal(AurxPacket.MaxPacketSize + 2, max.Length);
            Assert.Equal(AurxPacket.MaxPacketSize >> 8, max[0]);
            Assert.Equal(AurxPacket.MaxPacketSize & 0xff, max[1]);
            var one = TlsMediaTunnel.EncodeFrame(new byte[] { 0x42 });
            Assert.Equal(new byte[] { 0, 1, 0x42 }, one);
        }

        [Fact]
        public void PinsAreHexSha256OfTheDerCertificate()
        {
            using var node = new FakeTlsNode(Key);
            using var cert = new X509Certificate2(node.Der);
            Assert.Equal(64, node.Fingerprint.Length);
            Assert.Equal(cert.GetCertHashString(HashAlgorithmName.SHA256).ToLowerInvariant(), node.Fingerprint);
            Assert.True(TlsMediaTunnel.PinMatches(cert, node.Fingerprint));
            Assert.True(TlsMediaTunnel.PinMatches(cert, node.Fingerprint.ToUpperInvariant()));
            Assert.False(TlsMediaTunnel.PinMatches(cert, new string('0', 64)));
            Assert.False(TlsMediaTunnel.PinMatches(null, node.Fingerprint));
            Assert.Throws<ArgumentException>(() => TlsMediaTunnel.PinMatches(cert, "abc"));
        }

        [Fact]
        public async Task BindsOverTlsAndMovesSealedAudioBothWays()
        {
            using var node = new FakeTlsNode(Key);
            var link = await TlsMediaTunnel.ConnectAsync(node.EndPoint, "aurix-media", node.Fingerprint, Timeout, CancellationToken.None);
            Assert.Equal(SslProtocols.Tls13, link.Protocol);
            using var media = MediaTransport.OverTls(link, Session, Ssrc, Key);
            media.HeartbeatInterval = TimeSpan.FromMilliseconds(100);
            Assert.Equal(MediaPath.Tls, media.Path);
            Assert.Same(link, media.TlsTunnel);
            Assert.NotNull(media.LocalEndPoint);
            Assert.False(media.IsClosed);

            await media.BindAsync(CancellationToken.None, 2, 3000);
            Assert.Equal(1, node.BindsSeen);

            var frame = new byte[160];
            for (int i = 0; i < frame.Length; i++) frame[i] = (byte)i;
            media.SendAudio(0x1111, 960, frame, level: 42);
            var pkt = await Eventually(() =>
            {
                foreach (var p in node.Uplink) if (p.Header.Type == PacketType.Audio) return p;
                return null;
            }, p => p != null);
            Assert.NotNull(pkt);
            Assert.Equal(Ssrc, pkt.Header.Ssrc);
            Assert.True(pkt.Header.Sequence >= 1, "sequence numbers start at 1");
            Assert.Equal(0x1111u, pkt.Header.ChannelIdHash);
            Assert.Equal((byte)42, pkt.TakeAudioLevel());
            Assert.Equal(frame, pkt.Payload);
            // Heartbeats share the sequence space and the sent counter with audio, so only the
            // audio count is exact; every sealed packet the node saw was counted as sent.
            var uplink = node.Uplink.Where(p => p.Header.Type != PacketType.SessionBind).ToArray();
            Assert.Equal(1, uplink.Count(p => p.Header.Type == PacketType.Audio));
            Assert.True(media.PacketsSent >= uplink.Length, "sent counter covers every delivered packet");
            for (int i = 1; i < uplink.Length; i++) Assert.True(uplink[i - 1].Header.Sequence < uplink[i].Header.Sequence, "sequence is strictly increasing");

            await node.Deliver(AurxPacket.Audio(1, 960, 0x2222, 0x1111, new byte[] { 1, 2, 3 }));
            await node.Deliver(AurxPacket.Audio(1, 960, 0x2222, 0x1111, new byte[] { 1, 2, 3 })); // replay
            var got = await Eventually(() => { media.TryDequeueAudio(out var a); return a; }, a => a.Payload != null);
            Assert.Equal(0x2222u, got.SenderSsrc);
            Assert.Equal(new byte[] { 1, 2, 3 }, got.Payload);
            Assert.False(media.TryDequeueAudio(out _));
            var replayed = await Eventually(() => media.PacketsReplayed, r => r == 1);
            Assert.Equal(1, replayed);

            var acks = await Eventually(() => media.HeartbeatAcks, n => n >= 2, 5000);
            Assert.True(acks >= 2, "heartbeats travel the tunnel and are acked");
            Assert.True(media.IsAlive);
        }

        [Fact]
        public async Task WrongPinIsRefusedAtTheHandshake()
        {
            using var node = new FakeTlsNode(Key);
            var wrong = new string('a', 64);
            await Assert.ThrowsAnyAsync<AuthenticationException>(() =>
                TlsMediaTunnel.ConnectAsync(node.EndPoint, "aurix-media", wrong, Timeout, CancellationToken.None));
        }

        [Fact]
        public async Task WrongAlpnIsRefused()
        {
            using var node = new FakeTlsNode(Key) { AlpnOverride = "h2" };
            await Assert.ThrowsAnyAsync<Exception>(() =>
                TlsMediaTunnel.ConnectAsync(node.EndPoint, "aurix-media", node.Fingerprint, Timeout, CancellationToken.None));
        }

        [Fact]
        public async Task MalformedFramesFromThePeerCloseTheLink()
        {
            foreach (var bad in new[] { new byte[] { 0, 0 }, new byte[] { 0xff, 0xff } })
            {
                using var node = new FakeTlsNode(Key);
                var link = await TlsMediaTunnel.ConnectAsync(node.EndPoint, "aurix-media", node.Fingerprint, Timeout, CancellationToken.None);
                string reason = null;
                link.Closed += r => Volatile.Write(ref reason, r);
                await node.DeliverRaw(bad);
                reason = await Eventually(() => Volatile.Read(ref reason), r => r != null);
                Assert.True(link.IsClosed);
                Assert.Contains("frame", reason);
                Assert.False(link.TrySendMedia(new byte[] { 1 }));
                link.Dispose();
            }
        }

        [Fact]
        public async Task TruncatedFrameAtEofIsNotDelivered()
        {
            using var node = new FakeTlsNode(Key);
            var link = await TlsMediaTunnel.ConnectAsync(node.EndPoint, "aurix-media", node.Fingerprint, Timeout, CancellationToken.None);
            int delivered = 0;
            link.MediaReceived += _ => Interlocked.Increment(ref delivered);
            await node.DeliverRaw(new byte[] { 0, 10, 1, 2, 3 });
            await node.CloseClient();
            await Eventually(() => link.IsClosed, c => c);
            Assert.Equal(0, delivered);
            Assert.Contains("EOF", link.CloseReason);
            link.Dispose();
        }

        [Fact]
        public async Task BindFailsFastWhenTheTunnelCloses()
        {
            using var node = new FakeTlsNode(Key) { AnswerBinds = false };
            var link = await TlsMediaTunnel.ConnectAsync(node.EndPoint, "aurix-media", node.Fingerprint, Timeout, CancellationToken.None);
            using var media = MediaTransport.OverTls(link, Session, Ssrc, Key);
            var bind = media.BindAsync(CancellationToken.None, 1, 10000);
            await Eventually(() => node.BindsSeen, n => n >= 1);
            await node.CloseClient();
            var started = DateTime.UtcNow;
            await Assert.ThrowsAsync<IOException>(() => bind);
            Assert.True(DateTime.UtcNow - started < TimeSpan.FromSeconds(5), "the bind does not wait for its whole timeout once the link is gone");
            Assert.True(media.IsClosed);
        }

        [Fact]
        public async Task DisposingTheTransportClosesTheOwnedTunnel()
        {
            using var node = new FakeTlsNode(Key);
            var link = await TlsMediaTunnel.ConnectAsync(node.EndPoint, "aurix-media", node.Fingerprint, Timeout, CancellationToken.None);
            var media = MediaTransport.OverTls(link, Session, Ssrc, Key);
            await media.BindAsync(CancellationToken.None, 2, 3000);
            media.Dispose();
            Assert.True(link.IsClosed);
            Assert.Equal("disposed", link.CloseReason);
        }

        [Fact]
        public async Task UplinkQueueIsBoundedAndOversizedPacketsAreDropped()
        {
            using var node = new FakeTlsNode(Key);
            var link = await TlsMediaTunnel.ConnectAsync(node.EndPoint, "aurix-media", node.Fingerprint, Timeout, CancellationToken.None);
            Assert.False(link.TrySendMedia(new byte[AurxPacket.MaxPacketSize + 1]));
            Assert.False(link.TrySendMedia(Array.Empty<byte>()));
            Assert.Equal(2, link.PacketsDropped);
            int accepted = 0;
            for (int i = 0; i < TlsMediaTunnel.SendQueueLength * 4; i++)
                if (link.TrySendMedia(new byte[] { 1, 2, 3 })) accepted++;
            Assert.True(accepted >= TlsMediaTunnel.SendQueueLength, "at least one queue's worth is accepted");
            Assert.Equal(accepted, link.PacketsSent);
            link.Dispose();
        }
    }
}
