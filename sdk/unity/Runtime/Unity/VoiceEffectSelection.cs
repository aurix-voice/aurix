#if UNITY_5_3_OR_NEWER
using Aurix.Audio;

namespace Aurix.Unity
{
    /// <summary>Inspector choice of the microphone voice effect: off, a built-in preset, or hand-tuned parameters.</summary>
    public enum VoiceEffectSelection
    {
        None,
        Robot,
        Monster,
        Radio,
        Helium,
        Ghost,
        /// <summary>The component's <c>CustomVoiceEffect</c> parameters.</summary>
        Custom,
    }

    public static class VoiceEffectSelectionExtensions
    {
        /// <summary>The effect parameters a selection stands for (<see cref="VoiceEffectParams.Bypass"/> for <see cref="VoiceEffectSelection.None"/>).</summary>
        public static VoiceEffectParams Params(VoiceEffectSelection selection, VoiceEffectParams custom)
        {
            switch (selection)
            {
                case VoiceEffectSelection.Robot: return VoiceEffectParams.Preset(VoiceEffectPreset.Robot);
                case VoiceEffectSelection.Monster: return VoiceEffectParams.Preset(VoiceEffectPreset.Monster);
                case VoiceEffectSelection.Radio: return VoiceEffectParams.Preset(VoiceEffectPreset.Radio);
                case VoiceEffectSelection.Helium: return VoiceEffectParams.Preset(VoiceEffectPreset.Helium);
                case VoiceEffectSelection.Ghost: return VoiceEffectParams.Preset(VoiceEffectPreset.Ghost);
                case VoiceEffectSelection.Custom: return custom.Sanitized();
                default: return VoiceEffectParams.Bypass;
            }
        }
    }
}
#endif
