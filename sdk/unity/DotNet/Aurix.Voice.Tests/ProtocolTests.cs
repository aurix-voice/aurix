using System;
using System.Text;
using Aurix.Audio;
using Aurix.Protocol;
using Xunit;

namespace Aurix.Voice.Tests
{
    public class ProtocolTests
    {
        [Fact]
        public void Crc32MatchesIeeeVector()
        {
            // Same value crc32fast::hash(b"hello") produces on the server.
            Assert.Equal(0x3610A686u, Crc32.Compute(Encoding.ASCII.GetBytes("hello")));
            Assert.Equal(0u, Crc32.Compute(ReadOnlySpan<byte>.Empty));
        }

        [Fact]
        public void UuidBytesUseNetworkOrder()
        {
            var g = Guid.Parse("01a0afd2-71e8-77ca-a078-0a9e4242a953");
            var buf = new byte[16];
            UuidBytes.Write(g, buf);
            Assert.Equal(new byte[] { 0x01, 0xa0, 0xaf, 0xd2, 0x71, 0xe8, 0x77, 0xca, 0xa0, 0x78, 0x0a, 0x9e, 0x42, 0x42, 0xa9, 0x53 }, buf);
            Assert.Equal(g, UuidBytes.Read(buf));
        }

        [Fact]
        public void HeaderLayoutIs30BytesBigEndian()
        {
            var pkt = AurxPacket.Audio(0x01020304, 0x05060708, 0x090A0B0C, 0x0D0E0F10, new byte[] { 1, 2, 3 });
            var wire = pkt.Encode();
            Assert.Equal(AurxPacket.HeaderSize + 3, wire.Length);
            Assert.Equal((byte)'A', wire[0]); Assert.Equal((byte)'X', wire[3]);
            Assert.Equal(1, wire[4]);
            Assert.Equal(0x01, wire[5]);
            Assert.Equal(new byte[] { 0x01, 0x02, 0x03, 0x04 }, wire[8..12]);
            Assert.Equal(new byte[] { 0x0D, 0x0E, 0x0F, 0x10 }, wire[20..24]);
            Assert.Equal(new byte[] { 0x00, 0x03 }, wire[24..26]);
        }

        [Fact]
        public void AuthenticatedRoundTripAndTamperDetection()
        {
            var key = new byte[32];
            for (int i = 0; i < key.Length; i++) key[i] = 7;
            var pkt = AurxPacket.Audio(10, 20, 30, 40, Encoding.ASCII.GetBytes("opus-frame"));
            var wire = pkt.EncodeAuthenticated(key);
            Assert.Equal(AurxPacket.HeaderSize + 10 + AurxPacket.AuthTagSize, wire.Length);

            Assert.True(AurxPacket.TryDecode(wire, out var decoded, out var err), err);
            Assert.True(decoded.IsAuthenticated);
            Assert.True(decoded.VerifyAuth(key));
            var other = new byte[32];
            for (int i = 0; i < other.Length; i++) other[i] = 8;
            Assert.False(decoded.VerifyAuth(other));

            // Flip a payload byte and fix the CRC so only the HMAC catches it.
            var tampered = (byte[])wire.Clone();
            tampered[AurxPacket.HeaderSize] ^= 0x01;
            var crc = Crc32.Compute(new ReadOnlySpan<byte>(tampered, AurxPacket.HeaderSize, 10));
            tampered[26] = (byte)(crc >> 24); tampered[27] = (byte)(crc >> 16); tampered[28] = (byte)(crc >> 8); tampered[29] = (byte)crc;
            Assert.True(AurxPacket.TryDecode(tampered, out var d2, out _));
            Assert.False(d2.VerifyAuth(key));
        }

        [Fact]
        public void DecodeRejectsTrailingBytesChecksumAndOversize()
        {
            var wire = AurxPacket.Audio(1, 2, 3, 4, new byte[] { 9, 9 }).Encode();
            Assert.True(AurxPacket.TryDecode(wire, out _, out _));
            var trailing = new byte[wire.Length + 1];
            wire.CopyTo(trailing, 0);
            Assert.False(AurxPacket.TryDecode(trailing, out _, out _));
            var bad = (byte[])wire.Clone();
            bad[AurxPacket.HeaderSize] ^= 0xFF;
            Assert.False(AurxPacket.TryDecode(bad, out _, out _));
            Assert.False(AurxPacket.TryDecode(new byte[AurxPacket.MaxPacketSize + 1], out _, out _));
        }

        [Fact]
        public void VolumeAttenuatedPayloadIsSplit()
        {
            var h = PacketHeader.Create(PacketType.Audio, 1, 0, 5);
            h.Flags |= PacketFlags.VolumeAttenuated;
            var pkt = new AurxPacket(h, new byte[] { 128, 0xAA, 0xBB });
            Assert.InRange(pkt.Volume, 0.5f, 0.51f);
            Assert.Equal(new byte[] { 0xAA, 0xBB }, pkt.AudioPayload.ToArray());
        }

        [Fact]
        public void SessionBindPayloadLayout()
        {
            var sid = Guid.Parse("00000000-0000-0000-0000-000000000001");
            var pkt = AurxPacket.SessionBind(sid, 42, 0x0102030405060708, 0xAABBCCDDEEFF0011);
            Assert.Equal(AurxPacket.SessionBindPayloadSize, pkt.Payload.Length);
            Assert.Equal(1, pkt.Payload[15]);
            Assert.Equal(0x01, pkt.Payload[16]); Assert.Equal(0x08, pkt.Payload[23]);
            Assert.Equal(0xAA, pkt.Payload[24]); Assert.Equal(0x11, pkt.Payload[31]);
        }

        [Fact]
        public void ReplayWindowBehavesLikeServer()
        {
            var w = new ReplayWindow();
            Assert.True(w.CheckAndUpdate(100));
            Assert.False(w.CheckAndUpdate(100));
            Assert.True(w.CheckAndUpdate(99));
            Assert.False(w.CheckAndUpdate(99));
            Assert.True(w.CheckAndUpdate(200));
            Assert.False(w.CheckAndUpdate(100)); // older than the 64-wide window
            Assert.True(w.CheckAndUpdate(150));
        }

        [Fact]
        public void ControlMessageRoundTrip()
        {
            var json = ControlMessage.ChannelJoin(Guid.Parse("11111111-2222-3333-4444-555555555555"), "tok");
            var m = ControlMessage.Parse(json);
            Assert.Equal("ChannelJoin", m.Type);
            Assert.Equal("tok", m.Str("token"));
            Assert.Equal(Guid.Parse("11111111-2222-3333-4444-555555555555"), m.Id("channel_id"));

            var ack = ControlMessage.Parse("{\"type\":\"ChannelJoinAck\",\"data\":{\"channel_id\":\"11111111-2222-3333-4444-555555555555\"," +
                "\"participants\":[{\"user_id\":\"11111111-2222-3333-4444-555555555556\",\"display_name\":\"al\\\"ice\",\"ssrc\":4294967295,\"role\":\"moderator\",\"is_muted\":true,\"is_speaking\":false}]}}");
            var ps = ack.Participants();
            Assert.Single(ps);
            Assert.Equal("al\"ice", ps[0].DisplayName);
            Assert.Equal(uint.MaxValue, ps[0].Ssrc);
            Assert.Equal(ChannelRole.Moderator, ps[0].Role);
            Assert.True(ps[0].IsMuted);
        }

        [Fact]
        public void JitterBufferReordersAndFlagsLoss()
        {
            var jb = new JitterBuffer(targetDepthFrames: 2);
            Assert.False(jb.Pop(out _));
            jb.Push(11, new byte[] { 11 });
            jb.Push(10, new byte[] { 10 });
            Assert.True(jb.Pop(out var f)); Assert.Equal(10, f[0]);
            Assert.True(jb.Pop(out f)); Assert.Equal(11, f[0]);
            Assert.False(jb.Pop(out _)); // waiting for 12
            jb.Push(13, new byte[] { 13 });
            Assert.True(jb.Pop(out f)); Assert.Null(f); // 12 lost → PLC
            Assert.True(jb.Pop(out f)); Assert.Equal(13, f[0]);
            Assert.Equal(1, jb.Lost);
        }
    }
}
