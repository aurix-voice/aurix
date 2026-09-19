using System;
using System.Buffers.Binary;
using Aurix.Audio;

namespace Aurix.Protocol
{
    /// <summary>AURX wire packet types (mirrors crates/aurix-common/src/protocol.rs).</summary>
    public enum PacketType : byte
    {
        Audio = 0x01,
        AudioFec = 0x02,
        Control = 0x10,
        Heartbeat = 0x20,
        HeartbeatAck = 0x21,
        SessionInit = 0x30,
        SessionInitAck = 0x31,
        SessionClose = 0x32,
        SessionBind = 0x33,
        SessionBindAck = 0x34,
        ChannelJoin = 0x40,
        ChannelJoinAck = 0x41,
        ChannelLeave = 0x42,
        PositionUpdate = 0x50,
        MuteState = 0x60,
        SpeakingState = 0x61,
        QualityReport = 0x70,
        BitrateCommand = 0x71,
        /// <summary>Cascade envelope (node-to-node only; never seen by clients).</summary>
        Relay = 0x80,
        Error = 0xFF,
    }

    [Flags]
    public enum PacketFlags : ushort
    {
        None = 0,
        Encrypted = 0x0001,
        Compressed = 0x0002,
        Dtx = 0x0004,
        Fec = 0x0008,
        KeyFrame = 0x0010,
        Priority = 0x0020,
        Relay = 0x0040,
        VolumeAttenuated = 0x0080,
        E2ee = 0x0100,
        Rtp = 0x0200,
        Authenticated = 0x0400,
        /// <summary>First payload byte is an RFC 6464-style audio level (-dBov, 127 = silence).</summary>
        Energy = 0x0800,
        /// <summary>Downlink frame carries a 2-byte <see cref="Direction"/> (after the volume byte, if any).</summary>
        Directional = 0x1000,
        /// <summary>Payload is G.711 μ-law (8 kHz) instead of Opus — only on sessions that negotiated PCMU.</summary>
        Pcmu = 0x2000,
    }

    /// <summary>
    /// Where a speaker is relative to this listener, in the listener's own frame: azimuth 0 is
    /// straight ahead, positive is to the right (radians, -π..π); elevation positive is above
    /// (-π/2..π/2). Sent by the server for directional positional channels.
    /// </summary>
    public readonly struct Direction : IEquatable<Direction>
    {
        public const int WireSize = 2;
        private const float AzimuthScale = 127f / MathF.PI;
        private const float ElevationScale = 127f / (MathF.PI / 2f);

        public readonly float Azimuth;
        public readonly float Elevation;

        public Direction(float azimuth, float elevation)
        {
            Azimuth = azimuth;
            Elevation = elevation;
        }

        public static readonly Direction Ahead = new Direction(0f, 0f);

        public static Direction Decode(byte azimuth, byte elevation) =>
            new Direction((sbyte)azimuth / AzimuthScale, (sbyte)elevation / ElevationScale);

        public void Encode(Span<byte> dst)
        {
            dst[0] = (byte)(sbyte)Math.Clamp(MathF.Round(Azimuth * AzimuthScale), -127f, 127f);
            dst[1] = (byte)(sbyte)Math.Clamp(MathF.Round(Elevation * ElevationScale), -127f, 127f);
        }

        /// <summary>
        /// Constant-power stereo gains (left, right), normalised so a centred source is unity in
        /// both ears and a hard-panned one is +3 dB in one ear and silent in the other. Stereo has
        /// no front/back cue: a source behind pans like one in front.
        /// </summary>
        public (float Left, float Right) StereoGains()
        {
            float pan = Math.Clamp(MathF.Sin(Azimuth), -1f, 1f);
            float theta = (pan + 1f) * (MathF.PI / 4f);
            const float sqrt2 = 1.41421356f;
            return (MathF.Cos(theta) * sqrt2, MathF.Sin(theta) * sqrt2);
        }

        public bool Equals(Direction other) => Azimuth == other.Azimuth && Elevation == other.Elevation;
        public override bool Equals(object obj) => obj is Direction d && Equals(d);
        public override int GetHashCode() => HashCode.Combine(Azimuth, Elevation);
        public override string ToString() => $"az={Azimuth:F3} el={Elevation:F3}";
    }

    /// <summary>
    /// AURX packet header: 30 bytes, big-endian.
    /// magic(4) ver(1) type(1) flags(2) seq(4) ts(4) ssrc(4) chHash(4) len(2) crc32(4)
    /// </summary>
    public struct PacketHeader
    {
        public byte Version;
        public PacketType Type;
        public PacketFlags Flags;
        public uint Sequence;
        public uint Timestamp;
        public uint Ssrc;
        public uint ChannelIdHash;
        public ushort PayloadLength;
        public uint Checksum;

        public static PacketHeader Create(PacketType type, uint sequence, uint timestamp, uint ssrc)
        {
            return new PacketHeader
            {
                Version = AurxPacket.ProtocolVersion,
                Type = type,
                Flags = PacketFlags.None,
                Sequence = sequence,
                Timestamp = timestamp,
                Ssrc = ssrc,
            };
        }

        public void Encode(Span<byte> dst)
        {
            AurxPacket.Magic.CopyTo(dst);
            dst[4] = Version;
            dst[5] = (byte)Type;
            BinaryPrimitives.WriteUInt16BigEndian(dst.Slice(6), (ushort)Flags);
            BinaryPrimitives.WriteUInt32BigEndian(dst.Slice(8), Sequence);
            BinaryPrimitives.WriteUInt32BigEndian(dst.Slice(12), Timestamp);
            BinaryPrimitives.WriteUInt32BigEndian(dst.Slice(16), Ssrc);
            BinaryPrimitives.WriteUInt32BigEndian(dst.Slice(20), ChannelIdHash);
            BinaryPrimitives.WriteUInt16BigEndian(dst.Slice(24), PayloadLength);
            BinaryPrimitives.WriteUInt32BigEndian(dst.Slice(26), Checksum);
        }

        public static bool TryDecode(ReadOnlySpan<byte> src, out PacketHeader header, out string error)
        {
            header = default;
            error = null;
            if (src.Length < AurxPacket.HeaderSize) { error = "packet too short for header"; return false; }
            if (!src.Slice(0, 4).SequenceEqual(AurxPacket.Magic)) { error = "invalid magic"; return false; }
            header.Version = src[4];
            if (header.Version != AurxPacket.ProtocolVersion) { error = "unsupported protocol version"; return false; }
            if (!Enum.IsDefined(typeof(PacketType), src[5])) { error = "unknown packet type"; return false; }
            header.Type = (PacketType)src[5];
            header.Flags = (PacketFlags)BinaryPrimitives.ReadUInt16BigEndian(src.Slice(6));
            header.Sequence = BinaryPrimitives.ReadUInt32BigEndian(src.Slice(8));
            header.Timestamp = BinaryPrimitives.ReadUInt32BigEndian(src.Slice(12));
            header.Ssrc = BinaryPrimitives.ReadUInt32BigEndian(src.Slice(16));
            header.ChannelIdHash = BinaryPrimitives.ReadUInt32BigEndian(src.Slice(20));
            header.PayloadLength = BinaryPrimitives.ReadUInt16BigEndian(src.Slice(24));
            header.Checksum = BinaryPrimitives.ReadUInt32BigEndian(src.Slice(26));
            return true;
        }
    }

    /// <summary>
    /// A decoded AURX packet. Payload is a copy of the wire bytes.
    /// Protocol v2: every packet except <see cref="PacketType.SessionBind"/> is sent with
    /// <see cref="Seal"/> (AES-256-CTR payload + truncated HMAC tag) and received with <see cref="Open"/>.
    /// </summary>
    public sealed class AurxPacket
    {
        public const byte ProtocolVersion = 2;
        /// <summary>
        /// Set on the SSRC of server-synthesized speech: a participant's TTS voice is <c>participant.Ssrc | SynthSsrcFlag</c>,
        /// channel announcements use a per-channel SSRC with the flag set. Such streams are mixed like any other
        /// but never carry energy/speaking state of a real microphone.
        /// </summary>
        public const uint SynthSsrcFlag = 0x80000000u;
        public static readonly byte[] Magic = { 0x41, 0x55, 0x52, 0x58 }; // "AURX"
        public const int MaxPacketSize = 1400;
        public const int HeaderSize = 30;
        public const int AuthTagSize = 16;
        public const int SessionBindPayloadSize = 32;

        public PacketHeader Header;
        public byte[] Payload;
        /// <summary>Tag from the wire when <see cref="PacketFlags.Authenticated"/> is set; null otherwise.</summary>
        public byte[] AuthTag;

        public AurxPacket(PacketHeader header, byte[] payload)
        {
            Header = header;
            Payload = payload ?? Array.Empty<byte>();
        }

        public bool IsAuthenticated => AuthTag != null;
        public bool IsEncrypted => (Header.Flags & PacketFlags.Encrypted) != 0;

        /// <summary>Fixed-point gain byte scale: 128 = unchanged (1.0), 0 = silence, 255 ≈ 2.0 (+6 dB).</summary>
        public const float VolumeUnity = 128f;

        /// <summary>Gain factor (0..~2) carried by server-mixed downlink packets (positional attenuation ×
        /// the per-participant volume this client asked for), or 1.0.</summary>
        public float Volume => (Header.Flags & PacketFlags.VolumeAttenuated) != 0 && Payload.Length > 0
            ? Payload[0] / VolumeUnity
            : 1f;

        private int VolumeBytes => (Header.Flags & PacketFlags.VolumeAttenuated) != 0 && Payload.Length > 0 ? 1 : 0;

        private int DirectionBytes =>
            (Header.Flags & PacketFlags.Directional) != 0 && Payload.Length >= VolumeBytes + Protocol.Direction.WireSize
                ? Protocol.Direction.WireSize
                : 0;

        /// <summary>Speaker direction in this listener's frame (directional positional channels), or null.</summary>
        public Direction? Direction
        {
            get
            {
                if (DirectionBytes == 0) return null;
                int at = VolumeBytes;
                return Protocol.Direction.Decode(Payload[at], Payload[at + 1]);
            }
        }

        /// <summary>Audio bytes without the optional leading volume and direction metadata.</summary>
        public ReadOnlySpan<byte> AudioPayload
        {
            get
            {
                int skip = VolumeBytes + DirectionBytes;
                return skip == 0 ? Payload : new ReadOnlySpan<byte>(Payload, skip, Payload.Length - skip);
            }
        }

        /// <summary>
        /// Strips the server's downlink metadata (volume byte, direction) from the payload, returning
        /// them; header length/flags are updated to describe the bare Opus frame.
        /// </summary>
        public (float Volume, Direction? Direction) TakeDownlinkMeta()
        {
            float volume = Volume;
            Direction? direction = Direction;
            int skip = VolumeBytes + DirectionBytes;
            Header.Flags &= ~(PacketFlags.VolumeAttenuated | PacketFlags.Directional);
            if (skip == 0) return (volume, direction);
            var rest = new byte[Payload.Length - skip];
            Buffer.BlockCopy(Payload, skip, rest, 0, rest.Length);
            Payload = rest;
            Header.PayloadLength = (ushort)rest.Length;
            return (volume, direction);
        }

        /// <summary>
        /// Encode signed but NOT encrypted: header + plaintext payload + truncated HMAC tag.
        /// Only for <see cref="PacketType.SessionBind"/> (the server must read the session id to
        /// find the key). Everything else must use <see cref="Seal"/>.
        /// </summary>
        public byte[] EncodeAuthenticated(MediaKeys keys)
        {
            if (keys == null) throw new ArgumentNullException(nameof(keys));
            var header = Header;
            header.Flags |= PacketFlags.Authenticated;
            header.Flags &= ~PacketFlags.Encrypted;
            header.PayloadLength = (ushort)Payload.Length;
            header.Checksum = Crc32.Compute(Payload);
            var buf = new byte[HeaderSize + Payload.Length + AuthTagSize];
            header.Encode(buf);
            Payload.CopyTo(buf, HeaderSize);
            keys.ComputeTag(buf, HeaderSize + Payload.Length, buf.AsSpan(HeaderSize + Payload.Length, AuthTagSize));
            return buf;
        }

        /// <summary>
        /// Encrypt the payload (AES-256-CTR, IV from type/ssrc/seq/ts) and append the HMAC tag over
        /// header + ciphertext. The header CRC covers the ciphertext. Sets Encrypted | Authenticated.
        /// </summary>
        public byte[] Seal(MediaKeys keys)
        {
            if (keys == null) throw new ArgumentNullException(nameof(keys));
            var header = Header;
            header.Flags |= PacketFlags.Authenticated | PacketFlags.Encrypted;
            header.PayloadLength = (ushort)Payload.Length;
            var buf = new byte[HeaderSize + Payload.Length + AuthTagSize];
            Payload.CopyTo(buf, HeaderSize);
            Span<byte> iv = stackalloc byte[MediaKeys.IvSize];
            keys.Iv((byte)header.Type, header.Ssrc, header.Sequence, header.Timestamp, iv);
            keys.ApplyCtr(iv, buf, HeaderSize, Payload.Length);
            header.Checksum = Crc32.Compute(new ReadOnlySpan<byte>(buf, HeaderSize, Payload.Length));
            header.Encode(buf);
            keys.ComputeTag(buf, HeaderSize + Payload.Length, buf.AsSpan(HeaderSize + Payload.Length, AuthTagSize));
            return buf;
        }

        /// <summary>Encode without authentication (only valid when the server allows unauthenticated packets).</summary>
        public byte[] Encode()
        {
            var header = Header;
            header.Flags &= ~PacketFlags.Authenticated;
            header.PayloadLength = (ushort)Payload.Length;
            header.Checksum = Crc32.Compute(Payload);
            var buf = new byte[HeaderSize + Payload.Length];
            header.Encode(buf);
            Payload.CopyTo(buf, HeaderSize);
            return buf;
        }

        /// <summary>Strict decoder: exact length, bounded size, verified CRC. HMAC is NOT verified here.</summary>
        public static bool TryDecode(ReadOnlySpan<byte> data, out AurxPacket packet, out string error)
        {
            packet = null;
            if (data.Length > MaxPacketSize) { error = "packet exceeds max size"; return false; }
            if (!PacketHeader.TryDecode(data, out var header, out error)) return false;
            bool authenticated = (header.Flags & PacketFlags.Authenticated) != 0;
            int expected = header.PayloadLength + (authenticated ? AuthTagSize : 0);
            var rest = data.Slice(HeaderSize);
            if (rest.Length < expected) { error = "payload truncated"; return false; }
            if (rest.Length > expected) { error = "trailing bytes after payload"; return false; }
            var payload = rest.Slice(0, header.PayloadLength).ToArray();
            if (Crc32.Compute(payload) != header.Checksum) { error = "checksum mismatch"; return false; }
            packet = new AurxPacket(header, payload);
            if (authenticated) packet.AuthTag = rest.Slice(header.PayloadLength, AuthTagSize).ToArray();
            error = null;
            return true;
        }

        /// <summary>Verify the wire tag against <paramref name="keys"/>. False for unauthenticated packets. Does not decrypt.</summary>
        public bool VerifyAuth(MediaKeys keys)
        {
            if (AuthTag == null || keys == null) return false;
            var buf = new byte[HeaderSize + Payload.Length];
            Header.Encode(buf);
            Payload.CopyTo(buf, HeaderSize);
            Span<byte> expected = stackalloc byte[AuthTagSize];
            keys.ComputeTag(buf, buf.Length, expected);
            return ConstantTimeEquals(expected, AuthTag);
        }

        /// <summary>
        /// Verify the tag and, when <see cref="PacketFlags.Encrypted"/> is set, decrypt the payload in
        /// place (flag cleared, checksum recomputed over plaintext). False leaves the packet untouched.
        /// </summary>
        public bool Open(MediaKeys keys)
        {
            if (!VerifyAuth(keys)) return false;
            if (IsEncrypted)
            {
                Span<byte> iv = stackalloc byte[MediaKeys.IvSize];
                keys.Iv((byte)Header.Type, Header.Ssrc, Header.Sequence, Header.Timestamp, iv);
                keys.ApplyCtr(iv, Payload, 0, Payload.Length);
                Header.Flags &= ~PacketFlags.Encrypted;
                Header.Checksum = Crc32.Compute(Payload);
            }
            return true;
        }

        public static bool IsAurxPacket(ReadOnlySpan<byte> data) =>
            data.Length >= HeaderSize && data.Slice(0, 4).SequenceEqual(Magic);

        public static AurxPacket Audio(uint seq, uint ts, uint ssrc, uint channelHash, byte[] opusFrame)
        {
            var h = PacketHeader.Create(PacketType.Audio, seq, ts, ssrc);
            h.ChannelIdHash = channelHash;
            return new AurxPacket(h, opusFrame);
        }

        /// <summary>Audio frame prefixed with the sender's measured level (see <see cref="AudioLevel"/>).</summary>
        public static AurxPacket AudioWithLevel(uint seq, uint ts, uint ssrc, uint channelHash, byte level, byte[] opusFrame)
        {
            var payload = new byte[opusFrame.Length + 1];
            payload[0] = Math.Min(level, AudioLevel.Silence);
            Buffer.BlockCopy(opusFrame, 0, payload, 1, opusFrame.Length);
            var p = Audio(seq, ts, ssrc, channelHash, payload);
            p.Header.Flags |= PacketFlags.Energy;
            return p;
        }

        /// <summary>
        /// Strips the leading level byte of an <see cref="PacketFlags.Energy"/> packet; returns null when
        /// the packet carries no level. Header length/flags are updated to describe the bare Opus frame.
        /// </summary>
        public byte? TakeAudioLevel()
        {
            if ((Header.Flags & PacketFlags.Energy) == 0) return null;
            Header.Flags &= ~PacketFlags.Energy;
            if (Payload.Length == 0) return AudioLevel.Silence;
            byte level = Math.Min(Payload[0], AudioLevel.Silence);
            var rest = new byte[Payload.Length - 1];
            Buffer.BlockCopy(Payload, 1, rest, 0, rest.Length);
            Payload = rest;
            Header.PayloadLength = (ushort)rest.Length;
            return level;
        }

        public static AurxPacket Heartbeat(uint seq, uint ssrc, uint ts) =>
            new AurxPacket(PacketHeader.Create(PacketType.Heartbeat, seq, ts, ssrc), Array.Empty<byte>());

        public static AurxPacket MuteState(uint seq, uint ssrc, bool muted) =>
            new AurxPacket(PacketHeader.Create(PacketType.MuteState, seq, 0, ssrc), new[] { muted ? (byte)1 : (byte)0 });

        public static AurxPacket QualityReport(uint seq, uint ssrc, float rttMs, float jitterMs, float lossPercent)
        {
            var p = new byte[12];
            BinaryPrimitives.WriteInt32BigEndian(p.AsSpan(0), BitConverter.SingleToInt32Bits(rttMs));
            BinaryPrimitives.WriteInt32BigEndian(p.AsSpan(4), BitConverter.SingleToInt32Bits(jitterMs));
            BinaryPrimitives.WriteInt32BigEndian(p.AsSpan(8), BitConverter.SingleToInt32Bits(lossPercent));
            return new AurxPacket(PacketHeader.Create(PacketType.QualityReport, seq, 0, ssrc), p);
        }

        /// <summary>
        /// SessionBind payload: session_id (16, RFC 4122 byte order) | unix_ms (8) | nonce (8).
        /// Send with <see cref="EncodeAuthenticated"/> (signed, not encrypted).
        /// </summary>
        public static AurxPacket SessionBind(Guid sessionId, uint ssrc, long unixMs, ulong nonce)
        {
            var p = new byte[SessionBindPayloadSize];
            UuidBytes.Write(sessionId, p.AsSpan(0, 16));
            BinaryPrimitives.WriteInt64BigEndian(p.AsSpan(16), unixMs);
            BinaryPrimitives.WriteUInt64BigEndian(p.AsSpan(24), nonce);
            return new AurxPacket(PacketHeader.Create(PacketType.SessionBind, (uint)nonce, (uint)unixMs, ssrc), p);
        }

        /// <summary>Server-side channel hash: CRC32 over the UUID's 16 raw bytes.</summary>
        public static uint ChannelIdHash(Guid channelId)
        {
            Span<byte> raw = stackalloc byte[16];
            UuidBytes.Write(channelId, raw);
            return Crc32.Compute(raw);
        }

        public static bool ConstantTimeEquals(ReadOnlySpan<byte> a, ReadOnlySpan<byte> b)
        {
            if (a.Length != b.Length) return false;
            int diff = 0;
            for (int i = 0; i < a.Length; i++) diff |= a[i] ^ b[i];
            return diff == 0;
        }
    }

    /// <summary>
    /// .NET's <see cref="Guid.ToByteArray"/> uses mixed endianness for the first three fields;
    /// the server (Rust <c>uuid</c>) uses RFC 4122 network order. These helpers convert.
    /// </summary>
    public static class UuidBytes
    {
        public static void Write(Guid id, Span<byte> dst)
        {
            var le = id.ToByteArray();
            dst[0] = le[3]; dst[1] = le[2]; dst[2] = le[1]; dst[3] = le[0];
            dst[4] = le[5]; dst[5] = le[4];
            dst[6] = le[7]; dst[7] = le[6];
            for (int i = 8; i < 16; i++) dst[i] = le[i];
        }

        public static Guid Read(ReadOnlySpan<byte> src)
        {
            var le = new byte[16];
            le[0] = src[3]; le[1] = src[2]; le[2] = src[1]; le[3] = src[0];
            le[4] = src[5]; le[5] = src[4];
            le[6] = src[7]; le[7] = src[6];
            for (int i = 8; i < 16; i++) le[i] = src[i];
            return new Guid(le);
        }
    }

    /// <summary>CRC-32 (IEEE 802.3, reflected, init 0xFFFFFFFF) — identical to Rust <c>crc32fast</c>.</summary>
    public static class Crc32
    {
        private static readonly uint[] Table = BuildTable();

        private static uint[] BuildTable()
        {
            var t = new uint[256];
            for (uint i = 0; i < 256; i++)
            {
                uint c = i;
                for (int k = 0; k < 8; k++) c = (c & 1) != 0 ? 0xEDB88320u ^ (c >> 1) : c >> 1;
                t[i] = c;
            }
            return t;
        }

        public static uint Compute(ReadOnlySpan<byte> data)
        {
            uint crc = 0xFFFFFFFFu;
            foreach (var b in data) crc = Table[(crc ^ b) & 0xFF] ^ (crc >> 8);
            return crc ^ 0xFFFFFFFFu;
        }
    }

    /// <summary>64-packet anti-replay window over 32-bit sequence numbers (RFC 3711 §3.3.2 style).</summary>
    public sealed class ReplayWindow
    {
        public const uint Window = 64;
        private uint _highest;
        private ulong _bitmap;
        private bool _initialized;

        /// <summary>True (and records) if <paramref name="seq"/> is fresh; false for replays or packets older than the window.</summary>
        public bool CheckAndUpdate(uint seq)
        {
            if (!_initialized)
            {
                _initialized = true;
                _highest = seq;
                _bitmap = 1;
                return true;
            }
            if (seq > _highest)
            {
                uint delta = seq - _highest;
                _bitmap = delta >= Window ? 1UL : (_bitmap << (int)delta) | 1UL;
                _highest = seq;
                return true;
            }
            uint back = _highest - seq;
            if (back >= Window) return false;
            ulong mask = 1UL << (int)back;
            if ((_bitmap & mask) != 0) return false;
            _bitmap |= mask;
            return true;
        }
    }
}
