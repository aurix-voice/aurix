using System;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.Threading;
using System.Threading.Tasks;
using Aurix.Audio;
using Aurix.Protocol;
using Aurix.Transport;
using Xunit;

namespace Aurix.Voice.Tests
{
    /// <summary>
    /// An <see cref="IMediaTunnel"/> that plays the node: it opens every uplink packet with the session
    /// keys, answers <c>SessionBind</c> and <c>Heartbeat</c>, and lets the test push sealed downlink audio.
    /// </summary>
    internal sealed class FakeTunnel : IMediaTunnel
    {
        private readonly MediaKeys _keys;
        public readonly ConcurrentQueue<AurxPacket> Uplink = new ConcurrentQueue<AurxPacket>();
        public int Capacity = int.MaxValue;
        public volatile bool AnswerBinds = true;
        public volatile bool AnswerHeartbeats = true;
        public int Queued;
        public int BindsSeen;

        public FakeTunnel(byte[] mediaKey) { _keys = MediaKeys.Derive(mediaKey); }

        public event Action<byte[]> MediaReceived;

        public bool TrySendMedia(byte[] wire)
        {
            if (Interlocked.Increment(ref Queued) > Capacity) { Interlocked.Decrement(ref Queued); return false; }
            Assert.True(AurxPacket.TryDecode(wire, out var pkt, out var err), err);
            Assert.True(pkt.IsAuthenticated, "uplink packets are always authenticated");
            if (pkt.Header.Type == PacketType.SessionBind)
            {
                Assert.True(pkt.VerifyAuth(_keys), "bind signature");
                Interlocked.Increment(ref BindsSeen);
                if (AnswerBinds) Deliver(new AurxPacket(PacketHeader.Create(PacketType.SessionBindAck, 0, 0, pkt.Header.Ssrc), Array.Empty<byte>()));
            }
            else
            {
                Assert.True(pkt.IsEncrypted && pkt.Open(_keys), $"uplink {pkt.Header.Type} must be sealed");
                if (pkt.Header.Type == PacketType.Heartbeat && AnswerHeartbeats)
                    Deliver(new AurxPacket(PacketHeader.Create(PacketType.HeartbeatAck, 0, pkt.Header.Timestamp, pkt.Header.Ssrc), Array.Empty<byte>()));
            }
            Uplink.Enqueue(pkt);
            return true;
        }

        /// <summary>Seal a downlink packet with the session keys and hand it to the transport, like the node does.</summary>
        public void Deliver(AurxPacket pkt) => MediaReceived?.Invoke(pkt.Seal(_keys));

        /// <summary>Hand raw bytes to the transport (garbage, foreign keys…).</summary>
        public void DeliverRaw(byte[] wire) => MediaReceived?.Invoke(wire);

        public void Drain() => Interlocked.Exchange(ref Queued, 0);
    }

    public class TunnelTransportTests
    {
        private static readonly byte[] Key = { 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32 };
        private static readonly Guid Session = Guid.Parse("11111111-2222-3333-4444-555555555555");
        private const uint Ssrc = 0x1234abcd;

        private static async Task<T> Eventually<T>(Func<T> probe, Func<T, bool> ok, int timeoutMs = 3000)
        {
            var deadline = DateTime.UtcNow.AddMilliseconds(timeoutMs);
            T last = probe();
            while (!ok(last) && DateTime.UtcNow < deadline) { await Task.Delay(10); last = probe(); }
            return last;
        }

        [Fact]
        public async Task BindsOverTheTunnelAndMovesSealedAudioBothWays()
        {
            var tunnel = new FakeTunnel(Key);
            using var media = MediaTransport.OverTunnel(tunnel, Session, Ssrc, Key);
            Assert.Equal(MediaPath.Tunnel, media.Path);
            await media.BindAsync(CancellationToken.None, attempts: 2, timeoutMs: 1000);
            Assert.Equal(1, tunnel.BindsSeen);
            Assert.True(tunnel.Uplink.TryDequeue(out var bind) && bind.Header.Type == PacketType.SessionBind);

            // Uplink: the exact same sealed AURX packet the UDP path would send, sequence 1, 2, 3…
            uint hash = AurxPacket.ChannelIdHash(Guid.NewGuid());
            media.SendAudio(hash, 960, new byte[] { 0xAA, 0xBB, 0xCC }, level: 30);
            media.SendAudio(hash, 1920, new byte[] { 0xDD });
            var first = await Eventually(() => { tunnel.Uplink.TryPeek(out var p); return p; }, p => p != null);
            Assert.Equal(PacketType.Audio, first.Header.Type);
            Assert.Equal(1u, first.Header.Sequence);
            Assert.Equal(hash, first.Header.ChannelIdHash);
            Assert.Equal((byte)30, first.TakeAudioLevel());
            Assert.Equal(new byte[] { 0xAA, 0xBB, 0xCC }, first.Payload);
            Assert.Equal(2u, media.CurrentSequence);
            Assert.Equal(2, media.PacketsSent);
            Assert.Equal(0, media.UplinkDropped);

            // Downlink: sealed by the node, opened + replay-checked here, exposed like UDP audio.
            var down = AurxPacket.Audio(7, 4800, 0x99, hash, new byte[] { 1, 2, 3, 4 });
            tunnel.Deliver(down);
            tunnel.Deliver(AurxPacket.Audio(7, 4800, 0x99, hash, new byte[] { 1, 2, 3, 4 })); // replay
            Assert.True(media.TryDequeueAudio(out var heard));
            Assert.Equal(0x99u, heard.SenderSsrc);
            Assert.Equal(7u, heard.Sequence);
            Assert.Equal(AudioCodec.Opus, heard.Codec);
            Assert.Equal(new byte[] { 1, 2, 3, 4 }, heard.Payload);
            Assert.False(media.TryDequeueAudio(out _));
            Assert.Equal(1, media.PacketsReceived);
            Assert.Equal(1, media.PacketsReplayed);

            // Garbage and foreign-key packets are counted, never surfaced.
            tunnel.DeliverRaw(new byte[] { 1, 2, 3 });
            using (var other = MediaKeys.Derive(new byte[32]))
                tunnel.DeliverRaw(AurxPacket.Audio(8, 5760, 0x99, hash, new byte[] { 9 }).Seal(other));
            Assert.False(media.TryDequeueAudio(out _));
            Assert.Equal(1, media.PacketsBadAuth);
        }

        [Fact]
        public async Task BindRetriesAndFailsWhenTheNodeNeverAnswers()
        {
            var tunnel = new FakeTunnel(Key) { AnswerBinds = false };
            using var media = MediaTransport.OverTunnel(tunnel, Session, Ssrc, Key);
            var sw = System.Diagnostics.Stopwatch.StartNew();
            await Assert.ThrowsAsync<TimeoutException>(() => media.BindAsync(CancellationToken.None, attempts: 3, timeoutMs: 100));
            Assert.Equal(3, tunnel.BindsSeen);
            Assert.InRange(sw.ElapsedMilliseconds, 250, 3000);
            // Each retry carries a strictly newer timestamp (the node rejects stale re-binds).
            var stamps = new List<uint>();
            while (tunnel.Uplink.TryDequeue(out var p)) stamps.Add(p.Header.Timestamp);
            for (int i = 1; i < stamps.Count; i++) Assert.True(stamps[i] > stamps[i - 1], "bind timestamps must increase");
        }

        [Fact]
        public async Task HeartbeatsMeasureRttAndCountConsecutiveLosses()
        {
            var tunnel = new FakeTunnel(Key);
            using var media = MediaTransport.OverTunnel(tunnel, Session, Ssrc, Key);
            media.HeartbeatInterval = TimeSpan.FromMilliseconds(60);
            await media.BindAsync(CancellationToken.None, 2, 1000);
            var acks = await Eventually(() => media.HeartbeatAcks, a => a >= 2);
            Assert.True(acks >= 2, $"acks={acks}");
            Assert.Equal(0, media.HeartbeatsLostConsecutive);
            Assert.True(media.IsAlive);
            Assert.True(media.Rtt.Samples >= 2);

            tunnel.AnswerHeartbeats = false;
            var lost = await Eventually(() => media.HeartbeatsLostConsecutive, l => l >= 3);
            Assert.True(lost >= 3, $"consecutive={lost}");
            Assert.True(media.HeartbeatsLost >= 3);

            tunnel.AnswerHeartbeats = true;
            var reset = await Eventually(() => media.HeartbeatsLostConsecutive, l => l == 0);
            Assert.Equal(0, reset);
            Assert.True(media.HeartbeatsLost >= 3, "the lifetime counter keeps the history");
        }

        [Fact]
        public async Task FullQueueDropsAreCountedNotBlocking()
        {
            var tunnel = new FakeTunnel(Key);
            using var media = MediaTransport.OverTunnel(tunnel, Session, Ssrc, Key);
            await media.BindAsync(CancellationToken.None, 2, 1000);
            tunnel.Drain();
            tunnel.Capacity = 2;
            for (int i = 0; i < 5; i++) media.SendAudio(1, (uint)(960 * i), new byte[] { (byte)i });
            Assert.Equal(2, media.PacketsSent);
            Assert.Equal(3, media.UplinkDropped);
            // Sequence numbers are consumed even for dropped packets (the node tolerates gaps, never repeats).
            Assert.Equal(5u, media.CurrentSequence);
        }

        [Fact]
        public async Task SharedSequenceCounterSurvivesAPathSwitch()
        {
            var seq = new SequenceCounter(41);
            var a = new FakeTunnel(Key);
            var first = MediaTransport.OverTunnel(a, Session, Ssrc, Key, seq);
            await first.BindAsync(CancellationToken.None, 2, 1000);
            first.SendAudio(1, 0, new byte[] { 1 });
            Assert.Equal(42u, first.CurrentSequence);
            first.Dispose();

            // The next transport (the other link, after a fallback or a re-probe) continues the count.
            var b = new FakeTunnel(Key);
            using var second = MediaTransport.OverTunnel(b, Session, Ssrc, Key, first.Sequence);
            await second.BindAsync(CancellationToken.None, 2, 1000);
            second.SendAudio(1, 960, new byte[] { 2 });
            var audio = await Eventually(() =>
            {
                foreach (var p in b.Uplink) if (p.Header.Type == PacketType.Audio) return p;
                return null;
            }, p => p != null);
            Assert.Equal(43u, audio.Header.Sequence);

            // Frames delivered to the disposed transport are ignored, not surfaced or counted.
            a.Deliver(AurxPacket.Audio(1, 0, 0x77, 1, new byte[] { 5 }));
            Assert.False(first.TryDequeueAudio(out _));
        }

        [Fact]
        public async Task ReclaimRebindsAnAlreadyBoundTunnel()
        {
            var tunnel = new FakeTunnel(Key);
            using var media = MediaTransport.OverTunnel(tunnel, Session, Ssrc, Key);
            await media.BindAsync(CancellationToken.None, 2, 1000);
            Assert.Equal(1, tunnel.BindsSeen);
            await media.ReclaimAsync(CancellationToken.None, attempts: 2, timeoutMs: 500);
            Assert.Equal(2, tunnel.BindsSeen);
            Assert.True(media.IsAlive);

            using var udp = new MediaTransport(new System.Net.IPEndPoint(System.Net.IPAddress.Loopback, 9), Session, Ssrc, Key);
            Assert.Equal(MediaPath.Udp, udp.Path);
            await Assert.ThrowsAsync<InvalidOperationException>(() => udp.ReclaimAsync(CancellationToken.None));
        }
    }
}
