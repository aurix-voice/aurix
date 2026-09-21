// Unity Test Framework (NUnit) tests: run from Window ▸ General ▸ Test Runner in Edit or Play mode after
// adding "com.aurix.voice" to the "testables" list of Packages/manifest.json. They are a small, pure-managed
// subset of the xunit suite in DotNet~/Aurix.Voice.Tests (which stays the reference and runs in CI).
using System;
using System.Text;
using Aurix.Protocol;
using NUnit.Framework;

namespace Aurix.Voice.Tests
{
    public class ProtocolWireTests
    {
        [Test]
        public void Crc32MatchesTheServerVector()
        {
            Assert.AreEqual(0x3610A686u, Crc32.Compute(Encoding.ASCII.GetBytes("hello")));
            Assert.AreEqual(0u, Crc32.Compute(ReadOnlySpan<byte>.Empty));
        }

        [Test]
        public void UuidBytesUseNetworkOrder()
        {
            var g = Guid.Parse("01a0afd2-71e8-77ca-a078-0a9e4242a953");
            var buf = new byte[16];
            UuidBytes.Write(g, buf);
            CollectionAssert.AreEqual(
                new byte[] { 0x01, 0xa0, 0xaf, 0xd2, 0x71, 0xe8, 0x77, 0xca, 0xa0, 0x78, 0x0a, 0x9e, 0x42, 0x42, 0xa9, 0x53 }, buf);
            Assert.AreEqual(g, UuidBytes.Read(buf));
        }

        [Test]
        public void AurxHeaderIs30BytesBigEndian()
        {
            var pkt = AurxPacket.Audio(0x01020304, 0x05060708, 0x090A0B0C, 0x0D0E0F10, new byte[] { 1, 2, 3 });
            var wire = pkt.Encode();
            Assert.AreEqual(AurxPacket.HeaderSize + 3, wire.Length);
            Assert.AreEqual((byte)'A', wire[0]);
            Assert.AreEqual((byte)'X', wire[3]);
            Assert.AreEqual(2, wire[4]);
            CollectionAssert.AreEqual(new byte[] { 0x01, 0x02, 0x03, 0x04 }, wire[8..12]);
            CollectionAssert.AreEqual(new byte[] { 0x0D, 0x0E, 0x0F, 0x10 }, wire[20..24]);
            CollectionAssert.AreEqual(new byte[] { 0x00, 0x03 }, wire[24..26]);
        }

        [Test]
        public void SealedPacketsRoundTripAndRejectTampering()
        {
            var key = Keys(7);
            var pkt = AurxPacket.Audio(10, 20, 30, 40, Encoding.ASCII.GetBytes("opus-frame"));
            var wire = pkt.Seal(key);
            Assert.AreEqual(AurxPacket.HeaderSize + 10 + AurxPacket.AuthTagSize, wire.Length);

            Assert.IsTrue(AurxPacket.TryDecode(wire, out var decoded, out var err), err);
            Assert.IsTrue(decoded.IsAuthenticated);
            Assert.IsTrue(decoded.IsEncrypted);
            Assert.IsTrue(decoded.Open(key));
            Assert.AreEqual("opus-frame", Encoding.ASCII.GetString(decoded.Payload));

            Assert.IsTrue(AurxPacket.TryDecode(wire, out var wrongKey, out _));
            Assert.IsFalse(wrongKey.Open(Keys(8)));

            var tampered = (byte[])wire.Clone();
            tampered[11] ^= 0x01;
            Assert.IsTrue(AurxPacket.TryDecode(tampered, out var d2, out _));
            Assert.IsFalse(d2.Open(key));
        }

        [Test]
        public void MiniJsonRoundTripsTheBridgeShapes()
        {
            var parsed = MiniJson.AsObject(MiniJson.Parse("{\"ok\":true,\"pending\":true,\"value\":{\"n\":1.5,\"s\":\"é\\u00e9\",\"a\":[1,null,false]}}"));
            Assert.AreEqual(true, parsed["ok"]);
            var value = MiniJson.AsObject(parsed["value"]);
            Assert.AreEqual(1.5, MiniJson.GetNumber(value, "n"));
            Assert.AreEqual("éé", value["s"]);
            var back = MiniJson.Serialize(parsed);
            var again = MiniJson.AsObject(MiniJson.Parse(back));
            Assert.AreEqual("éé", MiniJson.AsObject(again["value"])["s"]);
        }

        private static MediaKeys Keys(byte fill)
        {
            var key = new byte[32];
            for (int i = 0; i < key.Length; i++) key[i] = fill;
            return MediaKeys.Derive(key);
        }
    }
}
