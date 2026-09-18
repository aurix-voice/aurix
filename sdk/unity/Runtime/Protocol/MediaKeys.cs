using System;
using System.Buffers.Binary;
using System.Security.Cryptography;

namespace Aurix.Protocol
{
    /// <summary>
    /// AURX v2 per-session keys derived from the 32-byte master media key delivered in
    /// <c>SessionInitAck</c> (mirrors <c>MediaKeys</c> in crates/aurix-common/src/crypto.rs):
    /// <code>
    /// auth = HMAC-SHA256(master, "AURXv2 auth")        -- packet tag (truncated to 16 bytes)
    /// enc  = HMAC-SHA256(master, "AURXv2 enc")         -- AES-256-CTR payload key
    /// salt = HMAC-SHA256(master, "AURXv2 salt")[0..16] -- XORed into the per-packet IV
    /// </code>
    /// The IV is <c>salt XOR (type | 0 | ssrc | seq | ts | 0x0000)</c>; the last 16 bits are the
    /// CTR block counter, so (type, ssrc, sequence, timestamp) must never repeat under one key —
    /// a single monotonic sequence counter per session guarantees that.
    /// </summary>
    public sealed class MediaKeys : IDisposable
    {
        public const int IvSize = 16;

        private readonly byte[] _auth;
        private readonly byte[] _salt;
        private readonly Aes _aes;
        private readonly ICryptoTransform _ecb;
        private readonly object _lock = new object();
        private bool _disposed;

        private MediaKeys(byte[] auth, byte[] enc, byte[] salt)
        {
            _auth = auth;
            _salt = salt;
            _aes = Aes.Create();
            _aes.Mode = CipherMode.ECB;
            _aes.Padding = PaddingMode.None;
            _aes.Key = enc;
            _ecb = _aes.CreateEncryptor();
        }

        public static MediaKeys Derive(byte[] master)
        {
            if (master == null || master.Length == 0) throw new ArgumentException("master key required", nameof(master));
            var auth = Label(master, "AURXv2 auth");
            var enc = Label(master, "AURXv2 enc");
            var saltFull = Label(master, "AURXv2 salt");
            var salt = new byte[16];
            Buffer.BlockCopy(saltFull, 0, salt, 0, 16);
            return new MediaKeys(auth, enc, salt);
        }

        private static byte[] Label(byte[] master, string label)
        {
            using (var h = new HMACSHA256(master))
                return h.ComputeHash(System.Text.Encoding.ASCII.GetBytes(label));
        }

        /// <summary>Truncated HMAC-SHA256 over <paramref name="buf"/>[0..len) with the auth key.</summary>
        public void ComputeTag(byte[] buf, int len, Span<byte> tag)
        {
            using (var h = new HMACSHA256(_auth))
            {
                var full = h.ComputeHash(buf, 0, len);
                new ReadOnlySpan<byte>(full, 0, AurxPacket.AuthTagSize).CopyTo(tag);
            }
        }

        public void Iv(byte packetType, uint ssrc, uint sequence, uint timestamp, Span<byte> iv)
        {
            iv.Clear();
            iv[0] = packetType;
            BinaryPrimitives.WriteUInt32BigEndian(iv.Slice(2), ssrc);
            BinaryPrimitives.WriteUInt32BigEndian(iv.Slice(6), sequence);
            BinaryPrimitives.WriteUInt32BigEndian(iv.Slice(10), timestamp);
            for (int i = 0; i < 16; i++) iv[i] ^= _salt[i];
        }

        /// <summary>
        /// AES-256-CTR keystream XOR over <paramref name="data"/>[offset..offset+len) (encrypt and
        /// decrypt are the same operation). Counter is the full 128-bit big-endian block.
        /// </summary>
        public void ApplyCtr(ReadOnlySpan<byte> iv, byte[] data, int offset, int len)
        {
            if (len == 0) return;
            if (_disposed) throw new ObjectDisposedException(nameof(MediaKeys));
            int blocks = (len + 15) / 16;
            var counters = new byte[blocks * 16];
            var block = new byte[16];
            iv.CopyTo(block);
            for (int b = 0; b < blocks; b++)
            {
                Buffer.BlockCopy(block, 0, counters, b * 16, 16);
                for (int i = 15; i >= 0; i--)
                {
                    if (++block[i] != 0) break;
                }
            }
            var stream = new byte[blocks * 16];
            lock (_lock)
            {
                _ecb.TransformBlock(counters, 0, counters.Length, stream, 0);
            }
            for (int i = 0; i < len; i++) data[offset + i] ^= stream[i];
        }

        public void Dispose()
        {
            if (_disposed) return;
            _disposed = true;
            _ecb.Dispose();
            _aes.Dispose();
            Array.Clear(_auth, 0, _auth.Length);
        }
    }
}
