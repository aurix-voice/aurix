#if UNITY_EDITOR
using System;
using System.IO;
using System.Text;
using UnityEditor;
using UnityEngine;

namespace Aurix.Editor
{
    /// <summary>
    /// Editor helpers under the <c>Aurix Voice</c> menu: a project-setup check for the current build target and a
    /// one-click copy of the Web SDK bundle into <c>Assets/StreamingAssets</c> for WebGL players.
    /// </summary>
    public static class AurixVoiceMenu
    {
        public const string WebSdkFileName = "aurix-web-sdk.js";
        public const string DocsUrl = "https://github.com/aurix-voice/aurix/tree/main/sdk/unity#readme";

        [MenuItem("Aurix Voice/Check project setup", false, 1)]
        public static void CheckProjectSetup()
        {
            var report = new StringBuilder();
            int problems = Check(EditorUserBuildSettings.activeBuildTarget, report);
            if (problems == 0) Debug.Log("Aurix Voice: project setup looks good for " + EditorUserBuildSettings.activeBuildTarget + "\n" + report);
            else Debug.LogWarning("Aurix Voice: " + problems + " thing(s) to look at for " + EditorUserBuildSettings.activeBuildTarget + "\n" + report);
        }

        /// <summary>Runs the checks for <paramref name="target"/>, appends one line per check and returns the number of problems.</summary>
        public static int Check(BuildTarget target, StringBuilder report)
        {
            int problems = 0;
            var audio = AudioSettings.GetConfiguration();
            if (audio.sampleRate == 48000) report.AppendLine("  ok    audio: 48 kHz output (no resampling of voice playback)");
            else report.AppendLine("  info  audio: output " + audio.sampleRate + " Hz — voice is 48 kHz, playback is resampled (Project Settings ▸ Audio ▸ System Sample Rate)");

            if (target == BuildTarget.WebGL)
            {
                string bundle = Path.Combine(Application.streamingAssetsPath, WebSdkFileName);
                bool template = PlayerSettings.WebGL.template.StartsWith("PROJECT:", StringComparison.Ordinal);
                if (File.Exists(bundle)) report.AppendLine("  ok    WebGL: " + WebSdkFileName + " in StreamingAssets");
                else if (template) report.AppendLine("  info  WebGL: no StreamingAssets/" + WebSdkFileName + " — a project template is selected (" + PlayerSettings.WebGL.template + "); make sure it loads the Web SDK");
                else { problems++; report.AppendLine("  FIX   WebGL: StreamingAssets/" + WebSdkFileName + " is missing (Aurix Voice ▸ Copy Web SDK bundle…) or select a template that loads it"); }
                if (PlayerSettings.WebGL.compressionFormat != WebGLCompressionFormat.Disabled && !PlayerSettings.WebGL.decompressionFallback)
                    report.AppendLine("  info  WebGL: compressed build without decompression fallback — the web server must send Content-Encoding for .br/.gz");
                report.AppendLine("  info  WebGL: voice uses browser WebRTC (AurixWebGLVoiceBehaviour); native AurixVoiceBehaviour is not available in WebGL players");
            }
            else if (target == BuildTarget.iOS)
            {
                if (string.IsNullOrEmpty(PlayerSettings.iOS.microphoneUsageDescription))
                {
                    problems++;
                    report.AppendLine("  FIX   iOS: Player ▸ Other Settings ▸ Microphone Usage Description is empty — iOS rejects microphone access without it");
                }
                else report.AppendLine("  ok    iOS: microphone usage description set");
            }
            else if (target == BuildTarget.Android)
            {
                report.AppendLine("  ok    Android: RECORD_AUDIO is added by Unity when Microphone is used; the behaviour requests it at runtime");
            }
            else
            {
                report.AppendLine("  ok    " + target + ": native AURX media (QUIC/UDP with WebSocket tunnel fallback)");
            }
            return problems;
        }

        [MenuItem("Aurix Voice/Copy Web SDK bundle to StreamingAssets…", false, 2)]
        public static void CopyWebSdkBundle()
        {
            string source = EditorUtility.OpenFilePanel("Select " + WebSdkFileName + " (sdk/web/dist)", "", "js");
            if (string.IsNullOrEmpty(source)) return;
            try
            {
                string message = CopyWebSdkBundle(source);
                AssetDatabase.Refresh();
                Debug.Log("Aurix Voice: " + message);
            }
            catch (Exception e)
            {
                EditorUtility.DisplayDialog("Aurix Voice", e.Message, "OK");
            }
        }

        /// <summary>Copies <paramref name="source"/> to <c>Assets/StreamingAssets/aurix-web-sdk.js</c> after a sanity check of its contents.</summary>
        public static string CopyWebSdkBundle(string source)
        {
            if (!File.Exists(source)) throw new FileNotFoundException("no such file: " + source);
            string head = ReadHead(source, 64 * 1024);
            if (!head.Contains("AurixWebSdk") && !File.ReadAllText(source).Contains("AurixWebSdk"))
                throw new InvalidDataException(Path.GetFileName(source) + " does not look like the Aurix Web SDK browser bundle (no AurixWebSdk global)");
            Directory.CreateDirectory(Application.streamingAssetsPath);
            string target = Path.Combine(Application.streamingAssetsPath, WebSdkFileName);
            File.Copy(source, target, true);
            return "copied " + Path.GetFileName(source) + " to Assets/StreamingAssets/" + WebSdkFileName + " (" + new FileInfo(target).Length / 1024 + " KiB)";
        }

        [MenuItem("Aurix Voice/Documentation", false, 20)]
        public static void OpenDocumentation() => Application.OpenURL(DocsUrl);

        private static string ReadHead(string path, int bytes)
        {
            using (var stream = File.OpenRead(path))
            {
                var buffer = new byte[Math.Min(bytes, (int)Math.Min(stream.Length, int.MaxValue))];
                int read = stream.Read(buffer, 0, buffer.Length);
                return Encoding.UTF8.GetString(buffer, 0, read);
            }
        }
    }
}
#endif
