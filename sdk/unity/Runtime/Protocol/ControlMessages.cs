using System;
using System.Collections.Generic;

namespace Aurix.Protocol
{
    public enum ChannelRole { Listener, Speaker, Moderator, Administrator }

    public enum RecordingConsent { Pending, Accepted, Declined }

    public sealed class ParticipantBrief
    {
        public Guid UserId;
        public string DisplayName;
        public uint Ssrc;
        public ChannelRole Role;
        public bool IsMuted;
        public bool IsSpeaking;
    }

    public struct Position3D { public float X, Y, Z; }

    public struct Orientation3D
    {
        public float ForwardX, ForwardY, ForwardZ;
        public float UpX, UpY, UpZ;
    }

    public sealed class UserPosition
    {
        public Guid UserId;
        public Position3D Position;
        public Orientation3D Orientation;
    }

    /// <summary>Receiver-local mute of one participant; <see cref="ChannelId"/> is null for "every channel".</summary>
    /// <summary>Moderation a player may perform with a one-time action token (<c>POST /v1/tokens/action</c>).</summary>
    public enum ModerationAction { Kick, Mute, Unmute }

    public sealed class LocalMute
    {
        public Guid UserId;
        public Guid? ChannelId;
    }

    /// <summary>Receiver-local gain for one participant: 0 silence, 1 as sent, up to 2 (about +6 dB).</summary>
    public sealed class ParticipantVolume
    {
        public Guid UserId;
        public float Volume;
    }

    /// <summary>Server-side snapshot of this user's receiver preferences, sent after <c>SessionInitAck</c>.</summary>
    public sealed class ReceiverPreferences
    {
        public List<Guid> BlockedUsers = new List<Guid>();
        public List<LocalMute> LocalMutes = new List<LocalMute>();
        public List<ParticipantVolume> Volumes = new List<ParticipantVolume>();
    }

    /// <summary>
    /// One text-chat message. Exactly one of <see cref="ChannelId"/> / <see cref="ToUserId"/> is set.
    /// <see cref="FromUserId"/> is <see cref="SystemUserId"/> for messages injected by the game server
    /// through the REST API. <see cref="ClientRef"/> is only present on the sender's own echo.
    /// </summary>
    public sealed class ChatMessage
    {
        /// <summary><c>from_user_id</c> of server-injected system messages (nil UUID).</summary>
        public static readonly Guid SystemUserId = Guid.Empty;

        public Guid Id;
        public Guid? ChannelId;
        public Guid FromUserId;
        public string DisplayName;
        public Guid? ToUserId;
        public string Text;
        /// <summary>Game payload as parsed JSON (<c>Dictionary&lt;string, object&gt;</c>, <c>List&lt;object&gt;</c>, string, double, bool) or null.</summary>
        public object Metadata;
        public DateTimeOffset SentAt;
        public string ClientRef;

        public bool IsSystem => FromUserId == SystemUserId;
        public bool IsDirect => ToUserId.HasValue;
        /// <summary>Echo of a message this client sent (the server returns <c>client_ref</c> only to the sender).</summary>
        public bool IsOwn => ClientRef != null;
    }

    /// <summary>
    /// One WebSocket control message: <c>{"type": "&lt;Variant&gt;", "data": {...}}</c>, mirroring the
    /// Rust <c>ControlMessage</c> enum. <see cref="Data"/> is the raw parsed <c>data</c> object; typed
    /// accessors are provided for the variants the client consumes.
    /// </summary>
    public sealed class ControlMessage
    {
        public string Type;
        public Dictionary<string, object> Data;

        public static ControlMessage Parse(string json)
        {
            var root = MiniJson.AsObject(MiniJson.Parse(json));
            var type = MiniJson.GetString(root, "type");
            if (string.IsNullOrEmpty(type)) throw new FormatException("control message without type");
            return new ControlMessage { Type = type, Data = MiniJson.AsObject(root.TryGetValue("data", out var d) ? d : null) };
        }

        public static string Serialize(string type, Dictionary<string, object> data) =>
            MiniJson.Serialize(new Dictionary<string, object> { { "type", type }, { "data", data } });

        public string Str(string key) => MiniJson.GetString(Data, key);
        public bool Bool(string key) => MiniJson.GetBool(Data, key);
        public double Num(string key) => MiniJson.GetNumber(Data, key);
        public uint U32(string key) => MiniJson.GetUInt32(Data, key);
        public Guid Id(string key) => MiniJson.GetGuid(Data, key) ?? Guid.Empty;

        public List<ParticipantBrief> Participants()
        {
            var list = new List<ParticipantBrief>();
            var arr = Data != null && Data.TryGetValue("participants", out var v) ? MiniJson.AsArray(v) : null;
            if (arr == null) return list;
            foreach (var item in arr)
            {
                var o = MiniJson.AsObject(item);
                if (o == null) continue;
                list.Add(new ParticipantBrief
                {
                    UserId = MiniJson.GetGuid(o, "user_id") ?? Guid.Empty,
                    DisplayName = MiniJson.GetString(o, "display_name") ?? string.Empty,
                    Ssrc = MiniJson.GetUInt32(o, "ssrc"),
                    Role = ParseRole(MiniJson.GetString(o, "role")),
                    IsMuted = MiniJson.GetBool(o, "is_muted"),
                    IsSpeaking = MiniJson.GetBool(o, "is_speaking"),
                });
            }
            return list;
        }

        public List<UserPosition> Positions()
        {
            var list = new List<UserPosition>();
            var arr = Data != null && Data.TryGetValue("positions", out var v) ? MiniJson.AsArray(v) : null;
            if (arr == null) return list;
            foreach (var item in arr)
            {
                var o = MiniJson.AsObject(item);
                var pos = MiniJson.AsObject(o != null && o.TryGetValue("position", out var pv) ? pv : null);
                var ori = MiniJson.AsObject(o != null && o.TryGetValue("orientation", out var ov) ? ov : null);
                if (o == null) continue;
                list.Add(new UserPosition
                {
                    UserId = MiniJson.GetGuid(o, "user_id") ?? Guid.Empty,
                    Position = new Position3D
                    {
                        X = (float)MiniJson.GetNumber(pos, "x"),
                        Y = (float)MiniJson.GetNumber(pos, "y"),
                        Z = (float)MiniJson.GetNumber(pos, "z"),
                    },
                    Orientation = new Orientation3D
                    {
                        ForwardX = (float)MiniJson.GetNumber(ori, "forward_x"),
                        ForwardY = (float)MiniJson.GetNumber(ori, "forward_y"),
                        ForwardZ = (float)MiniJson.GetNumber(ori, "forward_z"),
                        UpX = (float)MiniJson.GetNumber(ori, "up_x"),
                        UpY = (float)MiniJson.GetNumber(ori, "up_y"),
                        UpZ = (float)MiniJson.GetNumber(ori, "up_z"),
                    },
                });
            }
            return list;
        }

        public static ChannelRole ParseRole(string s)
        {
            switch (s)
            {
                case "speaker": return ChannelRole.Speaker;
                case "moderator": return ChannelRole.Moderator;
                case "administrator": return ChannelRole.Administrator;
                default: return ChannelRole.Listener;
            }
        }

        public ReceiverPreferences ReceiverPreferences()
        {
            var prefs = new ReceiverPreferences();
            if (Data == null) return prefs;
            if (Data.TryGetValue("blocked_users", out var bv) && MiniJson.AsArray(bv) is List<object> blocked)
                foreach (var item in blocked)
                    if (Guid.TryParse(item as string, out var g)) prefs.BlockedUsers.Add(g);
            if (Data.TryGetValue("local_mutes", out var mv) && MiniJson.AsArray(mv) is List<object> mutes)
                foreach (var item in mutes)
                {
                    var o = MiniJson.AsObject(item);
                    if (o == null) continue;
                    prefs.LocalMutes.Add(new LocalMute
                    {
                        UserId = MiniJson.GetGuid(o, "user_id") ?? Guid.Empty,
                        ChannelId = MiniJson.GetGuid(o, "channel_id"),
                    });
                }
            if (Data.TryGetValue("volumes", out var vv) && MiniJson.AsArray(vv) is List<object> volumes)
                foreach (var item in volumes)
                {
                    var o = MiniJson.AsObject(item);
                    if (o == null) continue;
                    prefs.Volumes.Add(new ParticipantVolume
                    {
                        UserId = MiniJson.GetGuid(o, "user_id") ?? Guid.Empty,
                        Volume = (float)MiniJson.GetNumber(o, "volume", 1.0),
                    });
                }
            return prefs;
        }

        /// <summary>Typed view of a <c>ChatMessageReceived</c> payload (<c>data.message</c>); null if absent.</summary>
        public ChatMessage ChatMessage()
        {
            var o = MiniJson.AsObject(Data != null && Data.TryGetValue("message", out var v) ? v : null);
            if (o == null) return null;
            var sentAt = MiniJson.GetString(o, "sent_at");
            return new ChatMessage
            {
                Id = MiniJson.GetGuid(o, "id") ?? Guid.Empty,
                ChannelId = MiniJson.GetGuid(o, "channel_id"),
                FromUserId = MiniJson.GetGuid(o, "from_user_id") ?? Guid.Empty,
                DisplayName = MiniJson.GetString(o, "display_name") ?? string.Empty,
                ToUserId = MiniJson.GetGuid(o, "to_user_id"),
                Text = MiniJson.GetString(o, "text") ?? string.Empty,
                Metadata = o.TryGetValue("metadata", out var md) ? md : null,
                SentAt = sentAt != null && DateTimeOffset.TryParse(sentAt, System.Globalization.CultureInfo.InvariantCulture,
                    System.Globalization.DateTimeStyles.RoundtripKind, out var ts) ? ts : DateTimeOffset.MinValue,
                ClientRef = MiniJson.GetString(o, "client_ref"),
            };
        }

        public static string ConsentToWire(RecordingConsent c)
        {
            switch (c)
            {
                case RecordingConsent.Accepted: return "accepted";
                case RecordingConsent.Declined: return "declined";
                default: return "pending";
            }
        }

        // ---- outbound builders ----------------------------------------------------------------

        public static string ChannelJoin(Guid channelId, string token) =>
            Serialize("ChannelJoin", new Dictionary<string, object> { { "channel_id", channelId }, { "token", token } });

        public static string ModerationActionToWire(ModerationAction a)
        {
            switch (a)
            {
                case ModerationAction.Kick: return "kick";
                case ModerationAction.Mute: return "mute";
                case ModerationAction.Unmute: return "unmute";
                default: throw new ArgumentOutOfRangeException(nameof(a));
            }
        }

        public static ModerationAction? ParseModerationAction(string s)
        {
            switch (s)
            {
                case "kick": return ModerationAction.Kick;
                case "mute": return ModerationAction.Mute;
                case "unmute": return ModerationAction.Unmute;
                default: return null;
            }
        }

        public static string ModerateParticipant(Guid channelId, Guid userId, ModerationAction action, string token, string reason) =>
            Serialize("ModerateParticipant", new Dictionary<string, object>
            {
                { "channel_id", channelId }, { "user_id", userId }, { "action", ModerationActionToWire(action) },
                { "token", token }, { "reason", reason },
            });

        public static string ChannelLeave(Guid channelId) =>
            Serialize("ChannelLeave", new Dictionary<string, object> { { "channel_id", channelId } });

        public static string Ping(ulong nonce) =>
            Serialize("Ping", new Dictionary<string, object> { { "nonce", nonce } });

        public static string QualityReport(float rttMs, float jitterMs, float lossPercent) =>
            Serialize("QualityReport", new Dictionary<string, object>
            {
                { "rtt_ms", rttMs }, { "jitter_ms", jitterMs }, { "packet_loss", lossPercent },
            });

        public static string RecordingConsentResponse(Guid recordingId, RecordingConsent consent) =>
            Serialize("RecordingConsentResponse", new Dictionary<string, object>
            {
                { "recording_id", recordingId }, { "consent", ConsentToWire(consent) },
            });

        public static string SetParticipantMute(Guid userId, Guid? channelId, bool muted) =>
            Serialize("SetParticipantMute", new Dictionary<string, object>
            {
                { "user_id", userId }, { "channel_id", channelId.HasValue ? (object)channelId.Value : null }, { "muted", muted },
            });

        public static string SetParticipantVolume(Guid userId, float volume) =>
            Serialize("SetParticipantVolume", new Dictionary<string, object> { { "user_id", userId }, { "volume", volume } });

        public static string SetUserBlock(Guid userId, bool blocked) =>
            Serialize("SetUserBlock", new Dictionary<string, object> { { "user_id", userId }, { "blocked", blocked } });

        public static string ChatSend(Guid channelId, string text, object metadata, string clientRef)
        {
            var d = new Dictionary<string, object> { { "channel_id", channelId }, { "text", text } };
            if (metadata != null) d["metadata"] = metadata;
            if (clientRef != null) d["client_ref"] = clientRef;
            return Serialize("ChatSend", d);
        }

        public static string ChatSendDirect(Guid userId, string text, object metadata, string clientRef)
        {
            var d = new Dictionary<string, object> { { "user_id", userId }, { "text", text } };
            if (metadata != null) d["metadata"] = metadata;
            if (clientRef != null) d["client_ref"] = clientRef;
            return Serialize("ChatSendDirect", d);
        }

        public static string ChatTyping(Guid channelId, bool typing) =>
            Serialize("ChatTyping", new Dictionary<string, object> { { "channel_id", channelId }, { "typing", typing } });

        public static string PositionUpdate(Guid channelId, Guid userId, Position3D pos, Orientation3D ori) =>
            Serialize("PositionUpdate", new Dictionary<string, object>
            {
                { "channel_id", channelId },
                {
                    "positions", new List<object>
                    {
                        new Dictionary<string, object>
                        {
                            { "user_id", userId },
                            { "position", new Dictionary<string, object> { { "x", pos.X }, { "y", pos.Y }, { "z", pos.Z } } },
                            {
                                "orientation", new Dictionary<string, object>
                                {
                                    { "forward_x", ori.ForwardX }, { "forward_y", ori.ForwardY }, { "forward_z", ori.ForwardZ },
                                    { "up_x", ori.UpX }, { "up_y", ori.UpY }, { "up_z", ori.UpZ },
                                }
                            },
                        },
                    }
                },
            });
    }
}
