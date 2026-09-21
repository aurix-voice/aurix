#include "AurixVoiceBlueprintLibrary.h"

#include "AurixVoiceSubsystem.h"
#include "Engine/GameInstance.h"
#include "Kismet/GameplayStatics.h"

UAurixVoiceSubsystem* UAurixVoiceBlueprintLibrary::GetAurixVoice(const UObject* WorldContextObject)
{
	UGameInstance* GameInstance = UGameplayStatics::GetGameInstance(WorldContextObject);
	return GameInstance ? GameInstance->GetSubsystem<UAurixVoiceSubsystem>() : nullptr;
}

FAurixVoiceSettings UAurixVoiceBlueprintLibrary::MakeVoiceSettings(const FString& WebSocketUrl, const FString& Token)
{
	FAurixVoiceSettings Settings;
	Settings.WebSocketUrl = WebSocketUrl;
	Settings.Token = Token;
	return Settings;
}

FString UAurixVoiceBlueprintLibrary::ConnectionStateToText(EAurixConnectionState State)
{
	switch (State)
	{
	case EAurixConnectionState::Disconnected: return TEXT("Disconnected");
	case EAurixConnectionState::Connecting: return TEXT("Connecting");
	case EAurixConnectionState::Connected: return TEXT("Connected");
	case EAurixConnectionState::MediaBound: return TEXT("Voice active");
	case EAurixConnectionState::Reconnecting: return TEXT("Reconnecting");
	case EAurixConnectionState::Failed: return TEXT("Failed");
	}
	return TEXT("Unknown");
}

FString UAurixVoiceBlueprintLibrary::MediaPathToText(EAurixMediaPath Path)
{
	switch (Path)
	{
	case EAurixMediaPath::None: return TEXT("-");
	case EAurixMediaPath::Udp: return TEXT("UDP");
	case EAurixMediaPath::Tunnel: return TEXT("WebSocket tunnel");
	case EAurixMediaPath::Quic: return TEXT("QUIC");
	}
	return TEXT("Unknown");
}

FString UAurixVoiceBlueprintLibrary::QualityBarsToText(int32 Bars)
{
	if (Bars <= 0)
	{
		return TEXT("-");
	}
	static const TCHAR* const Glyphs[] = { TEXT("\u2581"), TEXT("\u2583"), TEXT("\u2585"), TEXT("\u2587"), TEXT("\u2588") };
	FString Out;
	const int32 Lit = FMath::Clamp(Bars, 1, 5);
	for (int32 i = 0; i < 5; ++i)
	{
		Out += i < Lit ? Glyphs[i] : TEXT("\u00B7");
	}
	return Out;
}

FString UAurixVoiceBlueprintLibrary::FormatMos(float Mos)
{
	return Mos > 0.f ? FString::Printf(TEXT("MOS %.1f"), Mos) : TEXT("MOS -");
}

FString UAurixVoiceBlueprintLibrary::GuidToUuid(FGuid Guid)
{
	return Guid.ToString(EGuidFormats::DigitsWithHyphens).ToLower();
}
