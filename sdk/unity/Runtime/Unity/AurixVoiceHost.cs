#if UNITY_5_3_OR_NEWER
using UnityEngine;

namespace Aurix.Unity
{
    /// <summary>
    /// A scene component that owns an <see cref="IAurixVoiceClient"/> (<see cref="AurixVoiceBehaviour"/>,
    /// <see cref="AurixWebGLVoiceBehaviour"/>), so helper components (<see cref="AurixLipSync"/>,
    /// <see cref="AurixGameAudioDucker"/>) work with either. The client may be replaced on every
    /// <c>Connect()</c> — read it when needed rather than caching it.
    /// </summary>
    public interface IAurixVoiceHost
    {
        /// <summary>The live client, or null before connecting / after disconnecting.</summary>
        IAurixVoiceClient VoiceClient { get; }
    }

    internal static class AurixVoiceHost
    {
        /// <summary>
        /// The host a helper component should follow: the one assigned in the inspector, else one on the same
        /// GameObject, else the first voice component in the scene (native first, then WebGL). Null when none.
        /// </summary>
        internal static IAurixVoiceHost Resolve(MonoBehaviour assigned, Component self)
        {
            if (assigned is IAurixVoiceHost h) return h;
            var local = self != null ? self.GetComponent<IAurixVoiceHost>() : null;
            if (local != null) return local;
#if !(UNITY_WEBGL && !UNITY_EDITOR)
            var native = Object.FindObjectOfType<AurixVoiceBehaviour>();
            if (native != null) return native;
#endif
            return Object.FindObjectOfType<AurixWebGLVoiceBehaviour>();
        }
    }
}
#endif
