#pragma once

#include "CoreMinimal.h"
#include "AudioCaptureCore.h"

#include <atomic>

struct AurixClient;

/**
 * PCM clip mixed into (or replacing) the microphone before encoding: soundboards, voice lines,
 * microphone tests. Thread-safe; `Mix` / `Pull` run on the capture thread or the game thread.
 */
class FAurixAudioInjector
{
public:
	void Start(TArray<float> InPcm, int32 InSampleRate, int32 InChannels, bool bInLoop, float InGain, bool bInMixWithMicrophone);
	void Stop();
	bool IsActive() const { return bActive.load(); }
	/** Non-looping clip reached its end (cleared by Stop/Start). */
	bool IsFinished() const { return bFinished.load(); }
	bool MixesWithMicrophone() const { return bMixWithMicrophone; }
	/** Add the next `NumFrames` frames (resampled to `OutRate`) into `InOut`; replaces it when not mixing. */
	void Mix(float* InOut, int32 NumFrames, int32 NumChannels, int32 OutRate);
	/** Fill `Out` with the next `NumFrames` frames of `NumChannels` at `OutRate` (silence after the end). */
	void Pull(TArray<float>& Out, int32 NumFrames, int32 NumChannels, int32 OutRate);

private:
	float Sample(int32 Channel) const;
	void Advance(int32 OutRate);

	FCriticalSection Lock;
	TArray<float> Pcm;
	int32 SampleRate = 48000;
	int32 Channels = 1;
	bool bLoop = false;
	float Gain = 1.f;
	bool bMixWithMicrophone = true;
	double Position = 0.0;
	std::atomic<bool> bActive{false};
	std::atomic<bool> bFinished{false};
};

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
	/** Clip mixed into the microphone (see InjectAudio on the subsystem). */
	FAurixAudioInjector Injector;
	/** Name of the device opened by Start() (empty for the system default). */
	const FString& GetDeviceName() const { return CurrentDeviceName; }

private:
	Audio::FAudioCapture Capture;
	std::atomic<AurixClient*> Client{nullptr};
	bool bCapturing = false;
	int32 CurrentDeviceIndex = -1;
	FString CurrentDeviceName;
	TArray<float> InjectScratch;
};
