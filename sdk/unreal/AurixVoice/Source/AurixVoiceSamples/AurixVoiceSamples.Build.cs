using UnrealBuildTool;

// Blueprint-ready sample components on top of the public AurixVoice API. Optional: a game can
// delete this module (and its entry in AurixVoice.uplugin) and drive UAurixVoiceSubsystem itself.
public class AurixVoiceSamples : ModuleRules
{
	public AurixVoiceSamples(ReadOnlyTargetRules Target) : base(Target)
	{
		PCHUsage = PCHUsageMode.UseExplicitOrSharedPCHs;
		CppStandard = CppStandardVersion.Cpp17;

		PublicDependencyModuleNames.AddRange(new string[]
		{
			"Core",
			"CoreUObject",
			"Engine",
			"AurixVoice",
		});
	}
}
