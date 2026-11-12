// Drop-in sample: Aurix Voice through FMOD Studio for Unreal.
//
// Copy this pair of files into a game module that depends on "AurixVoice" and "FMODStudio"
// (it is not part of the plugin's modules, so the plugin builds without FMOD). One component on a
// persistent actor (GameMode, a voice manager) does three things:
//
//   * Output  — a custom FMOD DSP on the master channel group (or a bus) adds the remote mix from
//               UAurixVoiceSubsystem::MixOutputAudio; the engine's own voice playback is turned off.
//   * AEC     — a pass-through DSP at the master tail hands what the player hears to
//               PushRenderAudio so the echo canceller can subtract it from the microphone.
//   * Capture — optional: FMOD's recording API feeds PushCaptureAudio when FMOD owns the mic
//               (consoles, or when you already record through FMOD).
//
// For per-speaker 3D voice use BindParticipantToEvent: it creates a programmer sound that streams
// one participant (PullParticipantAudio, claimed so the mix skips them) into a UFMODAudioComponent
// playing an event with a Programmer instrument and a 3D spatializer.
//
// Format contract (see UAurixVoiceSubsystem): MixOutputAudio / PullParticipantAudio / PushRenderAudio
// are 48 kHz interleaved float, 1 or 2 channels. Set FMOD's sample rate to 48000 (Project Settings →
// FMOD Studio → Sample Rate); the component logs an error and stays silent otherwise, so a mismatch
// cannot go unnoticed. PushCaptureAudio accepts any rate and channel count.
#pragma once

#include "CoreMinimal.h"
#include "Components/ActorComponent.h"
#include "fmod.hpp"
#include "fmod_studio.hpp"
#include "AurixFmodVoiceComponent.generated.h"

class UAurixVoiceSubsystem;
class UFMODAudioComponent;

UCLASS(ClassGroup = (Aurix), meta = (BlueprintSpawnableComponent))
class UAurixFmodVoiceComponent : public UActorComponent
{
	GENERATED_BODY()

public:
	UAurixFmodVoiceComponent();

	/** FMOD bus for the voice mix, e.g. "bus:/Voice". Empty = master channel group. Ignored while bFeedEchoCanceller is set (the reference tap must be upstream of the voice DSP on the same group). */
	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix Voice|FMOD")
	FString VoiceBus;

	/** Feed FMOD's game mix (without the voice, which the core already references) to the echo canceller. */
	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix Voice|FMOD")
	bool bFeedEchoCanceller = true;

	/** Record the microphone through FMOD instead of the plugin's own capture. */
	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix Voice|FMOD")
	bool bCaptureWithFmod = false;

	/** FMOD record driver index when bCaptureWithFmod (0 = default device). */
	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Aurix Voice|FMOD", meta = (EditCondition = "bCaptureWithFmod"))
	int32 RecordDriver = 0;

	/** Stream one participant into an FMOD event (Programmer instrument + 3D) and claim them. */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|FMOD")
	bool BindParticipantToEvent(FGuid UserId, UFMODAudioComponent* EventComponent);

	/** Stop the participant's programmer sound and return them to the mix. */
	UFUNCTION(BlueprintCallable, Category = "Aurix Voice|FMOD")
	void UnbindParticipant(FGuid UserId);

	UFUNCTION(BlueprintPure, Category = "Aurix Voice|FMOD")
	bool IsActive() const { return VoiceDsp != nullptr; }

protected:
	virtual void BeginPlay() override;
	virtual void EndPlay(const EEndPlayReason::Type Reason) override;
	virtual void TickComponent(float DeltaTime, ELevelTick TickType, FActorComponentTickFunction* ThisTickFunction) override;

private:
	struct FParticipantStream
	{
		FMOD::Sound* Sound = nullptr;
		TWeakObjectPtr<UFMODAudioComponent> Event;
		TArray<float> Scratch;
		UAurixFmodVoiceComponent* Owner = nullptr;
		FGuid UserId;
	};

	UAurixVoiceSubsystem* GetVoice() const;
	FMOD::System* CoreSystem() const;
	FMOD::DSP* CreateDsp(const char* Name, FMOD_DSP_READ_CALLBACK Read);
	void StartFmodCapture();
	void StopFmodCapture();
	void PumpFmodCapture();

	static FMOD_RESULT F_CALLBACK VoiceRead(FMOD_DSP_STATE* State, float* In, float* Out, unsigned int Length, int InChannels, int* OutChannels);
	static FMOD_RESULT F_CALLBACK RenderRead(FMOD_DSP_STATE* State, float* In, float* Out, unsigned int Length, int InChannels, int* OutChannels);
	static FMOD_RESULT F_CALLBACK ParticipantRead(FMOD_SOUND* Sound, void* Data, unsigned int DataLen);

	FMOD::DSP* VoiceDsp = nullptr;
	FMOD::DSP* RenderDsp = nullptr;
	FMOD::ChannelGroup* VoiceGroup = nullptr;
	FMOD::ChannelGroup* MasterGroup = nullptr;
	int32 SampleRate = 0;
	TArray<float> VoiceScratch;
	TArray<float> RenderScratch;

	FMOD::Sound* CaptureSound = nullptr;
	unsigned int CaptureReadPos = 0;
	unsigned int CaptureLength = 0;
	int32 CaptureChannels = 1;
	int32 CaptureRate = 48000;
	TArray<float> CaptureScratch;

	TMap<FGuid, TUniquePtr<FParticipantStream>> Participants;
};
