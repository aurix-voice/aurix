// Prebuilt `aurix-client` native library (Rust, C ABI). Populate `include/` and `lib/<Platform>/`
// with `sdk/unreal/scripts/build_native.sh` (or `.ps1`) from the Aurix repository, or drop in the
// release artifacts. Layout:
//
//   include/aurix_client.h, aurix_client.hpp
//   lib/Win64/aurix_client.dll, aurix_client.dll.lib
//   lib/Linux/libaurix_client.so
//   lib/Mac/libaurix_client.dylib
//   lib/Android/<arm64-v8a|armeabi-v7a|x86_64>/libaurix_client.so   (+ AurixClientLibrary_APL.xml)

using System.IO;
using UnrealBuildTool;

public class AurixClientLibrary : ModuleRules
{
	public AurixClientLibrary(ReadOnlyTargetRules Target) : base(Target)
	{
		Type = ModuleType.External;

		string IncludeDir = Path.Combine(ModuleDirectory, "include");
		string LibDir = Path.Combine(ModuleDirectory, "lib");
		PublicSystemIncludePaths.Add(IncludeDir);

		if (!File.Exists(Path.Combine(IncludeDir, "aurix_client.h")))
		{
			throw new BuildException(
				"AurixClientLibrary: include/aurix_client.h is missing. Run sdk/unreal/scripts/build_native.sh " +
				"(or .ps1) from the Aurix repository, or copy the release artifacts into " + ModuleDirectory);
		}

		if (Target.Platform == UnrealTargetPlatform.Win64)
		{
			string Dir = Path.Combine(LibDir, "Win64");
			PublicAdditionalLibraries.Add(Path.Combine(Dir, "aurix_client.dll.lib"));
			PublicDelayLoadDLLs.Add("aurix_client.dll");
			RuntimeDependencies.Add("$(BinaryOutputDir)/aurix_client.dll", Path.Combine(Dir, "aurix_client.dll"));
		}
		else if (Target.Platform == UnrealTargetPlatform.Linux)
		{
			string Lib = Path.Combine(LibDir, "Linux", "libaurix_client.so");
			PublicAdditionalLibraries.Add(Lib);
			RuntimeDependencies.Add("$(BinaryOutputDir)/libaurix_client.so", Lib);
		}
		else if (Target.Platform == UnrealTargetPlatform.Mac)
		{
			string Lib = Path.Combine(LibDir, "Mac", "libaurix_client.dylib");
			PublicAdditionalLibraries.Add(Lib);
			RuntimeDependencies.Add("$(BinaryOutputDir)/libaurix_client.dylib", Lib);
		}
		else if (Target.Platform == UnrealTargetPlatform.Android)
		{
			// One .so per ABI; the APL copies them into the APK and loads the right one at startup.
			string Dir = Path.Combine(LibDir, "Android");
			string[] Abis = { "arm64-v8a", "armeabi-v7a", "x86_64" };
			bool Any = false;
			foreach (string Abi in Abis)
			{
				string Lib = Path.Combine(Dir, Abi, "libaurix_client.so");
				if (File.Exists(Lib))
				{
					PublicAdditionalLibraries.Add(Lib);
					Any = true;
				}
			}
			if (!Any)
			{
				throw new BuildException("AurixClientLibrary: no prebuilt aurix_client for Android under " + Dir +
					" (expected <abi>/libaurix_client.so; run sdk/unreal/scripts/build_native.sh --target aarch64-linux-android)");
			}
			AdditionalPropertiesForReceipt.Add("AndroidPlugin", Path.Combine(ModuleDirectory, "AurixClientLibrary_APL.xml"));
		}
		else
		{
			throw new BuildException("AurixClientLibrary: no prebuilt aurix_client for " + Target.Platform);
		}
	}
}
