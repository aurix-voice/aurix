#include "AurixVoiceLobbyComponent.h"

#include "AurixVoiceSubsystem.h"
#include "Engine/GameInstance.h"
#include "Engine/World.h"

UAurixVoiceLobbyComponent::UAurixVoiceLobbyComponent()
{
	PrimaryComponentTick.bCanEverTick = false;
}

UAurixVoiceSubsystem* UAurixVoiceLobbyComponent::GetVoice() const
{
	const UWorld* World = GetWorld();
	UGameInstance* GameInstance = World ? World->GetGameInstance() : nullptr;
	return GameInstance ? GameInstance->GetSubsystem<UAurixVoiceSubsystem>() : nullptr;
}

void UAurixVoiceLobbyComponent::BeginPlay()
{
	Super::BeginPlay();
	if (UAurixVoiceSubsystem* Voice = GetVoice())
	{
		Bind(*Voice);
	}
}

void UAurixVoiceLobbyComponent::EndPlay(const EEndPlayReason::Type EndPlayReason)
{
	if (BoundVoice)
	{
		Unbind(*BoundVoice);
	}
	Super::EndPlay(EndPlayReason);
}

void UAurixVoiceLobbyComponent::Bind(UAurixVoiceSubsystem& Voice)
{
	BoundVoice = &Voice;
	Voice.OnConnectionStateChanged.AddDynamic(this, &UAurixVoiceLobbyComponent::HandleConnectionState);
	Voice.OnSessionReady.AddDynamic(this, &UAurixVoiceLobbyComponent::HandleSessionReady);
	Voice.OnChannelJoined.AddDynamic(this, &UAurixVoiceLobbyComponent::HandleChannelJoined);
	Voice.OnChannelLeft.AddDynamic(this, &UAurixVoiceLobbyComponent::HandleChannelLeft);
	Voice.OnParticipantJoined.AddDynamic(this, &UAurixVoiceLobbyComponent::HandleParticipantJoined);
	Voice.OnParticipantLeft.AddDynamic(this, &UAurixVoiceLobbyComponent::HandleParticipantLeft);
	Voice.OnParticipantMuteChanged.AddDynamic(this, &UAurixVoiceLobbyComponent::HandleMuteChanged);
	Voice.OnParticipantSpeaking.AddDynamic(this, &UAurixVoiceLobbyComponent::HandleSpeaking);
	Voice.OnParticipantPriorityChanged.AddDynamic(this, &UAurixVoiceLobbyComponent::HandlePriorityChanged);
	Voice.OnChannelEnergy.AddDynamic(this, &UAurixVoiceLobbyComponent::HandleEnergy);
	Voice.OnLocalSpeaking.AddDynamic(this, &UAurixVoiceLobbyComponent::HandleLocalSpeaking);
	Voice.OnMediaPathChanged.AddDynamic(this, &UAurixVoiceLobbyComponent::HandleMediaPath);
	Voice.OnNetworkQuality.AddDynamic(this, &UAurixVoiceLobbyComponent::HandleNetworkQuality);
	Voice.OnEndpointChanged.AddDynamic(this, &UAurixVoiceLobbyComponent::HandleEndpointChanged);
	Voice.OnChatMessage.AddDynamic(this, &UAurixVoiceLobbyComponent::HandleChat);
	Voice.OnRequestFailed.AddDynamic(this, &UAurixVoiceLobbyComponent::HandleRequestFailed);
	Voice.OnServerError.AddDynamic(this, &UAurixVoiceLobbyComponent::HandleServerError);
	Voice.OnRejoinFailed.AddDynamic(this, &UAurixVoiceLobbyComponent::HandleRejoinFailed);
	Voice.OnKicked.AddDynamic(this, &UAurixVoiceLobbyComponent::HandleKicked);
	Voice.OnFailedToRecover.AddDynamic(this, &UAurixVoiceLobbyComponent::HandleFailedToRecover);
	Voice.OnDisconnected.AddDynamic(this, &UAurixVoiceLobbyComponent::HandleDisconnected);
}

void UAurixVoiceLobbyComponent::Unbind(UAurixVoiceSubsystem& Voice)
{
	Voice.OnConnectionStateChanged.RemoveDynamic(this, &UAurixVoiceLobbyComponent::HandleConnectionState);
	Voice.OnSessionReady.RemoveDynamic(this, &UAurixVoiceLobbyComponent::HandleSessionReady);
	Voice.OnChannelJoined.RemoveDynamic(this, &UAurixVoiceLobbyComponent::HandleChannelJoined);
	Voice.OnChannelLeft.RemoveDynamic(this, &UAurixVoiceLobbyComponent::HandleChannelLeft);
	Voice.OnParticipantJoined.RemoveDynamic(this, &UAurixVoiceLobbyComponent::HandleParticipantJoined);
	Voice.OnParticipantLeft.RemoveDynamic(this, &UAurixVoiceLobbyComponent::HandleParticipantLeft);
	Voice.OnParticipantMuteChanged.RemoveDynamic(this, &UAurixVoiceLobbyComponent::HandleMuteChanged);
	Voice.OnParticipantSpeaking.RemoveDynamic(this, &UAurixVoiceLobbyComponent::HandleSpeaking);
	Voice.OnParticipantPriorityChanged.RemoveDynamic(this, &UAurixVoiceLobbyComponent::HandlePriorityChanged);
	Voice.OnChannelEnergy.RemoveDynamic(this, &UAurixVoiceLobbyComponent::HandleEnergy);
	Voice.OnLocalSpeaking.RemoveDynamic(this, &UAurixVoiceLobbyComponent::HandleLocalSpeaking);
	Voice.OnMediaPathChanged.RemoveDynamic(this, &UAurixVoiceLobbyComponent::HandleMediaPath);
	Voice.OnNetworkQuality.RemoveDynamic(this, &UAurixVoiceLobbyComponent::HandleNetworkQuality);
	Voice.OnEndpointChanged.RemoveDynamic(this, &UAurixVoiceLobbyComponent::HandleEndpointChanged);
	Voice.OnChatMessage.RemoveDynamic(this, &UAurixVoiceLobbyComponent::HandleChat);
	Voice.OnRequestFailed.RemoveDynamic(this, &UAurixVoiceLobbyComponent::HandleRequestFailed);
	Voice.OnServerError.RemoveDynamic(this, &UAurixVoiceLobbyComponent::HandleServerError);
	Voice.OnRejoinFailed.RemoveDynamic(this, &UAurixVoiceLobbyComponent::HandleRejoinFailed);
	Voice.OnKicked.RemoveDynamic(this, &UAurixVoiceLobbyComponent::HandleKicked);
	Voice.OnFailedToRecover.RemoveDynamic(this, &UAurixVoiceLobbyComponent::HandleFailedToRecover);
	Voice.OnDisconnected.RemoveDynamic(this, &UAurixVoiceLobbyComponent::HandleDisconnected);
	BoundVoice = nullptr;
}

// ---- commands ------------------------------------------------------------------------------

bool UAurixVoiceLobbyComponent::ConnectWithToken(const FString& Token)
{
	UAurixVoiceSubsystem* Voice = GetVoice();
	if (!Voice)
	{
		OnError.Broadcast(TEXT("NO_SUBSYSTEM"), TEXT("Aurix Voice subsystem unavailable (no game instance)"));
		return false;
	}
	if (!BoundVoice)
	{
		Bind(*Voice);
	}

	FAurixVoiceSettings Connect = Settings;
	Connect.WebSocketUrl = WebSocketUrl;
	Connect.Token = Token;
	bSessionReady = false;
	if (!Voice->Connect(Connect))
	{
		OnError.Broadcast(TEXT("CONNECT_FAILED"), Voice->GetLastError());
		return false;
	}
	ApplyMicrophoneState();
	NotifyStatus();
	return true;
}

void UAurixVoiceLobbyComponent::Disconnect()
{
	if (UAurixVoiceSubsystem* Voice = GetVoice())
	{
		Voice->Disconnect();
	}
	bSessionReady = false;
	JoinedChannel.Invalidate();
	Roster.Reset();
	NotifyRoster();
	NotifyStatus();
}

bool UAurixVoiceLobbyComponent::JoinChannel(FGuid Channel, const FString& Token)
{
	UAurixVoiceSubsystem* Voice = GetVoice();
	if (!Voice || !Channel.IsValid())
	{
		return false;
	}
	ChannelId = Channel.ToString(EGuidFormats::DigitsWithHyphens).ToLower();
	JoinToken = Token;
	if (!bSessionReady)
	{
		// Joined from HandleSessionReady once the session exists.
		return true;
	}
	if (JoinedChannel.IsValid() && JoinedChannel != Channel)
	{
		Voice->LeaveChannel(JoinedChannel);
	}
	int64 RequestId = 0;
	if (!Voice->JoinChannel(Channel, Token, RequestId))
	{
		OnError.Broadcast(TEXT("JOIN_FAILED"), Voice->GetLastError());
		return false;
	}
	return true;
}

void UAurixVoiceLobbyComponent::LeaveChannel()
{
	UAurixVoiceSubsystem* Voice = GetVoice();
	if (Voice && JoinedChannel.IsValid())
	{
		Voice->LeaveChannel(JoinedChannel);
	}
	ChannelId.Empty();
}

void UAurixVoiceLobbyComponent::SetPushToTalkPressed(bool bPressed)
{
	bPushToTalkPressed = bPressed;
	if (bPushToTalk)
	{
		ApplyMicrophoneState();
	}
}

void UAurixVoiceLobbyComponent::ToggleMicrophoneMuted()
{
	SetMicrophoneMuted(!bOpenMicMuted);
}

void UAurixVoiceLobbyComponent::SetMicrophoneMuted(bool bMuted)
{
	bOpenMicMuted = bMuted;
	if (!bPushToTalk)
	{
		ApplyMicrophoneState();
	}
}

void UAurixVoiceLobbyComponent::ApplyMicrophoneState()
{
	UAurixVoiceSubsystem* Voice = GetVoice();
	if (!Voice)
	{
		return;
	}
	const bool bMuted = bPushToTalk ? !bPushToTalkPressed : bOpenMicMuted;
	if (Voice->IsMuted() != bMuted)
	{
		Voice->SetMuted(bMuted);
		NotifyStatus();
	}
}

void UAurixVoiceLobbyComponent::SetParticipantMutedLocally(FGuid UserId, bool bMuted)
{
	UAurixVoiceSubsystem* Voice = GetVoice();
	if (!Voice)
	{
		return;
	}
	if (Voice->SetParticipantMuted(UserId, JoinedChannel, bMuted))
	{
		if (FAurixLobbyEntry* Entry = FindEntry(UserId))
		{
			Entry->bLocallyMuted = bMuted;
			NotifyRoster();
		}
	}
}

void UAurixVoiceLobbyComponent::SetParticipantVolumeLocally(FGuid UserId, float Volume)
{
	UAurixVoiceSubsystem* Voice = GetVoice();
	if (!Voice)
	{
		return;
	}
	const float Clamped = FMath::Clamp(Volume, 0.f, 2.f);
	if (Voice->SetParticipantVolume(UserId, Clamped))
	{
		if (FAurixLobbyEntry* Entry = FindEntry(UserId))
		{
			Entry->Volume = Clamped;
			NotifyRoster();
		}
	}
}

bool UAurixVoiceLobbyComponent::SendChat(const FString& Text)
{
	UAurixVoiceSubsystem* Voice = GetVoice();
	if (!Voice || !JoinedChannel.IsValid() || Text.TrimStartAndEnd().IsEmpty())
	{
		return false;
	}
	int64 RequestId = 0;
	return Voice->SendChat(JoinedChannel, Text, FString(), RequestId);
}

FAurixLobbyStatus UAurixVoiceLobbyComponent::GetStatus() const
{
	FAurixLobbyStatus Status;
	if (const UAurixVoiceSubsystem* Voice = GetVoice())
	{
		Status.State = Voice->GetConnectionState();
		Status.bMicrophoneMuted = Voice->IsMuted();
		Status.MediaPath = Voice->GetMediaPath();
		Status.Endpoint = Voice->GetEndpoint();
	}
	Status.bInChannel = JoinedChannel.IsValid();
	Status.QualityBars = LastQuality.Bars;
	Status.Mos = LastQuality.Mos;
	Status.RttMs = LastQuality.RttMs;
	return Status;
}

// ---- subsystem events ----------------------------------------------------------------------

void UAurixVoiceLobbyComponent::HandleConnectionState(EAurixConnectionState State)
{
	if (State == EAurixConnectionState::Disconnected || State == EAurixConnectionState::Failed)
	{
		bSessionReady = false;
	}
	NotifyStatus();
}

void UAurixVoiceLobbyComponent::HandleSessionReady(const FAurixSessionInfo& Session)
{
	SelfUserId = Session.UserId;
	bSessionReady = true;
	ApplyMicrophoneState();

	// A resumed session keeps its channels; only a fresh one needs the configured join.
	if (!Session.bResumed && !ChannelId.IsEmpty())
	{
		FGuid Channel;
		if (!UAurixVoiceSubsystem::ParseUuid(ChannelId, Channel))
		{
			OnError.Broadcast(TEXT("BAD_CHANNEL_ID"), FString::Printf(TEXT("'%s' is not a UUID"), *ChannelId));
		}
		else if (UAurixVoiceSubsystem* Voice = GetVoice())
		{
			int64 RequestId = 0;
			if (!Voice->JoinChannel(Channel, JoinToken, RequestId))
			{
				OnError.Broadcast(TEXT("JOIN_FAILED"), Voice->GetLastError());
			}
		}
	}
	NotifyStatus();
}

void UAurixVoiceLobbyComponent::HandleChannelJoined(int64 RequestId, FGuid Channel, const TArray<FAurixParticipant>& Participants, bool bTranscription)
{
	JoinedChannel = Channel;
	RebuildRoster(Participants);
	NotifyStatus();
}

void UAurixVoiceLobbyComponent::HandleChannelLeft(FGuid Channel)
{
	if (Channel != JoinedChannel)
	{
		return;
	}
	JoinedChannel.Invalidate();
	Roster.Reset();
	NotifyRoster();
	NotifyStatus();
}

void UAurixVoiceLobbyComponent::RebuildRoster(const TArray<FAurixParticipant>& Participants)
{
	TArray<FAurixLobbyEntry> Previous = MoveTemp(Roster);
	Roster.Reset(Participants.Num());
	for (const FAurixParticipant& P : Participants)
	{
		FAurixLobbyEntry Entry;
		Entry.UserId = P.UserId;
		Entry.DisplayName = P.DisplayName;
		Entry.bSelf = P.UserId == SelfUserId;
		Entry.bSpeaking = Entry.bSelf ? bSelfSpeaking : P.bSpeaking;
		Entry.bMuted = P.bMuted;
		Entry.bServerMuted = P.bServerMuted;
		Entry.bPriority = P.bPriority;
		Entry.Energy = P.Energy;
		// Listener-local preferences survive a re-join; keep what the UI set.
		if (const FAurixLobbyEntry* Old = Previous.FindByPredicate([&](const FAurixLobbyEntry& E) { return E.UserId == P.UserId; }))
		{
			Entry.bLocallyMuted = Old->bLocallyMuted;
			Entry.Volume = Old->Volume;
		}
		Roster.Add(MoveTemp(Entry));
	}
	NotifyRoster();
}

FAurixLobbyEntry* UAurixVoiceLobbyComponent::FindEntry(FGuid UserId)
{
	return Roster.FindByPredicate([&](const FAurixLobbyEntry& E) { return E.UserId == UserId; });
}

void UAurixVoiceLobbyComponent::HandleParticipantJoined(FGuid Channel, const FAurixParticipant& Participant)
{
	if (Channel != JoinedChannel)
	{
		return;
	}
	if (FAurixLobbyEntry* Existing = FindEntry(Participant.UserId))
	{
		Existing->DisplayName = Participant.DisplayName;
		Existing->bMuted = Participant.bMuted;
		Existing->bServerMuted = Participant.bServerMuted;
		Existing->bPriority = Participant.bPriority;
	}
	else
	{
		FAurixLobbyEntry Entry;
		Entry.UserId = Participant.UserId;
		Entry.DisplayName = Participant.DisplayName;
		Entry.bSelf = Participant.UserId == SelfUserId;
		Entry.bMuted = Participant.bMuted;
		Entry.bServerMuted = Participant.bServerMuted;
		Entry.bPriority = Participant.bPriority;
		Roster.Add(MoveTemp(Entry));
	}
	NotifyRoster();
}

void UAurixVoiceLobbyComponent::HandleParticipantLeft(FGuid Channel, FGuid UserId)
{
	if (Channel != JoinedChannel)
	{
		return;
	}
	if (Roster.RemoveAll([&](const FAurixLobbyEntry& E) { return E.UserId == UserId; }) > 0)
	{
		NotifyRoster();
	}
}

void UAurixVoiceLobbyComponent::HandleMuteChanged(FGuid Channel, FGuid UserId, bool bMuted, bool bServerMuted)
{
	if (Channel != JoinedChannel)
	{
		return;
	}
	if (FAurixLobbyEntry* Entry = FindEntry(UserId))
	{
		Entry->bMuted = bMuted;
		Entry->bServerMuted = bServerMuted;
		if (bMuted || bServerMuted)
		{
			Entry->bSpeaking = false;
			Entry->Energy = 0.f;
		}
		NotifyRoster();
	}
}

void UAurixVoiceLobbyComponent::HandleSpeaking(FGuid Channel, FGuid UserId, bool bSpeaking)
{
	if (Channel != JoinedChannel)
	{
		return;
	}
	if (FAurixLobbyEntry* Entry = FindEntry(UserId))
	{
		Entry->bSpeaking = bSpeaking;
		if (!bSpeaking)
		{
			Entry->Energy = 0.f;
		}
		NotifyRoster();
	}
}

void UAurixVoiceLobbyComponent::HandlePriorityChanged(FGuid Channel, FGuid UserId, bool bPriority)
{
	if (Channel != JoinedChannel)
	{
		return;
	}
	if (FAurixLobbyEntry* Entry = FindEntry(UserId))
	{
		Entry->bPriority = bPriority;
		NotifyRoster();
	}
}

void UAurixVoiceLobbyComponent::HandleEnergy(FGuid Channel, const TArray<FAurixParticipant>& Levels)
{
	if (Channel != JoinedChannel)
	{
		return;
	}
	bool bChanged = false;
	for (const FAurixParticipant& Level : Levels)
	{
		if (FAurixLobbyEntry* Entry = FindEntry(Level.UserId))
		{
			Entry->Energy = Level.Energy;
			bChanged = true;
		}
	}
	if (bChanged)
	{
		NotifyRoster();
	}
}

void UAurixVoiceLobbyComponent::HandleLocalSpeaking(bool bSpeaking)
{
	bSelfSpeaking = bSpeaking;
	if (FAurixLobbyEntry* Self = FindEntry(SelfUserId))
	{
		Self->bSpeaking = bSpeaking;
		NotifyRoster();
	}
}

void UAurixVoiceLobbyComponent::HandleMediaPath(EAurixMediaPath Path, const FString& Reason)
{
	NotifyStatus();
}

void UAurixVoiceLobbyComponent::HandleNetworkQuality(const FAurixNetworkQuality& Quality)
{
	LastQuality = Quality;
	NotifyStatus();
}

void UAurixVoiceLobbyComponent::HandleEndpointChanged(const FString& Url)
{
	NotifyStatus();
}

void UAurixVoiceLobbyComponent::HandleChat(const FAurixChatMessage& Message)
{
	const bool bDirect = Message.RecipientId.IsValid();
	if (!bDirect && Message.ChannelId != JoinedChannel)
	{
		return;
	}
	FString Sender = Message.SenderName;
	if (Sender.IsEmpty())
	{
		if (const FAurixLobbyEntry* Entry = FindEntry(Message.SenderId))
		{
			Sender = Entry->DisplayName;
		}
	}
	if (Sender.IsEmpty())
	{
		Sender = Message.SenderId.ToString(EGuidFormats::DigitsWithHyphens).ToLower().Left(8);
	}
	OnChatLine.Broadcast(Sender, Message.Text, bDirect);
}

void UAurixVoiceLobbyComponent::HandleRequestFailed(int64 RequestId, const FString& Code, const FString& Message)
{
	OnError.Broadcast(Code, Message);
}

void UAurixVoiceLobbyComponent::HandleServerError(const FString& Code, const FString& Message)
{
	OnError.Broadcast(Code, Message);
}

void UAurixVoiceLobbyComponent::HandleRejoinFailed(FGuid Channel, const FString& Code, const FString& Message)
{
	if (Channel == JoinedChannel)
	{
		JoinedChannel.Invalidate();
		Roster.Reset();
		NotifyRoster();
		NotifyStatus();
	}
	OnError.Broadcast(Code, Message);
}

void UAurixVoiceLobbyComponent::HandleKicked(FGuid Channel, const FString& Reason)
{
	if (Channel == JoinedChannel)
	{
		JoinedChannel.Invalidate();
		Roster.Reset();
		NotifyRoster();
		NotifyStatus();
	}
	OnError.Broadcast(TEXT("KICKED"), Reason);
}

void UAurixVoiceLobbyComponent::HandleFailedToRecover(const FString& Reason)
{
	bSessionReady = false;
	JoinedChannel.Invalidate();
	Roster.Reset();
	NotifyRoster();
	NotifyStatus();
	OnError.Broadcast(TEXT("DISCONNECTED"), Reason);
}

void UAurixVoiceLobbyComponent::HandleDisconnected(const FString& Reason)
{
	bSessionReady = false;
	JoinedChannel.Invalidate();
	Roster.Reset();
	NotifyRoster();
	NotifyStatus();
}

void UAurixVoiceLobbyComponent::NotifyRoster()
{
	OnRosterChanged.Broadcast(Roster);
}

void UAurixVoiceLobbyComponent::NotifyStatus()
{
	OnStatusChanged.Broadcast(GetStatus());
}
