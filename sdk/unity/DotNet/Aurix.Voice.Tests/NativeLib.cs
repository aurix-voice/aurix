using System;
using System.Collections.Generic;
using System.IO;
using System.Runtime.InteropServices;
using Aurix.Audio;

namespace Aurix.Voice.Tests
{
    /// <summary>
    /// Resolves the <c>aurix_client</c> native library (Opus + capture DSP entry points) from
    /// <c>AURIX_NATIVE_LIB</c> or the Cargo target dir when the tests run inside the repo. The
    /// resolver can be registered once per assembly, so every test class goes through here.
    /// </summary>
    internal static class NativeLib
    {
        private static readonly object Lock = new object();
        private static bool? _loaded;

        public static bool TryLoad()
        {
            lock (Lock)
            {
                if (_loaded.HasValue) return _loaded.Value;
                _loaded = Register();
                return _loaded.Value;
            }
        }

        private static bool Register()
        {
            var candidates = new List<string>();
            var env = Environment.GetEnvironmentVariable("AURIX_NATIVE_LIB");
            if (!string.IsNullOrEmpty(env)) candidates.Add(env);
            string name = RuntimeInformation.IsOSPlatform(OSPlatform.Windows) ? "aurix_client.dll"
                : RuntimeInformation.IsOSPlatform(OSPlatform.OSX) ? "libaurix_client.dylib" : "libaurix_client.so";
            for (var dir = new DirectoryInfo(AppContext.BaseDirectory); dir != null; dir = dir.Parent)
            {
                candidates.Add(Path.Combine(dir.FullName, "target", "debug", name));
                candidates.Add(Path.Combine(dir.FullName, "target", "release", name));
            }
            foreach (var c in candidates)
            {
                if (!File.Exists(c)) continue;
                var path = c;
                NativeLibrary.SetDllImportResolver(typeof(NativeOpusCodec).Assembly, (lib, asm, search) =>
                    lib == "aurix_client" ? NativeLibrary.Load(path) : IntPtr.Zero);
                return true;
            }
            return false;
        }
    }
}
