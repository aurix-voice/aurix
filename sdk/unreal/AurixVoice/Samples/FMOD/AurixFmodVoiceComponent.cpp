#include "AurixFmodVoiceComponent.h"

#include "AurixVoiceSubsystem.h"
#include "Engine/GameInstance.h"
#include "FMODAudioComponent.h"
#include "FMODStudioModule.h"

DEFINE_LOG_CATEGORY_STATIC(LogAurixFmod, Log, All);

namespace
{
constexpr int32 AurixRate = 48000;

template <typename T>
T* UserDataOf(FMOD_DSP_STATE* State)
{
	void* Data = nullptr;
	if (State && State->functions && State->functions->getuserdata)
	{
		State->functions->getuserdata(State, &Data);
	}
	return static_cast<T*>(Data);
}
} // namespace

UAurixFmodVoiceComponent::UAurixFmodVoiceComponent()
{
	PrimaryComponentTick.bCanEverTick = true;
}

UAurixVoiceSubsystem* UAurixFmodVoiceComponent::GetVoice() const
{
	const UWorld* World = GetWorld();
	UGameInstance* GameInstance = World ? World->GetGameInstance() : nullptr;
	return GameInstance ? GameInstance->GetSubsystem<UAurixVoiceSubsystem>() : nullptr;
}

FMOD::System* UAurixFmodVoiceComponent::CoreSystem() const
{
	FMOD::Studio::System* Studio = IFMODStudioModule::Get().GetStudioSystem(EFMODSystemContext::Runtime);
	FMOD::System* Core = nullptr;
	if (Studio)
	{
		Studio->getCoreSystem(&Core);
	}
	return Core;
}

FMOD::DSP* UAurixFmodVoiceComponent::CreateDsp(const char* Name, FMOD_DSP_READ_CALLBACK Read)
{
	FMOD::System* Core = CoreSystem();
	if (!Core)
	{
		return nullptr;
	}
	FMOD_DSP_DESCRIPTION Desc = {};
	Desc.pluginsdkversion = FMOD_PLUGIN_SDK_VERSION;
	FCStringAnsi::Strncpy(Desc.name, Name, sizeof(Desc.name));
	Desc.version = 0x00010000;
	Desc.numinputbuffers = 1;
	Desc.numoutputbuffers = 1;
	Desc.read = Read;
	Desc.userdata = this;
	FMOD::DSP* Dsp = nullptr;
	if (Core->createDSP(&Desc, &Dsp) != FMOD_OK)
	{
		return nullptr;
	}
	Dsp->setUserData(this);
	return Dsp;
}

void UAurixFmodVoiceComponent::BeginPlay()
{
	Super::BeginPlay();
	FMOD::System* Core = CoreSystem();
	UAurixVoiceSubsystem* Voice = GetVoice();
	if (!Core || !Voice)
	{
		UE_LOG(LogAurixFmod, Warning, TEXT("AurixFmodVoice: FMOD runtime or AurixVoice subsystem unavailable"));
		return;
	}
	FMOD_SPEAKERMODE Mode;
	int32 RawSpeakers = 0;
	Core->getSoftwareFormat(&SampleRate, &Mode, &RawSpeakers);
	if (SampleRate != AurixRate)
	{
		UE_LOG(LogAurixFmod, Error, TEXT("AurixFmodVoice: FMOD runs at %d Hz; Aurix playout is 48000 Hz. Set Project Settings → FMOD Studio → Sample Rate to 48000."), SampleRate);
		return;
	}

	// The plugin must not also play the mix through Unreal's audio device (also clear
	// bAutoStartPlayback in the plugin settings so it does not come back after a reconnect).
	Voice->StopPlayback();

	Core->getMasterChannelGroup(&MasterGroup);
	VoiceGroup = MasterGroup;
	if (!VoiceBus.IsEmpty() && bFeedEchoCanceller)
	{
		// The AEC reference must not contain the voice mix (the core already references what
		// MixOutputAudio returns). That is only guaranteed when voice and reference share one
		// group with the reference tap upstream of the voice DSP, so the bus is ignored here.
		UE_LOG(LogAurixFmod, Warning, TEXT("AurixFmodVoice: VoiceBus is ignored while bFeedEchoCanceller is set; voice plays on the master group."));
	}
	else if (!VoiceBus.IsEmpty())
	{
		FMOD::Studio::System* Studio = IFMODStudioModule::Get().GetStudioSystem(EFMODSystemContext::Runtime);
		FMOD::Studio::Bus* Bus = nullptr;
		if (Studio && Studio->getBus(TCHAR_TO_UTF8(*VoiceBus), &Bus) == FMOD_OK && Bus)
		{
			Bus->lockChannelGroup();
			Studio->flushCommands();
			FMOD::ChannelGroup* Group = nullptr;
			if (Bus->getChannelGroup(&Group) == FMOD_OK && Group)
			{
				VoiceGroup = Group;
			}
		}
	}
	// Signal flows tail → head; a DSP added at the tail runs before those already present. The
	// voice DSP is added first, the reference tap second, so the tap sees the game mix without voice.
	VoiceDsp = CreateDsp("Aurix voice", &UAurixFmodVoiceComponent::VoiceRead);
	if (VoiceDsp)
	{
		VoiceGroup->addDSP(FMOD_CHANNELCONTROL_DSP_TAIL, VoiceDsp);
	}
	if (bFeedEchoCanceller)
	{
		RenderDsp = CreateDsp("Aurix AEC reference", &UAurixFmodVoiceComponent::RenderRead);
		if (RenderDsp)
		{
			MasterGroup->addDSP(FMOD_CHANNELCONTROL_DSP_TAIL, RenderDsp);
		}
	}
	if (bCaptureWithFmod)
	{
		StartFmodCapture();
	}
}

void UAurixFmodVoiceComponent::EndPlay(const EEndPlayReason::Type Reason)
{
	TArray<FGuid> Bound;
	Participants.GetKeys(Bound);
	for (const FGuid& Id : Bound)
	{
		UnbindParticipant(Id);
	}
	StopFmodCapture();
	if (VoiceDsp)
	{
		VoiceGroup->removeDSP(VoiceDsp);
		VoiceDsp->release();
		VoiceDsp = nullptr;
	}
	if (RenderDsp)
	{
		MasterGroup->removeDSP(RenderDsp);
		RenderDsp->release();
		RenderDsp = nullptr;
	}
	if (VoiceGroup && VoiceGroup != MasterGroup)
	{
		FMOD::Studio::System* Studio = IFMODStudioModule::Get().GetStudioSystem(EFMODSystemContext::Runtime);
		FMOD::Studio::Bus* Bus = nullptr;
		if (Studio && Studio->getBus(TCHAR_TO_UTF8(*VoiceBus), &Bus) == FMOD_OK && Bus)
		{
			Bus->unlockChannelGroup();
		}
	}
	Super::EndPlay(Reason);
}

void UAurixFmodVoiceComponent::TickComponent(float DeltaTime, ELevelTick TickType, FActorComponentTickFunction* ThisTickFunction)
{
	Super::TickComponent(DeltaTime, TickType, ThisTickFunction);
	if (CaptureSound)
	{
		PumpFmodCapture();
	}
}

// ---- output -------------------------------------------------------------------------------------

FMOD_RESULT F_CALLBACK UAurixFmodVoiceComponent::VoiceRead(FMOD_DSP_STATE* State, float* In, float* Out, unsigned int Length, int InChannels, int* OutChannels)
{
	const int32 Channels = *OutChannels;
	const int32 Samples = static_cast<int32>(Length) * Channels;
	if (In && InChannels == Channels)
	{
		FMemory::Memcpy(Out, In, Samples * sizeof(float));
	}
	else
	{
		FMemory::Memzero(Out, Samples * sizeof(float));
	}
	UAurixFmodVoiceComponent* Self = UserDataOf<UAurixFmodVoiceComponent>(State);
	UAurixVoiceSubsystem* Voice = Self ? Self->GetVoice() : nullptr;
	if (!Voice)
	{
		return FMOD_OK;
	}
	const int32 VoiceChannels = FMath::Min(Channels, 2);
	TArray<float>& Scratch = Self->VoiceScratch;
	Scratch.SetNumZeroed(static_cast<int32>(Length) * VoiceChannels, false);
	FMemory::Memzero(Scratch.GetData(), Scratch.Num() * sizeof(float));
	if (Voice->MixOutputAudio(Scratch, VoiceChannels) == 0)
	{
		return FMOD_OK;
	}
	if (Channels <= 2)
	{
		for (int32 I = 0; I < Samples; ++I)
		{
			Out[I] += Scratch[I];
		}
	}
	else
	{
		// Surround bus: voice on front left/right.
		for (unsigned int F = 0; F < Length; ++F)
		{
			Out[F * Channels] += Scratch[F * 2];
			Out[F * Channels + 1] += Scratch[F * 2 + 1];
		}
	}
	return FMOD_OK;
}

FMOD_RESULT F_CALLBACK UAurixFmodVoiceComponent::RenderRead(FMOD_DSP_STATE* State, float* In, float* Out, unsigned int Length, int InChannels, int* OutChannels)
{
	const int32 Channels = *OutChannels;
	const int32 Samples = static_cast<int32>(Length) * Channels;
	if (In && InChannels == Channels)
	{
		FMemory::Memcpy(Out, In, Samples * sizeof(float));
	}
	else
	{
		FMemory::Memzero(Out, Samples * sizeof(float));
	}
	UAurixFmodVoiceComponent* Self = UserDataOf<UAurixFmodVoiceComponent>(State);
	UAurixVoiceSubsystem* Voice = Self ? Self->GetVoice() : nullptr;
	if (!Voice)
	{
		return FMOD_OK;
	}
	const int32 RefChannels = FMath::Min(Channels, 2);
	TArray<float>& Scratch = Self->RenderScratch;
	Scratch.SetNumUninitialized(static_cast<int32>(Length) * RefChannels, false);
	for (unsigned int F = 0; F < Length; ++F)
	{
		for (int32 C = 0; C < RefChannels; ++C)
		{
			Scratch[F * RefChannels + C] = Out[F * Channels + C];
		}
	}
	Voice->PushRenderAudio(Scratch, RefChannels);
	return FMOD_OK;
}

// ---- capture ------------------------------------------------------------------------------------

void UAurixFmodVoiceComponent::StartFmodCapture()
{
	FMOD::System* Core = CoreSystem();
	UAurixVoiceSubsystem* Voice = GetVoice();
	if (!Core || !Voice)
	{
		return;
	}
	Voice->StopCapture(); // the plugin's own device capture must not run at the same time
	int32 Rate = 0, Channels = 0;
	FMOD_SPEAKERMODE Mode;
	FMOD_GUID Guid;
	FMOD_DRIVER_STATE DriverState;
	if (Core->getRecordDriverInfo(RecordDriver, nullptr, 0, &Guid, &Rate, &Mode, &Channels, &DriverState) == FMOD_OK)
	{
		CaptureChannels = FMath::Clamp(Channels, 1, 2);
	}
	CaptureRate = AurixRate;
	FMOD_CREATESOUNDEXINFO Ex = {};
	Ex.cbsize = sizeof(Ex);
	Ex.numchannels = CaptureChannels;
	Ex.format = FMOD_SOUND_FORMAT_PCMFLOAT;
	Ex.defaultfrequency = CaptureRate;
	Ex.length = CaptureRate * CaptureChannels * sizeof(float); // one second ring
	if (Core->createSound(nullptr, FMOD_LOOP_NORMAL | FMOD_OPENUSER, &Ex, &CaptureSound) != FMOD_OK)
	{
		CaptureSound = nullptr;
		return;
	}
	CaptureSound->getLength(&CaptureLength, FMOD_TIMEUNIT_PCM);
	CaptureReadPos = 0;
	Core->recordStart(RecordDriver, CaptureSound, true);
}

void UAurixFmodVoiceComponent::StopFmodCapture()
{
	if (!CaptureSound)
	{
		return;
	}
	if (FMOD::System* Core = CoreSystem())
	{
		Core->recordStop(RecordDriver);
	}
	CaptureSound->release();
	CaptureSound = nullptr;
}

void UAurixFmodVoiceComponent::PumpFmodCapture()
{
	FMOD::System* Core = CoreSystem();
	UAurixVoiceSubsystem* Voice = GetVoice();
	if (!Core || !Voice)
	{
		return;
	}
	unsigned int Pos = 0;
	Core->getRecordPosition(RecordDriver, &Pos);
	const unsigned int Avail = (Pos + CaptureLength - CaptureReadPos) % CaptureLength;
	if (Avail == 0)
	{
		return;
	}
	CaptureScratch.SetNumUninitialized(static_cast<int32>(Avail) * CaptureChannels, false);
	const unsigned int BytesPerFrame = CaptureChannels * sizeof(float);
	void *P1 = nullptr, *P2 = nullptr;
	unsigned int L1 = 0, L2 = 0;
	if (CaptureSound->lock(CaptureReadPos * BytesPerFrame, Avail * BytesPerFrame, &P1, &P2, &L1, &L2) == FMOD_OK)
	{
		FMemory::Memcpy(CaptureScratch.GetData(), P1, L1);
		if (P2 && L2)
		{
			FMemory::Memcpy(reinterpret_cast<uint8*>(CaptureScratch.GetData()) + L1, P2, L2);
		}
		CaptureSound->unlock(P1, P2, L1, L2);
		CaptureReadPos = (CaptureReadPos + Avail) % CaptureLength;
		Voice->PushCaptureAudio(CaptureScratch, CaptureRate, CaptureChannels);
	}
}

// ---- per-participant 3D -------------------------------------------------------------------------

bool UAurixFmodVoiceComponent::BindParticipantToEvent(FGuid UserId, UFMODAudioComponent* EventComponent)
{
	FMOD::System* Core = CoreSystem();
	UAurixVoiceSubsystem* Voice = GetVoice();
	if (!Core || !Voice || !UserId.IsValid() || !EventComponent || SampleRate != AurixRate)
	{
		return false;
	}
	UnbindParticipant(UserId);
	TUniquePtr<FParticipantStream> Stream = MakeUnique<FParticipantStream>();
	Stream->Owner = this;
	Stream->UserId = UserId;
	Stream->Event = EventComponent;
	FMOD_CREATESOUNDEXINFO Ex = {};
	Ex.cbsize = sizeof(Ex);
	Ex.numchannels = 1;
	Ex.format = FMOD_SOUND_FORMAT_PCMFLOAT;
	Ex.defaultfrequency = AurixRate;
	Ex.decodebuffersize = AurixRate / 50; // 20 ms
	Ex.length = static_cast<unsigned int>(AurixRate) * sizeof(float) * 3600; // "endless" stream
	Ex.pcmreadcallback = &UAurixFmodVoiceComponent::ParticipantRead;
	Ex.userdata = Stream.Get();
	if (Core->createSound(nullptr, FMOD_OPENUSER | FMOD_LOOP_NORMAL | FMOD_CREATESTREAM | FMOD_3D, &Ex, &Stream->Sound) != FMOD_OK)
	{
		return false;
	}
	Stream->Sound->setUserData(Stream.Get());
	Voice->SetParticipantClaimed(UserId, true);
	EventComponent->SetProgrammerSound(Stream->Sound);
	if (!EventComponent->IsPlaying())
	{
		EventComponent->Play();
	}
	Participants.Add(UserId, MoveTemp(Stream));
	return true;
}

void UAurixFmodVoiceComponent::UnbindParticipant(FGuid UserId)
{
	TUniquePtr<FParticipantStream> Stream;
	if (!Participants.RemoveAndCopyValue(UserId, Stream) || !Stream)
	{
		return;
	}
	if (UFMODAudioComponent* Event = Stream->Event.Get())
	{
		Event->Stop();
		Event->SetProgrammerSound(nullptr);
	}
	if (Stream->Sound)
	{
		Stream->Sound->release();
	}
	if (UAurixVoiceSubsystem* Voice = GetVoice())
	{
		Voice->SetParticipantClaimed(UserId, false);
	}
}

FMOD_RESULT F_CALLBACK UAurixFmodVoiceComponent::ParticipantRead(FMOD_SOUND* SoundRaw, void* Data, unsigned int DataLen)
{
	FMOD::Sound* Sound = reinterpret_cast<FMOD::Sound*>(SoundRaw);
	void* UserData = nullptr;
	Sound->getUserData(&UserData);
	FParticipantStream* Stream = static_cast<FParticipantStream*>(UserData);
	const int32 Samples = static_cast<int32>(DataLen / sizeof(float));
	UAurixVoiceSubsystem* Voice = Stream && Stream->Owner ? Stream->Owner->GetVoice() : nullptr;
	if (!Voice)
	{
		FMemory::Memzero(Data, DataLen);
		return FMOD_OK;
	}
	Stream->Scratch.SetNumUninitialized(Samples, false);
	Voice->PullParticipantAudio(Stream->UserId, Stream->Scratch, 1);
	FMemory::Memcpy(Data, Stream->Scratch.GetData(), Samples * sizeof(float));
	return FMOD_OK;
}
