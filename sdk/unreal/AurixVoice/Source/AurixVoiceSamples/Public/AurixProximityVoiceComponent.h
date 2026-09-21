#pragma once

#include "CoreMinimal.h"
#include "Components/ActorComponent.h"
#include "AurixProximityVoiceComponent.generated.h"

class UAurixVoiceSubsystem;
class UAudioComponent;
class USoundAttenuation;

/**
 * Positional voice for the locally controlled pawn: reports the owner's pose to a positional
 * channel at a fixed rate (only when it moved), so the node can apply distance roll-off and
 * radius visibility, and optionally attaches each remote talker's voice to their avatar so
 * Unreal's attenuation / spatializer / occlusion render it in-world.
 *
 * Add to the local pawn (or the PlayerController with a pawn), call SetChannel after the join,
 * and — for engine spatialization — AttachParticipantVoice(UserId, RemoteAvatarHeadComponent)
 * when a remote avatar is spawned and DetachParticipantVoice when it goes away. Talkers that
 * are not attached stay audible through the 2D mix with the node's positional gain.
 */
UCLASS(ClassGroup = (Aurix), meta = (BlueprintSpawnableComponent))
class AURIXVOICESAMPLES_API UAurixProximityVoiceComponent : public UActorComponent
{
	GENERATED_BODY()

public:
	UAurixProximityVoiceComponent();

	/** Positional channel receiving the pose; invalid = idle. */
	UPROPERTY(BlueprintReadOnly, Category = "Aurix Voice")
	FGuid ChannelId;

	/** Pose updates per second (the node rate-limits anyway; 5–10 is plenty). */
	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix Voice", meta = (ClampMin = "1", ClampMax = "30"))
	float UpdatesPerSecond = 8.f;

	/** Skip the update when the owner moved less than this (cm) and turned less than MinTurnDegrees. */
	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix Voice", meta = (ClampMin = "0"))
	float MinMoveCm = 10.f;

	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix Voice", meta = (ClampMin = "0"))
	float MinTurnDegrees = 2.f;

	/** Unreal units per metre (project default 100). */
	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix Voice", meta = (ClampMin = "1"))
	float WorldToMeters = 100.f;

	/** Report the pose of this component instead of the owner's root (e.g. the camera). */
	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix Voice")
	TObjectPtr<USceneComponent> PoseSource = nullptr;

	/** Attenuation for voices attached with AttachParticipantVoice (null = component defaults). */
	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix Voice")
	TObjectPtr<USoundAttenuation> VoiceAttenuation = nullptr;

	/** Start (valid) or stop (invalid) reporting; sends one update immediately. */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice")
	void SetChannel(FGuid Channel);

	/** Send the current pose now regardless of the rate limit. */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice")
	bool SendPoseNow();

	/**
	 * Play UserId's voice from AttachTo (typically the remote avatar's head) through Unreal's
	 * spatialization; the participant leaves the 2D mix while attached. Re-attaching moves the
	 * voice to the new component.
	 */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice")
	UAudioComponent* AttachParticipantVoice(FGuid UserId, USceneComponent* AttachTo);

	/** Return UserId to the 2D mix and destroy the attached audio component. */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice")
	void DetachParticipantVoice(FGuid UserId);

	/** Detach every voice attached through this component. */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice")
	void DetachAllParticipantVoices();

	UFUNCTION(BlueprintPure, Category = "Aurix Voice")
	bool IsParticipantVoiceAttached(FGuid UserId) const { return AttachedVoices.Contains(UserId); }

	virtual void TickComponent(float DeltaTime, ELevelTick TickType, FActorComponentTickFunction* ThisTickFunction) override;

protected:
	virtual void EndPlay(const EEndPlayReason::Type EndPlayReason) override;

private:
	UAurixVoiceSubsystem* GetVoice() const;
	bool ReadPose(FVector& OutLocation, FRotator& OutRotation) const;

	UPROPERTY(Transient)
	TMap<FGuid, TObjectPtr<UAudioComponent>> AttachedVoices;

	float SinceLastUpdate = 0.f;
	bool bHasLastPose = false;
	FVector LastLocation = FVector::ZeroVector;
	FRotator LastRotation = FRotator::ZeroRotator;
};
