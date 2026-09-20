using System;
using System.Collections.Generic;

namespace Aurix.Protocol
{
    public enum ChannelRole { Listener, Speaker, Moderator, Administrator }

    /// <summary>How the node delivers other speakers to this session (<c>SetDownlinkMode</c>).</summary>
    public enum DownlinkMode
    {
        /// <summary>Default: one stream per audible speaker, mixed by this client.</summary>
        Streams = 0,
        /// <summary>
        /// One server-mixed stereo stream per channel (<see cref="PacketFlags.Mixed"/>): constant downlink bandwidth
        /// and decode cost in large channels; mutes, volumes, focus and positional gains are applied by the node.
        /// </summary>
        Mixed = 1,
    }

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

    /// <summary>One entry of a <c>ChannelEnergy</c> report: linear audio energy 0..1 (0 = silent).</summary>
    public sealed class ParticipantEnergy
    {
        public Guid UserId;
        public float Energy;
    }

    public enum TransmissionKind { None, Single, All }

    /// <summary>
    /// Where this session's microphone audio is delivered: nowhere (<see cref="TransmissionKind.None"/>,
    /// a server-side push-to-talk release), exactly one joined channel, or every joined channel (default).
    /// Enforced by the server before fan-out.
    /// </summary>
    public readonly struct TransmissionMode : IEquatable<TransmissionMode>
    {
        public readonly TransmissionKind Kind;
        /// <summary>Target channel for <see cref="TransmissionKind.Single"/>; <see cref="Guid.Empty"/> otherwise.</summary>
        public readonly Guid ChannelId;

        private TransmissionMode(TransmissionKind kind, Guid channelId) { Kind = kind; ChannelId = channelId; }

        public static readonly TransmissionMode None = new TransmissionMode(TransmissionKind.None, Guid.Empty);
        public static readonly TransmissionMode All = new TransmissionMode(TransmissionKind.All, Guid.Empty);
        public static TransmissionMode Single(Guid channelId)
        {
            if (channelId == Guid.Empty) throw new ArgumentException("channel id required", nameof(channelId));
            return new TransmissionMode(TransmissionKind.Single, channelId);
        }

        /// <summary>Would a frame addressed to <paramref name="channelId"/> be forwarded under this mode?</summary>
        public bool Allows(Guid channelId)
        {
            switch (Kind)
            {
                case TransmissionKind.None: return false;
                case TransmissionKind.Single: return ChannelId == channelId;
                default: return true;
            }
        }

        public Dictionary<string, object> ToWire()
        {
            switch (Kind)
            {
                case TransmissionKind.None: return new Dictionary<string, object> { { "mode", "none" } };
                case TransmissionKind.Single: return new Dictionary<string, object> { { "mode", "single" }, { "channel_id", ChannelId } };
                default: return new Dictionary<string, object> { { "mode", "all" } };
            }
        }

        /// <summary>Parse the wire object; a missing/unknown value is <see cref="All"/> (the server default).</summary>
        public static TransmissionMode FromWire(object wire)
        {
            var o = MiniJson.AsObject(wire);
            if (o == null) return All;
            switch (MiniJson.GetString(o, "mode"))
            {
                case "none": return None;
                case "single":
                    var id = MiniJson.GetGuid(o, "channel_id");
                    return id.HasValue && id.Value != Guid.Empty ? Single(id.Value) : All;
                default: return All;
            }
        }

        public bool Equals(TransmissionMode other) => Kind == other.Kind && ChannelId == other.ChannelId;
        public override bool Equals(object obj) => obj is TransmissionMode m && Equals(m);
        public override int GetHashCode() => ((int)Kind * 397) ^ ChannelId.GetHashCode();
        public static bool operator ==(TransmissionMode a, TransmissionMode b) => a.Equals(b);
        public static bool operator !=(TransmissionMode a, TransmissionMode b) => !a.Equals(b);
        public override string ToString() => Kind == TransmissionKind.Single ? $"Single({ChannelId})" : Kind.ToString();
    }

    /// <summary>Server-side snapshot of this user's receiver preferences, sent after <c>SessionInitAck</c>.</summary>
    public sealed class ReceiverPreferences
    {
        public List<Guid> BlockedUsers = new List<Guid>();
        public List<LocalMute> LocalMutes = new List<LocalMute>();
        public List<ParticipantVolume> Volumes = new List<ParticipantVolume>();
        public TransmissionMode Transmission = TransmissionMode.All;
        /// <summary>Channel heard at full volume while the others are attenuated; null when unfocused.</summary>
        public Guid? FocusChannel;
        /// <summary>Codec this session sends and receives on the native media path (Opus unless negotiated).</summary>
        public Aurix.Audio.AudioCodec Codec = Aurix.Audio.AudioCodec.Opus;
        /// <summary>How channel audio reaches this session (<see cref="DownlinkMode.Streams"/> unless requested).</summary>
        public DownlinkMode Downlink = DownlinkMode.Streams;
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
        /// <summary>
        /// Directed message that waited for the recipient: replayed on connect (see
        /// <c>OnChatInboxSynced</c>) or, on the sender's own echo, stored because the recipient is offline.
        /// </summary>
        public bool Offline;

        public bool IsSystem => FromUserId == SystemUserId;
        public bool IsDirect => ToUserId.HasValue;
        /// <summary>Echo of a message this client sent (the server returns <c>client_ref</c> only to the sender).</summary>
        public bool IsOwn => ClientRef != null;
        /// <summary>Opaque history cursor of this message (<c>before</c>/<c>after</c> of a history request).</summary>
        public string Cursor => ChatCursor.Encode(SentAt, Id);
    }

    /// <summary>
    /// The server's opaque history cursor: URL-safe base64 (no padding) of the big-endian microsecond Unix
    /// timestamp followed by the 16 UUID bytes (RFC 4122 byte order). Computed locally so any message can
    /// anchor a history page.
    /// </summary>
    public static class ChatCursor
    {
        public static string Encode(DateTimeOffset sentAt, Guid id)
        {
            long micros = sentAt.UtcTicks / 10 - 62135596800000000L;
            var bytes = new byte[24];
            for (int i = 0; i < 8; i++) bytes[i] = (byte)(micros >> (56 - 8 * i));
            Array.Copy(UuidBytes(id), 0, bytes, 8, 16);
            return Convert.ToBase64String(bytes).TrimEnd('=').Replace('+', '-').Replace('/', '_');
        }

        /// <summary>Wire-order (big-endian) UUID bytes; <see cref="Guid.ToByteArray"/> is little-endian for the first three groups.</summary>
        private static byte[] UuidBytes(Guid id)
        {
            var b = id.ToByteArray();
            return new[]
            {
                b[3], b[2], b[1], b[0], b[5], b[4], b[7], b[6],
                b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15],
            };
        }
    }

    /// <summary>One page of stored chat history, newest first.</summary>
    public sealed class ChatHistoryPage
    {
        public Guid? ChannelId;
        /// <summary>The other party of a direct conversation.</summary>
        public Guid? PeerUserId;
        public List<ChatMessage> Messages = new List<ChatMessage>();
        /// <summary>Cursor for the next older page, or null when the beginning was reached.</summary>
        public string NextBefore;
        /// <summary>Cursor for the next newer page, or null when the page is the most recent.</summary>
        public string NextAfter;
    }

    /// <summary>A user's reading position in a channel or a direct conversation.</summary>
    public sealed class ChatReadMarker
    {
        public Guid UserId;
        public Guid? ChannelId;
        public Guid? PeerUserId;
        public Guid MessageId;
        public DateTimeOffset MessageSentAt;
        public DateTimeOffset ReadAt;
    }

    /// <summary>Result of a read-marker query: the markers plus this user's unread count (capped by the server).</summary>
    public sealed class ChatReadMarkers
    {
        public Guid? ChannelId;
        public Guid? PeerUserId;
        public List<ChatReadMarker> Markers = new List<ChatReadMarker>();
        public int UnreadCount;
    }

    /// <summary>Server-side speech-to-text of one utterance in a channel with transcription enabled (never stored).</summary>
    public sealed class Transcript
    {
        public Guid Id;
        public Guid ChannelId;
        public Guid UserId;
        public string Text;
        /// <summary>Language the provider detected, or null.</summary>
        public string Language;
        public DateTimeOffset StartedAt;
        public ulong DurationMs;
        /// <summary>Word timings relative to <see cref="StartedAt"/>; empty unless the server enables them.</summary>
        public List<TranscriptWord> Words = new List<TranscriptWord>();
    }

    public struct TranscriptWord
    {
        public string Word;
        public ulong StartMs;
        public ulong EndMs;
    }

    /// <summary>Who hears a synthesized utterance requested by this client.</summary>
    public enum TtsDestination
    {
        /// <summary>The other channel members (the requester does not hear itself).</summary>
        Channel,
        /// <summary>Only this client.</summary>
        Local,
        /// <summary>Everyone including this client.</summary>
        Both,
    }

    public enum TtsState { Queued, Playing, Finished, Cancelled, Failed }

    /// <summary>Lifecycle update of a text-to-speech request.</summary>
    public sealed class TtsStatus
    {
        public Guid RequestId;
        public string ClientRef;
        public TtsState State;
        /// <summary>Audio length in ms once synthesis succeeded.</summary>
        public ulong? DurationMs;
        /// <summary>Sanitized failure reason for <see cref="TtsState.Failed"/>.</summary>
        public string Message;

        public bool IsTerminal => State == TtsState.Finished || State == TtsState.Cancelled || State == TtsState.Failed;
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
        /// <summary>Whether <paramref name="key"/> is present with a non-null value.</summary>
        public bool Has(string key) => Data != null && Data.TryGetValue(key, out var v) && v != null;

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
            if (Data.TryGetValue("transmission", out var tv)) prefs.Transmission = TransmissionMode.FromWire(tv);
            prefs.FocusChannel = MiniJson.GetGuid(Data, "focus_channel");
            prefs.Codec = AudioCodecFromWire(MiniJson.GetString(Data, "codec"));
            prefs.Downlink = DownlinkModeFromWire(MiniJson.GetString(Data, "downlink"));
            return prefs;
        }

        /// <summary>Typed view of an <c>AudioCodecChanged</c> payload.</summary>
        public Aurix.Audio.AudioCodec AudioCodec() => AudioCodecFromWire(MiniJson.GetString(Data, "codec"));

        /// <summary>Typed view of a <c>DownlinkModeChanged</c> payload.</summary>
        public DownlinkMode DownlinkMode() => DownlinkModeFromWire(MiniJson.GetString(Data, "mode"));

        internal static DownlinkMode DownlinkModeFromWire(string s) => s == "mixed" ? Protocol.DownlinkMode.Mixed : Protocol.DownlinkMode.Streams;

        internal static string DownlinkModeToWire(DownlinkMode mode) => mode == Protocol.DownlinkMode.Mixed ? "mixed" : "streams";

        internal static Aurix.Audio.AudioCodec AudioCodecFromWire(string s) =>
            s == "pcmu" ? Aurix.Audio.AudioCodec.Pcmu : Aurix.Audio.AudioCodec.Opus;

        internal static string AudioCodecToWire(Aurix.Audio.AudioCodec codec) =>
            codec == Aurix.Audio.AudioCodec.Pcmu ? "pcmu" : "opus";

        /// <summary>Typed view of a <c>TransmissionChanged</c> payload.</summary>
        public TransmissionMode Transmission() =>
            TransmissionMode.FromWire(Data != null && Data.TryGetValue("mode", out var m) ? m : null);

        /// <summary>Typed view of a <c>ChannelFocusChanged</c> payload (null = focus cleared).</summary>
        public Guid? FocusChannel() => MiniJson.GetGuid(Data, "channel_id");

        /// <summary>Typed view of a <c>Transcript</c> payload (<c>data.transcript</c>); null if absent.</summary>
        public Transcript Transcript()
        {
            var o = MiniJson.AsObject(Data != null && Data.TryGetValue("transcript", out var v) ? v : null);
            if (o == null) return null;
            var startedAt = MiniJson.GetString(o, "started_at");
            var t = new Transcript
            {
                Id = MiniJson.GetGuid(o, "id") ?? Guid.Empty,
                ChannelId = MiniJson.GetGuid(o, "channel_id") ?? Guid.Empty,
                UserId = MiniJson.GetGuid(o, "user_id") ?? Guid.Empty,
                Text = MiniJson.GetString(o, "text") ?? string.Empty,
                Language = MiniJson.GetString(o, "language"),
                StartedAt = startedAt != null && DateTimeOffset.TryParse(startedAt, System.Globalization.CultureInfo.InvariantCulture,
                    System.Globalization.DateTimeStyles.RoundtripKind, out var ts) ? ts : DateTimeOffset.MinValue,
                DurationMs = (ulong)Math.Max(0, MiniJson.GetNumber(o, "duration_ms", 0)),
            };
            if (o.TryGetValue("words", out var wv) && MiniJson.AsArray(wv) is List<object> words)
                foreach (var item in words)
                {
                    var w = MiniJson.AsObject(item);
                    if (w == null) continue;
                    t.Words.Add(new TranscriptWord
                    {
                        Word = MiniJson.GetString(w, "word") ?? string.Empty,
                        StartMs = (ulong)Math.Max(0, MiniJson.GetNumber(w, "start_ms", 0)),
                        EndMs = (ulong)Math.Max(0, MiniJson.GetNumber(w, "end_ms", 0)),
                    });
                }
            return t;
        }

        /// <summary>Typed view of a <c>TtsStatus</c> payload.</summary>
        public TtsStatus TtsStatus()
        {
            if (Data == null) return null;
            return new TtsStatus
            {
                RequestId = MiniJson.GetGuid(Data, "request_id") ?? Guid.Empty,
                ClientRef = MiniJson.GetString(Data, "client_ref"),
                State = TtsStateFromWire(MiniJson.GetString(Data, "state")),
                DurationMs = Data.TryGetValue("duration_ms", out var d) && d is double dm ? (ulong?)Math.Max(0, dm) : null,
                Message = MiniJson.GetString(Data, "message"),
            };
        }

        public static TtsState TtsStateFromWire(string s)
        {
            switch (s)
            {
                case "playing": return TtsState.Playing;
                case "finished": return TtsState.Finished;
                case "cancelled": return TtsState.Cancelled;
                case "failed": return TtsState.Failed;
                default: return TtsState.Queued;
            }
        }

        public static string TtsDestinationToWire(TtsDestination d)
        {
            switch (d)
            {
                case TtsDestination.Local: return "local";
                case TtsDestination.Both: return "both";
                default: return "channel";
            }
        }

        /// <summary>Typed view of a <c>ChannelEnergy</c> payload (<c>data.levels</c>).</summary>
        public List<ParticipantEnergy> Levels()
        {
            var list = new List<ParticipantEnergy>();
            if (Data == null || !Data.TryGetValue("levels", out var lv) || !(MiniJson.AsArray(lv) is List<object> levels))
                return list;
            foreach (var item in levels)
            {
                var o = MiniJson.AsObject(item);
                if (o == null) continue;
                var user = MiniJson.GetGuid(o, "user_id");
                if (!user.HasValue) continue;
                float e = (float)MiniJson.GetNumber(o, "energy", 0.0);
                list.Add(new ParticipantEnergy { UserId = user.Value, Energy = Math.Max(0f, Math.Min(1f, e)) });
            }
            return list;
        }

        /// <summary>Typed view of a <c>ChatMessageReceived</c> payload (<c>data.message</c>); null if absent.</summary>
        public ChatMessage ChatMessage() =>
            ParseChatMessage(MiniJson.AsObject(Data != null && Data.TryGetValue("message", out var v) ? v : null));

        private static DateTimeOffset ParseTime(Dictionary<string, object> o, string key)
        {
            var s = MiniJson.GetString(o, key);
            return s != null && DateTimeOffset.TryParse(s, System.Globalization.CultureInfo.InvariantCulture,
                System.Globalization.DateTimeStyles.RoundtripKind, out var ts) ? ts : DateTimeOffset.MinValue;
        }

        private static ChatMessage ParseChatMessage(Dictionary<string, object> o)
        {
            if (o == null) return null;
            return new ChatMessage
            {
                Id = MiniJson.GetGuid(o, "id") ?? Guid.Empty,
                ChannelId = MiniJson.GetGuid(o, "channel_id"),
                FromUserId = MiniJson.GetGuid(o, "from_user_id") ?? Guid.Empty,
                DisplayName = MiniJson.GetString(o, "display_name") ?? string.Empty,
                ToUserId = MiniJson.GetGuid(o, "to_user_id"),
                Text = MiniJson.GetString(o, "text") ?? string.Empty,
                Metadata = o.TryGetValue("metadata", out var md) ? md : null,
                SentAt = ParseTime(o, "sent_at"),
                ClientRef = MiniJson.GetString(o, "client_ref"),
                Offline = MiniJson.GetBool(o, "offline"),
            };
        }

        private static ChatReadMarker ParseReadMarker(Dictionary<string, object> o)
        {
            if (o == null) return null;
            return new ChatReadMarker
            {
                UserId = MiniJson.GetGuid(o, "user_id") ?? Guid.Empty,
                ChannelId = MiniJson.GetGuid(o, "channel_id"),
                PeerUserId = MiniJson.GetGuid(o, "peer_user_id"),
                MessageId = MiniJson.GetGuid(o, "message_id") ?? Guid.Empty,
                MessageSentAt = ParseTime(o, "message_sent_at"),
                ReadAt = ParseTime(o, "read_at"),
            };
        }

        /// <summary>Typed view of a <c>ChatHistoryResult</c> payload.</summary>
        public ChatHistoryPage ChatHistory()
        {
            if (Data == null) return null;
            var page = new ChatHistoryPage
            {
                ChannelId = MiniJson.GetGuid(Data, "channel_id"),
                PeerUserId = MiniJson.GetGuid(Data, "user_id"),
                NextBefore = MiniJson.GetString(Data, "next_before"),
                NextAfter = MiniJson.GetString(Data, "next_after"),
            };
            if (Data.TryGetValue("messages", out var v) && MiniJson.AsArray(v) is List<object> arr)
                foreach (var item in arr)
                {
                    var m = ParseChatMessage(MiniJson.AsObject(item));
                    if (m != null) page.Messages.Add(m);
                }
            return page;
        }

        /// <summary>Typed view of a <c>ChatReadMarker</c> payload (<c>data.marker</c>); null if absent.</summary>
        public ChatReadMarker ReadMarker() =>
            ParseReadMarker(MiniJson.AsObject(Data != null && Data.TryGetValue("marker", out var v) ? v : null));

        /// <summary>Typed view of a <c>ChatReadMarkersResult</c> payload.</summary>
        public ChatReadMarkers ReadMarkers()
        {
            if (Data == null) return null;
            var r = new ChatReadMarkers
            {
                ChannelId = MiniJson.GetGuid(Data, "channel_id"),
                PeerUserId = MiniJson.GetGuid(Data, "user_id"),
                UnreadCount = (int)MiniJson.GetNumber(Data, "unread_count"),
            };
            if (Data.TryGetValue("markers", out var v) && MiniJson.AsArray(v) is List<object> arr)
                foreach (var item in arr)
                {
                    var m = ParseReadMarker(MiniJson.AsObject(item));
                    if (m != null) r.Markers.Add(m);
                }
            return r;
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

        public static string SetTransmission(TransmissionMode mode) =>
            Serialize("SetTransmission", new Dictionary<string, object> { { "mode", mode.ToWire() } });

        public static string SetChannelFocus(Guid? channelId) =>
            Serialize("SetChannelFocus", new Dictionary<string, object>
            {
                { "channel_id", channelId.HasValue ? (object)channelId.Value : null },
            });

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

        private static Dictionary<string, object> Scope(Guid? channelId, Guid? userId)
        {
            var d = new Dictionary<string, object>();
            if (channelId.HasValue) d["channel_id"] = channelId.Value;
            else if (userId.HasValue) d["user_id"] = userId.Value;
            else throw new ArgumentException("a channel or a peer user is required");
            return d;
        }

        public static string ChatHistory(Guid? channelId, Guid? userId, string before, string after, int? limit, string clientRef)
        {
            var d = Scope(channelId, userId);
            if (before != null) d["before"] = before;
            if (after != null) d["after"] = after;
            if (limit.HasValue) d["limit"] = limit.Value;
            if (clientRef != null) d["client_ref"] = clientRef;
            return Serialize("ChatHistory", d);
        }

        public static string ChatMarkRead(Guid? channelId, Guid? userId, Guid messageId)
        {
            var d = Scope(channelId, userId);
            d["message_id"] = messageId;
            return Serialize("ChatMarkRead", d);
        }

        public static string ChatReadMarkers(Guid? channelId, Guid? userId) =>
            Serialize("ChatReadMarkers", Scope(channelId, userId));

        public static string SetTranscripts(bool enabled) =>
            Serialize("SetTranscripts", new Dictionary<string, object> { { "enabled", enabled } });

        public static string SetAudioCodec(Aurix.Audio.AudioCodec codec) =>
            Serialize("SetAudioCodec", new Dictionary<string, object> { { "codec", AudioCodecToWire(codec) } });

        public static string SetDownlinkMode(DownlinkMode mode) =>
            Serialize("SetDownlinkMode", new Dictionary<string, object> { { "mode", DownlinkModeToWire(mode) } });

        public static string TtsSpeak(Guid? channelId, string text, string voice, TtsDestination destination, string clientRef)
        {
            var d = new Dictionary<string, object> { { "text", text }, { "destination", TtsDestinationToWire(destination) } };
            if (channelId.HasValue) d["channel_id"] = channelId.Value;
            if (voice != null) d["voice"] = voice;
            if (clientRef != null) d["client_ref"] = clientRef;
            return Serialize("TtsSpeak", d);
        }

        public static string TtsCancel() =>
            MiniJson.Serialize(new Dictionary<string, object> { { "type", "TtsCancel" } });

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
