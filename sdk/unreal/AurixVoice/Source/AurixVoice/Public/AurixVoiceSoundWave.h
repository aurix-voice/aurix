#pragma once

#include "CoreMinimal.h"
#include "Sound/SoundWaveProcedural.h"
#include "AurixVoiceSoundWave.generated.h"

struct AurixClient;

/**
 * Procedural stereo 48 kHz sound that pulls the mix of all remote voices straight from the
 * native client on the audio render thread (no intermediate queue, so playout latency is the
 * jitter buffer alone). Directional panning of positional channels is already applied.
 */
UCLASS(NotBlueprintable)
class AURIXVOICE_API UAurixVoiceSoundWave : public USoundWaveProcedural
{
	GENERATED_BODY()

public:
	UAurixVoiceSoundWave(const FObjectInitializer& ObjectInitializer);

	/** Bind (or, with nullptr, unbind) the client whose output this wave renders. Thread-safe. */
	void SetClient(AurixClient* InClient);

	//~ USoundWaveProcedural
	virtual int32 GeneratePCMData(uint8* PCMData, const int32 SamplesNeeded) override;

private:
	FCriticalSection ClientLock;
	AurixClient* Client = nullptr;
};
