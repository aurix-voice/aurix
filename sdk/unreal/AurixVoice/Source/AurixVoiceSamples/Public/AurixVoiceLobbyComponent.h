#pragma once

#include "CoreMinimal.h"
#include "Components/ActorComponent.h"
#include "AurixVoiceTypes.h"
#include "AurixVoiceLobbyComponent.generated.h"

class UAurixVoiceSubsystem;

/** One roster row for a lobby / party widget. */
USTRUCT(BlueprintType)
struct AURIXVOICESAMPLES_API FAurixLobbyEntry
{
	GENERATED_BODY()

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	FGuid UserId;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	FString DisplayName;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	bool bSelf = false;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	bool bSpeaking = false;

	/** Muted by the participant themself. */
	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	bool bMuted = false;

	/** Muted by a moderator. */
	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	bool bServerMuted = false;

	/** Muted locally for this listener only (SetParticipantMutedLocally). */
	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	bool bLocallyMuted = false;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	bool bPriority = false;

	/** Last reported level 0..1 (OnChannelEnergy). */
	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	float Energy = 0.f;

	/** Listener-local gain 0..2 (SetParticipantVolumeLocally). */
	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	float Volume = 1.f;
};

/** Connection / quality snapshot for a status line. */
USTRUCT(BlueprintType)
struct AURIXVOICESAMPLES_API FAurixLobbyStatus
{
	GENERATED_BODY()

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	EAurixConnectionState State = EAurixConnectionState::Disconnected;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	bool bInChannel = false;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	bool bMicrophoneMuted = false;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	EAurixMediaPath MediaPath = EAurixMediaPath::None;

	/** wss://… of the node currently serving the session. */
	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	FString Endpoint;

	/** 0 until the first server report. */
	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	int32 QualityBars = 0;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	float Mos = 0.f;

	UPROPERTY(BlueprintReadOnly, Category = "Aurix")
	float RttMs = 0.f;
};

DECLARE_DYNAMIC_MULTICAST_DELEGATE_OneParam(FAurixLobbyRosterChanged, const TArray<FAurixLobbyEntry>&, Roster);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_OneParam(FAurixLobbyStatusChanged, const FAurixLobbyStatus&, Status);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_ThreeParams(FAurixLobbyChatLine, const FString&, SenderName, const FString&, Text, bool, bDirect);
DECLARE_DYNAMIC_MULTICAST_DELEGATE_TwoParams(FAurixLobbyError, const FString&, Code, const FString&, Message);

/**
 * Party / lobby voice in one component: connect with a backend-minted token, join one channel,
 * keep a roster with speaking / mute / level flags, push-to-talk or open mic, chat, and a status
 * snapshot for the HUD. Drop it on the PlayerController (or any actor that lives as long as the
 * lobby), bind the four delegates in a widget, call ConnectWithToken.
 *
 * Everything here is a thin layer over UAurixVoiceSubsystem's public API; read the .cpp as the
 * reference for wiring the subsystem yourself.
 */
UCLASS(ClassGroup = (Aurix), meta = (BlueprintSpawnableComponent))
class AURIXVOICESAMPLES_API UAurixVoiceLobbyComponent : public UActorComponent
{
	GENERATED_BODY()

public:
	UAurixVoiceLobbyComponent();

	/** ws://host:port/ws or wss://…; usually the best entry of DiscoverRegions. */
	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix Voice")
	FString WebSocketUrl;

	/**
	 * Channel to join after the session is ready (UUID text). Empty = call JoinChannel yourself.
	 * Editable so a fixed lobby channel can live in a Blueprint default; game-created channels
	 * arrive from your backend at runtime.
	 */
	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix Voice")
	FString ChannelId;

	/** Join token for the channel when the app requires action tokens (`require_action_tokens`). */
	UPROPERTY(BlueprintReadWrite, Category = "Aurix Voice")
	FString JoinToken;

	/** Microphone stays muted unless SetPushToTalkPressed(true). Off = open mic gated by VAD. */
	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix Voice")
	bool bPushToTalk = false;

	/** Connection settings other than URL and token (reconnect, DSP, Opus, media path, capture). */
	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix Voice")
	FAurixVoiceSettings Settings;

	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events")
	FAurixLobbyRosterChanged OnRosterChanged;

	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events")
	FAurixLobbyStatusChanged OnStatusChanged;

	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events")
	FAurixLobbyChatLine OnChatLine;

	/** Request failures, server errors, kicks and a lost connection (Code = "KICKED" / "DISCONNECTED"). */
	UPROPERTY(BlueprintAssignable, Category = "Aurix Voice|Events")
	FAurixLobbyError OnError;

	/** Connect with a session JWT (or one-time login action token) fetched from your backend. */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice")
	bool ConnectWithToken(const FString& Token);

	UFUNCTION(BlueprintCallable, Category = "Aurix Voice")
	void Disconnect();

	/** Join a channel now (session must be ready); replaces ChannelId. */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice")
	bool JoinChannel(FGuid Channel, const FString& Token);

	UFUNCTION(BlueprintCallable, Category = "Aurix Voice")
	void LeaveChannel();

	/** With bPushToTalk: unmute while pressed. Without: no effect. */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice")
	void SetPushToTalkPressed(bool bPressed);

	/** Toggle the open-mic mute (ignored in push-to-talk mode). */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice")
	void ToggleMicrophoneMuted();

	UFUNCTION(BlueprintCallable, Category = "Aurix Voice")
	void SetMicrophoneMuted(bool bMuted);

	/** Stop hearing one participant on this client only (they are not told). */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice")
	void SetParticipantMutedLocally(FGuid UserId, bool bMuted);

	/** Listener-local gain 0..2 for one participant. */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice")
	void SetParticipantVolumeLocally(FGuid UserId, float Volume);

	UFUNCTION(BlueprintCallable, Category = "Aurix Voice")
	bool SendChat(const FString& Text);

	UFUNCTION(BlueprintPure, Category = "Aurix Voice")
	TArray<FAurixLobbyEntry> GetRoster() const { return Roster; }

	UFUNCTION(BlueprintPure, Category = "Aurix Voice")
	FAurixLobbyStatus GetStatus() const;

	UFUNCTION(BlueprintPure, Category = "Aurix Voice")
	FGuid GetJoinedChannel() const { return JoinedChannel; }

	UFUNCTION(BlueprintPure, Category = "Aurix Voice")
	UAurixVoiceSubsystem* GetVoice() const;

protected:
	virtual void BeginPlay() override;
	virtual void EndPlay(const EEndPlayReason::Type EndPlayReason) override;

private:
	UFUNCTION() void HandleConnectionState(EAurixConnectionState State);
	UFUNCTION() void HandleSessionReady(const FAurixSessionInfo& Session);
	UFUNCTION() void HandleChannelJoined(int64 RequestId, FGuid Channel, const TArray<FAurixParticipant>& Participants, bool bTranscription);
	UFUNCTION() void HandleChannelLeft(FGuid Channel);
	UFUNCTION() void HandleParticipantJoined(FGuid Channel, const FAurixParticipant& Participant);
	UFUNCTION() void HandleParticipantLeft(FGuid Channel, FGuid UserId);
	UFUNCTION() void HandleMuteChanged(FGuid Channel, FGuid UserId, bool bMuted, bool bServerMuted);
	UFUNCTION() void HandleSpeaking(FGuid Channel, FGuid UserId, bool bSpeaking);
	UFUNCTION() void HandlePriorityChanged(FGuid Channel, FGuid UserId, bool bPriority);
	UFUNCTION() void HandleEnergy(FGuid Channel, const TArray<FAurixParticipant>& Levels);
	UFUNCTION() void HandleLocalSpeaking(bool bSpeaking);
	UFUNCTION() void HandleMediaPath(EAurixMediaPath Path, const FString& Reason);
	UFUNCTION() void HandleNetworkQuality(const FAurixNetworkQuality& Quality);
	UFUNCTION() void HandleEndpointChanged(const FString& Url);
	UFUNCTION() void HandleChat(const FAurixChatMessage& Message);
	UFUNCTION() void HandleRequestFailed(int64 RequestId, const FString& Code, const FString& Message);
	UFUNCTION() void HandleServerError(const FString& Code, const FString& Message);
	UFUNCTION() void HandleRejoinFailed(FGuid Channel, const FString& Code, const FString& Message);
	UFUNCTION() void HandleKicked(FGuid Channel, const FString& Reason);
	UFUNCTION() void HandleFailedToRecover(const FString& Reason);
	UFUNCTION() void HandleDisconnected(const FString& Reason);

	void Bind(UAurixVoiceSubsystem& Voice);
	void Unbind(UAurixVoiceSubsystem& Voice);
	void ApplyMicrophoneState();
	void RebuildRoster(const TArray<FAurixParticipant>& Participants);
	FAurixLobbyEntry* FindEntry(FGuid UserId);
	void NotifyRoster();
	void NotifyStatus();

	UPROPERTY(Transient)
	TObjectPtr<UAurixVoiceSubsystem> BoundVoice = nullptr;

	TArray<FAurixLobbyEntry> Roster;
	FGuid JoinedChannel;
	FGuid SelfUserId;
	FAurixNetworkQuality LastQuality;
	bool bSelfSpeaking = false;
	bool bOpenMicMuted = false;
	bool bPushToTalkPressed = false;
	bool bSessionReady = false;
};
