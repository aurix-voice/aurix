using System;
using System.Collections.Generic;
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
            Assert.Equal(2, wire[4]); // protocol v2
            Assert.Equal(0x01, wire[5]);
            Assert.Equal(new byte[] { 0x01, 0x02, 0x03, 0x04 }, wire[8..12]);
            Assert.Equal(new byte[] { 0x0D, 0x0E, 0x0F, 0x10 }, wire[20..24]);
            Assert.Equal(new byte[] { 0x00, 0x03 }, wire[24..26]);
        }

        private static MediaKeys Keys(byte fill)
        {
            var key = new byte[32];
            for (int i = 0; i < key.Length; i++) key[i] = fill;
            return MediaKeys.Derive(key);
        }

        private static byte[] FromHex(string hex)
        {
            var b = new byte[hex.Length / 2];
            for (int i = 0; i < b.Length; i++) b[i] = Convert.ToByte(hex.Substring(i * 2, 2), 16);
            return b;
        }

        [Fact]
        public void SealedRoundTripAndTamperDetection()
        {
            var key = Keys(7);
            var pkt = AurxPacket.Audio(10, 20, 30, 40, Encoding.ASCII.GetBytes("opus-frame"));
            var wire = pkt.Seal(key);
            Assert.Equal(AurxPacket.HeaderSize + 10 + AurxPacket.AuthTagSize, wire.Length);
            // Ciphertext on the wire, never the plaintext.
            Assert.NotEqual(Encoding.ASCII.GetBytes("opus-frame"), wire[AurxPacket.HeaderSize..(AurxPacket.HeaderSize + 10)]);

            Assert.True(AurxPacket.TryDecode(wire, out var decoded, out var err), err);
            Assert.True(decoded.IsAuthenticated);
            Assert.True(decoded.IsEncrypted);
            Assert.True(decoded.Open(key));
            Assert.False(decoded.IsEncrypted);
            Assert.Equal("opus-frame", Encoding.ASCII.GetString(decoded.Payload));

            Assert.True(AurxPacket.TryDecode(wire, out var wrongKey, out _));
            Assert.False(wrongKey.Open(Keys(8)));
            Assert.True(wrongKey.IsEncrypted); // untouched on failure

            // Flip a payload byte and fix the CRC so only the HMAC catches it.
            var tampered = (byte[])wire.Clone();
            tampered[AurxPacket.HeaderSize] ^= 0x01;
            var crc = Crc32.Compute(new ReadOnlySpan<byte>(tampered, AurxPacket.HeaderSize, 10));
            tampered[26] = (byte)(crc >> 24); tampered[27] = (byte)(crc >> 16); tampered[28] = (byte)(crc >> 8); tampered[29] = (byte)crc;
            Assert.True(AurxPacket.TryDecode(tampered, out var d2, out _));
            Assert.False(d2.Open(key));

            // Header tampering (sequence) is caught too: the tag covers the header.
            var hdrTamper = (byte[])wire.Clone();
            hdrTamper[11] ^= 0x01;
            Assert.True(AurxPacket.TryDecode(hdrTamper, out var d3, out _));
            Assert.False(d3.Open(key));
        }

        [Fact]
        public void WireVectorsMatchServer()
        {
            // Pinned in crates/aurix-common/src/protocol.rs (v2_wire_vectors_are_stable).
            var key = Keys(7);
            var pkt = AurxPacket.Audio(10, 20, 30, 40, Encoding.ASCII.GetBytes("opus-frame"));
            Assert.Equal(
                FromHex("41555258020104010000000a000000140000001e00000028000afa37ba15f071d27d551588a11d2e7f3dcaf9bf4f86642b9ed1df5aee423d"),
                pkt.Seal(key));
            var sid = UuidBytes.Read(FromHex("11111111111111111111111111111111"));
            var bind = AurxPacket.SessionBind(sid, 30, 1_700_000_000_000L, 0x0102030405060708UL);
            Assert.Equal(
                FromHex("415552580233040005060708cfe568000000001e0000000000200e1d7cbb111111111111111111111111111111110000018bcfe5680001020304050607086b0c72c29a059fa0e0627df1d5942167"),
                bind.EncodeAuthenticated(key));
        }

        [Fact]
        public void SessionBindIsSignedNotEncryptedAndDifferentPacketsGetDifferentKeystreams()
        {
            var key = Keys(1);
            var bind = AurxPacket.SessionBind(Guid.NewGuid(), 5, 1_700_000_000_000L, 42).EncodeAuthenticated(key);
            Assert.True(AurxPacket.TryDecode(bind, out var b, out _));
            Assert.True(b.IsAuthenticated);
            Assert.False(b.IsEncrypted);
            Assert.True(b.VerifyAuth(key));
            Assert.Equal(AurxPacket.SessionBindPayloadSize, b.Payload.Length);

            var body = new byte[64];
            var w1 = AurxPacket.Audio(1, 960, 9, 1, body).Seal(key);
            var w2 = AurxPacket.Audio(2, 1920, 9, 1, body).Seal(key);
            Assert.NotEqual(w1[AurxPacket.HeaderSize..(AurxPacket.HeaderSize + 64)], w2[AurxPacket.HeaderSize..(AurxPacket.HeaderSize + 64)]);
            // Empty payloads (heartbeats) seal and open too.
            var hb = AurxPacket.Heartbeat(3, 9, 0).Seal(key);
            Assert.True(AurxPacket.TryDecode(hb, out var h, out _));
            Assert.True(h.Open(key));
            Assert.Empty(h.Payload);
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
            var pkt = new AurxPacket(h, new byte[] { 64, 0xAA, 0xBB });
            Assert.Equal(0.5f, pkt.Volume);
            Assert.Equal(new byte[] { 0xAA, 0xBB }, pkt.AudioPayload.ToArray());
            Assert.Equal(1f, new AurxPacket(h, new byte[] { 128, 0xAA }).Volume);
            Assert.InRange(new AurxPacket(h, new byte[] { 255, 0xAA }).Volume, 1.99f, 2f);
            Assert.Equal(1f, new AurxPacket(PacketHeader.Create(PacketType.Audio, 1, 0, 5), new byte[] { 64 }).Volume);
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
        public void ReceiverPreferenceMessagesMatchServerWire()
        {
            var alice = Guid.Parse("11111111-2222-3333-4444-555555555556");
            var team = Guid.Parse("11111111-2222-3333-4444-555555555555");

            // Outbound: exactly the JSON shape serde expects (null channel_id = every channel).
            Assert.Equal(
                "{\"type\":\"SetParticipantMute\",\"data\":{\"user_id\":\"" + alice + "\",\"channel_id\":\"" + team + "\",\"muted\":true}}",
                ControlMessage.SetParticipantMute(alice, team, true));
            Assert.Equal(
                "{\"type\":\"SetParticipantMute\",\"data\":{\"user_id\":\"" + alice + "\",\"channel_id\":null,\"muted\":false}}",
                ControlMessage.SetParticipantMute(alice, null, false));
            Assert.Equal(
                "{\"type\":\"SetParticipantVolume\",\"data\":{\"user_id\":\"" + alice + "\",\"volume\":0.5}}",
                ControlMessage.SetParticipantVolume(alice, 0.5f));
            Assert.Equal(
                "{\"type\":\"SetUserBlock\",\"data\":{\"user_id\":\"" + alice + "\",\"blocked\":true}}",
                ControlMessage.SetUserBlock(alice, true));

            // Inbound snapshot as the server sends it after SessionInitAck.
            var prefs = ControlMessage.Parse("{\"type\":\"ReceiverPreferences\",\"data\":{\"blocked_users\":[\"" + alice + "\"]," +
                "\"local_mutes\":[{\"user_id\":\"" + alice + "\",\"channel_id\":null},{\"user_id\":\"" + alice + "\",\"channel_id\":\"" + team + "\"}]," +
                "\"volumes\":[{\"user_id\":\"" + alice + "\",\"volume\":1.5}]}}").ReceiverPreferences();
            Assert.Equal(new[] { alice }, prefs.BlockedUsers);
            Assert.Equal(2, prefs.LocalMutes.Count);
            Assert.Null(prefs.LocalMutes[0].ChannelId);
            Assert.Equal(team, prefs.LocalMutes[1].ChannelId);
            Assert.Single(prefs.Volumes);
            Assert.Equal(1.5f, prefs.Volumes[0].Volume);

            var empty = ControlMessage.Parse("{\"type\":\"ReceiverPreferences\",\"data\":{}}").ReceiverPreferences();
            Assert.Empty(empty.BlockedUsers);
            Assert.Empty(empty.LocalMutes);
            Assert.Empty(empty.Volumes);
        }

        [Fact]
        public void ModerationMessagesMatchServerWire()
        {
            var team = Guid.Parse("01a0b6d1-131f-7160-afd2-056622380dd3");
            var bob = Guid.Parse("01a0b6d1-132a-7385-8e7a-9ac1eceaeb0c");
            Assert.Equal(
                "{\"type\":\"ModerateParticipant\",\"data\":{\"channel_id\":\"" + team + "\",\"user_id\":\"" + bob +
                "\",\"action\":\"kick\",\"token\":\"t.o.k\",\"reason\":\"afk\"}}",
                ControlMessage.ModerateParticipant(team, bob, ModerationAction.Kick, "t.o.k", "afk"));
            Assert.Equal(
                "{\"type\":\"ModerateParticipant\",\"data\":{\"channel_id\":\"" + team + "\",\"user_id\":\"" + bob +
                "\",\"action\":\"unmute\",\"token\":\"t.o.k\",\"reason\":null}}",
                ControlMessage.ModerateParticipant(team, bob, ModerationAction.Unmute, "t.o.k", null));

            var ack = ControlMessage.Parse("{\"type\":\"ModerateParticipantAck\",\"data\":{\"channel_id\":\"" + team +
                "\",\"user_id\":\"" + bob + "\",\"action\":\"mute\"}}");
            Assert.Equal(team, ack.Id("channel_id"));
            Assert.Equal(bob, ack.Id("user_id"));
            Assert.Equal(ModerationAction.Mute, ControlMessage.ParseModerationAction(ack.Str("action")));
            Assert.Null(ControlMessage.ParseModerationAction("ban"));
        }

        [Fact]
        public void ChatMessagesMatchServerWire()
        {
            var team = Guid.Parse("01a0b6d1-131f-7160-afd2-056622380dd3");
            var bob = Guid.Parse("01a0b6d1-132a-7385-8e7a-9ac1eceaeb0c");

            // Outbound: optional fields are omitted entirely (serde `default`), not sent as null.
            Assert.Equal(
                "{\"type\":\"ChatSend\",\"data\":{\"channel_id\":\"" + team + "\",\"text\":\"gg\"}}",
                ControlMessage.ChatSend(team, "gg", null, null));
            Assert.Equal(
                "{\"type\":\"ChatSend\",\"data\":{\"channel_id\":\"" + team + "\",\"text\":\"/ping\",\"metadata\":{\"x\":1.5,\"tags\":[\"a\"]},\"client_ref\":\"m1\"}}",
                ControlMessage.ChatSend(team, "/ping", new Dictionary<string, object> { { "x", 1.5 }, { "tags", new List<object> { "a" } } }, "m1"));
            Assert.Equal(
                "{\"type\":\"ChatSendDirect\",\"data\":{\"user_id\":\"" + bob + "\",\"text\":\"psst\",\"client_ref\":\"m2\"}}",
                ControlMessage.ChatSendDirect(bob, "psst", null, "m2"));
            Assert.Equal(
                "{\"type\":\"ChatTyping\",\"data\":{\"channel_id\":\"" + team + "\",\"typing\":true}}",
                ControlMessage.ChatTyping(team, true));

            // Inbound channel message as the sender sees it (client_ref echoed) — server omits null fields.
            var own = ControlMessage.Parse("{\"type\":\"ChatMessageReceived\",\"data\":{\"message\":{\"id\":\"11111111-2222-3333-4444-555555555555\"," +
                "\"channel_id\":\"" + team + "\",\"from_user_id\":\"" + bob + "\",\"display_name\":\"Bob\",\"text\":\"gg\"," +
                "\"metadata\":{\"ping\":{\"x\":1}},\"sent_at\":\"2026-09-18T21:24:05.123456Z\",\"client_ref\":\"m1\"}}}").ChatMessage();
            Assert.Equal(Guid.Parse("11111111-2222-3333-4444-555555555555"), own.Id);
            Assert.Equal(team, own.ChannelId);
            Assert.Null(own.ToUserId);
            Assert.Equal(bob, own.FromUserId);
            Assert.Equal("Bob", own.DisplayName);
            Assert.Equal("gg", own.Text);
            Assert.Equal(1.0, MiniJson.GetNumber(MiniJson.AsObject(MiniJson.AsObject(own.Metadata)["ping"]), "x"));
            Assert.Equal(new DateTimeOffset(2026, 9, 18, 21, 24, 5, TimeSpan.Zero).AddTicks(1234560), own.SentAt);
            Assert.Equal("m1", own.ClientRef);
            Assert.True(own.IsOwn);
            Assert.False(own.IsDirect);
            Assert.False(own.IsSystem);

            // Directed system message as a recipient sees it: nil sender, no client_ref, no metadata.
            var sys = ControlMessage.Parse("{\"type\":\"ChatMessageReceived\",\"data\":{\"message\":{\"id\":\"11111111-2222-3333-4444-555555555556\"," +
                "\"from_user_id\":\"00000000-0000-0000-0000-000000000000\",\"display_name\":\"Server\",\"to_user_id\":\"" + bob + "\"," +
                "\"text\":\"Match starts\",\"sent_at\":\"2026-09-18T21:24:06Z\"}}}").ChatMessage();
            Assert.Null(sys.ChannelId);
            Assert.Equal(bob, sys.ToUserId);
            Assert.Null(sys.Metadata);
            Assert.Null(sys.ClientRef);
            Assert.True(sys.IsSystem);
            Assert.True(sys.IsDirect);
            Assert.False(sys.IsOwn);

            var typing = ControlMessage.Parse("{\"type\":\"ParticipantTyping\",\"data\":{\"channel_id\":\"" + team + "\",\"user_id\":\"" + bob + "\",\"typing\":false}}");
            Assert.Equal(team, typing.Id("channel_id"));
            Assert.Equal(bob, typing.Id("user_id"));
            Assert.False(typing.Bool("typing"));

            var err = ControlMessage.Parse("{\"type\":\"Error\",\"data\":{\"code\":\"RATE_LIMIT_EXCEEDED\",\"message\":\"slow down\",\"client_ref\":\"m1\"}}");
            Assert.Equal("m1", err.Str("client_ref"));
            Assert.Null(ControlMessage.Parse("{\"type\":\"Error\",\"data\":{\"code\":\"X\",\"message\":\"y\"}}").Str("client_ref"));
        }

        [Fact]
        public void AudioLevelMatchesServerEncoding()
        {
            // Same vectors as encode_audio_level / decode_audio_level in crates/aurix-common/src/protocol.rs.
            Assert.Equal(0, AudioLevel.Encode(1f));
            Assert.Equal(20, AudioLevel.Encode(0.1f));
            Assert.Equal(40, AudioLevel.Encode(0.01f));
            Assert.Equal(AudioLevel.Silence, AudioLevel.Encode(0f));
            Assert.Equal(AudioLevel.Silence, AudioLevel.Encode(float.NaN));
            Assert.Equal(AudioLevel.Silence, AudioLevel.Encode(1e-9f));
            Assert.Equal(0, AudioLevel.Encode(4f)); // clipped input clamps to full scale
            Assert.Equal(1f, AudioLevel.Decode(0));
            Assert.InRange(AudioLevel.Decode(20), 0.0999f, 0.1001f);
            Assert.Equal(0f, AudioLevel.Decode(AudioLevel.Silence));
            Assert.Equal(0f, AudioLevel.Decode(255));

            var tone = new float[960];
            for (int i = 0; i < tone.Length; i++) tone[i] = 0.5f * (float)Math.Sin(2 * Math.PI * 440 * i / 48000.0);
            Assert.InRange(AudioLevel.Rms(tone, tone.Length), 0.35f, 0.36f);
        }

        [Fact]
        public void EnergyPacketCarriesLevelByteAndServerStripsIt()
        {
            var key = Keys(3);
            var opus = new byte[] { 0xF8, 0x01, 0x02 };
            var pkt = AurxPacket.AudioWithLevel(1, 960, 5, 0xAB, level: 42, opus);
            Assert.True((pkt.Header.Flags & PacketFlags.Energy) != 0);
            Assert.Equal(new byte[] { 42, 0xF8, 0x01, 0x02 }, pkt.Payload);

            Assert.True(AurxPacket.TryDecode(pkt.Seal(key), out var decoded, out _));
            Assert.True(decoded.Open(key));
            Assert.Equal((byte)42, decoded.TakeAudioLevel());
            Assert.True((decoded.Header.Flags & PacketFlags.Energy) == 0);
            Assert.Equal(opus, decoded.Payload);
            Assert.Equal(3, decoded.Header.PayloadLength);
            Assert.Null(decoded.TakeAudioLevel());

            // Out-of-range level bytes clamp to silence; a bare frame has no level.
            Assert.Equal(AudioLevel.Silence, AurxPacket.AudioWithLevel(1, 0, 5, 1, 200, opus).Payload[0]);
            Assert.Null(AurxPacket.Audio(1, 0, 5, 1, opus).TakeAudioLevel());
        }

        [Fact]
        public void VoiceActivityDetectorHasHangover()
        {
            var vad = new VoiceActivityDetector { Threshold = 0.01f, HangoverFrames = 3, Smoothing = 0f };
            var loud = new float[960]; for (int i = 0; i < loud.Length; i++) loud[i] = 0.2f;
            var quiet = new float[960];

            Assert.False(vad.Process(quiet, quiet.Length));
            Assert.False(vad.Speaking);
            Assert.Equal(AudioLevel.Silence, vad.Level);

            Assert.True(vad.Process(loud, loud.Length));
            Assert.True(vad.Speaking);
            Assert.Equal(14, vad.Level); // -20*log10(0.2) ≈ 13.98
            Assert.InRange(vad.Energy, 0.199f, 0.201f);

            Assert.False(vad.Process(quiet, quiet.Length)); // 1 quiet frame: still speaking
            Assert.False(vad.Process(quiet, quiet.Length)); // 2
            Assert.True(vad.Process(quiet, quiet.Length));  // 3 → speech over
            Assert.False(vad.Speaking);

            vad.Process(loud, loud.Length);
            vad.Reset();
            Assert.False(vad.Speaking);
            Assert.Equal(0f, vad.Energy);
        }

        [Fact]
        public void ChannelEnergyMessageParses()
        {
            var team = Guid.Parse("aaaaaaaa-0000-0000-0000-000000000001");
            var bob = Guid.Parse("bbbbbbbb-0000-0000-0000-000000000002");
            var msg = ControlMessage.Parse("{\"type\":\"ChannelEnergy\",\"data\":{\"channel_id\":\"" + team + "\",\"levels\":[" +
                "{\"user_id\":\"" + bob + "\",\"energy\":0.25},{\"user_id\":\"not-a-uuid\",\"energy\":0.5},{\"user_id\":\"" + team + "\",\"energy\":7}]}}");
            Assert.Equal(team, msg.Id("channel_id"));
            var levels = msg.Levels();
            Assert.Equal(2, levels.Count);
            Assert.Equal(bob, levels[0].UserId);
            Assert.Equal(0.25f, levels[0].Energy);
            Assert.Equal(1f, levels[1].Energy); // clamped
            Assert.Empty(ControlMessage.Parse("{\"type\":\"ChannelEnergy\",\"data\":{\"channel_id\":\"" + team + "\"}}").Levels());
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
