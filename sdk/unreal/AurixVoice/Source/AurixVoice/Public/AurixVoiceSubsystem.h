#pragma once

#include "CoreMinimal.h"
#include "Subsystems/GameInstanceSubsystem.h"
#include "Tickable.h"
#include "AurixVoiceTypes.h"
#include "AurixVoiceSubsystem.generated.h"

class UAudioComponent;
class UAurixVoiceSoundWave;
class FAurixAudioCapture;
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
DECLARE_DYNAMIC_MULTICAST_DELEGATE_TwoParams(FAurixUserBlockChanged, FGuid, UserId, bool, bBlocked);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_FourParams(FAurixRecording, FGuid, ChannelId, FGuid, RecordingId, bool, bActive, FGuid, InitiatedBy);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_TwoParams(FAurixBitrateChanged, int32, BitrateBps, const FString&, Reason);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_TwoParams(FAurixKicked, FGuid, ChannelId, const FString&, Reason);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_FourParams(FAurixModerationApplied, int64, RequestId, FGuid, ChannelId, FGuid, UserId, EAurixModerationAction, Action);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_OneParam(FAurixChatMessageReceived, const FAurixChatMessage&, Message);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_ThreeParams(FAurixParticipantTyping, FGuid, ChannelId, FGuid, UserId, bool, bTyping);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_OneParam(FAurixTranscriptReceived, const FAurixTranscript&, Transcript);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_OneParam(FAurixTtsStatusChanged, const FAurixTtsStatus&, Status);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_ThreeParams(FAurixRequestFailed, int64, RequestId, const FString&, Code, const FString&, Message);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_TwoParams(FAurixServerError, const FString&, Code, const FString&, Message);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_ThreeParams(FAurixRejoinFailed, FGuid, ChannelId, const FString&, Code, const FString&, Message);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_ThreeParams(FAurixRecovering, int32, Attempt, int32, DelayMs, const FString&, Cause);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_OneParam(FAurixRecovered, bool, bResumed);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_OneParam(FAurixConnectionEnded, const FString&, Reason);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_OneParam(FAurixRawEvent, const FString&, Json);

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

	/** Server-side text-to-speech as this participant's voice (ChannelId may be invalid for Local). */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Speech")
	bool Speak(const FString& Text, FGuid ChannelId, EAurixTtsDestination Destination, const FString& Voice, int64& RequestId);

	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Speech")
	bool CancelSpeech();

	UFUNCTION(BlueprintPure, Category = "Aurix Voice")
	bool GetStats(FAurixStats& OutStats) const;

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
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixUserBlockChanged OnUserBlockChanged;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixRecording OnRecording;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixBitrateChanged OnBitrateChanged;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixKicked OnKicked;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixModerationApplied OnModerationApplied;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixChatMessageReceived OnChatMessage;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixParticipantTyping OnParticipantTyping;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixTranscriptReceived OnTranscript;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixTtsStatusChanged OnTtsStatus;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixRequestFailed OnRequestFailed;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixServerError OnServerError;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixRejoinFailed OnRejoinFailed;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixRecovering OnRecovering;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixRecovered OnRecovered;
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
	FAurixVoiceSettings ActiveSettings;

	UPROPERTY(Transient)
	TObjectPtr<UAurixVoiceSoundWave> SoundWave;

	UPROPERTY(Transient)
	TObjectPtr<UAudioComponent> PlaybackComponent;

	FDelegateHandle PostLoadMapHandle;
	bool bPlaybackRequested = false;
};
