#include "AurixAudioCapture.h"

#include "AurixVoiceLog.h"
#include "aurix_client.h"

FAurixAudioCapture::~FAurixAudioCapture()
{
	Stop();
}

TArray<FString> FAurixAudioCapture::ListDevices()
{
	TArray<FString> Names;
	Audio::FAudioCapture Probe;
	TArray<Audio::FCaptureDeviceInfo> Devices;
	Probe.GetCaptureDevicesAvailable(Devices);
	for (const Audio::FCaptureDeviceInfo& Info : Devices)
	{
		Names.Add(Info.DeviceName);
	}
	return Names;
}

bool FAurixAudioCapture::Start(AurixClient* InClient, int32 DeviceIndex)
{
	Stop();
	if (!InClient)
	{
		return false;
	}
	Client.store(InClient);

	Audio::FAudioCaptureDeviceParams Params;
	Params.DeviceIndex = DeviceIndex < 0 ? INDEX_NONE : DeviceIndex;
	Params.bUseHardwareAEC = true;

	// Runs on the capture thread; `aurix_client_push_capture_f32` is audio-thread safe and the
	// client pointer is cleared before the client is destroyed (see Stop()).
	Audio::FOnAudioCaptureFunction OnCapture =
		[this](const void* InAudio, int32 NumFrames, int32 NumChannels, int32 InSampleRate, double /*StreamTime*/, bool /*bOverflow*/)
	{
		AurixClient* Target = Client.load();
		if (!Target || NumFrames <= 0 || NumChannels <= 0 || InSampleRate <= 0)
		{
			return;
		}
		const float* Pcm = static_cast<const float*>(InAudio);
		if (Injector.IsActive())
		{
			const int32 Count = NumFrames * NumChannels;
			InjectScratch.SetNumUninitialized(Count, false);
			FMemory::Memcpy(InjectScratch.GetData(), Pcm, Count * sizeof(float));
			Injector.Mix(InjectScratch.GetData(), NumFrames, NumChannels, InSampleRate);
			Pcm = InjectScratch.GetData();
		}
		aurix_client_push_capture_f32(
			Target,
			Pcm,
			static_cast<size_t>(NumFrames) * static_cast<size_t>(NumChannels),
			static_cast<uint32_t>(InSampleRate),
			static_cast<uint8_t>(FMath::Clamp(NumChannels, 1, 255)));
	};

	if (!Capture.OpenAudioCaptureStream(Params, MoveTemp(OnCapture), AURIX_FRAME_SAMPLES))
	{
		UE_LOG(LogAurixVoice, Warning, TEXT("AudioCapture: could not open capture device %d"), DeviceIndex);
		Client.store(nullptr);
		return false;
	}
	if (!Capture.StartStream())
	{
		UE_LOG(LogAurixVoice, Warning, TEXT("AudioCapture: could not start capture device %d"), DeviceIndex);
		Capture.CloseStream();
		Client.store(nullptr);
		return false;
	}
	bCapturing = true;
	CurrentDeviceIndex = DeviceIndex;
	CurrentDeviceName.Reset();
	if (DeviceIndex >= 0)
	{
		const TArray<FString> Names = ListDevices();
		if (Names.IsValidIndex(DeviceIndex))
		{
			CurrentDeviceName = Names[DeviceIndex];
		}
	}
	return true;
}

void FAurixAudioCapture::Stop()
{
	if (Capture.IsStreamOpen())
	{
		Capture.StopStream();
		Capture.CloseStream();
	}
	Client.store(nullptr);
	bCapturing = false;
}

// ---- injector ------------------------------------------------------------------------------

void FAurixAudioInjector::Start(TArray<float> InPcm, int32 InSampleRate, int32 InChannels, bool bInLoop, float InGain, bool bInMixWithMicrophone)
{
	FScopeLock Guard(&Lock);
	Pcm = MoveTemp(InPcm);
	SampleRate = FMath::Max(InSampleRate, 1);
	Channels = FMath::Clamp(InChannels, 1, 8);
	bLoop = bInLoop;
	Gain = FMath::Max(InGain, 0.f);
	bMixWithMicrophone = bInMixWithMicrophone;
	Position = 0.0;
	bFinished.store(false);
	bActive.store(Pcm.Num() >= Channels);
}

void FAurixAudioInjector::Stop()
{
	FScopeLock Guard(&Lock);
	bActive.store(false);
	bFinished.store(false);
	Pcm.Reset();
	Position = 0.0;
}

float FAurixAudioInjector::Sample(int32 Channel) const
{
	// Linear interpolation between neighbouring source frames; mono sources feed every channel.
	const int32 Frames = Pcm.Num() / Channels;
	if (Frames == 0)
	{
		return 0.f;
	}
	const int32 Ch = Channels == 1 ? 0 : FMath::Min(Channel, Channels - 1);
	const int32 I0 = FMath::Clamp(static_cast<int32>(Position), 0, Frames - 1);
	const int32 I1 = I0 + 1 < Frames ? I0 + 1 : (bLoop ? 0 : I0);
	const float Frac = static_cast<float>(Position - static_cast<double>(I0));
	return FMath::Lerp(Pcm[I0 * Channels + Ch], Pcm[I1 * Channels + Ch], Frac) * Gain;
}

void FAurixAudioInjector::Advance(int32 OutRate)
{
	const int32 Frames = Pcm.Num() / Channels;
	Position += static_cast<double>(SampleRate) / static_cast<double>(FMath::Max(OutRate, 1));
	if (Position >= static_cast<double>(Frames))
	{
		if (bLoop)
		{
			Position = FMath::Fmod(Position, static_cast<double>(Frames));
		}
		else
		{
			bFinished.store(true);
			bActive.store(false);
		}
	}
}

void FAurixAudioInjector::Mix(float* InOut, int32 NumFrames, int32 NumChannels, int32 OutRate)
{
	FScopeLock Guard(&Lock);
	for (int32 F = 0; F < NumFrames; ++F)
	{
		const bool bLive = bActive.load();
		for (int32 C = 0; C < NumChannels; ++C)
		{
			const float S = bLive ? Sample(C) : 0.f;
			float& Dst = InOut[F * NumChannels + C];
			Dst = bMixWithMicrophone ? FMath::Clamp(Dst + S, -1.f, 1.f) : S;
		}
		if (bLive)
		{
			Advance(OutRate);
		}
	}
}

void FAurixAudioInjector::Pull(TArray<float>& Out, int32 NumFrames, int32 NumChannels, int32 OutRate)
{
	Out.SetNumZeroed(NumFrames * NumChannels, false);
	FScopeLock Guard(&Lock);
	for (int32 F = 0; F < NumFrames && bActive.load(); ++F)
	{
		for (int32 C = 0; C < NumChannels; ++C)
		{
			Out[F * NumChannels + C] = Sample(C);
		}
		Advance(OutRate);
	}
}
