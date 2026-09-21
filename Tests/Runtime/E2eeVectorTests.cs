using System;
using System.Text;
using Aurix.Protocol;
using NUnit.Framework;

namespace Aurix.Voice.Tests
{
    /// <summary>Shared Rust/TypeScript/C# E2EE test vectors — proves the managed crypto in this player build.</summary>
    public class E2eeVectorTests
    {
        [Test]
        public void HkdfMatchesRfc5869Case1()
        {
            var ikm = new byte[22];
            for (int i = 0; i < ikm.Length; i++) ikm[i] = 0x0b;
            var salt = new byte[13];
            for (int i = 0; i < salt.Length; i++) salt[i] = (byte)i;
            var info = new byte[10];
            for (int i = 0; i < info.Length; i++) info[i] = (byte)(0xf0 + i);
            Assert.AreEqual(
                "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865",
                Hex(E2ee.HkdfSha256(salt, ikm, info, 42)));
        }

        [Test]
        public void X25519MatchesRfc7748()
        {
            var k = Bytes("a546e36bf0527c9d3b16154b82465edd62144c0ac1fc5a18506a2244ba449ac4");
            var u = Bytes("e6db6867583030db3594c1a424b15f7c726624ec26b3353b10a903a6d0ab1c4c");
            Assert.AreEqual("c3da55379de9c6908e94ea4df28d084f32eccf03491c71f754b4075577a28552", Hex(X25519.ScalarMult(k, u)));
        }

        [Test]
        public void SenderKeyFrameMatchesTheSharedVector()
        {
            var secret = new byte[32];
            for (int i = 0; i < 32; i++) secret[i] = (byte)i;
            using (var key = E2eeSenderKey.Derive(1, secret))
            {
                var plain = new byte[] { 0xf8, 0xff, 0xfe, 0x00, 0x01 };
                var frame = key.Seal(0x01020304, plain);
                Assert.AreEqual("0101020304eb1575b9e6b52709cbdab928367122", Hex(frame));
                CollectionAssert.AreEqual(plain, key.Open(frame));
            }
        }

        [Test]
        public void SenderKeyRejectsTamperedFrames()
        {
            var secret = new byte[32];
            for (int i = 0; i < 32; i++) secret[i] = 7;
            using (var key = E2eeSenderKey.Derive(3, secret))
            {
                var frame = key.Seal(42, Encoding.ASCII.GetBytes("opus frame"));
                Assert.AreEqual(10 + E2ee.FrameOverhead, frame.Length);
                frame[frame.Length - 1] ^= 0x01;
                Assert.IsNull(key.Open(frame));
            }
        }

        private static string Hex(byte[] b)
        {
            var sb = new StringBuilder(b.Length * 2);
            foreach (var x in b) sb.Append(x.ToString("x2"));
            return sb.ToString();
        }

        private static byte[] Bytes(string hex)
        {
            var b = new byte[hex.Length / 2];
            for (int i = 0; i < b.Length; i++) b[i] = Convert.ToByte(hex.Substring(i * 2, 2), 16);
            return b;
        }
    }
}
