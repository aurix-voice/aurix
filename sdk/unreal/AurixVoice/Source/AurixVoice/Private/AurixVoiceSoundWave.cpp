#include "AurixVoiceSoundWave.h"

#include "aurix_client.h"

UAurixVoiceSoundWave::UAurixVoiceSoundWave(const FObjectInitializer& ObjectInitializer)
	: Super(ObjectInitializer)
{
	SetSampleRate(AURIX_SAMPLE_RATE);
	NumChannels = 2;
	SampleByteSize = 2;
	Duration = INDEFINITELY_LOOPING_DURATION;
	bLooping = false;
	bProcedural = true;
	SoundGroup = SOUNDGROUP_Voice;
	bCanProcessAsync = false;
	VirtualizationMode = EVirtualizationMode::PlayWhenSilent;
}

void UAurixVoiceSoundWave::SetClient(AurixClient* InClient)
{
	FScopeLock Lock(&ClientLock);
	Client = InClient;
}

int32 UAurixVoiceSoundWave::GeneratePCMData(uint8* PCMData, const int32 SamplesNeeded)
{
	if (SamplesNeeded <= 0)
	{
		return 0;
	}
	int16* Out = reinterpret_cast<int16*>(PCMData);
	FScopeLock Lock(&ClientLock);
	if (Client)
	{
		// Overwrites the buffer (silence when nobody is talking), so the stream never underruns.
		aurix_client_mix_output_i16(Client, Out, static_cast<size_t>(SamplesNeeded), static_cast<uint8_t>(NumChannels));
	}
	else
	{
		FMemory::Memzero(PCMData, SamplesNeeded * sizeof(int16));
	}
	return SamplesNeeded * sizeof(int16);
}
