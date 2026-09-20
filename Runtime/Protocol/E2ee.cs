using System;
using System.Buffers.Binary;
using System.Collections.Generic;
using System.Security.Cryptography;
using System.Text;

namespace Aurix.Protocol
{
    /// <summary>
    /// Group end-to-end encryption of voice frames ("Aurix E2EE v1"), byte for byte the
    /// <c>aurix_common::e2ee</c> module of the native core and the Web SDK's <c>e2ee.ts</c>.
    /// Every client holds an X25519 identity key and a symmetric sender key its own frames are
    /// sealed with; the sender key is wrapped individually for every peer (X25519 → HKDF-SHA256 →
    /// AES-256-CTR + HMAC-SHA256) and relayed by the control plane, which cannot read it.
    /// <code>
    /// frame  = generation(1) | counter(4, BE) | AES-256-CTR(enc, IV, opus) | HMAC-SHA256(auth, header|ct)[..10]
    /// IV     = (salt(12) XOR (generation | counter | 0*7)) | 0*4
    /// enc/auth/salt = HKDF-SHA256(secret, "aurix-e2ee-v1 enc" / "… auth" / "… salt")
    /// wrap   = nonce(12) | AES-256-CTR(wk_enc, nonce|0*4, secret) | HMAC-SHA256(wk_auth, generation|nonce|ct)[..16]
    /// wk_*   = HKDF-SHA256(X25519(sender_sk, recipient_pk), salt = sender_pk|recipient_pk, "aurix-e2ee-v1 wrap enc" / "… wrap auth")
    /// </code>
    /// </summary>
    public static class E2ee
    {
        public const int Version = 1;
        public const int PublicKeyLen = 32;
        public const int SecretLen = 32;
        public const int FrameHeaderLen = 5;
        public const int FrameTagLen = 10;
        /// <summary>Bytes an encrypted frame is longer than the Opus frame inside it.</summary>
        public const int FrameOverhead = FrameHeaderLen + FrameTagLen;
        public const int WrapNonceLen = 12;
        public const int WrapTagLen = 16;
        public const int WrappedKeyLen = WrapNonceLen + SecretLen + WrapTagLen;
        /// <summary>Generations of one peer a receiver keeps decrypting.</summary>
        public const int KeptGenerations = 4;
        /// <summary>A sender rotates before its frame counter gets anywhere near wrapping.</summary>
        public const uint RotateAtCounter = 1u << 31;
        internal const uint ReplayWindow = 128;

        internal static readonly byte[] InfoEnc = Encoding.ASCII.GetBytes("aurix-e2ee-v1 enc");
        internal static readonly byte[] InfoAuth = Encoding.ASCII.GetBytes("aurix-e2ee-v1 auth");
        internal static readonly byte[] InfoSalt = Encoding.ASCII.GetBytes("aurix-e2ee-v1 salt");
        internal static readonly byte[] InfoWrapEnc = Encoding.ASCII.GetBytes("aurix-e2ee-v1 wrap enc");
        internal static readonly byte[] InfoWrapAuth = Encoding.ASCII.GetBytes("aurix-e2ee-v1 wrap auth");

        private static readonly RandomNumberGenerator Rng = RandomNumberGenerator.Create();

        internal static byte[] RandomBytes(int len)
        {
            var b = new byte[len];
            lock (Rng) Rng.GetBytes(b);
            return b;
        }

        /// <summary>HKDF-SHA256 (RFC 5869).</summary>
        public static byte[] HkdfSha256(byte[] salt, byte[] ikm, byte[] info, int len)
        {
            byte[] prk;
            using (var h = new HMACSHA256(salt ?? Array.Empty<byte>())) prk = h.ComputeHash(ikm);
            var okm = new byte[len];
            var prev = Array.Empty<byte>();
            int filled = 0;
            byte i = 1;
            using (var h = new HMACSHA256(prk))
            {
                while (filled < len)
                {
                    var block = new byte[prev.Length + info.Length + 1];
                    Buffer.BlockCopy(prev, 0, block, 0, prev.Length);
                    Buffer.BlockCopy(info, 0, block, prev.Length, info.Length);
                    block[block.Length - 1] = i++;
                    prev = h.ComputeHash(block);
                    int n = Math.Min(prev.Length, len - filled);
                    Buffer.BlockCopy(prev, 0, okm, filled, n);
                    filled += n;
                }
            }
            Array.Clear(prk, 0, prk.Length);
            return okm;
        }

        internal static byte[] HmacSha256(byte[] key, params byte[][] parts)
        {
            using (var h = new IncrementalHmac(key))
            {
                foreach (var p in parts) h.Append(p, 0, p.Length);
                return h.Finish();
            }
        }

        /// <summary>AES-256-CTR keystream XOR over <paramref name="data"/>[offset..offset+len); the whole 128-bit block is the counter.</summary>
        internal static void Aes256Ctr(ICryptoTransform ecb, byte[] iv, byte[] data, int offset, int len)
        {
            if (len == 0) return;
            int blocks = (len + 15) / 16;
            var counters = new byte[blocks * 16];
            var block = new byte[16];
            Buffer.BlockCopy(iv, 0, block, 0, 16);
            for (int b = 0; b < blocks; b++)
            {
                Buffer.BlockCopy(block, 0, counters, b * 16, 16);
                for (int i = 15; i >= 0; i--)
                    if (++block[i] != 0) break;
            }
            var stream = new byte[blocks * 16];
            ecb.TransformBlock(counters, 0, counters.Length, stream, 0);
            for (int i = 0; i < len; i++) data[offset + i] ^= stream[i];
            Array.Clear(stream, 0, stream.Length);
        }

        internal static ICryptoTransform EcbEncryptor(byte[] key, out Aes aes)
        {
            aes = Aes.Create();
            aes.Mode = CipherMode.ECB;
            aes.Padding = PaddingMode.None;
            aes.Key = key;
            return aes.CreateEncryptor();
        }

        /// <summary>Hex SHA-256 of an identity public key, the value users compare out of band.</summary>
        public static string Fingerprint(byte[] publicKey)
        {
            if (publicKey == null || publicKey.Length != PublicKeyLen) throw new ArgumentException("public key must be 32 bytes", nameof(publicKey));
            using (var sha = SHA256.Create()) return Hex(sha.ComputeHash(publicKey));
        }

        internal static string Hex(byte[] b)
        {
            var sb = new StringBuilder(b.Length * 2);
            foreach (var x in b) sb.Append(x.ToString("x2"));
            return sb.ToString();
        }

        /// <summary>Decodes a base64 identity public key from the wire, or null when malformed.</summary>
        public static byte[] ParsePublicKey(string b64)
        {
            var bytes = DecodeBytes(b64);
            return bytes != null && bytes.Length == PublicKeyLen ? bytes : null;
        }

        public static string EncodeBytes(byte[] bytes) => Convert.ToBase64String(bytes);

        public static byte[] DecodeBytes(string b64)
        {
            if (string.IsNullOrEmpty(b64)) return null;
            try { return Convert.FromBase64String(b64); }
            catch (FormatException) { return null; }
        }

        /// <summary>HMAC-SHA256 fed in parts (the frame path avoids concatenating buffers).</summary>
        internal sealed class IncrementalHmac : IDisposable
        {
            private readonly HMACSHA256 _h;
            private static readonly byte[] Empty = Array.Empty<byte>();

            public IncrementalHmac(byte[] key) { _h = new HMACSHA256(key); }
            public void Append(byte[] data, int offset, int len) => _h.TransformBlock(data, offset, len, null, 0);
            public byte[] Finish() { _h.TransformFinalBlock(Empty, 0, 0); return _h.Hash; }
            public void Dispose() => _h.Dispose();
        }
    }

    /// <summary>X25519 (RFC 7748) scalar multiplication; constant-time TweetNaCl arithmetic.</summary>
    public static class X25519
    {
        private static readonly long[] Const121665 = { 0xDB41, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0 };
        private static readonly byte[] BasePoint = { 9, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0 };

        /// <summary>Public key of a 32-byte secret scalar (clamped as the RFC prescribes).</summary>
        public static byte[] PublicKey(byte[] secret) => ScalarMult(secret, BasePoint);

        /// <summary>X25519(<paramref name="scalar"/>, <paramref name="point"/>); the all-zero output marks a low-order point.</summary>
        public static byte[] ScalarMult(byte[] scalar, byte[] point)
        {
            if (scalar == null || scalar.Length != 32) throw new ArgumentException("scalar must be 32 bytes", nameof(scalar));
            if (point == null || point.Length != 32) throw new ArgumentException("point must be 32 bytes", nameof(point));
            var z = new byte[32];
            Buffer.BlockCopy(scalar, 0, z, 0, 32);
            z[31] = (byte)((scalar[31] & 127) | 64);
            z[0] &= 248;
            var x = new long[16];
            Unpack(x, point);
            long[] a = new long[16], b = new long[16], c = new long[16], d = new long[16], e = new long[16], f = new long[16];
            for (int i = 0; i < 16; i++) b[i] = x[i];
            a[0] = 1;
            d[0] = 1;
            for (int i = 254; i >= 0; i--)
            {
                long r = (z[i >> 3] >> (i & 7)) & 1;
                Select(a, b, r);
                Select(c, d, r);
                Add(e, a, c);
                Sub(a, a, c);
                Add(c, b, d);
                Sub(b, b, d);
                Mul(d, e, e);
                Mul(f, a, a);
                Mul(a, c, a);
                Mul(c, b, e);
                Add(e, a, c);
                Sub(a, a, c);
                Mul(b, a, a);
                Sub(c, d, f);
                Mul(a, c, Const121665);
                Add(a, a, d);
                Mul(c, c, a);
                Mul(a, d, f);
                Mul(d, b, x);
                Mul(b, e, e);
                Select(a, b, r);
                Select(c, d, r);
            }
            Invert(c, c);
            Mul(a, a, c);
            var q = new byte[32];
            Pack(q, a);
            Array.Clear(z, 0, z.Length);
            return q;
        }

        /// <summary>True when every byte of <paramref name="shared"/> is zero (a low-order peer key).</summary>
        public static bool IsZero(byte[] shared)
        {
            int acc = 0;
            foreach (var b in shared) acc |= b;
            return acc == 0;
        }

        private static void Carry(long[] o)
        {
            for (int i = 0; i < 16; i++)
            {
                o[i] += 1L << 16;
                long c = o[i] >> 16;
                o[(i + 1) * (i < 15 ? 1 : 0)] += c - 1 + 37 * (c - 1) * (i == 15 ? 1 : 0);
                o[i] -= c << 16;
            }
        }

        private static void Select(long[] p, long[] q, long b)
        {
            long c = ~(b - 1);
            for (int i = 0; i < 16; i++)
            {
                long t = c & (p[i] ^ q[i]);
                p[i] ^= t;
                q[i] ^= t;
            }
        }

        private static void Pack(byte[] o, long[] n)
        {
            var m = new long[16];
            var t = new long[16];
            for (int i = 0; i < 16; i++) t[i] = n[i];
            Carry(t);
            Carry(t);
            Carry(t);
            for (int j = 0; j < 2; j++)
            {
                m[0] = t[0] - 0xffed;
                for (int i = 1; i < 15; i++)
                {
                    m[i] = t[i] - 0xffff - ((m[i - 1] >> 16) & 1);
                    m[i - 1] &= 0xffff;
                }
                m[15] = t[15] - 0x7fff - ((m[14] >> 16) & 1);
                long b = (m[15] >> 16) & 1;
                m[14] &= 0xffff;
                Select(t, m, 1 - b);
            }
            for (int i = 0; i < 16; i++)
            {
                o[2 * i] = (byte)(t[i] & 0xff);
                o[2 * i + 1] = (byte)(t[i] >> 8);
            }
        }

        private static void Unpack(long[] o, byte[] n)
        {
            for (int i = 0; i < 16; i++) o[i] = n[2 * i] + ((long)n[2 * i + 1] << 8);
            o[15] &= 0x7fff;
        }

        private static void Add(long[] o, long[] a, long[] b) { for (int i = 0; i < 16; i++) o[i] = a[i] + b[i]; }
        private static void Sub(long[] o, long[] a, long[] b) { for (int i = 0; i < 16; i++) o[i] = a[i] - b[i]; }

        private static void Mul(long[] o, long[] a, long[] b)
        {
            var t = new long[31];
            for (int i = 0; i < 16; i++)
                for (int j = 0; j < 16; j++)
                    t[i + j] += a[i] * b[j];
            for (int i = 0; i < 15; i++) t[i] += 38 * t[i + 16];
            for (int i = 0; i < 16; i++) o[i] = t[i];
            Carry(o);
            Carry(o);
        }

        private static void Invert(long[] o, long[] i)
        {
            var c = new long[16];
            for (int a = 0; a < 16; a++) c[a] = i[a];
            for (int a = 253; a >= 0; a--)
            {
                Mul(c, c, c);
                if (a != 2 && a != 4) Mul(c, c, i);
            }
            for (int a = 0; a < 16; a++) o[a] = c[a];
        }
    }

    /// <summary>Long-lived X25519 key of one client (see <see cref="E2ee"/>).</summary>
    public sealed class E2eeIdentityKey
    {
        private readonly byte[] _secret;
        private readonly byte[] _public;

        private E2eeIdentityKey(byte[] secret)
        {
            _secret = secret;
            _public = X25519.PublicKey(secret);
            Fingerprint = E2ee.Fingerprint(_public);
        }

        public static E2eeIdentityKey Generate() => new E2eeIdentityKey(E2ee.RandomBytes(32));

        /// <summary>Import a 32-byte secret previously returned by <see cref="ExportSecret"/> (keeps the fingerprint across sessions).</summary>
        public static E2eeIdentityKey FromBytes(byte[] secret)
        {
            if (secret == null || secret.Length != E2ee.SecretLen) throw new ArgumentException("identity secret must be 32 bytes", nameof(secret));
            return new E2eeIdentityKey((byte[])secret.Clone());
        }

        /// <summary>The 32-byte X25519 public key (a copy).</summary>
        public byte[] PublicKey => (byte[])_public.Clone();
        /// <summary>Hex SHA-256 of the public key.</summary>
        public string Fingerprint { get; }
        public byte[] ExportSecret() => (byte[])_secret.Clone();

        internal bool PublicKeyEquals(byte[] pk) => CryptographicOperations.FixedTimeEquals(_public, pk);

        /// <summary>Wrapping keys shared with <paramref name="peer"/>; sender/recipient fix the direction.</summary>
        internal void WrapKeys(byte[] peer, byte[] senderPk, byte[] recipientPk, out byte[] enc, out byte[] auth)
        {
            var shared = X25519.ScalarMult(_secret, peer);
            if (X25519.IsZero(shared)) throw new CryptographicException("low-order X25519 public key");
            var salt = new byte[64];
            Buffer.BlockCopy(senderPk, 0, salt, 0, 32);
            Buffer.BlockCopy(recipientPk, 0, salt, 32, 32);
            enc = E2ee.HkdfSha256(salt, shared, E2ee.InfoWrapEnc, 32);
            auth = E2ee.HkdfSha256(salt, shared, E2ee.InfoWrapAuth, 32);
            Array.Clear(shared, 0, shared.Length);
        }

        /// <summary>Seals <paramref name="secret"/> (generation <paramref name="generation"/>) for the peer holding <paramref name="recipient"/>.</summary>
        public byte[] Wrap(byte[] recipient, byte generation, byte[] secret) => Wrap(recipient, generation, secret, E2ee.RandomBytes(E2ee.WrapNonceLen));

        internal byte[] Wrap(byte[] recipient, byte generation, byte[] secret, byte[] nonce)
        {
            if (recipient == null || recipient.Length != E2ee.PublicKeyLen) throw new ArgumentException("recipient key must be 32 bytes", nameof(recipient));
            if (secret == null || secret.Length != E2ee.SecretLen) throw new ArgumentException("secret must be 32 bytes", nameof(secret));
            WrapKeys(recipient, _public, recipient, out var enc, out var auth);
            var iv = new byte[16];
            Buffer.BlockCopy(nonce, 0, iv, 0, E2ee.WrapNonceLen);
            var ct = (byte[])secret.Clone();
            using (var ecb = E2ee.EcbEncryptor(enc, out var aes))
            using (aes)
                E2ee.Aes256Ctr(ecb, iv, ct, 0, ct.Length);
            var tag = E2ee.HmacSha256(auth, new[] { generation }, nonce, ct);
            var outBuf = new byte[E2ee.WrappedKeyLen];
            Buffer.BlockCopy(nonce, 0, outBuf, 0, E2ee.WrapNonceLen);
            Buffer.BlockCopy(ct, 0, outBuf, E2ee.WrapNonceLen, E2ee.SecretLen);
            Buffer.BlockCopy(tag, 0, outBuf, E2ee.WrapNonceLen + E2ee.SecretLen, E2ee.WrapTagLen);
            Array.Clear(enc, 0, enc.Length);
            Array.Clear(auth, 0, auth.Length);
            return outBuf;
        }

        /// <summary>Opens a key wrapped by the peer holding <paramref name="sender"/> for us; throws <see cref="CryptographicException"/> otherwise.</summary>
        public byte[] Unwrap(byte[] sender, byte generation, byte[] wrapped)
        {
            if (sender == null || sender.Length != E2ee.PublicKeyLen) throw new ArgumentException("sender key must be 32 bytes", nameof(sender));
            if (wrapped == null || wrapped.Length != E2ee.WrappedKeyLen) throw new CryptographicException("wrapped key has a wrong length");
            WrapKeys(sender, sender, _public, out var enc, out var auth);
            var nonce = new byte[E2ee.WrapNonceLen];
            var ct = new byte[E2ee.SecretLen];
            Buffer.BlockCopy(wrapped, 0, nonce, 0, E2ee.WrapNonceLen);
            Buffer.BlockCopy(wrapped, E2ee.WrapNonceLen, ct, 0, E2ee.SecretLen);
            var expected = E2ee.HmacSha256(auth, new[] { generation }, nonce, ct);
            var ok = CryptographicOperations.FixedTimeEquals(
                new ReadOnlySpan<byte>(expected, 0, E2ee.WrapTagLen),
                new ReadOnlySpan<byte>(wrapped, E2ee.WrapNonceLen + E2ee.SecretLen, E2ee.WrapTagLen));
            Array.Clear(auth, 0, auth.Length);
            if (!ok)
            {
                Array.Clear(enc, 0, enc.Length);
                throw new CryptographicException("wrapped key failed authentication");
            }
            var iv = new byte[16];
            Buffer.BlockCopy(nonce, 0, iv, 0, E2ee.WrapNonceLen);
            using (var ecb = E2ee.EcbEncryptor(enc, out var aes))
            using (aes)
                E2ee.Aes256Ctr(ecb, iv, ct, 0, ct.Length);
            Array.Clear(enc, 0, enc.Length);
            return ct;
        }
    }

    /// <summary>Frame keys of one sender generation.</summary>
    public sealed class E2eeSenderKey : IDisposable
    {
        private readonly byte[] _auth;
        private readonly byte[] _salt;
        private readonly Aes _aes;
        private readonly ICryptoTransform _ecb;
        private readonly object _lock = new object();
        internal readonly byte[] EncForTests;

        public byte Generation { get; }
        internal byte[] Auth => _auth;
        internal byte[] Salt => _salt;

        private E2eeSenderKey(byte generation, byte[] enc, byte[] auth, byte[] salt)
        {
            Generation = generation;
            _auth = auth;
            _salt = salt;
            EncForTests = enc;
            _ecb = E2ee.EcbEncryptor(enc, out _aes);
        }

        public static E2eeSenderKey Derive(byte generation, byte[] secret)
        {
            if (secret == null || secret.Length != E2ee.SecretLen) throw new ArgumentException("secret must be 32 bytes", nameof(secret));
            return new E2eeSenderKey(
                generation,
                E2ee.HkdfSha256(null, secret, E2ee.InfoEnc, 32),
                E2ee.HkdfSha256(null, secret, E2ee.InfoAuth, 32),
                E2ee.HkdfSha256(null, secret, E2ee.InfoSalt, 12));
        }

        private byte[] Iv(uint counter)
        {
            var iv = new byte[16];
            iv[0] = Generation;
            BinaryPrimitives.WriteUInt32BigEndian(new Span<byte>(iv, 1, 4), counter);
            for (int i = 0; i < 12; i++) iv[i] ^= _salt[i];
            return iv;
        }

        /// <summary>Encrypts <paramref name="plain"/>[offset..offset+len) as frame number <paramref name="counter"/> of this generation.</summary>
        public byte[] Seal(uint counter, byte[] plain, int offset = 0, int len = -1)
        {
            if (len < 0) len = plain.Length - offset;
            var frame = new byte[len + E2ee.FrameOverhead];
            frame[0] = Generation;
            BinaryPrimitives.WriteUInt32BigEndian(new Span<byte>(frame, 1, 4), counter);
            Buffer.BlockCopy(plain, offset, frame, E2ee.FrameHeaderLen, len);
            lock (_lock) E2ee.Aes256Ctr(_ecb, Iv(counter), frame, E2ee.FrameHeaderLen, len);
            using (var h = new E2ee.IncrementalHmac(_auth))
            {
                h.Append(frame, 0, E2ee.FrameHeaderLen + len);
                var tag = h.Finish();
                Buffer.BlockCopy(tag, 0, frame, E2ee.FrameHeaderLen + len, E2ee.FrameTagLen);
            }
            return frame;
        }

        /// <summary>Generation byte and counter of an encrypted frame (no authentication); false when too short.</summary>
        public static bool Peek(byte[] frame, int offset, int len, out byte generation, out uint counter)
        {
            generation = 0;
            counter = 0;
            if (frame == null || len < E2ee.FrameOverhead || offset + len > frame.Length) return false;
            generation = frame[offset];
            counter = BinaryPrimitives.ReadUInt32BigEndian(new ReadOnlySpan<byte>(frame, offset + 1, 4));
            return true;
        }

        /// <summary>Authenticates and decrypts a frame of this generation; null when it is not one.</summary>
        public byte[] Open(byte[] frame, int offset = 0, int len = -1)
        {
            if (len < 0) len = frame.Length - offset;
            if (!Peek(frame, offset, len, out var generation, out var counter) || generation != Generation) return null;
            int bodyLen = len - E2ee.FrameTagLen;
            byte[] expected;
            using (var h = new E2ee.IncrementalHmac(_auth))
            {
                h.Append(frame, offset, bodyLen);
                expected = h.Finish();
            }
            if (!CryptographicOperations.FixedTimeEquals(
                    new ReadOnlySpan<byte>(expected, 0, E2ee.FrameTagLen),
                    new ReadOnlySpan<byte>(frame, offset + bodyLen, E2ee.FrameTagLen)))
                return null;
            var plain = new byte[bodyLen - E2ee.FrameHeaderLen];
            Buffer.BlockCopy(frame, offset + E2ee.FrameHeaderLen, plain, 0, plain.Length);
            lock (_lock) E2ee.Aes256Ctr(_ecb, Iv(counter), plain, 0, plain.Length);
            return plain;
        }

        public void Dispose()
        {
            _ecb.Dispose();
            _aes.Dispose();
            Array.Clear(_auth, 0, _auth.Length);
            Array.Clear(EncForTests, 0, EncForTests.Length);
        }
    }

    /// <summary>Anti-replay state of one received generation (window of <see cref="E2ee.ReplayWindow"/> counters).</summary>
    internal sealed class E2eeReplayState
    {
        private bool _any;
        private uint _highest;
        private ulong _lo, _hi;

        /// <summary>True (and records it) when <paramref name="counter"/> was not seen before and is not too old.</summary>
        public bool Accept(uint counter)
        {
            if (!_any)
            {
                _any = true;
                _highest = counter;
                _lo = 1;
                _hi = 0;
                return true;
            }
            if (counter > _highest)
            {
                uint shift = counter - _highest;
                if (shift >= E2ee.ReplayWindow) { _lo = 0; _hi = 0; }
                else ShiftLeft((int)shift);
                _lo |= 1;
                _highest = counter;
                return true;
            }
            uint age = _highest - counter;
            if (age >= E2ee.ReplayWindow) return false;
            if (TestBit((int)age)) return false;
            SetBit((int)age);
            return true;
        }

        private bool TestBit(int i) => i < 64 ? (_lo & (1UL << i)) != 0 : (_hi & (1UL << (i - 64))) != 0;
        private void SetBit(int i) { if (i < 64) _lo |= 1UL << i; else _hi |= 1UL << (i - 64); }

        private void ShiftLeft(int n)
        {
            if (n == 0) return;
            if (n >= 64) { _hi = _lo << (n - 64); _lo = 0; return; }
            _hi = (_hi << n) | (_lo >> (64 - n));
            _lo <<= n;
        }
    }

    /// <summary>Receiving side of one peer: its last <see cref="E2ee.KeptGenerations"/> sender keys.</summary>
    public sealed class E2eePeerKeys
    {
        private readonly List<(E2eeSenderKey key, E2eeReplayState replay)> _generations = new List<(E2eeSenderKey, E2eeReplayState)>();

        public void Insert(E2eeSenderKey key)
        {
            for (int i = _generations.Count - 1; i >= 0; i--)
                if (_generations[i].key.Generation == key.Generation)
                {
                    _generations[i].key.Dispose();
                    _generations.RemoveAt(i);
                }
            _generations.Add((key, new E2eeReplayState()));
            while (_generations.Count > E2ee.KeptGenerations)
            {
                _generations[0].key.Dispose();
                _generations.RemoveAt(0);
            }
        }

        public bool IsEmpty => _generations.Count == 0;

        public bool HasGeneration(byte generation)
        {
            foreach (var g in _generations) if (g.key.Generation == generation) return true;
            return false;
        }

        /// <summary>Decrypts <paramref name="frame"/>, rejecting unknown generations, bad tags and replayed counters.</summary>
        public byte[] Open(byte[] frame, int offset = 0, int len = -1)
        {
            if (len < 0) len = frame.Length - offset;
            if (!E2eeSenderKey.Peek(frame, offset, len, out var generation, out var counter)) return null;
            foreach (var g in _generations)
            {
                if (g.key.Generation != generation) continue;
                var plain = g.key.Open(frame, offset, len);
                if (plain == null) return null;
                return g.replay.Accept(counter) ? plain : null;
            }
            return null;
        }

        internal void Clear()
        {
            foreach (var g in _generations) g.key.Dispose();
            _generations.Clear();
        }
    }

    public enum E2eeOutgoingKind
    {
        /// <summary>"I am in <see cref="E2eeOutgoing.ChannelId"/> with this identity key" — to every other member.</summary>
        Hello,
        /// <summary>Our current sender key, wrapped for <see cref="E2eeOutgoing.To"/>.</summary>
        SenderKey,
    }

    /// <summary>A control-plane message the group wants sent; the server relays both without reading them.</summary>
    public readonly struct E2eeOutgoing
    {
        public readonly E2eeOutgoingKind Kind;
        public readonly Guid ChannelId;
        public readonly Guid To;
        public readonly byte Generation;
        public readonly byte[] Wrapped;

        private E2eeOutgoing(E2eeOutgoingKind kind, Guid channelId, Guid to, byte generation, byte[] wrapped)
        {
            Kind = kind;
            ChannelId = channelId;
            To = to;
            Generation = generation;
            Wrapped = wrapped;
        }

        public static E2eeOutgoing Hello(Guid channelId) => new E2eeOutgoing(E2eeOutgoingKind.Hello, channelId, Guid.Empty, 0, null);
        public static E2eeOutgoing SenderKey(Guid channelId, Guid to, byte generation, byte[] wrapped) =>
            new E2eeOutgoing(E2eeOutgoingKind.SenderKey, channelId, to, generation, wrapped);
    }

    public enum E2eePeerChangeKind
    {
        None,
        /// <summary>First identity key seen for this peer.</summary>
        New,
        /// <summary>The peer's identity key differs from the one we knew (reconnect, or an impostor).</summary>
        KeyChanged,
    }

    /// <summary>Why the group changed a peer's trust state.</summary>
    public readonly struct E2eePeerChange
    {
        public readonly E2eePeerChangeKind Kind;
        /// <summary>Fingerprint the peer had before, for <see cref="E2eePeerChangeKind.KeyChanged"/>.</summary>
        public readonly string PreviousFingerprint;

        public E2eePeerChange(E2eePeerChangeKind kind, string previousFingerprint)
        {
            Kind = kind;
            PreviousFingerprint = previousFingerprint;
        }

        public static readonly E2eePeerChange None = new E2eePeerChange(E2eePeerChangeKind.None, null);
        public bool Changed => Kind != E2eePeerChangeKind.None;
    }

    /// <summary>
    /// One client's view of the encrypted groups it belongs to: its identity, its sender key and the
    /// peers (across all its encrypted channels) it exchanges keys with. Not thread-safe; the client
    /// serialises access.
    /// </summary>
    public sealed class E2eeGroup
    {
        private sealed class Peer
        {
            public byte[] PublicKey;
            /// <summary>Encrypted channels we share with the peer (any one of them routes our messages).</summary>
            public readonly HashSet<Guid> Channels = new HashSet<Guid>();
            public readonly E2eePeerKeys Keys = new E2eePeerKeys();
            /// <summary>Generation of our key we last wrapped for this peer.</summary>
            public byte? SentGeneration;
        }

        private byte[] _secret;
        private E2eeSenderKey _key;
        private uint _counter;
        private readonly HashSet<Guid> _channels = new HashSet<Guid>();
        private readonly Dictionary<Guid, Peer> _peers = new Dictionary<Guid, Peer>();

        public E2eeGroup(E2eeIdentityKey identity)
        {
            Identity = identity ?? throw new ArgumentNullException(nameof(identity));
            _secret = E2ee.RandomBytes(E2ee.SecretLen);
            _key = E2eeSenderKey.Derive(0, _secret);
        }

        public E2eeIdentityKey Identity { get; }
        public byte Generation => _key.Generation;
        /// <summary>Encrypted channels we are currently in.</summary>
        public IReadOnlyCollection<Guid> Channels => _channels;
        public bool IsEncrypted(Guid channelId) => _channels.Contains(channelId);
        /// <summary>Whether any encrypted channel is joined, i.e. our uplink must be encrypted.</summary>
        public bool Active => _channels.Count > 0;
        /// <summary>Whether a rotation is due (<see cref="Rotate"/> performs it).</summary>
        public bool RotationPending { get; private set; }

        public string PeerFingerprint(Guid userId) => _peers.TryGetValue(userId, out var p) ? E2ee.Fingerprint(p.PublicKey) : null;

        /// <summary>Peers we hold a sender key of (we can decrypt their frames).</summary>
        public List<Guid> DecryptablePeers()
        {
            var list = new List<Guid>();
            foreach (var kv in _peers) if (!kv.Value.Keys.IsEmpty) list.Add(kv.Key);
            return list;
        }

        public bool HasKeyFor(Guid userId) => _peers.TryGetValue(userId, out var p) && !p.Keys.IsEmpty;

        /// <summary>We joined an encrypted channel: announce ourselves so members send us their keys (and rotate for us).</summary>
        public List<E2eeOutgoing> Joined(Guid channelId)
        {
            var outList = new List<E2eeOutgoing>();
            if (_channels.Add(channelId)) outList.Add(E2eeOutgoing.Hello(channelId));
            return outList;
        }

        /// <summary>
        /// Our membership of <paramref name="channelId"/> was re-acknowledged (session resume): peers absent
        /// from the server's <paramref name="members"/> roster left while we were away and are forgotten
        /// (rotating away from them); a fresh hello makes the remaining members re-send keys we may have
        /// missed. <paramref name="gone"/> receives the forgotten peers.
        /// </summary>
        public List<E2eeOutgoing> Rejoined(Guid channelId, ICollection<Guid> members, List<Guid> gone)
        {
            if (!_channels.Contains(channelId)) return Joined(channelId);
            var absent = new List<Guid>();
            foreach (var kv in _peers)
                if (kv.Value.Channels.Contains(channelId) && !members.Contains(kv.Key)) absent.Add(kv.Key);
            foreach (var user in absent)
            {
                PeerLeft(channelId, user);
                if (!_peers.ContainsKey(user)) gone.Add(user);
            }
            return new List<E2eeOutgoing> { E2eeOutgoing.Hello(channelId) };
        }

        /// <summary>We left an encrypted channel: peers we no longer share any channel with are dropped (and our key rotates away from them).</summary>
        public void Left(Guid channelId)
        {
            if (!_channels.Remove(channelId)) return;
            var gone = new List<Guid>();
            foreach (var kv in _peers)
            {
                kv.Value.Channels.Remove(channelId);
                if (kv.Value.Channels.Count == 0) gone.Add(kv.Key);
            }
            foreach (var user in gone) RemovePeer(user);
        }

        /// <summary>Drops every channel and peer (session ended); the identity key stays.</summary>
        public void Reset()
        {
            _channels.Clear();
            foreach (var p in _peers.Values) p.Keys.Clear();
            _peers.Clear();
            RotationPending = true;
        }

        /// <summary>A peer left <paramref name="channelId"/> (or the channel as a whole went away for them).</summary>
        public void PeerLeft(Guid channelId, Guid userId)
        {
            if (!_peers.TryGetValue(userId, out var peer)) return;
            peer.Channels.Remove(channelId);
            if (peer.Channels.Count == 0) RemovePeer(userId);
        }

        private void RemovePeer(Guid userId)
        {
            if (_peers.TryGetValue(userId, out var peer)) peer.Keys.Clear();
            _peers.Remove(userId);
            RotationPending = true;
        }

        private E2eePeerChange LearnPeer(Guid channelId, Guid userId, byte[] publicKey)
        {
            if (_peers.TryGetValue(userId, out var peer))
            {
                if (CryptographicOperations.FixedTimeEquals(peer.PublicKey, publicKey))
                {
                    peer.Channels.Add(channelId);
                    return E2eePeerChange.None;
                }
                var previous = E2ee.Fingerprint(peer.PublicKey);
                peer.PublicKey = (byte[])publicKey.Clone();
                peer.Channels.Add(channelId);
                peer.Keys.Clear();
                peer.SentGeneration = null;
                RotationPending = true;
                return new E2eePeerChange(E2eePeerChangeKind.KeyChanged, previous);
            }
            var fresh = new Peer { PublicKey = (byte[])publicKey.Clone() };
            fresh.Channels.Add(channelId);
            _peers[userId] = fresh;
            RotationPending = true;
            return new E2eePeerChange(E2eePeerChangeKind.New, null);
        }

        /// <summary>
        /// A peer announced itself in <paramref name="channelId"/>. A peer we already trust gets our current key
        /// right away (they may have lost it); a new or re-keyed peer triggers a rotation instead, so they only
        /// ever receive a key that post-dates their arrival.
        /// </summary>
        public List<E2eeOutgoing> OnHello(Guid channelId, Guid userId, byte[] publicKey, out E2eePeerChange change)
        {
            change = E2eePeerChange.None;
            var outList = new List<E2eeOutgoing>();
            if (publicKey == null || publicKey.Length != E2ee.PublicKeyLen) throw new ArgumentException("public key must be 32 bytes", nameof(publicKey));
            if (!_channels.Contains(channelId)) return outList;
            change = LearnPeer(channelId, userId, publicKey);
            if (change.Changed) return outList;
            WrapFor(userId, outList);
            return outList;
        }

        /// <summary>A peer sent us their sender key; throws <see cref="CryptographicException"/> when it is not for us or forged.</summary>
        public List<E2eeOutgoing> OnSenderKey(Guid channelId, Guid userId, byte[] publicKey, byte generation, byte[] wrapped, out E2eePeerChange change)
        {
            change = E2eePeerChange.None;
            var outList = new List<E2eeOutgoing>();
            if (publicKey == null || publicKey.Length != E2ee.PublicKeyLen) throw new ArgumentException("public key must be 32 bytes", nameof(publicKey));
            if (!_channels.Contains(channelId)) return outList;
            var secret = Identity.Unwrap(publicKey, generation, wrapped);
            change = LearnPeer(channelId, userId, publicKey);
            var peer = _peers[userId];
            peer.Keys.Insert(E2eeSenderKey.Derive(generation, secret));
            Array.Clear(secret, 0, secret.Length);
            // A peer that keyed us before we keyed them (both joined at once) still needs ours.
            if (!change.Changed && !peer.SentGeneration.HasValue) WrapFor(userId, outList);
            return outList;
        }

        private void WrapFor(Guid userId, List<E2eeOutgoing> outList)
        {
            if (!_peers.TryGetValue(userId, out var peer)) return;
            Guid channelId = Guid.Empty;
            bool any = false;
            foreach (var c in peer.Channels) { channelId = c; any = true; break; }
            if (!any) return;
            var wrapped = Identity.Wrap(peer.PublicKey, _key.Generation, _secret);
            peer.SentGeneration = _key.Generation;
            outList.Add(E2eeOutgoing.SenderKey(channelId, userId, _key.Generation, wrapped));
        }

        /// <summary>
        /// Picks a fresh sender key and wraps it for every peer. Callers debounce this (a join wave should
        /// cost one rotation, not one per newcomer); no-op unless <see cref="RotationPending"/> or <paramref name="force"/>.
        /// </summary>
        public List<E2eeOutgoing> Rotate(bool force)
        {
            var outList = new List<E2eeOutgoing>();
            if (!(force || RotationPending)) return outList;
            RotationPending = false;
            var old = _key;
            var oldSecret = _secret;
            _secret = E2ee.RandomBytes(E2ee.SecretLen);
            _key = E2eeSenderKey.Derive(unchecked((byte)(old.Generation + 1)), _secret);
            _counter = 0;
            old.Dispose();
            Array.Clear(oldSecret, 0, oldSecret.Length);
            var users = new List<Guid>(_peers.Keys);
            foreach (var user in users) WrapFor(user, outList);
            return outList;
        }

        /// <summary>Encrypts one of our frames.</summary>
        public byte[] Encrypt(byte[] plain, int offset = 0, int len = -1)
        {
            uint counter = _counter;
            _counter = unchecked(_counter + 1);
            if (_counter >= E2ee.RotateAtCounter) RotationPending = true;
            return _key.Seal(counter, plain, offset, len);
        }

        /// <summary>Decrypts a frame sent by <paramref name="userId"/>; null for unknown senders, unknown generations, bad tags and replays.</summary>
        public byte[] Decrypt(Guid userId, byte[] frame, int offset = 0, int len = -1) =>
            _peers.TryGetValue(userId, out var peer) ? peer.Keys.Open(frame, offset, len) : null;
    }
}
