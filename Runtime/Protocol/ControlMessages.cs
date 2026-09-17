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
