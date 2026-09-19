// Blueprint-visible mirrors of the aurix-client C ABI types.
#pragma once

#include "CoreMinimal.h"
#include "AurixVoiceTypes.generated.h"

class USoundClass;

UENUM(BlueprintType)
enum class EAurixConnectionState : uint8
{
	Disconnected,
	Connecting,
	/** WebSocket session open; media not yet bound. */
	Connected,
	/** UDP media bound: audio flows. */
	MediaBound,
	Reconnecting,
	Failed,
};

UENUM(BlueprintType)
enum class EAurixTransmissionMode : uint8
{
	/** Microphone reaches no channel. */
	None,
	/** Microphone reaches one channel. */
	Single,
	/** Microphone reaches every joined channel. */
	All,
};

UENUM(BlueprintType)
enum class EAurixRole : uint8
{
	Listener,
	Speaker,
	Moderator,
	Administrator,
};

UENUM(BlueprintType)
enum class EAurixModerationAction : uint8
{
	Kick,
	Mute,
	Unmute,
};

UENUM(BlueprintType)
enum class EAurixTtsDestination : uint8
{
	/** Heard by the channel (as this participant's voice) and locally. */
	Both,
	Channel,
	Local,
};

UENUM(BlueprintType)
enum class EAurixTtsState : uint8
{
	Queued,
	Playing,
	Finished,
	Cancelled,
	Failed,
};

/** Connection parameters for UAurixVoiceSubsystem::Connect. */
USTRUCT(BlueprintType)
struct AURIXVOICE_API FAurixVoiceSettings
{
	GENERATED_BODY()

	/** ws://host:port/ws or wss://… */
	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix")
	FString WebSocketUrl;

	/**
	 * Per-user session JWT (or one-time login action token) minted by the game backend at
	 * runtime. Deliberately not editable in assets: never bake tokens into Blueprints or configs.
	 */
	UPROPERTY(BlueprintReadWrite, Category = "Aurix")
	FString Token;

	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix|Reconnect")
	bool bAutoReconnect = true;

	/** 0 = never give up. */
	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix|Reconnect")
	int32 ReconnectMaxAttempts = 0;

	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix|Reconnect")
	int32 ReconnectInitialDelayMs = 500;

	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix|Reconnect")
	int32 ReconnectMaxDelayMs = 15000;

	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix")
	int32 RequestTimeoutMs = 10000;

	/** Opus target bitrate, 6000..128000. */
	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix|Audio")
	int32 BitrateBps = 32000;

	/** Jitter buffer depth before playout starts, in 20 ms frames. */
	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix|Audio")
	int32 JitterTargetFrames = 3;

	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix|Audio")
	int32 JitterMaxFrames = 25;

	/** Only send frames the voice activity detector marks as speech. */
	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix|Audio")
	bool bVadGate = true;

	/** Open the microphone (AudioCapture) as soon as the session is ready. */
	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix|Audio")
	bool bAutoStartCapture = true;

	/** Capture device index from GetCaptureDevices(); -1 = system default. */
	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix|Audio")
	int32 CaptureDeviceIndex = -1;

	/** Create a 2D audio component that plays the mixed remote voices. */
	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix|Audio")
	bool bAutoStartPlayback = true;

	/** Optional sound class for the playback component (for the game's mixer / ducking). */
	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix|Audio")
	TObjectPtr<USoundClass> PlaybackSoundClass = nullptr;
};

USTRUCT(BlueprintType)
struct AURIXVOICE_API FAurixSessionInfo
{
	GENERATED_BODY()

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	FGuid SessionId;

	/** This client's user id (invalid GUID when the token carries no `sub`). */
	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	FGuid UserId;

	/** Own SSRC (also the base of the TTS voice SSRC). Stored as int64 for Blueprint. */
	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	int64 Ssrc = 0;

	/** How long the server keeps the session resumable after a drop. */
	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	int32 ResumeGraceMs = 0;

	/** The latest (re)connect resumed the previous session. */
	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	bool bResumed = false;
};

/** Channel member snapshot. For OnChannelEnergy only UserId and Energy are meaningful. */
USTRUCT(BlueprintType)
struct AURIXVOICE_API FAurixParticipant
{
	GENERATED_BODY()

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	FGuid UserId;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	FString DisplayName;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	int64 Ssrc = 0;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	EAurixRole Role = EAurixRole::Listener;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	bool bMuted = false;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	bool bServerMuted = false;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	bool bSpeaking = false;

	/** 0..1 */
	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	float Energy = 0.f;
};

USTRUCT(BlueprintType)
struct AURIXVOICE_API FAurixChatMessage
{
	GENERATED_BODY()

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	FGuid MessageId;

	/** Invalid GUID for directed messages. */
	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	FGuid ChannelId;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	FGuid SenderId;

	/** Invalid GUID for channel messages. */
	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	FGuid RecipientId;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	FString SenderName;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	FString Text;

	/** JSON or empty. */
	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	FString MetadataJson;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	FDateTime SentAt;

	/** Non-zero when this is the echo of a message this client sent. */
	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	int64 RequestId = 0;
};

USTRUCT(BlueprintType)
struct AURIXVOICE_API FAurixTranscript
{
	GENERATED_BODY()

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	FGuid ChannelId;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	FGuid UserId;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	FString Text;

	/** BCP-47 or empty. */
	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	FString Language;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	FDateTime StartedAt;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	int32 DurationMs = 0;
};

USTRUCT(BlueprintType)
struct AURIXVOICE_API FAurixTtsStatus
{
	GENERATED_BODY()

	/** Id returned by Speak(); 0 if the request was not made by this client. */
	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	int64 RequestId = 0;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	FGuid ServerRequestId;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	EAurixTtsState State = EAurixTtsState::Queued;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	int32 DurationMs = 0;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	FString Message;
};

/** Pose of one user for a positional channel, in metres (the channel config decides handedness). */
USTRUCT(BlueprintType)
struct AURIXVOICE_API FAurixPosition
{
	GENERATED_BODY()

	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix")
	FGuid UserId;

	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix")
	FVector Location = FVector::ZeroVector;

	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix")
	FVector Forward = FVector::ForwardVector;

	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix")
	FVector Up = FVector::UpVector;
};

/**
 * Server-side view of the connection in both directions. Bars are 1..5
 * (R >= 80/70/60/50 -> 5/4/3/2, else 1); loss values are percentages.
 */
USTRUCT(BlueprintType)
struct AURIXVOICE_API FAurixNetworkQuality
{
	GENERATED_BODY()

	/** 1 (unusable) .. 5 (excellent); 0 until the server has reported. */
	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	int32 Bars = 0;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	float RFactor = 0.f;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	float Mos = 0.f;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	float RttMs = 0.f;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	float DownlinkJitterMs = 0.f;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	float DownlinkLossPercent = 0.f;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	float UplinkJitterMs = 0.f;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	float UplinkLossPercent = 0.f;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	int32 UplinkBitrateKbps = 0;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	int64 UplinkPacketsReceived = 0;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	int64 UplinkPacketsLost = 0;
};

/**
 * Transport and codec counters for a network-quality indicator. Counters are lifetime
 * totals; LossPercent / RFactor / Mos / Bars describe the last quality period.
 */
USTRUCT(BlueprintType)
struct AURIXVOICE_API FAurixStats
{
	GENERATED_BODY()

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	int64 PacketsSent = 0;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	int64 BytesSent = 0;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	int64 PacketsReceived = 0;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	int64 BytesReceived = 0;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	int64 AudioFramesReceived = 0;

	/** Packets that failed authentication (tampering or a stale key). */
	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	int64 BadAuth = 0;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	int64 Replayed = 0;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	int64 HeartbeatsLost = 0;

	/** Downlink frames concealed (PLC), discarded as late, and jitter-buffer underruns. */
	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	int64 FramesLost = 0;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	int64 FramesLate = 0;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	int64 Underruns = 0;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	float RttMs = 0.f;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	float RttMinMs = 0.f;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	float RttAvgMs = 0.f;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	float RttMaxMs = 0.f;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	float JitterMs = 0.f;

	/** Downlink loss over the last quality period, as a percentage (0..100). */
	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	float LossPercent = 0.f;

	/** Client-measured downlink quality; Bars is 1..5. */
	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	float RFactor = 0.f;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	float Mos = 0.f;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	int32 Bars = 0;

	/** True when Server holds the latest server-reported quality. */
	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	bool bHasServer = false;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	FAurixNetworkQuality Server;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	int64 FramesEncoded = 0;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	int64 FramesSent = 0;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	int64 FramesGated = 0;

	/** Remote streams currently decoding. */
	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	int32 ActiveStreams = 0;
};
