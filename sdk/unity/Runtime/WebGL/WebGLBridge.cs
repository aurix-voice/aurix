using System;
using System.Runtime.InteropServices;

namespace Aurix.WebGL
{
    /// <summary>Where the browser-side SDK stands (<see cref="IWebGLBridge.SdkStatus"/>).</summary>
    public enum WebGLSdkStatus
    {
        /// <summary><see cref="IWebGLBridge.LoadSdk"/> has not been called.</summary>
        NotLoaded,
        /// <summary>The script tag was injected and is still downloading / evaluating.</summary>
        Loading,
        /// <summary><c>window.AurixWebSdk</c> is available and a bridge instance exists.</summary>
        Ready,
        /// <summary>The script failed to load or did not define <c>AurixWebSdk</c>; see <see cref="IWebGLBridge.SdkError"/>.</summary>
        Failed,
    }

    /// <summary>
    /// The thin JavaScript boundary <see cref="AurixWebGLVoiceClient"/> talks through: one Web SDK
    /// <c>AurixBridge</c> in the page, integer handles per client, JSON strings both ways. Implemented by
    /// <see cref="NativeWebGLBridge"/> (the <c>AurixWebGL.jslib</c> plugin in WebGL players) and by test doubles.
    /// </summary>
    public interface IWebGLBridge
    {
        /// <summary>
        /// Start loading the standalone Web SDK bundle (<c>aurix-web-sdk.js</c>) from <paramref name="url"/>
        /// unless it is already present on the page. Idempotent; the result is reported by <see cref="SdkStatus"/>.
        /// </summary>
        void LoadSdk(string url);
        WebGLSdkStatus SdkStatus { get; }
        string SdkError { get; }

        /// <summary>Create a browser client from <c>BridgeClientOptions</c> JSON; returns its handle (&gt; 0).</summary>
        int Create(string optionsJson);
        /// <summary>
        /// Call a client method. Returns <c>{"ok":true,"value":…}</c>, <c>{"ok":true,"pending":true}</c> (the
        /// result arrives later as a <c>result</c> event carrying <paramref name="rid"/>) or <c>{"ok":false,"error":{…}}</c>.
        /// </summary>
        string Invoke(int handle, string method, string argsJson, int rid);
        /// <summary>Take every queued event of the client as a JSON array (oldest first); <c>[]</c> when idle.</summary>
        string Drain(int handle);
        /// <summary>Disconnect the browser client and free its handle.</summary>
        void Destroy(int handle);
    }

    /// <summary>
    /// <see cref="IWebGLBridge"/> over the <c>AurixWebGL.jslib</c> plugin (Unity WebGL players only). On any
    /// other platform every call throws <see cref="PlatformNotSupportedException"/>.
    /// </summary>
    public sealed class NativeWebGLBridge : IWebGLBridge
    {
        public static readonly NativeWebGLBridge Instance = new NativeWebGLBridge();

#if UNITY_WEBGL && !UNITY_EDITOR
        [DllImport("__Internal")] private static extern void AurixWebGL_LoadSdk(string url);
        [DllImport("__Internal")] private static extern int AurixWebGL_SdkStatus();
        [DllImport("__Internal")] private static extern string AurixWebGL_SdkError();
        [DllImport("__Internal")] private static extern int AurixWebGL_Create(string optionsJson);
        [DllImport("__Internal")] private static extern string AurixWebGL_Invoke(int handle, string method, string argsJson, int rid);
        [DllImport("__Internal")] private static extern string AurixWebGL_Drain(int handle);
        [DllImport("__Internal")] private static extern void AurixWebGL_Destroy(int handle);

        public void LoadSdk(string url) => AurixWebGL_LoadSdk(url ?? string.Empty);
        public WebGLSdkStatus SdkStatus => (WebGLSdkStatus)AurixWebGL_SdkStatus();
        public string SdkError => AurixWebGL_SdkError();
        public int Create(string optionsJson) => AurixWebGL_Create(optionsJson);
        public string Invoke(int handle, string method, string argsJson, int rid) => AurixWebGL_Invoke(handle, method, argsJson, rid);
        public string Drain(int handle) => AurixWebGL_Drain(handle);
        public void Destroy(int handle) => AurixWebGL_Destroy(handle);

        public static bool IsSupported => true;
#else
        private static PlatformNotSupportedException Unsupported() =>
            new PlatformNotSupportedException("AurixWebGL.jslib is only linked into Unity WebGL players; use AurixVoiceClient (native core) elsewhere");

        public void LoadSdk(string url) => throw Unsupported();
        public WebGLSdkStatus SdkStatus => throw Unsupported();
        public string SdkError => throw Unsupported();
        public int Create(string optionsJson) => throw Unsupported();
        public string Invoke(int handle, string method, string argsJson, int rid) => throw Unsupported();
        public string Drain(int handle) => throw Unsupported();
        public void Destroy(int handle) => throw Unsupported();

        public static bool IsSupported => false;
#endif

        private NativeWebGLBridge() { }
    }
}
