// Records the microphone with FMOD's recording API and feeds it to Aurix.
//
// Use it when FMOD owns the input device (consoles, or projects that already use FMOD recording):
// Unity's Microphone class is unavailable or fights FMOD for the device. The component polls
// FMOD's record buffer every frame and pushes the new samples with AurixVoiceBehaviour.PushCapture();
// AEC/NS/AGC, voice effects, VAD and push-to-talk keep working unchanged.
#if AURIX_FMOD
using System;
using UnityEngine;
using Aurix.Unity;

namespace Aurix.Samples.Fmod
{
    [AddComponentMenu("Aurix/FMOD/Voice Capture (FMOD recording)")]
    [DisallowMultipleComponent]
    public sealed class AurixFmodCapture : MonoBehaviour
    {
        public AurixVoiceBehaviour Voice;
        [Tooltip("FMOD record driver index (0 = default device). See FMOD.System.getRecordDriverInfo.")]
        public int RecordDriver = 0;
        [Tooltip("Capture rate requested from FMOD. 48000 avoids a resampling step in Aurix.")]
        public int SampleRate = 48000;

        private FMOD.Sound _sound;
        private uint _readPos, _length;
        private float[] _scratch;
        private int _channels = 1;

        private void OnEnable()
        {
            if (Voice == null) Voice = AurixFind.First<AurixVoiceBehaviour>();
            if (Voice == null) { enabled = false; return; }
            Voice.ExternalCapture = true;
            var core = FMODUnity.RuntimeManager.CoreSystem;
            core.getRecordDriverInfo(RecordDriver, out _, 0, out _, out int rate, out _, out int channels, out _);
            if (rate <= 0) rate = SampleRate; if (channels <= 0) channels = 1;
            _channels = Math.Min(channels, 2);
            var ex = new FMOD.CREATESOUNDEXINFO
            {
                cbsize = System.Runtime.InteropServices.Marshal.SizeOf(typeof(FMOD.CREATESOUNDEXINFO)),
                numchannels = _channels, format = FMOD.SOUND_FORMAT.PCMFLOAT, defaultfrequency = SampleRate,
                length = (uint)(SampleRate * _channels * 4), // one second ring buffer
            };
            if (core.createSound("aurix-capture", FMOD.MODE.LOOP_NORMAL | FMOD.MODE.OPENUSER, ref ex, out _sound) != FMOD.RESULT.OK) { enabled = false; return; }
            _sound.getLength(out _length, FMOD.TIMEUNIT.PCM);
            core.recordStart(RecordDriver, _sound, true);
            _readPos = 0;
        }

        private void Update()
        {
            if (!_sound.hasHandle()) return;
            var core = FMODUnity.RuntimeManager.CoreSystem;
            core.getRecordPosition(RecordDriver, out uint pos);
            uint avail = (pos + _length - _readPos) % _length;
            if (avail == 0) return;
            int n = (int)avail * _channels;
            if (_scratch == null || _scratch.Length < n) _scratch = new float[n];
            // The ring may wrap: read in up to two pieces.
            uint first = Math.Min(avail, _length - _readPos);
            Read(_readPos, first, 0);
            if (first < avail) Read(0, avail - first, (int)first * _channels);
            _readPos = (_readPos + avail) % _length;
            Voice.PushCapture(_scratch, 0, n, SampleRate, _channels);
        }

        private void Read(uint fromFrame, uint frames, int into)
        {
            uint bytes = frames * (uint)_channels * 4;
            _sound.@lock(fromFrame * (uint)_channels * 4, bytes, out IntPtr p1, out IntPtr p2, out uint l1, out uint l2);
            System.Runtime.InteropServices.Marshal.Copy(p1, _scratch, into, (int)(l1 / 4));
            if (p2 != IntPtr.Zero && l2 > 0) System.Runtime.InteropServices.Marshal.Copy(p2, _scratch, into + (int)(l1 / 4), (int)(l2 / 4));
            _sound.unlock(p1, p2, l1, l2);
        }

        private void OnDisable()
        {
            var core = FMODUnity.RuntimeManager.CoreSystem;
            if (_sound.hasHandle()) { core.recordStop(RecordDriver); _sound.release(); _sound.clearHandle(); }
            if (Voice != null) Voice.ExternalCapture = false;
        }
    }
}
#endif
