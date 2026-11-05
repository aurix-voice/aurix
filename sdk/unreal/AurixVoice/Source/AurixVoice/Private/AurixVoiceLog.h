#pragma once

#include "CoreMinimal.h"

DECLARE_LOG_CATEGORY_EXTERN(LogAurixVoice, Log, All);

#include "Runtime/Launch/Resources/Version.h"

// TArray::SetNum*(Count, bool bAllowShrinking) is deprecated from UE 5.4 in favour of EAllowShrinking.
#if ENGINE_MAJOR_VERSION > 5 || (ENGINE_MAJOR_VERSION == 5 && ENGINE_MINOR_VERSION >= 4)
#define AURIX_NO_SHRINK EAllowShrinking::No
#else
#define AURIX_NO_SHRINK false
#endif
