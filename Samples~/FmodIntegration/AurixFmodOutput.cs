// Plays the Aurix remote voice mix through FMOD Studio instead of Unity's audio system.
//
// FMOD for Unity disables Unity's AudioListener / AudioSource pipeline, so AurixVoiceBehaviour's own
// AudioSource (OnAudioFilterRead) never runs. This component creates a custom FMOD DSP on a channel
// group of your choice (master by default, or a bus such as "bus:/Voice") whose read callback pulls the
// mix with AurixVoiceBehaviour.MixOutput() at FMOD's own sample rate — no resampling on your side.
//
// Define AURIX_FMOD in Project Settings → Player → Scripting Define Symbols once the FMOD for Unity
// package is in the project; without it this file compiles to nothing.
#if AURIX_FMOD
using System;
using System.Runtime.InteropServices;
using UnityEngine;
using Aurix.Unity;

namespace Aurix.Samples.Fmod
{
    [AddComponentMenu("Aurix/FMOD/Voice Output (FMOD DSP)")]
    [DisallowMultipleComponent]
    public sealed class AurixFmodOutput : MonoBehaviour
    {
        [Tooltip("Voice component whose mix to play. Found automatically when empty.")]
        public AurixVoiceBehaviour Voice;
        [Tooltip("FMOD Studio bus the voice plays on, e.g. \"bus:/Voice\". Empty = master channel group.")]
        public string Bus = "";
        [Tooltip("Also hand FMOD's rendered game audio to the echo canceller (needs a second DSP on the master group).")]
        public bool FeedEchoCancellerFromMaster = true;

        private FMOD.DSP _voiceDsp, _renderDsp;
        private FMOD.ChannelGroup _voiceGroup, _masterGroup;
        private GCHandle _self;
        private int _rate;
        private float[] _scratch;
        private static FMOD.DSP_READ_CALLBACK s_voiceRead = VoiceRead, s_renderRead = RenderRead;

        private void OnEnable()
        {
            if (Voice == null) Voice = AurixFind.First<AurixVoiceBehaviour>();
            if (Voice == null) { Debug.LogWarning("AurixFmodOutput: no AurixVoiceBehaviour in the scene"); enabled = false; return; }
            // The component's own AudioSource must not play the same mix a second time.
            var src = Voice.GetComponent<AudioSource>(); if (src != null) src.enabled = false;
            Voice.RenderFedExternally = FeedEchoCancellerFromMaster;

            var core = FMODUnity.RuntimeManager.CoreSystem;
            core.getSoftwareFormat(out _rate, out _, out _);
            core.getMasterChannelGroup(out _masterGroup);
            _voiceGroup = _masterGroup;
            if (!string.IsNullOrEmpty(Bus))
            {
                var bus = FMODUnity.RuntimeManager.GetBus(Bus);
                bus.lockChannelGroup(); FMODUnity.RuntimeManager.StudioSystem.flushCommands();
                if (bus.getChannelGroup(out var g) == FMOD.RESULT.OK) _voiceGroup = g;
            }
            _self = GCHandle.Alloc(this);
            _voiceDsp = CreateDsp(core, "Aurix voice", s_voiceRead);
            _voiceGroup.addDSP(FMOD.CHANNELCONTROL_DSP_INDEX.TAIL, _voiceDsp);
            if (FeedEchoCancellerFromMaster)
            {
                _renderDsp = CreateDsp(core, "Aurix AEC reference", s_renderRead);
                _masterGroup.addDSP(FMOD.CHANNELCONTROL_DSP_INDEX.TAIL, _renderDsp);
            }
        }

        private FMOD.DSP CreateDsp(FMOD.System core, string name, FMOD.DSP_READ_CALLBACK read)
        {
            var desc = new FMOD.DSP_DESCRIPTION
            {
                name = System.Text.Encoding.UTF8.GetBytes(name.PadRight(32, '\0')),
                version = 0x00010000,
                numinputbuffers = 1,
                numoutputbuffers = 1,
                read = read,
                userdata = GCHandle.ToIntPtr(_self),
            };
            core.createDSP(ref desc, out var dsp);
            dsp.setUserData(GCHandle.ToIntPtr(_self));
            return dsp;
        }

        private void OnDisable()
        {
            if (_voiceDsp.hasHandle()) { _voiceGroup.removeDSP(_voiceDsp); _voiceDsp.release(); _voiceDsp.clearHandle(); }
            if (_renderDsp.hasHandle()) { _masterGroup.removeDSP(_renderDsp); _renderDsp.release(); _renderDsp.clearHandle(); }
            if (!string.IsNullOrEmpty(Bus)) FMODUnity.RuntimeManager.GetBus(Bus).unlockChannelGroup();
            if (_self.IsAllocated) _self.Free();
            if (Voice != null) { Voice.RenderFedExternally = false; var src = Voice.GetComponent<AudioSource>(); if (src != null) src.enabled = true; }
        }

        private static AurixFmodOutput From(IntPtr dsp)
        {
            var d = new FMOD.DSP(dsp); d.getUserData(out var ud);
            return ud == IntPtr.Zero ? null : GCHandle.FromIntPtr(ud).Target as AurixFmodOutput;
        }

        // FMOD mixer thread: copy the input through, then add the remote voice mix on top.
        [AOT.MonoPInvokeCallback(typeof(FMOD.DSP_READ_CALLBACK))]
        private static FMOD.RESULT VoiceRead(ref FMOD.DSP_STATE state, IntPtr inbuffer, IntPtr outbuffer, uint length, int inchannels, ref int outchannels)
        {
            var self = From(state.instance);
            int ch = outchannels, n = (int)length * ch;
            if (inbuffer != IntPtr.Zero && inchannels == ch) CopyMemory(inbuffer, outbuffer, n);
            else Zero(outbuffer, n);
            if (self == null || self.Voice == null) return FMOD.RESULT.OK;
            if (self._scratch == null || self._scratch.Length < n) self._scratch = new float[n];
            var s = self._scratch;
            Array.Clear(s, 0, n);
            if (self.Voice.MixOutput(s, Math.Min(ch, 2), self._rate))
            {
                unsafe
                {
                    float* o = (float*)outbuffer;
                    if (ch <= 2) for (int i = 0; i < n; i++) o[i] += s[i];
                    else for (int f = 0; f < length; f++) { o[f * ch] += s[f * 2]; o[f * ch + 1] += s[f * 2 + 1]; } // voice on L/R of a surround bus
                }
            }
            return FMOD.RESULT.OK;
        }

        // Master group tail: what the player actually hears → AEC reference. Pass-through.
        [AOT.MonoPInvokeCallback(typeof(FMOD.DSP_READ_CALLBACK))]
        private static FMOD.RESULT RenderRead(ref FMOD.DSP_STATE state, IntPtr inbuffer, IntPtr outbuffer, uint length, int inchannels, ref int outchannels)
        {
            var self = From(state.instance);
            int ch = outchannels, n = (int)length * ch;
            if (inbuffer != IntPtr.Zero && inchannels == ch) CopyMemory(inbuffer, outbuffer, n); else Zero(outbuffer, n);
            if (self == null || self.Voice == null) return FMOD.RESULT.OK;
            int rc = Math.Min(ch, 2), m = (int)length * rc;
            if (self._renderScratch == null || self._renderScratch.Length < m) self._renderScratch = new float[m];
            unsafe { float* o = (float*)outbuffer; for (int f = 0; f < length; f++) for (int c = 0; c < rc; c++) self._renderScratch[f * rc + c] = o[f * ch + c]; }
            self.Voice.PushRender(self._renderScratch, rc, self._rate);
            return FMOD.RESULT.OK;
        }
        private float[] _renderScratch;

        private static unsafe void CopyMemory(IntPtr from, IntPtr to, int floats) { Buffer.MemoryCopy((void*)from, (void*)to, floats * 4L, floats * 4L); }
        private static unsafe void Zero(IntPtr p, int floats) { float* o = (float*)p; for (int i = 0; i < floats; i++) o[i] = 0f; }
    }
}
#endif
