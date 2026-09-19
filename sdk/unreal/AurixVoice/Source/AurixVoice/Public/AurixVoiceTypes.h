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
enum class EAurixAudioCodec : uint8
{
	/** Default: 48 kHz Opus. */
	Opus,
	/** G.711 mu-law fallback: 8 kHz, 64 kbit/s, no Opus CPU cost, telephone quality. */
	Pcmu,
};

UENUM(BlueprintType)
enum class EAurixRole : uint8
{
	Listener,
	Speaker,
	Moderator,
	Administrator,
};

/** Which link carries media (FAurixVoiceSettings::MediaPath). */
UENUM(BlueprintType)
enum class EAurixMediaPathPolicy : uint8
{
	/** UDP first; the WebSocket tunnel when UDP is blocked; back to UDP once it answers again. */
	Auto,
	UdpOnly,
	TunnelOnly,
};

/** Link the media currently travels over. */
UENUM(BlueprintType)
enum class EAurixMediaPath : uint8
{
	/** No media link yet (before OnMediaBound). */
	None,
	/** Native AURX over UDP. */
	Udp,
	/** AURX packets as binary frames on the control WebSocket (TCP: higher latency under loss). */
	Tunnel,
};

UENUM(BlueprintType)
enum class EAurixModerationAction : uint8
{
	Kick,
	Mute,
	Unmute,
};

/** Widest audio band the Opus encoder may code (OPUS_SET_MAX_BANDWIDTH). */
UENUM(BlueprintType)
enum class EAurixOpusBandwidth : uint8
{
	/** 4 kHz. */
	Narrowband,
	/** 6 kHz. */
	Mediumband,
	/** 8 kHz. */
	Wideband,
	/** 12 kHz. */
	Superwideband,
	/** 20 kHz. */
	Fullband,
};

/** Opus content hint (OPUS_SET_SIGNAL). */
UENUM(BlueprintType)
enum class EAurixOpusSignal : uint8
{
	Auto,
	Voice,
	Music,
};

/** Uplink Opus encoder settings; values outside libopus' ranges are clamped by the core. */
USTRUCT(BlueprintType)
struct AURIXVOICE_API FAurixEncoderSettings
{
	GENERATED_BODY()

	/** 6000..300000 bit/s. */
	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix")
	int32 BitrateBps = 32000;

	/** 0..10; 10 = best quality at the most CPU (libopus default 9). */
	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix")
	int32 Complexity = 9;

	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix")
	EAurixOpusBandwidth MaxBandwidth = EAurixOpusBandwidth::Fullband;

	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix")
	EAurixOpusSignal Signal = EAurixOpusSignal::Voice;

	/** Variable bitrate; off = hard CBR at BitrateBps. */
	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix")
	bool bVbr = true;

	/** Constrained VBR keeps every frame within the bitrate's byte budget. */
	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix")
	bool bConstrainedVbr = true;

	/** In-band forward error correction. */
	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix")
	bool bFec = true;

	/** Packet loss the FEC is tuned for, 0..100 %. */
	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix")
	int32 ExpectedLossPercent = 5;

	/** Discontinuous transmission during silence. */
	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix")
	bool bDtx = false;
};

/** A channel's audio policy (operator-set ChannelConfig), merged over the joined channels. */
USTRUCT(BlueprintType)
struct AURIXVOICE_API FAurixAudioPolicy
{
	GENERATED_BODY()

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	int32 BitrateBps = 0;

	/** Floor the server's adaptive bitrate never goes below. */
	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	int32 MinBitrateBps = 0;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	bool bFec = false;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	bool bDtx = false;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	EAurixOpusBandwidth MaxBandwidth = EAurixOpusBandwidth::Fullband;

	/** -1 = the channel gives no complexity hint. */
	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	int32 Complexity = -1;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	EAurixOpusSignal Signal = EAurixOpusSignal::Auto;
};

/** Strength of the RNNoise-derived neural noise suppressor in the capture chain. */
UENUM(BlueprintType)
enum class EAurixNoiseSuppression : uint8
{
	Off,
	Low,
	Moderate,
	High,
};

/**
 * Microphone processing in the native core, applied after resampling and before the input gain,
 * VAD and encoder: high-pass → echo cancellation → noise suppression → AGC. Out-of-range values
 * are clamped by the core.
 */
USTRUCT(BlueprintType)
struct AURIXVOICE_API FAurixDspSettings
{
	GENERATED_BODY()

	/** 80 Hz second-order high-pass: rumble, handling noise, DC offset. */
	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix")
	bool bHighPass = true;

	/**
	 * Acoustic echo cancellation. The reference is whatever the core renders (MixOutputAudio /
	 * the plugin's sound wave); feed other speaker audio with PushRenderAudio.
	 */
	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix")
	bool bEchoCancellation = true;

	/** Longest echo path modelled, 40..500 ms (rounded to 10 ms). Headsets 100–200, open speakers 300–500. */
	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix", meta = (ClampMin = "40", ClampMax = "500"))
	int32 EchoTailMs = 200;

	/** Known extra render→capture latency, 0..500 ms; 0 lets the delay estimator find it. */
	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix", meta = (ClampMin = "0", ClampMax = "500"))
	int32 StreamDelayMs = 0;

	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix")
	EAurixNoiseSuppression NoiseSuppression = EAurixNoiseSuppression::High;

	/** Speech-gated automatic gain control with a soft limiter. */
	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix")
	bool bAgc = true;

	/** Speech level the AGC aims for, -30..-6 dBFS. */
	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix", meta = (ClampMin = "-30", ClampMax = "-6"))
	float AgcTargetDbfs = -18.f;

	/** Most boost the AGC applies, 0..40 dB. */
	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix", meta = (ClampMin = "0", ClampMax = "40"))
	float AgcMaxGainDb = 24.f;
};

/** Live diagnostics of the capture DSP. */
USTRUCT(BlueprintType)
struct AURIXVOICE_API FAurixDspStats
{
	GENERATED_BODY()

	/** Echo return loss enhancement of the canceller, dB (0 while idle). */
	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	float ErleDb = 0.f;

	/** Render→capture delay the estimator locked on to, ms. */
	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	int32 EchoDelayMs = 0;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	bool bEchoConverged = false;

	/** The speakers are currently playing something the canceller tracks. */
	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	bool bFarEndActive = false;

	/** 0..1 from the noise suppressor's voice model (0.5 when it is off). */
	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	float SpeechProbability = 0.f;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	float AgcGainDb = 0.f;

	/** Times the canceller needed render audio that had not been pushed yet. */
	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	int64 FarEndUnderruns = 0;
};

/**
 * How far presence and text reach in a positional channel (`PositionalConfig.roster_radius` /
 * `text_radius`, from `ChannelJoinAck`). A radius of 0 means "the whole channel".
 */
USTRUCT(BlueprintType)
struct AURIXVOICE_API FAurixChannelScope
{
	GENERATED_BODY()

	/**
	 * The roster only lists members within this distance of us (once both positions are known);
	 * OnParticipantJoined / OnParticipantLeft also fire when someone moves in or out of range
	 * (leaving uses a 10 % wider radius so the edge does not flicker).
	 */
	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	float RosterRadius = 0.f;

	/** Channel chat, typing and transcripts reach only members within this distance. */
	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	float TextRadius = 0.f;
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

	/** Uplink Opus encoder before any channel policy applies. */
	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix|Audio")
	FAurixEncoderSettings Encoder;

	/** Microphone processing (high-pass, echo cancellation, noise suppression, AGC). */
	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix|Audio")
	FAurixDspSettings Dsp;

	/**
	 * Adopt each joined channel's audio policy (bitrate, FEC/DTX, bandwidth, signal and the
	 * complexity hint unless pinned with SetComplexity). Server bitrate commands apply either way.
	 */
	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix|Audio")
	bool bFollowChannelPolicy = true;

	/** Jitter buffer depth before playout starts, in 20 ms frames. */
	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix|Audio")
	int32 JitterTargetFrames = 3;

	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix|Audio")
	int32 JitterMaxFrames = 25;

	/** Only send frames the voice activity detector marks as speech. */
	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix|Audio")
	bool bVadGate = true;

	/**
	 * Media link: UDP with the WebSocket tunnel as fallback (default), UDP only, or tunnel only.
	 * The tunnel keeps the same session, SSRC, key and encryption; only latency under loss differs.
	 */
	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix|Network")
	EAurixMediaPathPolicy MediaPath = EAurixMediaPathPolicy::Auto;

	/** Auto: unanswered UDP heartbeats in a row before media moves to the tunnel (0 = never mid-session). */
	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix|Network")
	int32 UdpFallbackLostHeartbeats = 3;

	/** Auto: how often a tunnelled session re-probes UDP and moves back when it answers (0 = never). */
	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix|Network")
	int32 UdpReprobeIntervalMs = 30000;

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

	/** The node accepts media tunnelled over the control WebSocket (fallback when UDP is blocked). */
	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	bool bMediaTunnel = false;
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

	/** Unanswered heartbeats in a row on the current link (resets on every reply). */
	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	int32 HeartbeatsLostConsecutive = 0;

	/** Link the media uses right now. */
	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	EAurixMediaPath MediaPath = EAurixMediaPath::None;

	/** Uplink packets dropped because the tunnel send queue was full (0 on UDP). */
	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	int64 UplinkDropped = 0;

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

/** Parameters for UAurixVoiceSubsystem::DiscoverRegions (GET /v1/me/regions + optional RTT probes). */
USTRUCT(BlueprintType)
struct AURIXVOICE_API FAurixRegionDiscoveryRequest
{
	GENERATED_BODY()

	/** Base REST URL of any node or the shared API name, e.g. https://voice.example.com */
	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix")
	FString ApiUrl;

	/** The player's session JWT; discovery is player-scoped. Never bake tokens into assets. */
	UPROPERTY(BlueprintReadWrite, Category = "Aurix")
	FString Token;

	/** Region to put first when it is reachable (for example the party leader's region). */
	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix")
	FString PreferredRegion;

	/** Send Latitude/Longitude so the server can order regions by distance. */
	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix")
	bool bHasLocation = false;

	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix")
	double Latitude = 0.0;

	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix")
	double Longitude = 0.0;

	/** Measure HTTP round-trip time to each region's probe URL and rank by it. */
	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix|Probe")
	bool bProbe = true;

	/** Successful samples per region (one extra warm-up request is discarded). */
	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix|Probe")
	int32 ProbeSamples = 3;

	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix|Probe")
	float ProbeTimeoutSeconds = 2.f;

	/** Regions whose RTT differs by less than this keep the server's order. */
	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix|Probe")
	float RttToleranceMs = 15.f;
};

/** One region as advertised by the server, plus the local probe result. Pass WsUrl to Connect. */
USTRUCT(BlueprintType)
struct AURIXVOICE_API FAurixRegionEndpoint
{
	GENERATED_BODY()

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	FString Region;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	FGuid NodeId;

	/** Public WebSocket URL of the least-loaded node in the region (sessions are node-local). */
	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	FString WsUrl;

	/** HTTP URL suitable for RTT probing; empty when the node advertises none. */
	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	FString ProbeUrl;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	bool bHasLocation = false;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	double Latitude = 0.0;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	double Longitude = 0.0;

	/** Great-circle distance from the location sent with the request. */
	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	bool bHasDistance = false;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	float DistanceKm = 0.f;

	/** Healthy nodes with spare capacity in the region. */
	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	int32 Nodes = 0;

	/** Load of the advertised node, 0..1. */
	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	float LoadFactor = 0.f;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	bool bHasRtt = false;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	float RttMs = 0.f;

	/** Every probe of this region failed; such regions rank last. */
	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	bool bProbeFailed = false;
};
