#include "AurixProximityVoiceComponent.h"

#include "AurixVoiceSubsystem.h"
#include "Components/AudioComponent.h"
#include "Engine/GameInstance.h"
#include "Engine/World.h"
#include "GameFramework/Actor.h"

UAurixProximityVoiceComponent::UAurixProximityVoiceComponent()
{
	PrimaryComponentTick.bCanEverTick = true;
	PrimaryComponentTick.bStartWithTickEnabled = true;
}

UAurixVoiceSubsystem* UAurixProximityVoiceComponent::GetVoice() const
{
	const UWorld* World = GetWorld();
	UGameInstance* GameInstance = World ? World->GetGameInstance() : nullptr;
	return GameInstance ? GameInstance->GetSubsystem<UAurixVoiceSubsystem>() : nullptr;
}

void UAurixProximityVoiceComponent::EndPlay(const EEndPlayReason::Type EndPlayReason)
{
	DetachAllParticipantVoices();
	Super::EndPlay(EndPlayReason);
}

void UAurixProximityVoiceComponent::SetChannel(FGuid Channel)
{
	ChannelId = Channel;
	bHasLastPose = false;
	SinceLastUpdate = 0.f;
	if (ChannelId.IsValid())
	{
		SendPoseNow();
	}
}

bool UAurixProximityVoiceComponent::ReadPose(FVector& OutLocation, FRotator& OutRotation) const
{
	if (PoseSource)
	{
		OutLocation = PoseSource->GetComponentLocation();
		OutRotation = PoseSource->GetComponentRotation();
		return true;
	}
	const AActor* Owner = GetOwner();
	if (!Owner)
	{
		return false;
	}
	OutLocation = Owner->GetActorLocation();
	OutRotation = Owner->GetActorRotation();
	return true;
}

bool UAurixProximityVoiceComponent::SendPoseNow()
{
	UAurixVoiceSubsystem* Voice = GetVoice();
	FVector Location;
	FRotator Rotation;
	if (!Voice || !ChannelId.IsValid() || !Voice->IsConnected() || !ReadPose(Location, Rotation))
	{
		return false;
	}
	if (!Voice->UpdateOwnPosition(ChannelId, Location, Rotation, WorldToMeters))
	{
		return false;
	}
	LastLocation = Location;
	LastRotation = Rotation;
	bHasLastPose = true;
	SinceLastUpdate = 0.f;
	return true;
}

void UAurixProximityVoiceComponent::TickComponent(float DeltaTime, ELevelTick TickType, FActorComponentTickFunction* ThisTickFunction)
{
	Super::TickComponent(DeltaTime, TickType, ThisTickFunction);
	if (!ChannelId.IsValid())
	{
		return;
	}
	SinceLastUpdate += DeltaTime;
	if (SinceLastUpdate < 1.f / FMath::Max(UpdatesPerSecond, 1.f))
	{
		return;
	}

	FVector Location;
	FRotator Rotation;
	if (!ReadPose(Location, Rotation))
	{
		return;
	}
	if (bHasLastPose)
	{
		const bool bMoved = FVector::DistSquared(Location, LastLocation) >= FMath::Square(MinMoveCm);
		const bool bTurned = !Rotation.Equals(LastRotation, MinTurnDegrees);
		if (!bMoved && !bTurned)
		{
			// Keep the timer saturated so the next real move goes out immediately.
			SinceLastUpdate = 1.f / FMath::Max(UpdatesPerSecond, 1.f);
			return;
		}
	}
	SendPoseNow();
}

UAudioComponent* UAurixProximityVoiceComponent::AttachParticipantVoice(FGuid UserId, USceneComponent* AttachTo)
{
	UAurixVoiceSubsystem* Voice = GetVoice();
	if (!Voice || !UserId.IsValid() || !AttachTo)
	{
		return nullptr;
	}
	if (TObjectPtr<UAudioComponent>* Existing = AttachedVoices.Find(UserId))
	{
		if (*Existing && (*Existing)->GetAttachParent() == AttachTo)
		{
			return *Existing;
		}
		if (*Existing)
		{
			(*Existing)->Stop();
			(*Existing)->DestroyComponent();
		}
		AttachedVoices.Remove(UserId);
	}
	UAudioComponent* Component = Voice->SpawnParticipantAudioComponent(UserId, AttachTo, VoiceAttenuation);
	if (Component)
	{
		AttachedVoices.Add(UserId, Component);
	}
	return Component;
}

void UAurixProximityVoiceComponent::DetachParticipantVoice(FGuid UserId)
{
	TObjectPtr<UAudioComponent> Component;
	if (AttachedVoices.RemoveAndCopyValue(UserId, Component) && Component)
	{
		Component->Stop();
		Component->DestroyComponent();
	}
	if (UAurixVoiceSubsystem* Voice = GetVoice())
	{
		Voice->ReleaseParticipantSound(UserId);
	}
}

void UAurixProximityVoiceComponent::DetachAllParticipantVoices()
{
	TArray<FGuid> Users;
	AttachedVoices.GetKeys(Users);
	for (const FGuid& UserId : Users)
	{
		DetachParticipantVoice(UserId);
	}
}
