using System;
using System.Collections.Generic;
using System.Security.Cryptography;
using Aurix.Protocol;
using Xunit;

namespace Aurix.Voice.Tests
{
    /// <summary>
    /// Group E2EE primitives against the vectors shared with <c>crates/aurix-common/src/e2ee.rs</c>
    /// and <c>sdk/web/test/e2ee.test.mjs</c>, plus the sender-key state machine.
    /// </summary>
    public class E2eeTests
    {
        private static byte[] Hex(string s)
        {
            var b = new byte[s.Length / 2];
            for (int i = 0; i < b.Length; i++) b[i] = Convert.ToByte(s.Substring(2 * i, 2), 16);
            return b;
        }

        private static string ToHex(byte[] b) => BitConverter.ToString(b).Replace("-", "").ToLowerInvariant();

        private static byte[] Fill(byte v)
        {
            var b = new byte[32];
            for (int i = 0; i < 32; i++) b[i] = v;
            return b;
        }

        [Fact]
        public void HkdfMatchesRfc5869Case1()
        {
            var ikm = new byte[22];
            for (int i = 0; i < ikm.Length; i++) ikm[i] = 0x0b;
            var salt = new byte[13];
            for (int i = 0; i < salt.Length; i++) salt[i] = (byte)i;
            var info = new byte[10];
            for (int i = 0; i < info.Length; i++) info[i] = (byte)(0xf0 + i);
            Assert.Equal(
                "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865",
                ToHex(E2ee.HkdfSha256(salt, ikm, info, 42)));
        }

        [Fact]
        public void X25519MatchesRfc7748Vectors()
        {
            // RFC 7748 §5.2, first vector.
            var k = Hex("a546e36bf0527c9d3b16154b82465edd62144c0ac1fc5a18506a2244ba449ac4");
            var u = Hex("e6db6867583030db3594c1a424b15f7c726624ec26b3353b10a903a6d0ab1c4c");
            Assert.Equal("c3da55379de9c6908e94ea4df28d084f32eccf03491c71f754b4075577a28552", ToHex(X25519.ScalarMult(k, u)));
            // §6.1 Alice/Bob.
            var a = Hex("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a");
            var b = Hex("5dab087e624a8a4b79e17f8b83800ee66f3bb1292618b6fd1c2f8b27ff88e0eb");
            Assert.Equal("8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a", ToHex(X25519.PublicKey(a)));
            Assert.Equal("de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f", ToHex(X25519.PublicKey(b)));
            var shared = X25519.ScalarMult(a, X25519.PublicKey(b));
            Assert.Equal("4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742", ToHex(shared));
            Assert.Equal(ToHex(shared), ToHex(X25519.ScalarMult(b, X25519.PublicKey(a))));
            // Low-order point yields all zeros.
            Assert.True(X25519.IsZero(X25519.ScalarMult(a, new byte[32])));
        }

        [Fact]
        public void FrameTestVector()
        {
            var secret = new byte[32];
            for (int i = 0; i < 32; i++) secret[i] = (byte)i;
            using var key = E2eeSenderKey.Derive(1, secret);
            Assert.Equal("495d7612bbcfa75aada371e8facda163a223fac6a9469018e159a3d825ead1c6", ToHex(key.EncForTests));
            Assert.Equal("386f83d423b3d5a81e15c2c09e21d9588c2768da88c96bef6f1f5633be1ff64e", ToHex(key.Auth));
            Assert.Equal("6ca0258f60de84e095dc2066", ToHex(key.Salt));
            var plain = new byte[] { 0xf8, 0xff, 0xfe, 0x00, 0x01 };
            var frame = key.Seal(0x01020304, plain);
            Assert.Equal("0101020304eb1575b9e6b52709cbdab928367122", ToHex(frame));
            Assert.Equal(plain, key.Open(frame));
        }

        [Fact]
        public void FrameRoundTripAndTamper()
        {
            using var key = E2eeSenderKey.Derive(3, Fill(7));
            var plain = System.Text.Encoding.ASCII.GetBytes("opus frame");
            var frame = key.Seal(42, plain);
            Assert.Equal(plain.Length + E2ee.FrameOverhead, frame.Length);
            Assert.True(E2eeSenderKey.Peek(frame, 0, frame.Length, out var gen, out var counter));
            Assert.Equal(3, gen);
            Assert.Equal(42u, counter);
            Assert.Equal(plain, key.Open(frame));
            var bad = (byte[])frame.Clone();
            bad[7] ^= 1;
            Assert.Null(key.Open(bad));
            var wrongGen = (byte[])frame.Clone();
            wrongGen[0] = 4;
            Assert.Null(key.Open(wrongGen));
            Assert.Null(key.Open(frame, 0, E2ee.FrameOverhead - 1));
            var empty = key.Seal(0, Array.Empty<byte>());
            Assert.Empty(key.Open(empty));
            // Offset/length addressing (frames arrive inside larger packet buffers).
            var padded = new byte[frame.Length + 8];
            Buffer.BlockCopy(frame, 0, padded, 3, frame.Length);
            Assert.Equal(plain, key.Open(padded, 3, frame.Length));
        }

        [Fact]
        public void WrapTestVector()
        {
            var alice = E2eeIdentityKey.FromBytes(Fill(1));
            var bob = E2eeIdentityKey.FromBytes(Fill(2));
            Assert.Equal("a4e09292b651c278b9772c569f5fa9bb13d906b46ab68c9df9dc2b4409f8a209", ToHex(alice.PublicKey));
            Assert.Equal("ce8d3ad1ccb633ec7b70c17814a5c76ecd029685050d344745ba05870e587d59", ToHex(bob.PublicKey));
            Assert.Equal("1a92f23852dc908d97316a3b13578281196c1dd73d9ae5e313f3fb6b8954bf55", alice.Fingerprint);
            alice.WrapKeys(bob.PublicKey, alice.PublicKey, bob.PublicKey, out var enc, out var auth);
            Assert.Equal("d769905c2b8b1019b7e9da448b11c6b59106e1f9858efe13763ff2e3a3ab5507", ToHex(enc));
            Assert.Equal("e56126305aeea09d0929942837301ccdca1c74954283a0895accb5a55e6253fc", ToHex(auth));
            var nonce = new byte[12];
            for (int i = 0; i < 12; i++) nonce[i] = 0x42;
            var wrapped = alice.Wrap(bob.PublicKey, 5, Fill(9), nonce);
            Assert.Equal(
                "42424242424242424242424249d1a4e521c229f2380da9fe507d631f090452c1bbc07032dcda4bba6be02de573dc00012e371ae34d5fbd125518632b",
                ToHex(wrapped));
            Assert.Equal(Fill(9), bob.Unwrap(alice.PublicKey, 5, wrapped));
        }

        [Fact]
        public void WrapRoundTripDirectionAndTamper()
        {
            var alice = E2eeIdentityKey.FromBytes(Fill(1));
            var bob = E2eeIdentityKey.FromBytes(Fill(2));
            var carol = E2eeIdentityKey.FromBytes(Fill(3));
            var secret = Fill(9);
            var wrapped = alice.Wrap(bob.PublicKey, 5, secret);
            Assert.Equal(E2ee.WrappedKeyLen, wrapped.Length);
            Assert.Equal(secret, bob.Unwrap(alice.PublicKey, 5, wrapped));
            Assert.Throws<CryptographicException>(() => bob.Unwrap(alice.PublicKey, 6, wrapped));
            Assert.Throws<CryptographicException>(() => carol.Unwrap(alice.PublicKey, 5, wrapped));
            Assert.Throws<CryptographicException>(() => bob.Unwrap(carol.PublicKey, 5, wrapped));
            var bad = (byte[])wrapped.Clone();
            bad[20] ^= 0x80;
            Assert.Throws<CryptographicException>(() => bob.Unwrap(alice.PublicKey, 5, bad));
            var shortWrap = new byte[E2ee.WrappedKeyLen - 1];
            Buffer.BlockCopy(wrapped, 0, shortWrap, 0, shortWrap.Length);
            Assert.Throws<CryptographicException>(() => bob.Unwrap(alice.PublicKey, 5, shortWrap));
            Assert.Throws<CryptographicException>(() => alice.Wrap(new byte[32], 0, secret));
            // Export/import keeps the identity.
            Assert.Equal(alice.Fingerprint, E2eeIdentityKey.FromBytes(alice.ExportSecret()).Fingerprint);
            Assert.NotEqual(E2eeIdentityKey.Generate().Fingerprint, E2eeIdentityKey.Generate().Fingerprint);
        }

        [Fact]
        public void ReplayWindow()
        {
            var r = new E2eeReplayState();
            Assert.True(r.Accept(10));
            Assert.False(r.Accept(10));
            Assert.True(r.Accept(8));
            Assert.False(r.Accept(8));
            Assert.True(r.Accept(500));
            Assert.False(r.Accept(10));
            Assert.True(r.Accept(499));
            Assert.False(r.Accept(499));
            // Window edges across the two 64-bit halves.
            Assert.True(r.Accept(500 - 127));
            Assert.False(r.Accept(500 - 127));
            Assert.False(r.Accept(500 - 128));
            Assert.True(r.Accept(500 - 64));
            Assert.True(r.Accept(500 - 63));
            Assert.False(r.Accept(500 - 64));
        }

        [Fact]
        public void PeerKeysKeepFourGenerationsAndRejectReplay()
        {
            var keys = new E2eePeerKeys();
            var frames = new Dictionary<byte, byte[]>();
            for (byte g = 0; g < 5; g++)
            {
                var k = E2eeSenderKey.Derive(g, Fill(g));
                frames[g] = k.Seal(1, new byte[] { g });
                keys.Insert(E2eeSenderKey.Derive(g, Fill(g)));
            }
            Assert.False(keys.HasGeneration(0));
            Assert.Null(keys.Open(frames[0]));
            for (byte g = 1; g < 5; g++)
            {
                Assert.True(keys.HasGeneration(g));
                Assert.Equal(new byte[] { g }, keys.Open(frames[g]));
                Assert.Null(keys.Open(frames[g]));
            }
        }

        // ---- group state machine -------------------------------------------------------------

        private static void Drive(Dictionary<Guid, E2eeGroup> groups, Guid from, List<E2eeOutgoing> msgs)
        {
            var pk = groups[from].Identity.PublicKey;
            foreach (var m in msgs)
            {
                if (m.Kind == E2eeOutgoingKind.Hello)
                {
                    foreach (var u in new List<Guid>(groups.Keys))
                    {
                        if (u == from) continue;
                        var outMsgs = groups[u].OnHello(m.ChannelId, from, pk, out _);
                        Drive(groups, u, outMsgs);
                    }
                }
                else
                {
                    var outMsgs = groups[m.To].OnSenderKey(m.ChannelId, from, pk, m.Generation, m.Wrapped, out _);
                    Drive(groups, m.To, outMsgs);
                }
            }
        }

        private static void Settle(Dictionary<Guid, E2eeGroup> groups)
        {
            for (int i = 0; i < 4; i++)
                foreach (var u in new List<Guid>(groups.Keys))
                    Drive(groups, u, groups[u].Rotate(false));
        }

        private static void AssertHear(Dictionary<Guid, E2eeGroup> groups, Guid speaker, Guid listener, bool expect)
        {
            var frame = groups[speaker].Encrypt(new byte[] { 1, 2, 3 });
            var plain = groups[listener].Decrypt(speaker, frame);
            if (expect) Assert.Equal(new byte[] { 1, 2, 3 }, plain);
            else Assert.Null(plain);
        }

        [Fact]
        public void JoinLeaveRotationAndForwardBackwardSecrecy()
        {
            var ch = Guid.NewGuid();
            var alice = Guid.NewGuid();
            var bob = Guid.NewGuid();
            var carol = Guid.NewGuid();
            var groups = new Dictionary<Guid, E2eeGroup>
            {
                [alice] = new E2eeGroup(E2eeIdentityKey.Generate()),
                [bob] = new E2eeGroup(E2eeIdentityKey.Generate()),
            };
            Drive(groups, alice, groups[alice].Joined(ch));
            Drive(groups, bob, groups[bob].Joined(ch));
            Settle(groups);
            Assert.True(groups[alice].HasKeyFor(bob));
            Assert.True(groups[bob].HasKeyFor(alice));
            AssertHear(groups, alice, bob, true);
            AssertHear(groups, bob, alice, true);
            Assert.Equal(groups[bob].Identity.Fingerprint, groups[alice].PeerFingerprint(bob));
            Assert.Equal(1, groups[alice].Generation);

            // A frame sealed before Carol arrives must stay unreadable to her.
            var before = groups[alice].Encrypt(new byte[] { 9 });
            groups[carol] = new E2eeGroup(E2eeIdentityKey.Generate());
            Drive(groups, carol, groups[carol].Joined(ch));
            Settle(groups);
            Assert.Equal(2, groups[alice].Generation);
            Assert.Null(groups[carol].Decrypt(alice, before));
            AssertHear(groups, alice, carol, true);
            AssertHear(groups, carol, bob, true);
            // Bob still opens Alice's previous generation (frames in flight across the rotation).
            Assert.Equal(new byte[] { 9 }, groups[bob].Decrypt(alice, before));

            // Carol leaves: Alice and Bob rotate away from her.
            groups[alice].PeerLeft(ch, carol);
            groups[bob].PeerLeft(ch, carol);
            groups[carol].Left(ch);
            Assert.True(groups[alice].RotationPending);
            Settle(groups);
            Assert.Equal(3, groups[alice].Generation);
            AssertHear(groups, alice, carol, false);
            AssertHear(groups, alice, bob, true);
            Assert.False(groups[alice].HasKeyFor(carol));
            Assert.Empty(groups[carol].DecryptablePeers());
        }

        [Fact]
        public void SimultaneousJoinAndRekeyedPeer()
        {
            var ch = Guid.NewGuid();
            var alice = Guid.NewGuid();
            var bob = Guid.NewGuid();
            var groups = new Dictionary<Guid, E2eeGroup>
            {
                [alice] = new E2eeGroup(E2eeIdentityKey.Generate()),
                [bob] = new E2eeGroup(E2eeIdentityKey.Generate()),
            };
            // Both hellos before either answer.
            var ha = groups[alice].Joined(ch);
            var hb = groups[bob].Joined(ch);
            Drive(groups, alice, ha);
            Drive(groups, bob, hb);
            Settle(groups);
            AssertHear(groups, alice, bob, true);
            AssertHear(groups, bob, alice, true);

            // Bob reconnects with a new identity: Alice sees the key change and re-keys him.
            var oldFp = groups[bob].Identity.Fingerprint;
            groups[bob] = new E2eeGroup(E2eeIdentityKey.Generate());
            var outMsgs = groups[alice].OnHello(ch, bob, groups[bob].Identity.PublicKey, out var change);
            Assert.Empty(outMsgs);
            Assert.Equal(E2eePeerChangeKind.KeyChanged, change.Kind);
            Assert.Equal(oldFp, change.PreviousFingerprint);
            Assert.False(groups[alice].HasKeyFor(bob));
            Drive(groups, bob, groups[bob].Joined(ch));
            Settle(groups);
            AssertHear(groups, alice, bob, true);
            AssertHear(groups, bob, alice, true);

            // A key wrapped for someone else (or forged) is rejected without learning anything.
            var mallory = new E2eeGroup(E2eeIdentityKey.Generate());
            var forged = mallory.Identity.Wrap(groups[alice].Identity.PublicKey, 7, Fill(1));
            Assert.Throws<CryptographicException>(() =>
                groups[alice].OnSenderKey(ch, bob, groups[bob].Identity.PublicKey, 7, forged, out _));
            // Messages for channels we are not in are ignored.
            Assert.Empty(groups[alice].OnHello(Guid.NewGuid(), Guid.NewGuid(), mallory.Identity.PublicKey, out var none));
            Assert.False(none.Changed);
        }

        [Fact]
        public void RejoinForgetsAbsentPeersAndResetDropsEverything()
        {
            var ch = Guid.NewGuid();
            var alice = Guid.NewGuid();
            var bob = Guid.NewGuid();
            var carol = Guid.NewGuid();
            var groups = new Dictionary<Guid, E2eeGroup>
            {
                [alice] = new E2eeGroup(E2eeIdentityKey.Generate()),
                [bob] = new E2eeGroup(E2eeIdentityKey.Generate()),
                [carol] = new E2eeGroup(E2eeIdentityKey.Generate()),
            };
            foreach (var u in new[] { alice, bob, carol }) Drive(groups, u, groups[u].Joined(ch));
            Settle(groups);
            Assert.Equal(2, groups[alice].DecryptablePeers().Count);

            var gone = new List<Guid>();
            var outMsgs = groups[alice].Rejoined(ch, new HashSet<Guid> { alice, bob }, gone);
            Assert.Single(outMsgs);
            Assert.Equal(E2eeOutgoingKind.Hello, outMsgs[0].Kind);
            Assert.Equal(new[] { carol }, gone);
            Assert.True(groups[alice].RotationPending);
            Assert.True(groups[alice].HasKeyFor(bob));
            Assert.False(groups[alice].HasKeyFor(carol));

            groups[alice].Reset();
            Assert.False(groups[alice].Active);
            Assert.Empty(groups[alice].DecryptablePeers());
            Assert.Null(groups[alice].PeerFingerprint(bob));
            Assert.Equal(groups[alice].Identity.Fingerprint, E2ee.Fingerprint(groups[alice].Identity.PublicKey));
        }

        [Fact]
        public void ControlMessagesCarryTheRustWireFields()
        {
            var pk = Fill(0x42);
            var hello = ControlMessage.Parse(ControlMessage.E2eeHello(null, pk));
            Assert.Equal("E2eeHello", hello.Type);
            Assert.Equal(E2ee.EncodeBytes(pk), hello.Str("public_key"));
            Assert.Null(MiniJson.GetGuid(hello.Data, "channel_id"));
            Assert.False(hello.Data.ContainsKey("user_id"), "user_id is server-populated");

            var ch = Guid.Parse("11111111-2222-3333-4444-555555555555");
            var to = Guid.Parse("11111111-2222-3333-4444-555555555556");
            hello = ControlMessage.Parse(ControlMessage.E2eeHello(ch, pk));
            Assert.Equal(ch, MiniJson.GetGuid(hello.Data, "channel_id"));

            var wrapped = new byte[E2ee.WrappedKeyLen];
            for (int i = 0; i < wrapped.Length; i++) wrapped[i] = (byte)i;
            var key = ControlMessage.Parse(ControlMessage.E2eeSenderKey(ch, to, pk, 200, wrapped));
            Assert.Equal("E2eeSenderKey", key.Type);
            Assert.Equal(ch, key.Id("channel_id"));
            Assert.Equal(to, key.Id("to"));
            Assert.Equal(E2ee.EncodeBytes(pk), key.Str("public_key"));
            Assert.Equal(200, key.Num("generation"));
            Assert.Equal(wrapped, E2ee.DecodeBytes(key.Str("key")));
            Assert.False(key.Data.ContainsKey("from"), "from is server-populated");
        }

        [Fact]
        public async System.Threading.Tasks.Task TransportFlagsSealedFramesAndKeepsSessionSealingSeparate()
        {
            var sessionKey = Fill(0x11);
            var tunnel = new FakeTunnel(sessionKey);
            using var media = Aurix.Transport.MediaTransport.OverTunnel(tunnel, Guid.NewGuid(), 0x1234abcd, sessionKey);
            await media.BindAsync(System.Threading.CancellationToken.None, attempts: 2, timeoutMs: 1000);
            tunnel.Uplink.TryDequeue(out _);

            var group = new E2eeGroup(E2eeIdentityKey.Generate());
            var opus = new byte[] { 0xF8, 1, 2, 3, 4, 5 };
            var sealedFrame = group.Encrypt(opus);
            uint hash = AurxPacket.ChannelIdHash(Guid.NewGuid());
            media.SendAudioE2ee(hash, 960, sealedFrame, level: 12);
            media.SendAudio(hash, 1920, Aurix.Audio.AudioCodec.Opus, opus);

            var deadline = DateTime.UtcNow.AddSeconds(3);
            while (tunnel.Uplink.Count < 2 && DateTime.UtcNow < deadline) await System.Threading.Tasks.Task.Delay(10);
            Assert.True(tunnel.Uplink.TryDequeue(out var e2ee));
            Assert.True(tunnel.Uplink.TryDequeue(out var plain));
            // Session sealing (FakeTunnel opened both with the media key) is independent of the E2EE layer.
            Assert.True((e2ee.Header.Flags & PacketFlags.E2ee) != 0);
            Assert.True((plain.Header.Flags & PacketFlags.E2ee) == 0);
            Assert.Equal((byte)12, e2ee.TakeAudioLevel());
            Assert.Equal(sealedFrame, e2ee.Payload);
            Assert.Equal(opus, plain.Payload);
            Assert.Equal(E2ee.FrameOverhead, e2ee.Payload.Length - plain.Payload.Length);

            // Downlink keeps the flag so the client knows to open the payload before decoding.
            var down = AurxPacket.Audio(7, 4800, 0x99, hash, sealedFrame);
            down.Header.Flags |= PacketFlags.E2ee;
            tunnel.Deliver(down);
            tunnel.Deliver(AurxPacket.Audio(8, 5760, 0x99, hash, opus));
            Assert.True(media.TryDequeueAudio(out var heard));
            Assert.True(heard.E2ee);
            Assert.False(heard.Mixed);
            Assert.Equal(sealedFrame, heard.Payload);
            Assert.True(media.TryDequeueAudio(out heard));
            Assert.False(heard.E2ee);
            Assert.Equal(opus, heard.Payload);
        }

        [Fact]
        public void ClientExposesItsIdentityAndRefusesRotationWhileDisconnected()
        {
            var client = new AurixVoiceClient("ws://127.0.0.1:1/ws", "token");
            Assert.True(client.E2ee);
            Assert.Equal(0, client.E2eeGeneration);
            var secret = client.ExportE2eeIdentity();
            Assert.Equal(E2ee.SecretLen, secret.Length);
            Assert.Equal(E2eeIdentityKey.FromBytes(secret).Fingerprint, client.E2eeFingerprint);

            var other = new AurixVoiceClient("ws://127.0.0.1:1/ws", "token");
            Assert.NotEqual(client.E2eeFingerprint, other.E2eeFingerprint);
            other.SetE2eeIdentity(secret);
            Assert.Equal(client.E2eeFingerprint, other.E2eeFingerprint);

            Assert.Null(client.RotateE2eeKey());
            Assert.False(client.IsChannelEncrypted(Guid.NewGuid()));
            Assert.Null(client.E2eePeerFingerprint(Guid.NewGuid()));
            Assert.Empty(client.GetE2eeDecryptablePeers());
            var stats = client.GetStats();
            Assert.Equal(0, stats.FramesE2ee);
            Assert.Equal(0, stats.E2eeUndecryptable);
        }
    }
}
