// Minimal UnityEngine surface used by Runtime/Unity/*.cs and Samples~/ — compile check only, never shipped.
using System;
using System.Collections;

namespace UnityEngine
{
    public class Object
    {
        public static implicit operator bool(Object o) => o != null;
        public static bool operator ==(Object a, Object b) => ReferenceEquals(a, b);
        public static bool operator !=(Object a, Object b) => !ReferenceEquals(a, b);
        public override bool Equals(object o) => ReferenceEquals(this, o);
        public override int GetHashCode() => base.GetHashCode();
        public static T FindObjectOfType<T>() where T : Object => default;
    }
    public class GameObject : Object { public T AddComponent<T>() where T : Component => default; }
    public class Component : Object
    {
        public GameObject gameObject => null;
        public T GetComponent<T>() where T : Component => default;
    }
    public class Behaviour : Component { }
    public class Coroutine { }
    public class MonoBehaviour : Behaviour { public Coroutine StartCoroutine(IEnumerator r) => null; }
    public class YieldInstruction { }
    public class AsyncOperation : YieldInstruction { }
    [AttributeUsage(AttributeTargets.All)] public class RequireComponent : Attribute { public RequireComponent(Type t) { } }
    [AttributeUsage(AttributeTargets.All)] public class TooltipAttribute : Attribute { public TooltipAttribute(string s) { } }
    [AttributeUsage(AttributeTargets.All)] public class HeaderAttribute : Attribute { public HeaderAttribute(string s) { } }
    [AttributeUsage(AttributeTargets.All)] public class RangeAttribute : Attribute { public RangeAttribute(float a, float b) { } }
    public class AudioClip : Object { public int samples; public int channels; public int frequency; public bool GetData(float[] d, int off) => true; }
    public class AudioSource : Behaviour { public AudioClip clip; public bool loop; public float spatialBlend; public bool isPlaying; public void Play() { } }
    public class AudioListener : Behaviour { }
    public static class Microphone
    {
        public static string[] devices => Array.Empty<string>();
        public static AudioClip Start(string d, bool loop, int len, int freq) => null;
        public static void End(string d) { }
        public static int GetPosition(string d) => 0;
        public static bool IsRecording(string d) => false;
        public static void GetDeviceCaps(string d, out int min, out int max) { min = 0; max = 0; }
    }
    public delegate void AudioConfigurationChangeHandler(bool deviceWasChanged);
    public static class AudioSettings { public static int outputSampleRate => 48000; public static event AudioConfigurationChangeHandler OnAudioConfigurationChanged; }
    public enum NetworkReachability { NotReachable, ReachableViaCarrierDataNetwork, ReachableViaLocalAreaNetwork }
    public enum UserAuthorization { WebCam = 1, Microphone = 2 }
    public static class Application
    {
        public static NetworkReachability internetReachability => NetworkReachability.ReachableViaLocalAreaNetwork;
        public static bool HasUserAuthorization(UserAuthorization m) => true;
        public static AsyncOperation RequestUserAuthorization(UserAuthorization m) => null;
    }
    public static class Debug { public static void LogWarning(object o) { } public static void Log(object o) { } }
    public static class Time { public static float unscaledTime; public static float unscaledDeltaTime; public static float realtimeSinceStartup; }
    public static class Mathf
    {
        public static int Clamp(int v, int min, int max) => Math.Min(Math.Max(v, min), max);
        public static int RoundToInt(float f) => (int)Math.Round(f);
    }
    public static class Screen { public static int width => 1280; public static int height => 720; }
    public enum KeyCode { None, Return, V }
    public static class Input { public static bool GetKey(KeyCode k) => false; }
    public struct Rect { public Rect(float x, float y, float w, float h) { } }
    public struct Vector2 { }
    public enum EventType { KeyDown }
    public class Event { public static Event current => new Event(); public EventType type; public KeyCode keyCode; }
    public class GUIStyle { public GUIStyle(GUIStyle other) { } public bool richText; }
    public class GUISkin { public GUIStyle box; public GUIStyle label; }
    public static class GUI { public static bool enabled; public static GUISkin skin => new GUISkin(); }
    public class GUILayoutOption { }
    public static class GUILayout
    {
        public static void BeginArea(Rect r, GUIStyle s) { }
        public static void EndArea() { }
        public static Vector2 BeginScrollView(Vector2 v) => v;
        public static void EndScrollView() { }
        public static void BeginHorizontal() { }
        public static void EndHorizontal() { }
        public static void Label(string s, params GUILayoutOption[] o) { }
        public static void Label(string s, GUIStyle st, params GUILayoutOption[] o) { }
        public static string TextField(string s, params GUILayoutOption[] o) => s;
        public static bool Button(string s, params GUILayoutOption[] o) => false;
        public static bool Toggle(bool v, string s, params GUILayoutOption[] o) => v;
        public static float HorizontalSlider(float v, float min, float max, params GUILayoutOption[] o) => v;
        public static void Space(float px) { }
        public static GUILayoutOption Width(float w) => new GUILayoutOption();
    }
}

namespace UnityEngine.Android
{
    public class PermissionCallbacks { public event Action<string> PermissionGranted, PermissionDenied, PermissionDeniedAndDontAskAgain; }
    public static class Permission
    {
        public const string Microphone = "android.permission.RECORD_AUDIO";
        public static bool HasUserAuthorizedPermission(string p) => true;
        public static void RequestUserPermission(string p, PermissionCallbacks cb) { }
    }
}
