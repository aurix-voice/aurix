#pragma once

#include "CoreMinimal.h"
#include "AudioCaptureCore.h"

#include <atomic>

struct AurixClient;

/**
 * Microphone → native client bridge on top of the engine's AudioCapture module. The capture
 * thread pushes float PCM straight into `aurix_client_push_capture_f32`, which resamples to
 * 48 kHz, applies gain/VAD, encodes Opus and sends.
 */
class FAurixAudioCapture
{
public:
	FAurixAudioCapture() = default;
	~FAurixAudioCapture();
	FAurixAudioCapture(const FAurixAudioCapture&) = delete;
	FAurixAudioCapture& operator=(const FAurixAudioCapture&) = delete;

	static TArray<FString> ListDevices();

	/** Open and start the stream for `DeviceIndex` (-1 = default). Returns false on failure. */
	bool Start(AurixClient* InClient, int32 DeviceIndex);
	void Stop();
	bool IsCapturing() const { return bCapturing; }
	int32 GetDeviceIndex() const { return CurrentDeviceIndex; }

private:
	Audio::FAudioCapture Capture;
	std::atomic<AurixClient*> Client{nullptr};
	bool bCapturing = false;
	int32 CurrentDeviceIndex = -1;
};
