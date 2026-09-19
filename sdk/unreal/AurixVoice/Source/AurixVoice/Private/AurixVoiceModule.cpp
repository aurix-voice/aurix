#include "AurixVoiceLog.h"
#include "Interfaces/IPluginManager.h"
#include "Misc/Paths.h"
#include "Modules/ModuleManager.h"
#include "HAL/PlatformProcess.h"

#include "aurix_client.h"

DEFINE_LOG_CATEGORY(LogAurixVoice);

class FAurixVoiceModule : public IModuleInterface
{
public:
	virtual void StartupModule() override
	{
#if PLATFORM_WINDOWS
		// The import library delay-loads aurix_client.dll; pin it from the plugin so the loader
		// finds it even when the editor's working directory is elsewhere.
		TSharedPtr<IPlugin> Plugin = IPluginManager::Get().FindPlugin(TEXT("AurixVoice"));
		if (Plugin.IsValid())
		{
			const FString BaseDir = Plugin->GetBaseDir();
			const FString Candidates[] = {
				FPaths::Combine(BaseDir, TEXT("Binaries/Win64/aurix_client.dll")),
				FPaths::Combine(BaseDir, TEXT("Source/ThirdParty/AurixClientLibrary/lib/Win64/aurix_client.dll")),
			};
			for (const FString& Path : Candidates)
			{
				if (FPaths::FileExists(Path))
				{
					DllHandle = FPlatformProcess::GetDllHandle(*Path);
					if (DllHandle)
					{
						break;
					}
				}
			}
			if (!DllHandle)
			{
				UE_LOG(LogAurixVoice, Error, TEXT("aurix_client.dll not found under %s; run sdk/unreal/scripts/build_native.ps1"), *BaseDir);
				return;
			}
		}
#endif
		UE_LOG(LogAurixVoice, Log, TEXT("Aurix native client %s"), UTF8_TO_TCHAR(aurix_version()));
	}

	virtual void ShutdownModule() override
	{
#if PLATFORM_WINDOWS
		if (DllHandle)
		{
			FPlatformProcess::FreeDllHandle(DllHandle);
			DllHandle = nullptr;
		}
#endif
	}

private:
	void* DllHandle = nullptr;
};

IMPLEMENT_MODULE(FAurixVoiceModule, AurixVoice)
