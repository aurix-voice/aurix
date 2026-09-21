#pragma once

#include "CoreMinimal.h"
#include "Kismet/BlueprintFunctionLibrary.h"
#include "AurixVoiceTypes.h"
#include "AurixVoiceBlueprintLibrary.generated.h"

class UAurixVoiceSubsystem;

/** Small Blueprint helpers around UAurixVoiceSubsystem: lookup, settings, HUD text. */
UCLASS()
class AURIXVOICESAMPLES_API UAurixVoiceBlueprintLibrary : public UBlueprintFunctionLibrary
{
	GENERATED_BODY()

public:
	/** The game instance's voice subsystem (nullptr without a game instance, e.g. in the editor world). */
	UFUNCTION(BlueprintPure, Category = "Aurix Voice|Samples", meta = (WorldContext = "WorldContextObject"))
	static UAurixVoiceSubsystem* GetAurixVoice(const UObject* WorldContextObject);

	/**
	 * Default settings for a node URL and a token minted by the game backend (the token is never
	 * an editable asset property — pass it at runtime).
	 */
	UFUNCTION(BlueprintPure, Category = "Aurix Voice|Samples")
	static FAurixVoiceSettings MakeVoiceSettings(const FString& WebSocketUrl, const FString& Token);

	/** "Disconnected" / "Connecting" / … for a status line. */
	UFUNCTION(BlueprintPure, Category = "Aurix Voice|Samples")
	static FString ConnectionStateToText(EAurixConnectionState State);

	/** "QUIC" / "UDP" / "WebSocket tunnel" / "-". */
	UFUNCTION(BlueprintPure, Category = "Aurix Voice|Samples")
	static FString MediaPathToText(EAurixMediaPath Path);

	/** Bars 1..5 as "▂▄▆█" style glyphs (0 = "-"), for a HUD widget without textures. */
	UFUNCTION(BlueprintPure, Category = "Aurix Voice|Samples")
	static FString QualityBarsToText(int32 Bars);

	/** "MOS 4.1" or "MOS -" when no report has arrived yet. */
	UFUNCTION(BlueprintPure, Category = "Aurix Voice|Samples")
	static FString FormatMos(float Mos);

	/** Canonical lower-case UUID text of a GUID (what the REST API and tokens use). */
	UFUNCTION(BlueprintPure, Category = "Aurix Voice|Samples")
	static FString GuidToUuid(FGuid Guid);
};
