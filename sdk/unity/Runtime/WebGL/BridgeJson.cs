using System;
using System.Collections.Generic;
using System.Globalization;
using Aurix.Protocol;

namespace Aurix.WebGL
{
    /// <summary>
    /// Readers for the Web SDK's camelCase JSON objects as they cross the bridge (the SDK types, not the
    /// snake_case wire protocol — <see cref="ControlMessage"/> reads the latter).
    /// </summary>
    internal static class BridgeJson
    {
        internal static Dictionary<string, object> Obj(Dictionary<string, object> o, string key) =>
            o != null && o.TryGetValue(key, out var v) ? MiniJson.AsObject(v) : null;

        internal static List<object> Arr(Dictionary<string, object> o, string key) =>
            o != null && o.TryGetValue(key, out var v) ? MiniJson.AsArray(v) : null;

        internal static Guid Id(Dictionary<string, object> o, string key) => MiniJson.GetGuid(o, key) ?? Guid.Empty;

        internal static float F32(Dictionary<string, object> o, string key, double fallback = 0) => (float)MiniJson.GetNumber(o, key, fallback);

        internal static long I64(Dictionary<string, object> o, string key) => (long)MiniJson.GetNumber(o, key);

        internal static DateTimeOffset Time(Dictionary<string, object> o, string key)
        {
            var s = MiniJson.GetString(o, key);
            return s != null && DateTimeOffset.TryParse(s, CultureInfo.InvariantCulture, DateTimeStyles.RoundtripKind, out var ts)
                ? ts
                : DateTimeOffset.MinValue;
        }

        internal static List<string> Strings(Dictionary<string, object> o, string key)
        {
            var list = new List<string>();
            var arr = Arr(o, key);
            if (arr == null) return list;
            foreach (var item in arr)
                if (item is string s) list.Add(s);
            return list;
        }

        internal static VoiceConnectionState ConnectionState(string s)
        {
            switch (s)
            {
                case "connecting": return VoiceConnectionState.Connecting;
                case "connected":
                case "media-connecting": return VoiceConnectionState.Connected;
                case "media-connected": return VoiceConnectionState.MediaBound;
                case "reconnecting": return VoiceConnectionState.Reconnecting;
                case "failed": return VoiceConnectionState.Failed;
                default: return VoiceConnectionState.Disconnected;
            }
        }

        internal static SessionInfo Session(Dictionary<string, object> o)
        {
            if (o == null) return null;
            var failover = Strings(o, "failover");
            return new SessionInfo
            {
                SessionId = Id(o, "sessionId"),
                Ssrc = MiniJson.GetUInt32(o, "ssrc"),
                MediaAddr = null,
                Resumed = MiniJson.GetBool(o, "resumed"),
                Migrated = MiniJson.GetBool(o, "migrated"),
                Endpoint = MiniJson.GetString(o, "endpoint"),
                Failover = failover,
                MediaTunnel = false,
                DownlinkMix = false,
                Translation = Translation(Obj(o, "translation")),
            };
        }

        internal static TranslationInfo Translation(Dictionary<string, object> o)
        {
            if (o == null) return null;
            return new TranslationInfo { Speech = MiniJson.GetBool(o, "speech"), Languages = Strings(o, "languages") };
        }

        internal static TranslationPrefs TranslationPrefs(Dictionary<string, object> o)
        {
            return new TranslationPrefs
            {
                Language = MiniJson.GetString(o, "language"),
                SpokenLanguage = MiniJson.GetString(o, "spokenLanguage"),
                Speech = MiniJson.GetBool(o, "speech"),
            };
        }

        internal static Guid SessionUser(Dictionary<string, object> o) => Id(o, "userId");

        internal static Participant Participant(Dictionary<string, object> o)
        {
            if (o == null) return null;
            return new Participant
            {
                UserId = Id(o, "userId"),
                DisplayName = MiniJson.GetString(o, "displayName") ?? string.Empty,
                Ssrc = MiniJson.GetUInt32(o, "ssrc"),
                Role = ControlMessage.ParseRole(MiniJson.GetString(o, "role")),
                IsMuted = MiniJson.GetBool(o, "muted"),
                IsServerMuted = MiniJson.GetBool(o, "serverMuted"),
                IsSpeaking = MiniJson.GetBool(o, "speaking"),
                Energy = Math.Max(0f, Math.Min(1f, F32(o, "energy"))),
            };
        }

        internal static List<Participant> Participants(List<object> arr)
        {
            var list = new List<Participant>();
            if (arr == null) return list;
            foreach (var item in arr)
            {
                var p = Participant(MiniJson.AsObject(item));
                if (p != null) list.Add(p);
            }
            return list;
        }

        internal static void Apply(Participant target, Participant update)
        {
            target.DisplayName = update.DisplayName;
            target.Ssrc = update.Ssrc;
            target.Role = update.Role;
            target.IsMuted = update.IsMuted;
            target.IsServerMuted = update.IsServerMuted;
            target.IsSpeaking = update.IsSpeaking;
            target.Energy = update.Energy;
        }

        internal static ChannelInfo? ChannelInfo(Dictionary<string, object> o)
        {
            if (o == null) return null;
            return new ChannelInfo
            {
                Role = ControlMessage.ParseRole(MiniJson.GetString(o, "role")),
                ParticipantCount = MiniJson.GetUInt32(o, "participantCount"),
                HiddenListeners = MiniJson.GetBool(o, "hiddenListeners"),
                Transcription = MiniJson.GetBool(o, "transcription"),
                SafetyVoice = MiniJson.GetBool(o, "safetyVoice"),
            };
        }

        internal static ChannelScope? ChannelScope(Dictionary<string, object> o)
        {
            if (o == null) return null;
            return new ChannelScope
            {
                RosterRadius = o.TryGetValue("rosterRadius", out var r) && r is double rr ? (float?)rr : null,
                TextRadius = o.TryGetValue("textRadius", out var t) && t is double tt ? (float?)tt : null,
            };
        }

        internal static TransmissionMode Transmission(object v)
        {
            var o = MiniJson.AsObject(v);
            if (o == null) return TransmissionMode.All;
            switch (MiniJson.GetString(o, "type") ?? MiniJson.GetString(o, "mode"))
            {
                case "none": return TransmissionMode.None;
                case "single":
                    var id = MiniJson.GetGuid(o, "channelId") ?? MiniJson.GetGuid(o, "channel_id");
                    return id.HasValue && id.Value != Guid.Empty ? TransmissionMode.Single(id.Value) : TransmissionMode.All;
                default: return TransmissionMode.All;
            }
        }

        internal static Dictionary<string, object> TransmissionToBridge(TransmissionMode mode)
        {
            switch (mode.Kind)
            {
                case TransmissionKind.None: return new Dictionary<string, object> { { "type", "none" } };
                case TransmissionKind.Single: return new Dictionary<string, object> { { "type", "single" }, { "channelId", mode.ChannelId } };
                default: return new Dictionary<string, object> { { "type", "all" } };
            }
        }

        internal static ReceiverPreferences Preferences(Dictionary<string, object> o)
        {
            var prefs = new ReceiverPreferences();
            if (o == null) return prefs;
            foreach (var s in Strings(o, "blockedUsers"))
                if (Guid.TryParse(s, out var g)) prefs.BlockedUsers.Add(g);
            var mutes = Arr(o, "localMutes");
            if (mutes != null)
                foreach (var item in mutes)
                {
                    var m = MiniJson.AsObject(item);
                    if (m == null) continue;
                    prefs.LocalMutes.Add(new LocalMute { UserId = Id(m, "user_id"), ChannelId = MiniJson.GetGuid(m, "channel_id") });
                }
            var volumes = Arr(o, "volumes");
            if (volumes != null)
                foreach (var item in volumes)
                {
                    var v = MiniJson.AsObject(item);
                    if (v == null) continue;
                    prefs.Volumes.Add(new ParticipantVolume { UserId = Id(v, "user_id"), Volume = F32(v, "volume", 1.0) });
                }
            if (o.TryGetValue("transmission", out var tv)) prefs.Transmission = Transmission(tv);
            prefs.FocusChannel = MiniJson.GetGuid(o, "focusChannel");
            return prefs;
        }

        internal static ChatMessage Message(Dictionary<string, object> o)
        {
            if (o == null) return null;
            return new ChatMessage
            {
                Id = Id(o, "id"),
                ChannelId = MiniJson.GetGuid(o, "channelId"),
                FromUserId = Id(o, "fromUserId"),
                DisplayName = MiniJson.GetString(o, "displayName") ?? string.Empty,
                ToUserId = MiniJson.GetGuid(o, "toUserId"),
                Text = MiniJson.GetString(o, "text") ?? string.Empty,
                Metadata = o.TryGetValue("metadata", out var md) ? md : null,
                SentAt = Time(o, "sentAt"),
                ClientRef = MiniJson.GetString(o, "clientRef"),
                Offline = MiniJson.GetBool(o, "offline"),
            };
        }

        internal static ChatHistoryPage History(Dictionary<string, object> o, Guid? channelId, Guid? peerUserId)
        {
            var page = new ChatHistoryPage
            {
                ChannelId = channelId,
                PeerUserId = peerUserId,
                NextBefore = MiniJson.GetString(o, "nextBefore"),
                NextAfter = MiniJson.GetString(o, "nextAfter"),
            };
            var arr = Arr(o, "messages");
            if (arr != null)
                foreach (var item in arr)
                {
                    var m = Message(MiniJson.AsObject(item));
                    if (m != null) page.Messages.Add(m);
                }
            return page;
        }

        internal static ChatReadMarker Marker(Dictionary<string, object> o)
        {
            if (o == null) return null;
            return new ChatReadMarker
            {
                UserId = Id(o, "userId"),
                ChannelId = MiniJson.GetGuid(o, "channelId"),
                PeerUserId = MiniJson.GetGuid(o, "peerUserId"),
                MessageId = Id(o, "messageId"),
                MessageSentAt = Time(o, "messageSentAt"),
                ReadAt = Time(o, "readAt"),
            };
        }

        internal static ChatReadMarkers Markers(Dictionary<string, object> o, Guid? channelId, Guid? peerUserId)
        {
            var result = new ChatReadMarkers
            {
                ChannelId = channelId,
                PeerUserId = peerUserId,
                UnreadCount = (int)MiniJson.GetNumber(o, "unreadCount"),
            };
            var arr = Arr(o, "markers");
            if (arr != null)
                foreach (var item in arr)
                {
                    var m = Marker(MiniJson.AsObject(item));
                    if (m != null) result.Markers.Add(m);
                }
            return result;
        }

        internal static Transcript Transcript(Dictionary<string, object> o)
        {
            if (o == null) return null;
            var t = new Transcript
            {
                Id = Id(o, "id"),
                ChannelId = Id(o, "channelId"),
                UserId = Id(o, "userId"),
                Text = MiniJson.GetString(o, "text") ?? string.Empty,
                Language = MiniJson.GetString(o, "language"),
                StartedAt = Time(o, "startedAt"),
                DurationMs = (ulong)Math.Max(0, MiniJson.GetNumber(o, "durationMs")),
            };
            var original = Obj(o, "original");
            if (original != null)
            {
                t.OriginalText = MiniJson.GetString(original, "text") ?? string.Empty;
                t.OriginalLanguage = MiniJson.GetString(original, "language");
            }
            var words = Arr(o, "words");
            if (words != null)
                foreach (var item in words)
                {
                    var w = MiniJson.AsObject(item);
                    if (w == null) continue;
                    t.Words.Add(new TranscriptWord
                    {
                        Word = MiniJson.GetString(w, "word") ?? string.Empty,
                        StartMs = (ulong)Math.Max(0, MiniJson.GetNumber(w, "startMs")),
                        EndMs = (ulong)Math.Max(0, MiniJson.GetNumber(w, "endMs")),
                    });
                }
            return t;
        }

        internal static TtsStatus Tts(Dictionary<string, object> o)
        {
            if (o == null) return null;
            return new TtsStatus
            {
                RequestId = Id(o, "requestId"),
                ClientRef = MiniJson.GetString(o, "clientRef"),
                State = ControlMessage.TtsStateFromWire(MiniJson.GetString(o, "state")),
                DurationMs = o.TryGetValue("durationMs", out var d) && d is double dm ? (ulong?)Math.Max(0, dm) : null,
                Message = MiniJson.GetString(o, "message"),
            };
        }

        internal static NetworkQuality Quality(Dictionary<string, object> o) => new NetworkQuality
        {
            Bars = (int)MiniJson.GetNumber(o, "bars"),
            RFactor = F32(o, "rFactor"),
            Mos = F32(o, "mos"),
            RttMs = F32(o, "rttMs"),
            DownlinkJitterMs = F32(o, "downlinkJitterMs"),
            DownlinkLossPercent = F32(o, "downlinkLossPercent"),
            UplinkJitterMs = F32(o, "uplinkJitterMs"),
            UplinkLossPercent = F32(o, "uplinkLossPercent"),
            UplinkBitrateKbps = MiniJson.GetUInt32(o, "uplinkBitrateKbps"),
            UplinkPacketsReceived = I64(o, "uplinkPacketsReceived"),
            UplinkPacketsLost = I64(o, "uplinkPacketsLost"),
        };

        internal static WebGLE2eeStats E2eeStats(Dictionary<string, object> o) => new WebGLE2eeStats
        {
            FramesE2ee = (long)MiniJson.GetNumber(o, "framesE2ee"),
            Undecryptable = (long)MiniJson.GetNumber(o, "undecryptable"),
            Held = (long)MiniJson.GetNumber(o, "held"),
        };

        internal static WebGLStats Stats(Dictionary<string, object> o) => new WebGLStats
        {
            RttMs = F32(o, "rttMs"),
            RttMinMs = F32(o, "rttMinMs"),
            RttAvgMs = F32(o, "rttAvgMs"),
            RttMaxMs = F32(o, "rttMaxMs"),
            IceRttMs = F32(o, "iceRttMs"),
            JitterMs = F32(o, "jitterMs"),
            LossPercent = F32(o, "lossPercent"),
            PacketsReceived = I64(o, "packetsReceived"),
            PacketsLost = I64(o, "packetsLost"),
            BytesReceived = I64(o, "bytesReceived"),
            ConcealedSamples = I64(o, "concealedSamples"),
            PacketsDiscarded = I64(o, "packetsDiscarded"),
            JitterBufferDelayMs = F32(o, "jitterBufferDelayMs"),
            PacketsSent = I64(o, "packetsSent"),
            BytesSent = I64(o, "bytesSent"),
            RemoteLossPercent = F32(o, "remoteLossPercent"),
            RemoteJitterMs = F32(o, "remoteJitterMs"),
            RFactor = F32(o, "rFactor"),
            Mos = F32(o, "mos"),
            Bars = (int)MiniJson.GetNumber(o, "bars"),
        };

        internal static IReadOnlyList<WebGLParticipantStream> ParticipantStreams(List<object> arr)
        {
            var list = new List<WebGLParticipantStream>();
            if (arr == null) return list;
            foreach (var item in arr)
            {
                var s = MiniJson.AsObject(item);
                if (s == null) continue;
                list.Add(new WebGLParticipantStream
                {
                    Mid = MiniJson.GetString(s, "mid") ?? string.Empty,
                    UserId = MiniJson.GetGuid(s, "userId"),
                    Live = MiniJson.GetBool(s, "live"),
                });
            }
            return list;
        }

        internal static WebGLAudioDevices Devices(Dictionary<string, object> o)
        {
            var devices = new WebGLAudioDevices();
            Fill(devices.Inputs, Arr(o, "inputs"), false);
            Fill(devices.Outputs, Arr(o, "outputs"), true);
            return devices;
        }

        private static void Fill(List<WebGLAudioDevice> list, List<object> arr, bool output)
        {
            if (arr == null) return;
            foreach (var item in arr)
            {
                var d = MiniJson.AsObject(item);
                if (d == null) continue;
                list.Add(new WebGLAudioDevice
                {
                    DeviceId = MiniJson.GetString(d, "deviceId") ?? string.Empty,
                    GroupId = MiniJson.GetString(d, "groupId") ?? string.Empty,
                    Label = MiniJson.GetString(d, "label") ?? string.Empty,
                    IsOutput = output,
                });
            }
        }

        internal static WebGLBridgeException Error(Dictionary<string, object> o, string fallback)
        {
            var e = Obj(o, "error");
            return new WebGLBridgeException(MiniJson.GetString(e, "message") ?? fallback, MiniJson.GetString(e, "name"), MiniJson.GetString(e, "code"));
        }

        internal static Dictionary<string, object> Position(Position3D p) => new Dictionary<string, object>
        {
            { "x", (double)p.X }, { "y", (double)p.Y }, { "z", (double)p.Z },
        };

        internal static Dictionary<string, object> Orientation(Orientation3D o) => new Dictionary<string, object>
        {
            { "forward_x", (double)o.ForwardX }, { "forward_y", (double)o.ForwardY }, { "forward_z", (double)o.ForwardZ },
            { "up_x", (double)o.UpX }, { "up_y", (double)o.UpY }, { "up_z", (double)o.UpZ },
        };

        internal static string Consent(RecordingConsent c)
        {
            switch (c)
            {
                case RecordingConsent.Accepted: return "accepted";
                case RecordingConsent.Declined: return "declined";
                default: return "pending";
            }
        }

        internal static string Moderation(ModerationAction a)
        {
            switch (a)
            {
                case ModerationAction.Kick: return "kick";
                case ModerationAction.Mute: return "mute";
                default: return "unmute";
            }
        }
    }
}
