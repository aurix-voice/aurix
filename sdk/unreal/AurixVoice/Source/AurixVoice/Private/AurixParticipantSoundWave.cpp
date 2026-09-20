#include "AurixParticipantSoundWave.h"

#include "AurixNativeConversions.h"

UAurixParticipantSoundWave::UAurixParticipantSoundWave(const FObjectInitializer& ObjectInitializer)
	: Super(ObjectInitializer)
{
	SetSampleRate(AURIX_SAMPLE_RATE);
	NumChannels = 1;
	SampleByteSize = 2;
	Duration = INDEFINITELY_LOOPING_DURATION;
	bLooping = false;
	bProcedural = true;
	SoundGroup = SOUNDGROUP_Voice;
	bCanProcessAsync = false;
	// Keep pulling while culled / out of range so the jitter buffer never piles up.
	VirtualizationMode = EVirtualizationMode::PlayWhenSilent;
}

void UAurixParticipantSoundWave::SetSource(AurixClient* InClient, const FGuid& InUserId)
{
	FScopeLock Lock(&SourceLock);
	Client = InClient;
	UserId = InUserId;
	FMemory::Memcpy(NativeUserId, ToUuid(InUserId).raw.bytes, sizeof(NativeUserId));
	if (!InClient)
	{
		bReceiving.store(false, std::memory_order_relaxed);
	}
}

void UAurixParticipantSoundWave::SetStereo(bool bStereo)
{
	NumChannels = bStereo ? 2 : 1;
}

int32 UAurixParticipantSoundWave::GeneratePCMData(uint8* PCMData, const int32 SamplesNeeded)
{
	if (SamplesNeeded <= 0)
	{
		return 0;
	}
	int16* Out = reinterpret_cast<int16*>(PCMData);
	size_t Frames = 0;
	{
		FScopeLock Lock(&SourceLock);
		if (Client)
		{
			AurixUuid Uuid;
			FMemory::Memcpy(Uuid.bytes, NativeUserId, sizeof(Uuid.bytes));
			// Overwrites the buffer (silence past what is buffered), so the stream never underruns.
			Frames = aurix_client_pull_participant_i16(Client, &Uuid, Out, static_cast<size_t>(SamplesNeeded), static_cast<uint8_t>(NumChannels));
		}
		else
		{
			FMemory::Memzero(PCMData, SamplesNeeded * sizeof(int16));
		}
	}
	bReceiving.store(Frames > 0, std::memory_order_relaxed);
	return SamplesNeeded * sizeof(int16);
}
