using UnrealBuildTool;

public class AurixVoice : ModuleRules
{
	public AurixVoice(ReadOnlyTargetRules Target) : base(Target)
	{
		PCHUsage = PCHUsageMode.UseExplicitOrSharedPCHs;
		CppStandard = CppStandardVersion.Cpp17;

		PublicDependencyModuleNames.AddRange(new string[]
		{
			"Core",
			"CoreUObject",
			"Engine",
		});

		PrivateDependencyModuleNames.AddRange(new string[]
		{
			"AudioCaptureCore",
			"AudioCapture",
			"HTTP",
			"Projects",
			"AurixClientLibrary",
		});
	}
}
