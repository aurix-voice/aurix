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
		aurix_client_push_capture_f32(
			Target,
			static_cast<const float*>(InAudio),
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
