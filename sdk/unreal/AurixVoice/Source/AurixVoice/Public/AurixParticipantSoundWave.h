#pragma once

#include "CoreMinimal.h"
#include "Sound/SoundWaveProcedural.h"

#include <atomic>

#include "AurixParticipantSoundWave.generated.h"

struct AurixClient;

/**
 * Procedural 48 kHz sound carrying **one** participant's voice (microphone + TTS), pulled
 * straight from the native client's jitter buffers on the audio render thread without any
 * local panning. Play it through an attached UAudioComponent with attenuation / a spatializer
 * plugin / occlusion / reverb sends like any other in-world sound — the engine does the
 * positioning. Mono by default (Unreal only spatializes mono sources); stereo keeps a music
 * sender's L/R image for non-spatialized playback.
 *
 * While bound, the participant is *claimed*: UAurixVoiceSubsystem's aggregate mix skips it,
 * so unclaimed talkers keep playing through the 2D mix and nobody is heard twice. The pulled
 * audio is not fed to the echo canceller — pass the engine's final output to
 * UAurixVoiceSubsystem::PushRenderAudio (e.g. from a submix listener) if AEC matters.
 */
UCLASS(NotBlueprintable)
class AURIXVOICE_API UAurixParticipantSoundWave : public USoundWaveProcedural
{
	GENERATED_BODY()

public:
	UAurixParticipantSoundWave(const FObjectInitializer& ObjectInitializer);

	/** Participant whose streams this wave renders (set by UAurixVoiceSubsystem::CreateParticipantSound). */
	UFUNCTION(BlueprintPure, Category = "Aurix Voice|Playback")
	FGuid GetUserId() const { return UserId; }

	/** True while the last render block carried decoded audio (the participant is talking). */
	UFUNCTION(BlueprintPure, Category = "Aurix Voice|Playback")
	bool IsReceivingAudio() const { return bReceiving.load(std::memory_order_relaxed); }

	/** Bind (or, with nullptr, unbind) the client and user this wave pulls from. Thread-safe. */
	void SetSource(AurixClient* InClient, const FGuid& InUserId);

	/** Switch to two output channels (music senders). Call before the wave starts playing. */
	void SetStereo(bool bStereo);

	//~ USoundWaveProcedural
	virtual int32 GeneratePCMData(uint8* PCMData, const int32 SamplesNeeded) override;

private:
	FCriticalSection SourceLock;
	AurixClient* Client = nullptr;
	uint8 NativeUserId[16] = {};
	FGuid UserId;
	std::atomic<bool> bReceiving{false};
};
