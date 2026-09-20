#pragma once

#include "CoreMinimal.h"

#include <string>

#include "aurix_client.hpp"

inline FGuid ToGuid(const AurixUuid& U)
{
	auto Read = [&U](int32 Offset) {
		return (uint32(U.bytes[Offset]) << 24) | (uint32(U.bytes[Offset + 1]) << 16) | (uint32(U.bytes[Offset + 2]) << 8) | uint32(U.bytes[Offset + 3]);
	};
	return FGuid(Read(0), Read(4), Read(8), Read(12));
}

inline aurix::Uuid ToUuid(const FGuid& G)
{
	aurix::Uuid U;
	const uint32 Parts[4] = {G.A, G.B, G.C, G.D};
	for (int32 i = 0; i < 4; ++i)
	{
		U.raw.bytes[i * 4 + 0] = uint8(Parts[i] >> 24);
		U.raw.bytes[i * 4 + 1] = uint8(Parts[i] >> 16);
		U.raw.bytes[i * 4 + 2] = uint8(Parts[i] >> 8);
		U.raw.bytes[i * 4 + 3] = uint8(Parts[i]);
	}
	return U;
}

inline FString FromUtf8(const char* S)
{
	return S ? FString(UTF8_TO_TCHAR(S)) : FString();
}

inline std::string ToUtf8(const FString& S)
{
	FTCHARToUTF8 Conv(*S);
	return std::string(reinterpret_cast<const char*>(Conv.Get()), static_cast<size_t>(Conv.Length()));
}

/// Decodes exactly `Size` bytes of hex (`2 * Size` digits, either case); false on any other input.
inline bool DecodeHex(const FString& Hex, uint8* Out, int32 Size)
{
	if (Hex.Len() != Size * 2)
	{
		return false;
	}
	for (int32 i = 0; i < Size; ++i)
	{
		const TCHAR Hi = Hex[i * 2];
		const TCHAR Lo = Hex[i * 2 + 1];
		if (!FChar::IsHexDigit(Hi) || !FChar::IsHexDigit(Lo))
		{
			return false;
		}
		Out[i] = static_cast<uint8>((FParse::HexDigit(Hi) << 4) | FParse::HexDigit(Lo));
	}
	return true;
}
