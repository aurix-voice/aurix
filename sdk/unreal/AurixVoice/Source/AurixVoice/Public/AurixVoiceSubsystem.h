#pragma once

#include "CoreMinimal.h"
#include "Subsystems/GameInstanceSubsystem.h"
#include "Tickable.h"
#include "AurixVoiceTypes.h"
#include "AurixVoiceSubsystem.generated.h"

class UAudioComponent;
class UAurixVoiceSoundWave;
class UAurixParticipantSoundWave;
class USceneComponent;
class USoundAttenuation;
class FAurixAudioCapture;
class FAurixRegionDiscovery;
struct FAurixNativeClient;
struct AurixEvent;

DECLARE_DYNAMIC_MULTICAST_DELEGATE_OneParam(FAurixStateChanged, EAurixConnectionState, State);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_OneParam(FAurixSessionReady, const FAurixSessionInfo&, Session);
DECLARE_DYNAMIC_MULTICAST_DELEGATE(FAurixMediaBound);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_FourParams(FAurixChannelJoined, int64, RequestId, FGuid, ChannelId, const TArray<FAurixParticipant>&, Participants, bool, bTranscription);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_OneParam(FAurixChannelLeft, FGuid, ChannelId);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_TwoParams(FAurixParticipantJoined, FGuid, ChannelId, const FAurixParticipant&, Participant);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_TwoParams(FAurixParticipantLeft, FGuid, ChannelId, FGuid, UserId);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_FourParams(FAurixParticipantMuteChanged, FGuid, ChannelId, FGuid, UserId, bool, bMuted, bool, bServerMuted);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_ThreeParams(FAurixParticipantSpeaking, FGuid, ChannelId, FGuid, UserId, bool, bSpeaking);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_TwoParams(FAurixChannelEnergy, FGuid, ChannelId, const TArray<FAurixParticipant>&, Levels);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_OneParam(FAurixLocalSpeaking, bool, bSpeaking);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_TwoParams(FAurixTransmissionChanged, EAurixTransmissionMode, Mode, FGuid, ChannelId);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_OneParam(FAurixChannelFocusChanged, FGuid, ChannelId);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_OneParam(FAurixAudioCodecChanged, EAurixAudioCodec, Codec);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_OneParam(FAurixDownlinkModeChanged, EAurixDownlinkMode, Mode);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_TwoParams(FAurixMediaPathChanged, EAurixMediaPath, Path, const FString&, Reason);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_TwoParams(FAurixUserBlockChanged, FGuid, UserId, bool, bBlocked);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_FourParams(FAurixRecording, FGuid, ChannelId, FGuid, RecordingId, bool, bActive, FGuid, InitiatedBy);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_TwoParams(FAurixBitrateChanged, int32, BitrateBps, const FString&, Reason);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_OneParam(FAurixNetworkQualityChanged, const FAurixNetworkQuality&, Quality);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_OneParam(FAurixAudioPolicyChanged, const FAurixAudioPolicy&, Policy);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_TwoParams(FAurixKicked, FGuid, ChannelId, const FString&, Reason);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_FourParams(FAurixModerationApplied, int64, RequestId, FGuid, ChannelId, FGuid, UserId, EAurixModerationAction, Action);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_OneParam(FAurixChatMessageReceived, const FAurixChatMessage&, Message);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_TwoParams(FAurixChatHistoryReceived, int64, RequestId, const FAurixChatHistoryPage&, Page);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_OneParam(FAurixChatReadMarkerChanged, const FAurixReadMarker&, Marker);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_FourParams(FAurixChatReadMarkersReceived, FGuid, ChannelId, FGuid, PeerUserId, const TArray<FAurixReadMarker>&, Markers, int32, UnreadCount);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_TwoParams(FAurixChatInboxSynced, int32, Delivered, bool, bTruncated);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_ThreeParams(FAurixParticipantTyping, FGuid, ChannelId, FGuid, UserId, bool, bTyping);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_OneParam(FAurixTranscriptReceived, const FAurixTranscript&, Transcript);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_OneParam(FAurixTtsStatusChanged, const FAurixTtsStatus&, Status);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_ThreeParams(FAurixRequestFailed, int64, RequestId, const FString&, Code, const FString&, Message);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_TwoParams(FAurixServerError, const FString&, Code, const FString&, Message);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_ThreeParams(FAurixRejoinFailed, FGuid, ChannelId, const FString&, Code, const FString&, Message);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_ThreeParams(FAurixRecovering, int32, Attempt, int32, DelayMs, const FString&, Cause);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_TwoParams(FAurixRecovered, bool, bResumed, bool, bMigrated);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_OneParam(FAurixEndpointChanged, const FString&, Url);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_OneParam(FAurixConnectionEnded, const FString&, Reason);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_OneParam(FAurixRawEvent, const FString&, Json);
DECLARE_DYNAMIC_DELEGATE_ThreeParams(FAurixRegionsDiscovered, bool, bSuccess, const TArray<FAurixRegionEndpoint>&, Regions, const FString&, Error);

/**
 * One voice session per game instance: connects to an Aurix node with the player's JWT, binds
 * encrypted UDP media, joins channels, captures the microphone (AudioCapture) and plays the mix
 * of remote voices through a 2D audio component. Events from the native client are pumped on
 * the game thread every tick and broadcast through the delegates below.
 */
UCLASS()
class AURIXVOICE_API UAurixVoiceSubsystem : public UGameInstanceSubsystem, public FTickableGameObject
{
	GENERATED_BODY()

public:
	UAurixVoiceSubsystem();
	virtual ~UAurixVoiceSubsystem() override;

	//~ USubsystem
	virtual void Initialize(FSubsystemCollectionBase& Collection) override;
	virtual void Deinitialize() override;

	//~ FTickableGameObject
	virtual void Tick(float DeltaTime) override;
	virtual ETickableTickType GetTickableTickType() const override { return IsTemplate() ? ETickableTickType::Never : ETickableTickType::Conditional; }
	virtual bool IsTickable() const override;
	virtual bool IsTickableInEditor() const override { return false; }
	virtual bool IsTickableWhenPaused() const override { return true; }
	virtual TStatId GetStatId() const override { RETURN_QUICK_DECLARE_CYCLE_STAT(UAurixVoiceSubsystem, STATGROUP_Tickables); }

	// ---- lifecycle -------------------------------------------------------------------------

	/** Create the native client and start connecting (asynchronous; watch OnSessionReady / OnConnectionEnded). */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice")
	bool Connect(const FAurixVoiceSettings& Settings);

	/** Leave the session, stop capture/playback and free the native client. */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice")
	void Disconnect();

	/** Replace the token used by the next (re)connect. */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice")
	bool SetToken(const FString& Token);

	UFUNCTION(BlueprintPure, Category = "Aurix Voice")
	EAurixConnectionState GetConnectionState() const;

	UFUNCTION(BlueprintPure, Category = "Aurix Voice")
	bool IsConnected() const;

	UFUNCTION(BlueprintPure, Category = "Aurix Voice")
	bool GetSession(FAurixSessionInfo& OutSession) const;

	/** Message of the last failed call (see the log for details). */
	UFUNCTION(BlueprintPure, Category = "Aurix Voice")
	FString GetLastError() const;

	/** Version of the native aurix-client library. */
	UFUNCTION(BlueprintPure, Category = "Aurix Voice")
	static FString GetNativeVersion();

	// ---- channels --------------------------------------------------------------------------

	/** Join a channel; the returned request id is echoed by OnChannelJoined / OnRequestFailed. */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Channels")
	bool JoinChannel(FGuid ChannelId, const FString& JoinToken, int64& RequestId);

	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Channels")
	bool LeaveChannel(FGuid ChannelId);

	UFUNCTION(BlueprintPure, Category = "Aurix Voice|Channels")
	TArray<FGuid> GetJoinedChannels() const;

	UFUNCTION(BlueprintPure, Category = "Aurix Voice|Channels")
	TArray<FAurixParticipant> GetParticipants(FGuid ChannelId) const;

	/** Speech in the channel is transcribed server-side (captions arrive as OnTranscript). */
	UFUNCTION(BlueprintPure, Category = "Aurix Voice|Channels")
	bool IsChannelTranscribed(FGuid ChannelId) const;

	/** Speech in the channel is analysed by the server's content-safety classifier — disclose it to the player. */
	UFUNCTION(BlueprintPure, Category = "Aurix Voice|Channels")
	bool IsChannelMonitored(FGuid ChannelId) const;

	/** Presence / text range of a joined positional channel (0 = whole channel); false until the join is acknowledged. */
	UFUNCTION(BlueprintPure, Category = "Aurix Voice|Channels")
	bool GetChannelScope(FGuid ChannelId, FAurixChannelScope& OutScope) const;

	/** Our role, the member count and the roster policy of a joined channel; false until the join is acknowledged. */
	UFUNCTION(BlueprintPure, Category = "Aurix Voice|Channels")
	bool GetChannelInfo(FGuid ChannelId, FAurixChannelInfo& OutInfo) const;

	/** Whether this session may transmit in the channel (false for listeners and unknown channels). */
	UFUNCTION(BlueprintPure, Category = "Aurix Voice|Channels")
	bool CanSpeakIn(FGuid ChannelId) const;

	/** Owner of an SSRC (microphone or its TTS voice) across joined channels. */
	UFUNCTION(BlueprintPure, Category = "Aurix Voice|Channels")
	bool GetUserForSsrc(int64 Ssrc, FGuid& OutUserId) const;

	// ---- microphone ------------------------------------------------------------------------

	UFUNCTION(BlueprintPure, Category = "Aurix Voice|Microphone")
	static TArray<FString> GetCaptureDevices();

	/** Open the microphone (-1 = default device). Also called automatically when bAutoStartCapture is set. */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Microphone")
	bool StartCapture(int32 DeviceIndex = -1);

	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Microphone")
	void StopCapture();

	UFUNCTION(BlueprintPure, Category = "Aurix Voice|Microphone")
	bool IsCapturing() const;

	/** Feed your own capture pipeline instead of the built-in one (interleaved float PCM, any rate). */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Microphone")
	void PushCaptureAudio(const TArray<float>& InterleavedPcm, int32 SampleRate, int32 Channels);

	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Microphone")
	void SetMuted(bool bMuted);

	UFUNCTION(BlueprintPure, Category = "Aurix Voice|Microphone")
	bool IsMuted() const;

	UFUNCTION(BlueprintPure, Category = "Aurix Voice|Microphone")
	bool IsSpeaking() const;

	/** Software gain 0..4. */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Microphone")
	void SetInputGain(float Gain);

	/** RMS 0..1 of the last captured frame. */
	UFUNCTION(BlueprintPure, Category = "Aurix Voice|Microphone")
	float GetInputEnergy() const;

	/** Voice activity detector: RMS threshold 0..1 and hangover in 20 ms frames. */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Microphone")
	void SetVoiceActivityDetector(float Threshold, int32 HangoverFrames);

	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Microphone")
	void SetVadGate(bool bEnabled);

	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Microphone")
	bool SetBitrate(int32 BitrateBps);

	/**
	 * Replace the baseline Opus settings (bitrate, complexity, bandwidth, VBR/FEC/DTX). The channel
	 * policy is laid over them while bFollowChannelPolicy is set. Takes effect from the next frame.
	 */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Microphone")
	bool SetEncoderSettings(const FAurixEncoderSettings& Settings);

	/** Settings the encoder is running with right now (after policy and server bitrate commands). */
	UFUNCTION(BlueprintPure, Category = "Aurix Voice|Microphone")
	bool GetEncoderSettings(FAurixEncoderSettings& OutSettings) const;

	/** Pin Opus complexity 0..10 regardless of channel hints (e.g. lower on a weak CPU); -1 unpins. */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Microphone")
	bool SetComplexity(int32 Complexity);

	/** Replace the capture processing (high-pass / AEC / NS / AGC) at runtime, e.g. from a settings menu. */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Microphone")
	bool SetDspSettings(const FAurixDspSettings& Settings);

	/** Capture processing the core is running with (after clamping). */
	UFUNCTION(BlueprintPure, Category = "Aurix Voice|Microphone")
	bool GetDspSettings(FAurixDspSettings& OutSettings) const;

	/** Echo canceller / noise suppressor / AGC diagnostics for an overlay. */
	UFUNCTION(BlueprintPure, Category = "Aurix Voice|Microphone")
	bool GetDspStats(FAurixDspStats& OutStats) const;

	/**
	 * Give the echo canceller speaker audio the core did not render itself (game audio, music):
	 * interleaved 48 kHz float PCM, 1..2 channels, in playout order. Not needed for the remote
	 * voice mix — MixOutputAudio and the plugin's sound wave feed it automatically.
	 */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Microphone")
	void PushRenderAudio(const TArray<float>& InterleavedPcm, int32 Channels);

	/** Merged audio policy of the joined channels; false before the first join. */
	UFUNCTION(BlueprintPure, Category = "Aurix Voice|Microphone")
	bool GetAudioPolicy(FAurixAudioPolicy& OutPolicy) const;

	// ---- playback --------------------------------------------------------------------------

	/** Start the 2D playback component (automatic when bAutoStartPlayback is set). */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Playback")
	bool StartPlayback();

	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Playback")
	void StopPlayback();

	/** The component playing the remote mix (nullptr until StartPlayback). */
	UFUNCTION(BlueprintPure, Category = "Aurix Voice|Playback")
	UAudioComponent* GetPlaybackComponent() const { return PlaybackComponent; }

	/** Add the remote mix into your own output buffer (interleaved float, 48 kHz, 1 or 2 channels); returns active streams. */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Playback")
	int32 MixOutputAudio(UPARAM(ref) TArray<float>& InterleavedPcm, int32 Channels);

	// ---- per-participant playback (engine spatialization) ----------------------------------

	/**
	 * A procedural sound carrying only UserId's voice (microphone + TTS), with no local
	 * panning — play it through an in-world UAudioComponent with attenuation / spatializer
	 * plugin / occlusion / reverb of your choice. The participant is claimed: the aggregate
	 * 2D mix (StartPlayback / MixOutputAudio) skips it, unclaimed talkers keep playing there,
	 * so both can run side by side without anyone being heard twice. One sound per user;
	 * repeated calls return the same object. Stereo keeps a music sender's L/R image but is
	 * not spatialized by Unreal. Survives reconnects and node failover; release it with
	 * ReleaseParticipantSound when the actor goes away (the user leaving does not release it).
	 */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Playback", meta = (AdvancedDisplay = "bStereo"))
	UAurixParticipantSoundWave* CreateParticipantSound(FGuid UserId, bool bStereo = false);

	/**
	 * CreateParticipantSound + an audio component attached to AttachTo (typically the avatar's
	 * head), spatialized with Attenuation (or the component's defaults when null) and playing
	 * immediately. Returns nullptr without a world or audio device. The component is owned by
	 * AttachTo's actor; stopping/destroying it does not release the claim — call
	 * ReleaseParticipantSound.
	 */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Playback", meta = (AdvancedDisplay = "Attenuation,bStereo"))
	UAudioComponent* SpawnParticipantAudioComponent(FGuid UserId, USceneComponent* AttachTo, USoundAttenuation* Attenuation = nullptr, bool bStereo = false);

	/** Return UserId to the aggregate mix and silence its participant sound (if any). */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Playback")
	void ReleaseParticipantSound(FGuid UserId);

	/** The participant sound created for UserId, or nullptr. */
	UFUNCTION(BlueprintPure, Category = "Aurix Voice|Playback")
	UAurixParticipantSoundWave* GetParticipantSound(FGuid UserId) const;

	/**
	 * Pull UserId's voice into your own buffer (interleaved float, 48 kHz, 1 or 2 channels,
	 * **overwritten**, no panning) for a custom audio graph. Claim the user first with
	 * SetParticipantClaimed so the aggregate mix leaves the frames for you. Returns frames
	 * that carried audio; the rest is silence.
	 */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Playback")
	int32 PullParticipantAudio(FGuid UserId, UPARAM(ref) TArray<float>& InterleavedPcm, int32 Channels);

	/** Keep UserId out of the aggregate mix while you render it yourself with PullParticipantAudio. */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Playback")
	void SetParticipantClaimed(FGuid UserId, bool bClaimed);

	/** Downlink streams currently buffered, with their owners (one per talker, plus TTS voices). */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Playback")
	TArray<FAurixParticipantStream> GetParticipantStreams() const;

	/** Master playback volume 0..2. */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Playback")
	void SetOutputVolume(float Volume);

	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Playback")
	void SetOutputMuted(bool bMuted);

	// ---- receiver preferences --------------------------------------------------------------

	/** Receiver-local mute of a user in one channel (invalid ChannelId = everywhere). */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Preferences")
	bool SetParticipantMuted(FGuid UserId, FGuid ChannelId, bool bMuted);

	/** Per-participant gain 0..2 for this listener. */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Preferences")
	bool SetParticipantVolume(FGuid UserId, float Volume);

	/** Persistent mutual block (acked by OnUserBlockChanged). */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Preferences")
	bool SetUserBlocked(FGuid UserId, bool bBlocked);

	/** Which joined channels receive the microphone; ChannelId is required for Single. */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Preferences")
	bool SetTransmission(EAurixTransmissionMode Mode, FGuid ChannelId);

	/** Focus one channel (others attenuated server-side); invalid ChannelId clears. */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Preferences")
	bool SetChannelFocus(FGuid ChannelId);

	/**
	 * Ask the server to run this session on Codec. Pcmu (G.711) is a low-CPU fallback for weak
	 * devices; the node transcodes, so Opus participants are unaffected. Needs
	 * `media.pcmu_fallback` on the node; applied on OnAudioCodecChanged.
	 */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Preferences")
	bool SetAudioCodec(EAurixAudioCodec Codec);

	/** Codec the session currently uses (server-acknowledged). */
	UFUNCTION(BlueprintPure, Category = "Aurix Voice|Preferences")
	EAurixAudioCodec GetAudioCodec() const;

	/**
	 * Receive other speakers as one server-mixed stereo stream per channel (Mixed) instead of one
	 * stream per speaker: constant bandwidth and decode cost in large channels. Needs
	 * FAurixSessionInfo::bDownlinkMix; applied on OnDownlinkModeChanged. E2EE speakers still
	 * arrive as separate streams.
	 */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Preferences")
	bool SetDownlinkMode(EAurixDownlinkMode Mode);

	/** Downlink mode the server acknowledged. */
	UFUNCTION(BlueprintPure, Category = "Aurix Voice|Preferences")
	EAurixDownlinkMode GetDownlinkMode() const;

	/**
	 * Link the media uses right now: UDP, the WebSocket tunnel (UDP blocked — expect higher
	 * latency under packet loss) or None before the first OnMediaBound.
	 */
	UFUNCTION(BlueprintPure, Category = "Aurix Voice|Preferences")
	EAurixMediaPath GetMediaPath() const;

	/** WebSocket URL of the node serving the session (the configured URL until a failover moved it). */
	UFUNCTION(BlueprintPure, Category = "Aurix Voice|Connection")
	FString GetEndpoint() const;

	/**
	 * Alternate nodes the server advertised for this session. A dropped connection retries the
	 * current node first, then these in order; OnEndpointChanged reports a switch.
	 */
	UFUNCTION(BlueprintPure, Category = "Aurix Voice|Connection")
	TArray<FString> GetFailoverEndpoints() const;

	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Preferences")
	bool SetTranscripts(bool bEnabled);

	// ---- positional audio ------------------------------------------------------------------

	/** Report 1..64 poses (metres, engine axes) for a positional channel. */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Positional")
	bool UpdatePositions(FGuid ChannelId, const TArray<FAurixPosition>& Positions);

	/** Report this player's pose from Unreal units (cm) and rotation. */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Positional")
	bool UpdateOwnPosition(FGuid ChannelId, FVector Location, FRotator Rotation, float WorldToMeters = 100.0f);

	// ---- recording / raw -------------------------------------------------------------------

	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Recording")
	bool RespondRecordingConsent(FGuid RecordingId, bool bAccept);

	/** Escape hatch: send a raw client→server control message as JSON. */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice")
	bool SendControlJson(const FString& Json);

	// ---- chat / moderation / speech --------------------------------------------------------

	/** Kick / mute / unmute with a one-time action token minted by the game backend. */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Moderation")
	bool Moderate(FGuid ChannelId, FGuid UserId, EAurixModerationAction Action, const FString& ActionToken, const FString& Reason, int64& RequestId);

	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Chat")
	bool SendChat(FGuid ChannelId, const FString& Text, const FString& MetadataJson, int64& RequestId);

	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Chat")
	bool SendDirectChat(FGuid UserId, const FString& Text, const FString& MetadataJson, int64& RequestId);

	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Chat")
	bool SetTyping(FGuid ChannelId, bool bTyping);

	/**
	 * One page of the stored history of a joined channel, newest first; answered by
	 * OnChatHistory (or OnRequestFailed). Before / After are cursors from an earlier page or
	 * from a message (empty = from the present); Limit 0 = server default.
	 */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Chat")
	bool ChannelHistory(FGuid ChannelId, const FString& Before, const FString& After, int32 Limit, int64& RequestId);

	/** One page of the stored direct conversation with UserId, newest first (see ChannelHistory). */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Chat")
	bool DirectHistory(FGuid UserId, const FString& Before, const FString& After, int32 Limit, int64& RequestId);

	/** Moves this user's read marker in the channel to MessageId (never backwards); every device gets OnChatReadMarker. */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Chat")
	bool MarkChannelRead(FGuid ChannelId, FGuid MessageId);

	/** Moves this user's read marker in the direct conversation with UserId to MessageId. */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Chat")
	bool MarkDirectRead(FGuid UserId, FGuid MessageId);

	/** Asks for the read markers and unread count of a channel; answered by OnChatReadMarkers. */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Chat")
	bool ChannelReadMarkers(FGuid ChannelId);

	/** Asks for the read markers and unread count of the direct conversation with UserId. */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Chat")
	bool DirectReadMarkers(FGuid UserId);

	/** Server-side text-to-speech as this participant's voice (ChannelId may be invalid for Local). */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Speech")
	bool Speak(const FString& Text, FGuid ChannelId, EAurixTtsDestination Destination, const FString& Voice, int64& RequestId);

	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Speech")
	bool CancelSpeech();

	UFUNCTION(BlueprintPure, Category = "Aurix Voice")
	bool GetStats(FAurixStats& OutStats) const;

	/** Latest server-reported quality (both directions); false until the first report. */
	UFUNCTION(BlueprintPure, Category = "Aurix Voice")
	bool GetNetworkQuality(FAurixNetworkQuality& OutQuality) const;

	// ---- regions ---------------------------------------------------------------------------

	/**
	 * Fetch the regions the player may connect to (GET /v1/me/regions with the player's JWT),
	 * optionally probe each region's RTT over HTTP and return them best-first: the preferred
	 * region when reachable, then by RTT (server order within RttToleranceMs), unprobed regions,
	 * unreachable ones last. Connect with Regions[0].WsUrl. Runs asynchronously; OnComplete fires
	 * on the game thread. Only one discovery runs at a time — a new call cancels the previous one.
	 */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Regions")
	void DiscoverRegions(const FAurixRegionDiscoveryRequest& Request, FAurixRegionsDiscovered OnComplete);

	/** Abort an in-flight DiscoverRegions; its OnComplete is not called. */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Regions")
	void CancelRegionDiscovery();

	// ---- events ----------------------------------------------------------------------------

	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixStateChanged OnConnectionStateChanged;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixSessionReady OnSessionReady;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixMediaBound OnMediaBound;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixChannelJoined OnChannelJoined;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixChannelLeft OnChannelLeft;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixParticipantJoined OnParticipantJoined;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixParticipantLeft OnParticipantLeft;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixParticipantMuteChanged OnParticipantMuteChanged;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixParticipantSpeaking OnParticipantSpeaking;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixChannelEnergy OnChannelEnergy;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixLocalSpeaking OnLocalSpeaking;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixTransmissionChanged OnTransmissionChanged;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixChannelFocusChanged OnChannelFocusChanged;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixAudioCodecChanged OnAudioCodecChanged;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixDownlinkModeChanged OnDownlinkModeChanged;
	/** Media moved between UDP and the WebSocket tunnel (also fires after every OnMediaBound). */
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixMediaPathChanged OnMediaPathChanged;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixUserBlockChanged OnUserBlockChanged;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixRecording OnRecording;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixBitrateChanged OnBitrateChanged;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixNetworkQualityChanged OnNetworkQuality;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixAudioPolicyChanged OnAudioPolicyChanged;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixKicked OnKicked;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixModerationApplied OnModerationApplied;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixChatMessageReceived OnChatMessage;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixChatHistoryReceived OnChatHistory;
	/** A read marker moved: this user's (any device) or, with server-side read receipts, another participant's. */
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixChatReadMarkerChanged OnChatReadMarker;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixChatReadMarkersReceived OnChatReadMarkers;
	/** Directed messages that arrived while offline were replayed (as OnChatMessage with bOffline); fires once per connection. */
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixChatInboxSynced OnChatInboxSynced;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixParticipantTyping OnParticipantTyping;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixTranscriptReceived OnTranscript;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixTtsStatusChanged OnTtsStatus;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixRequestFailed OnRequestFailed;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixServerError OnServerError;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixRejoinFailed OnRejoinFailed;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixRecovering OnRecovering;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixRecovered OnRecovered;
	/** A failover node answered while the previous one did not; fires before that connection's OnSessionReady. */
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixEndpointChanged OnEndpointChanged;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixConnectionEnded OnFailedToRecover;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixConnectionEnded OnDisconnected;
	/** Every event as JSON (positions arrive only here). */
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixRawEvent OnRawEvent;

	// ---- conversions -----------------------------------------------------------------------

	/** Parse an RFC 4122 UUID string into an FGuid whose ToString(DigitsWithHyphens) round-trips. */
	UFUNCTION(BlueprintPure, Category = "Aurix Voice|Utility")
	static bool ParseUuid(const FString& Text, FGuid& OutGuid);

	/** Format an FGuid as the lowercase RFC 4122 string the Aurix REST API uses. */
	UFUNCTION(BlueprintPure, Category = "Aurix Voice|Utility")
	static FString FormatUuid(FGuid Guid);

private:
	void PumpEvents();
	void DispatchEvent(const AurixEvent* Event);
	void OnPostLoadMap(UWorld* LoadedWorld);
	void ReleaseNative();

	TUniquePtr<FAurixNativeClient> Native;
	TUniquePtr<FAurixAudioCapture> Capture;
	TSharedPtr<FAurixRegionDiscovery> RegionDiscovery;
	FAurixVoiceSettings ActiveSettings;

	UPROPERTY(Transient)
	TObjectPtr<UAurixVoiceSoundWave> SoundWave;

	UPROPERTY(Transient)
	TObjectPtr<UAudioComponent> PlaybackComponent;

	UPROPERTY(Transient)
	TMap<FGuid, TObjectPtr<UAurixParticipantSoundWave>> ParticipantSounds;

	/** Users claimed through SetParticipantClaimed (without a participant sound). */
	TSet<FGuid> ManualClaims;

	FDelegateHandle PostLoadMapHandle;
	bool bPlaybackRequested = false;
};
