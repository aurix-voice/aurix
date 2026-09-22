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
DECLARE_DYNAMIC_MULTICAST_DELEGATE_OneParam(FAurixServerNoiseSuppressionChanged, bool, bEnabled);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_TwoParams(FAurixMediaPathChanged, EAurixMediaPath, Path, const FString&, Reason);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_TwoParams(FAurixUserBlockChanged, FGuid, UserId, bool, bBlocked);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_FourParams(FAurixRecording, FGuid, ChannelId, FGuid, RecordingId, bool, bActive, FGuid, InitiatedBy);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_TwoParams(FAurixBitrateChanged, int32, BitrateBps, const FString&, Reason);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_OneParam(FAurixNetworkQualityChanged, const FAurixNetworkQuality&, Quality);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_OneParam(FAurixAudioPolicyChanged, const FAurixAudioPolicy&, Policy);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_TwoParams(FAurixLossProfileChanged, EAurixLossProfile, Profile, int32, UplinkLossPercent);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_TwoParams(FAurixKicked, FGuid, ChannelId, const FString&, Reason);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_FourParams(FAurixModerationApplied, int64, RequestId, FGuid, ChannelId, FGuid, UserId, EAurixModerationAction, Action);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_OneParam(FAurixChatMessageReceived, const FAurixChatMessage&, Message);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_TwoParams(FAurixChatHistoryReceived, int64, RequestId, const FAurixChatHistoryPage&, Page);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_TwoParams(FAurixChatMessageUpdated, int64, RequestId, const FAurixChatMessage&, Message);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_OneParam(FAurixChatReactionChanged, const FAurixChatReactionChange&, Change);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_OneParam(FAurixChatReadMarkerChanged, const FAurixReadMarker&, Marker);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_FourParams(FAurixChatReadMarkersReceived, FGuid, ChannelId, FGuid, PeerUserId, const TArray<FAurixReadMarker>&, Markers, int32, UnreadCount);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_ThreeParams(FAurixChatInboxSynced, int32, Delivered, bool, bTruncated, bool, bPerDevice);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_ThreeParams(FAurixParticipantTyping, FGuid, ChannelId, FGuid, UserId, bool, bTyping);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_OneParam(FAurixTranscriptReceived, const FAurixTranscript&, Transcript);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_ThreeParams(FAurixTranslationChanged, const FString&, Language, const FString&, SpokenLanguage, bool, bSpeech);

/** Custom capture effect: one 20 ms 48 kHz interleaved frame (SamplesPerChannel * Channels floats) to modify in place. */
typedef void (*FAurixVoiceEffectFn)(void* UserData, float* Frame, uint32 SamplesPerChannel, uint8 Channels);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_OneParam(FAurixTtsStatusChanged, const FAurixTtsStatus&, Status);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_ThreeParams(FAurixRequestFailed, int64, RequestId, const FString&, Code, const FString&, Message);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_TwoParams(FAurixServerError, const FString&, Code, const FString&, Message);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_ThreeParams(FAurixRejoinFailed, FGuid, ChannelId, const FString&, Code, const FString&, Message);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_ThreeParams(FAurixRecovering, int32, Attempt, int32, DelayMs, const FString&, Cause);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_TwoParams(FAurixRecovered, bool, bResumed, bool, bMigrated);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_OneParam(FAurixEndpointChanged, const FString&, Url);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_ThreeParams(FAurixE2eePeerKey, FGuid, UserId, const FString&, Fingerprint, const FString&, PreviousFingerprint);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_TwoParams(FAurixE2eePeerDecryptable, FGuid, UserId, bool, bDecryptable);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_OneParam(FAurixE2eeKeyRotated, int32, Generation);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_ThreeParams(FAurixParticipantPriorityChanged, FGuid, ChannelId, FGuid, UserId, bool, bPriority);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_FourParams(FAurixParticipantRoleChanged, FGuid, ChannelId, FGuid, UserId, EAurixRole, Role, bool, bAdmitted);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_ThreeParams(FAurixDuckingChanged, FGuid, ChannelId, bool, bActive, const FAurixDucking&, Ducking);
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

	/** Whether we hold a speaking grant but wait for an `audience.max_speakers` slot (false for unknown channels). */
	UFUNCTION(BlueprintPure, Category = "Aurix Voice|Channels")
	bool IsWaitingToSpeak(FGuid ChannelId) const;

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

	/**
	 * Choose the uplink redundancy tier from the server's loss reports (Auto, default) or pin one.
	 * FEC tuning / DRED apply to the encoder at once; OnLossProfileChanged reports later moves.
	 */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Microphone")
	bool SetLossAdaptation(EAurixLossAdaptation Adaptation);

	UFUNCTION(BlueprintPure, Category = "Aurix Voice|Microphone")
	EAurixLossAdaptation GetLossAdaptation() const;

	/** Redundancy tier the encoder runs with right now. */
	UFUNCTION(BlueprintPure, Category = "Aurix Voice|Microphone")
	EAurixLossProfile GetLossProfile() const;

	/** Retune every downlink decoder (complexity → neural PLC / OSCE); streams already playing switch at once. */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Playback")
	bool SetDecoderSettings(const FAurixDecoderSettings& Settings);

	UFUNCTION(BlueprintPure, Category = "Aurix Voice|Playback")
	bool GetDecoderSettings(FAurixDecoderSettings& OutSettings) const;

	/** Whether this build's libopus codes / decodes Deep REDundancy (else lost frames get FEC + PLC only). */
	UFUNCTION(BlueprintPure, Category = "Aurix Voice|Playback")
	static bool IsDredSupported();

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
	 * Built-in voice effects on the microphone (filters, formant / pitch shift, ring modulator,
	 * distortion, tremolo, static, reverb), after the DSP and input gain and before VAD /
	 * encoding. Only your uplink is affected. Values are clamped by the core; read back with
	 * GetVoiceEffects. An all-zero struct switches the effects off.
	 */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Microphone")
	bool SetVoiceEffects(const FAurixVoiceEffects& Effects);

	UFUNCTION(BlueprintPure, Category = "Aurix Voice|Microphone")
	bool GetVoiceEffects(FAurixVoiceEffects& OutEffects) const;

	/** The parameters behind a named preset — a starting point to tweak and pass to SetVoiceEffects. */
	UFUNCTION(BlueprintPure, Category = "Aurix Voice|Microphone")
	static FAurixVoiceEffects MakeVoicePreset(EAurixVoicePreset Preset);

	/** Apply a named preset voice. */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Microphone")
	bool SetVoicePreset(EAurixVoicePreset Preset);

	/**
	 * Analyse decoded participant audio and our own outgoing voice for lip-sync (off by default;
	 * one small FFT per stream per 20 ms when on). Purely local: no audio or mouth data leaves
	 * the machine. Read with GetParticipantVisemes / GetLocalVisemes every tick.
	 */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Lip Sync")
	bool SetVisemesEnabled(bool bEnabled);

	UFUNCTION(BlueprintPure, Category = "Aurix Voice|Lip Sync")
	bool AreVisemesEnabled() const;

	/**
	 * Mouth state of UserId from the audio most recently played for them (their voice or TTS).
	 * False with analysis off or for someone whose audio is not decoded here (unknown, or only
	 * inside the server mix).
	 */
	UFUNCTION(BlueprintPure, Category = "Aurix Voice|Lip Sync")
	bool GetParticipantVisemes(FGuid UserId, FAurixVisemeFrame& OutFrame) const;

	/** Mouth state of our own voice as sent (after DSP, gain and effects); frozen while nothing is captured. */
	UFUNCTION(BlueprintPure, Category = "Aurix Voice|Lip Sync")
	bool GetLocalVisemes(FAurixVisemeFrame& OutFrame) const;

	/**
	 * Native-only: install a custom effect run on every 20 ms 48 kHz capture frame after the
	 * built-ins (nullptr removes it). Runs on the capture thread — no blocking or allocation.
	 */
	bool SetVoiceEffectCallback(FAurixVoiceEffectFn Callback, void* UserData);

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

	/**
	 * Grant / revoke priority speaker for UserId (invalid GUID = ourselves) in ChannelId. Others
	 * need a moderator role; our own flag can be raised with a `priority` grant and always
	 * lowered. Everyone learns the outcome through OnParticipantPriorityChanged.
	 */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Preferences")
	bool SetPriority(FGuid ChannelId, FGuid UserId, bool bPriority);

	/** Another member's priority speech is ducking ChannelId right now (see OnDuckingChanged). */
	UFUNCTION(BlueprintPure, Category = "Aurix Voice|Preferences")
	bool IsDuckingActive(FGuid ChannelId) const;

	/** Which joined channels receive the microphone; ChannelId is required for Single. */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Preferences")
	bool SetTransmission(EAurixTransmissionMode Mode, FGuid ChannelId);

	/** Focus one channel (others attenuated server-side); invalid ChannelId clears. */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Preferences")
	bool SetChannelFocus(FGuid ChannelId);

	/**
	 * Ask the server to run this session on Codec. Pcmu / Pcma (G.711) are low-CPU fallbacks for
	 * weak devices; in plaintext channels the node transcodes, so Opus participants are unaffected,
	 * in end-to-end encrypted channels the sealed G.711 frames are relayed as they are and peers
	 * decode them. Needs `media.pcmu_fallback` on the node; applied on OnAudioCodecChanged.
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
	 * Let the node denoise our uplink before anyone hears it — for builds that run no capture
	 * DSP of their own (FAurixDspConfig; do not stack the two). Never touches end-to-end
	 * encrypted frames or stereo channels. Needs FAurixSessionInfo::bNoiseSuppression (otherwise
	 * OnError NOISE_SUPPRESSION_UNAVAILABLE, also when the node's session budget is spent);
	 * applied on OnServerNoiseSuppressionChanged.
	 */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Preferences")
	bool SetServerNoiseSuppression(bool bEnabled);

	/** Whether the node currently denoises our uplink on our request. */
	UFUNCTION(BlueprintPure, Category = "Aurix Voice|Preferences")
	bool IsServerNoiseSuppressionEnabled() const;

	/**
	 * Link the media uses right now: QUIC, UDP, the WebSocket tunnel (native links blocked —
	 * expect higher latency under packet loss) or None before the first OnMediaBound.
	 */
	UFUNCTION(BlueprintPure, Category = "Aurix Voice|Preferences")
	EAurixMediaPath GetMediaPath() const;

	/**
	 * The device's network changed (Wi-Fi ↔ cellular, VPN, new interface). A QUIC link migrates
	 * in place (same session, sequence counter and E2EE state; OnMediaPathChanged follows), a UDP
	 * link re-announces the session. False when not connected.
	 */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Connection")
	bool NetworkChanged();

	/** WebSocket URL of the node serving the session (the configured URL until a failover moved it). */
	UFUNCTION(BlueprintPure, Category = "Aurix Voice|Connection")
	FString GetEndpoint() const;

	/**
	 * Alternate nodes the server advertised for this session. A dropped connection retries the
	 * current node first, then these in order; OnEndpointChanged reports a switch.
	 */
	UFUNCTION(BlueprintPure, Category = "Aurix Voice|Connection")
	TArray<FString> GetFailoverEndpoints() const;

	// ---- end-to-end encryption -------------------------------------------------------------

	/** Fingerprint of this client's E2EE identity key; peers see it as OnE2eePeerKey. */
	UFUNCTION(BlueprintPure, Category = "Aurix Voice|Security")
	FString GetE2eeFingerprint() const;

	/** Fingerprint of a peer's identity key; empty until the peer announced one in a shared encrypted channel. */
	UFUNCTION(BlueprintPure, Category = "Aurix Voice|Security")
	FString GetE2eePeerFingerprint(FGuid UserId) const;

	/** Whether the peer's encrypted frames currently decode (their sender key arrived). */
	UFUNCTION(BlueprintPure, Category = "Aurix Voice|Security")
	bool IsE2eePeerDecryptable(FGuid UserId) const;

	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Preferences")
	bool SetTranscripts(bool bEnabled);

	/**
	 * Receive transcripts translated into Language (BCP-47; empty = originals only), optionally
	 * declaring the language you speak (helps the recogniser) and asking for the translation
	 * to be spoken privately to you (bSpeech; needs FAurixSessionInfo::bTranslationSpeech).
	 * Requires SetTranscripts(true). Acked by OnTranslationChanged; replayed on reconnect.
	 */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Preferences")
	bool SetTranslation(const FString& Language, const FString& SpokenLanguage, bool bSpeech);

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

	/**
	 * Replaces the text of this user's own message (within the server's edit window; empty
	 * MetadataJson clears the metadata). Answered by OnChatMessageUpdated with RequestId, or
	 * OnRequestFailed.
	 */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Chat")
	bool EditChat(FGuid MessageId, const FString& Text, const FString& MetadataJson, int64& RequestId);

	/**
	 * Deletes a message: this user's own (within the edit window) or, in a channel this user
	 * moderates, anyone's. Answered by a tombstone OnChatMessageUpdated with RequestId.
	 */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Chat")
	bool DeleteChat(FGuid MessageId, int64& RequestId);

	/** Adds (bAdd) or removes this user's reaction on a message; everyone gets OnChatReactionChanged. */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Chat")
	bool ReactChat(FGuid MessageId, const FString& Reaction, bool bAdd);

	/**
	 * Full-text search in the stored history of a joined channel; FromUserId (optional) keeps
	 * one author's messages, Before continues from a previous page's NextBefore, Limit 0 =
	 * server default. Answered by OnChatSearchResult (newest match first) or OnRequestFailed.
	 */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Chat")
	bool SearchChannelChat(FGuid ChannelId, const FString& Query, FGuid FromUserId, const FString& Before, int32 Limit, int64& RequestId);

	/** Full-text search in the direct conversation with UserId (invalid GUID = every direct conversation of this user). */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|Chat")
	bool SearchDirectChat(FGuid UserId, const FString& Query, FGuid FromUserId, const FString& Before, int32 Limit, int64& RequestId);

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
	/** A member (possibly us) became or stopped being a priority speaker. */
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixParticipantPriorityChanged OnParticipantPriorityChanged;
	/**
	 * A member's (possibly our) effective role changed under `audience.speaker_admission`: bAdmitted
	 * = it got its granted role (a speaker slot) back, otherwise an idle speaker slot was taken from
	 * it and its audio is dropped meanwhile. The grant itself is unchanged.
	 */
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixParticipantRoleChanged OnParticipantRoleChanged;
	/**
	 * Game-audio hook: another member's priority speech started (bActive) or stopped ducking
	 * the channel — fade your music / SFX bus to Ducking.Gain over AttackMs and back over
	 * ReleaseMs. Remote voices are already attenuated by the server; our own speech never
	 * triggers this.
	 */
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixDuckingChanged OnDuckingChanged;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixChannelEnergy OnChannelEnergy;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixLocalSpeaking OnLocalSpeaking;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixTransmissionChanged OnTransmissionChanged;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixChannelFocusChanged OnChannelFocusChanged;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixAudioCodecChanged OnAudioCodecChanged;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixDownlinkModeChanged OnDownlinkModeChanged;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixServerNoiseSuppressionChanged OnServerNoiseSuppressionChanged;
	/** Media moved between UDP and the WebSocket tunnel (also fires after every OnMediaBound). */
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixMediaPathChanged OnMediaPathChanged;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixUserBlockChanged OnUserBlockChanged;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixRecording OnRecording;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixBitrateChanged OnBitrateChanged;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixNetworkQualityChanged OnNetworkQuality;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixAudioPolicyChanged OnAudioPolicyChanged;
	/** The uplink redundancy tier moved (with the server-measured uplink loss that triggered it). */
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixLossProfileChanged OnLossProfileChanged;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixKicked OnKicked;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixModerationApplied OnModerationApplied;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixChatMessageReceived OnChatMessage;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixChatHistoryReceived OnChatHistory;
	/** A message of one of this client's conversations was edited or deleted (replace the copy with the same MessageId). */
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixChatMessageUpdated OnChatMessageUpdated;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixChatReactionChanged OnChatReactionChanged;
	/** Answer to SearchChannelChat / SearchDirectChat (Page.Query set, NextAfter always empty). */
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixChatHistoryReceived OnChatSearchResult;
	/** A read marker moved: this user's (any device) or, with server-side read receipts, another participant's. */
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixChatReadMarkerChanged OnChatReadMarker;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixChatReadMarkersReceived OnChatReadMarkers;
	/** Directed messages that arrived while offline were replayed (as OnChatMessage with bOffline); fires once per connection. */
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixChatInboxSynced OnChatInboxSynced;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixParticipantTyping OnParticipantTyping;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixTranscriptReceived OnTranscript;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixTranslationChanged OnTranslationChanged;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixTtsStatusChanged OnTtsStatus;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixRequestFailed OnRequestFailed;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixServerError OnServerError;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixRejoinFailed OnRejoinFailed;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixRecovering OnRecovering;
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixRecovered OnRecovered;
	/** A failover node answered while the previous one did not; fires before that connection's OnSessionReady. */
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixEndpointChanged OnEndpointChanged;
	/** A peer announced its E2EE identity; PreviousFingerprint is non-empty when a known user now presents a different key (verify out of band). */
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixE2eePeerKey OnE2eePeerKey;
	/** A peer's encrypted frames became decodable (its sender key arrived). */
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixE2eePeerDecryptable OnE2eePeerDecryptable;
	/** Our sender key rotated (a member joined or left an encrypted channel). */
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events") FAurixE2eeKeyRotated OnE2eeKeyRotated;
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
