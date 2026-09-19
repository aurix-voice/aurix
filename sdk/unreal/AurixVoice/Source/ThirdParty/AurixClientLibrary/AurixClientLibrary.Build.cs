// Prebuilt `aurix-client` native library (Rust, C ABI). Populate `include/` and `lib/<Platform>/`
// with `sdk/unreal/scripts/build_native.sh` (or `.ps1`) from the Aurix repository, or drop in the
// release artifacts. Layout:
//
//   include/aurix_client.h, aurix_client.hpp
//   lib/Win64/aurix_client.dll, aurix_client.dll.lib
//   lib/Linux/libaurix_client.so
//   lib/Mac/libaurix_client.dylib

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
		else
		{
			throw new BuildException("AurixClientLibrary: no prebuilt aurix_client for " + Target.Platform);
		}
	}
}
