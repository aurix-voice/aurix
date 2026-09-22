using System;
using System.Collections.Generic;
using System.Threading;
using System.Threading.Tasks;
using Aurix.Protocol;

namespace Aurix
{
    /// <summary>
    /// The platform-neutral part of the voice client: what game code can rely on whether the build
    /// runs the native core (<see cref="AurixVoiceClient"/> — AURX/UDP media, engine-side audio) or the
    /// browser bridge (<c>Aurix.WebGL.AurixWebGLVoiceClient</c> — WebRTC through the Web SDK).
    /// Audio-pipeline members (codecs, mixers, DSP, per-participant PCM, media path) stay on the
    /// concrete classes because the browser owns them on WebGL. Events are raised from
    /// <see cref="Update"/>, which the host calls every frame.
    /// </summary>
    public interface IAurixVoiceClient : IDisposable
    {
        VoiceConnectionState State { get; }
        SessionInfo Session { get; }
        bool IsMuted { get; }
        string Endpoint { get; }
        IReadOnlyList<string> FailoverEndpoints { get; }
        bool AutoReconnect { get; set; }
        IReadOnlyCollection<Guid> JoinedChannels { get; }
        TransmissionMode Transmission { get; }
        Guid? FocusChannel { get; }
        bool TranscriptsEnabled { get; }
        TranslationPrefs TranslationPrefs { get; }
        NetworkQuality? LastNetworkQuality { get; }

        event Action<VoiceConnectionState> OnStateChanged;
        event Action<SessionInfo> OnSessionReady;
        event Action<Guid, IReadOnlyList<Participant>> OnChannelJoined;
        event Action<Guid> OnChannelLeft;
        event Action<Guid, Participant> OnParticipantJoined;
        event Action<Guid, Participant> OnParticipantLeft;
        event Action<Guid, Participant> OnParticipantUpdated;
        event Action<Guid, Participant, bool> OnSpeaking;
        event Action<Guid, IReadOnlyList<ParticipantEnergy>> OnChannelEnergy;
        event Action<Guid, IReadOnlyList<UserPosition>> OnPositions;
        event Action<RecordingNotice> OnRecording;
        event Action<NetworkQuality> OnNetworkQuality;
        event Action<Guid, string> OnKicked;
        event Action<Guid, bool> OnUserBlockChanged;
        event Action<Guid, Guid, bool> OnParticipantPriorityChanged;
        event Action<Guid, Guid, ChannelRole, bool> OnParticipantRoleChanged;
        event Action<Guid, bool, DuckingConfig> OnDuckingChanged;
        event Action<TransmissionMode> OnTransmissionChanged;
        event Action<Guid?> OnChannelFocusChanged;
        event Action<ChatMessage> OnChatMessage;
        event Action<ChatReadMarker> OnChatReadMarker;
        event Action<int, bool> OnChatInboxSynced;
        event Action<ChatMessage> OnChatMessageUpdated;
        event Action<ChatReactionChange> OnChatReactionChanged;
        event Action<Guid, Guid, bool> OnParticipantTyping;
        event Action<Transcript> OnTranscript;
        event Action<TranslationPrefs> OnTranslationChanged;
        event Action<TtsStatus> OnTtsStatus;
        event Action<string, string> OnServerError;
        event Action<string> OnDisconnected;
        event Action<int, TimeSpan, string> OnRecovering;
        event Action<SessionInfo> OnRecovered;
        event Action<string> OnEndpointChanged;
        event Action<Exception> OnFailedToRecover;
        event Action<string> OnSessionClosed;

        Task<SessionInfo> ConnectAsync(CancellationToken ct = default);
        Task DisconnectAsync(string reason = "client disconnect");
        void ReconnectNow();
        Task<IReadOnlyList<Participant>> JoinChannelAsync(Guid channelId, CancellationToken ct = default);
        Task<IReadOnlyList<Participant>> JoinChannelAsync(Guid channelId, string joinToken, CancellationToken ct = default);
        Task LeaveChannelAsync(Guid channelId, CancellationToken ct = default);
        Task ModerateAsync(Guid channelId, Guid userId, ModerationAction action, string token, string reason = null, CancellationToken ct = default);

        void SetMuted(bool muted);
        Task SetParticipantMutedAsync(Guid userId, bool muted, Guid? channelId = null, CancellationToken ct = default);
        bool IsParticipantMuted(Guid userId, Guid? channelId = null);
        Task SetParticipantVolumeAsync(Guid userId, float volume, CancellationToken ct = default);
        float GetParticipantVolume(Guid userId);
        Task SetUserBlockedAsync(Guid userId, bool blocked, CancellationToken ct = default);
        bool IsUserBlocked(Guid userId);
        Task SetPriorityAsync(Guid channelId, Guid? userId, bool priority, CancellationToken ct = default);
        bool IsPriority(Guid channelId);
        bool IsDuckingActive(Guid channelId);
        Task SetTransmissionAsync(TransmissionMode mode, CancellationToken ct = default);
        Task TransmitToChannelAsync(Guid channelId, CancellationToken ct = default);
        bool TransmitsTo(Guid channelId);
        Task SetChannelFocusAsync(Guid? channelId, CancellationToken ct = default);

        /// <summary>Lip-sync analysis can run on this platform (native library / browser AudioWorklet).</summary>
        bool SupportsVisemes { get; }
        bool VisemesEnabled { get; }
        Task SetVisemesAsync(bool enabled, CancellationToken ct = default);
        /// <summary>Latest mouth state of a heard participant; null when unknown or with lip-sync off.</summary>
        Audio.VisemeFrame? GetParticipantVisemes(Guid userId);
        /// <summary>Latest mouth state of the local microphone; null with lip-sync off.</summary>
        Audio.VisemeFrame? GetLocalVisemes();
        /// <summary>Voice effects can run on this platform (native library / browser AudioWorklet).</summary>
        bool SupportsVoiceEffects { get; }
        Audio.VoiceEffectParams VoiceEffects { get; }
        Task SetVoiceEffectsAsync(Audio.VoiceEffectParams effects, CancellationToken ct = default);

        IReadOnlyList<Participant> GetParticipants(Guid channelId);
        Participant FindByUser(Guid userId);
        ChannelInfo? GetChannelInfo(Guid channelId);
        ChannelScope? GetChannelScope(Guid channelId);
        bool CanSpeakIn(Guid channelId);
        bool IsWaitingToSpeak(Guid channelId);
        bool IsChannelTranscribed(Guid channelId);
        bool IsChannelMonitored(Guid channelId);
        Task SetTranscriptsAsync(bool enabled, CancellationToken ct = default);
        Task SetTranslationAsync(string language, string spokenLanguage = null, bool speech = false, CancellationToken ct = default);

        Task<ChatMessage> SendMessageAsync(Guid channelId, string text, object metadata = null, string clientRef = null, CancellationToken ct = default);
        Task<ChatMessage> SendDirectMessageAsync(Guid userId, string text, object metadata = null, string clientRef = null, CancellationToken ct = default);
        Task<ChatHistoryPage> HistoryAsync(Guid channelId, string before = null, string after = null, int? limit = null, CancellationToken ct = default);
        Task<ChatHistoryPage> DirectHistoryAsync(Guid userId, string before = null, string after = null, int? limit = null, CancellationToken ct = default);
        Task MarkReadAsync(Guid channelId, Guid messageId, CancellationToken ct = default);
        Task MarkDirectReadAsync(Guid userId, Guid messageId, CancellationToken ct = default);
        Task<ChatReadMarkers> ReadMarkersAsync(Guid channelId, CancellationToken ct = default);
        Task<ChatReadMarkers> DirectReadMarkersAsync(Guid userId, CancellationToken ct = default);
        Task<ChatMessage> EditMessageAsync(Guid messageId, string text, object metadata = null, CancellationToken ct = default);
        Task<ChatMessage> DeleteMessageAsync(Guid messageId, CancellationToken ct = default);
        Task ReactAsync(Guid messageId, string reaction, bool add = true, CancellationToken ct = default);
        Task<ChatHistoryPage> SearchAsync(Guid channelId, string query, Guid? fromUserId = null, string before = null, int? limit = null, CancellationToken ct = default);
        Task<ChatHistoryPage> SearchDirectAsync(Guid? userId, string query, Guid? fromUserId = null, string before = null, int? limit = null, CancellationToken ct = default);
        Task SetTypingAsync(Guid channelId, bool typing, TimeSpan? interval = null, CancellationToken ct = default);

        Task<SpeechRequest> SpeakAsync(string text, Guid? channelId = null, TtsDestination destination = TtsDestination.Channel,
            string voice = null, string clientRef = null, CancellationToken ct = default);
        Task CancelSpeechAsync(CancellationToken ct = default);

        Task UpdatePositionAsync(Guid channelId, Guid selfUserId, Position3D position, Orientation3D orientation, CancellationToken ct = default);
        Task RespondToRecordingAsync(Guid recordingId, RecordingConsent consent, CancellationToken ct = default);
        Task ReportQualityAsync(CancellationToken ct = default);

        /// <summary>Pump queued events onto the calling thread; call once per frame.</summary>
        void Update();
    }
}
