// One FMOD 3D event per remote speaker: positional voice spatialised by FMOD (attenuation, occlusion,
// reverb zones, HRTF plugins) instead of Unity's AudioSource.
//
// Author a Studio event with a single-instrument "Programmer" sound and a 3D spatializer on its track
// (e.g. "event:/Voice/Speaker"). Attach this component to the speaker's avatar, set UserId (or Bind()),
// and the programmer-sound callback streams that participant through AurixVoiceBehaviour.PullParticipant().
// Set Voice.Playback = PerParticipant so the aggregate mix does not also play them.
#if AURIX_FMOD
using System;
using System.Runtime.InteropServices;
using UnityEngine;
using Aurix.Unity;

namespace Aurix.Samples.Fmod
{
    [AddComponentMenu("Aurix/FMOD/Participant Voice (FMOD 3D event)")]
    public sealed class AurixFmodParticipantVoice : MonoBehaviour
    {
        public AurixVoiceBehaviour Voice;
        [Tooltip("Participant to play (UUID from the roster). Empty until Bind() is called.")]
        public string UserId;
        [Tooltip("Studio event with a Programmer instrument and 3D panning.")]
        public FMODUnity.EventReference Event;

        public Guid BoundUserId { get; private set; }
        private FMOD.Studio.EventInstance _instance;
        private FMOD.Sound _sound;
        private GCHandle _self;
        private int _rate;
        private float[] _scratch;
        private static FMOD.SOUND_PCMREAD_CALLBACK s_read = PcmRead;
        private static FMOD.Studio.EVENT_CALLBACK s_event = EventCallback;

        public void Bind(Guid userId) { Release(); BoundUserId = userId; UserId = userId == Guid.Empty ? null : userId.ToString(); if (isActiveAndEnabled) Start3D(); }

        private void OnEnable()
        {
            if (Voice == null) Voice = AurixFind.First<AurixVoiceBehaviour>();
            if (BoundUserId == Guid.Empty && Guid.TryParse(UserId ?? "", out var g)) BoundUserId = g;
            Start3D();
        }
        private void OnDisable() => Release();
        private void Update() { if (_instance.isValid()) _instance.set3DAttributes(FMODUnity.RuntimeUtils.To3DAttributes(transform)); }

        private void Start3D()
        {
            if (Voice == null || BoundUserId == Guid.Empty || Event.IsNull) return;
            var core = FMODUnity.RuntimeManager.CoreSystem;
            core.getSoftwareFormat(out _rate, out _, out _);
            _self = GCHandle.Alloc(this);
            var ex = new FMOD.CREATESOUNDEXINFO
            {
                cbsize = Marshal.SizeOf(typeof(FMOD.CREATESOUNDEXINFO)),
                numchannels = 1, format = FMOD.SOUND_FORMAT.PCMFLOAT, defaultfrequency = _rate,
                decodebuffersize = (uint)(_rate / 50), length = (uint)(_rate * 4 * 3600), // "infinite" stream
                pcmreadcallback = s_read, userdata = GCHandle.ToIntPtr(_self),
            };
            core.createSound("aurix-participant", FMOD.MODE.OPENUSER | FMOD.MODE.LOOP_NORMAL | FMOD.MODE.CREATESTREAM | FMOD.MODE._3D, ref ex, out _sound);
            _instance = FMODUnity.RuntimeManager.CreateInstance(Event);
            _instance.setUserData(GCHandle.ToIntPtr(_self));
            _instance.setCallback(s_event, FMOD.Studio.EVENT_CALLBACK_TYPE.CREATE_PROGRAMMER_SOUND | FMOD.Studio.EVENT_CALLBACK_TYPE.DESTROY_PROGRAMMER_SOUND);
            _instance.set3DAttributes(FMODUnity.RuntimeUtils.To3DAttributes(transform));
            _instance.start();
        }

        private void Release()
        {
            if (_instance.isValid()) { _instance.stop(FMOD.Studio.STOP_MODE.IMMEDIATE); _instance.release(); _instance.clearHandle(); }
            if (_sound.hasHandle()) { _sound.release(); _sound.clearHandle(); }
            if (Voice != null && BoundUserId != Guid.Empty) Voice.ReleaseParticipant(BoundUserId);
            if (_self.IsAllocated) _self.Free();
        }

        [AOT.MonoPInvokeCallback(typeof(FMOD.Studio.EVENT_CALLBACK))]
        private static FMOD.RESULT EventCallback(FMOD.Studio.EVENT_CALLBACK_TYPE type, IntPtr instancePtr, IntPtr parameterPtr)
        {
            var inst = new FMOD.Studio.EventInstance(instancePtr); inst.getUserData(out var ud);
            var self = ud == IntPtr.Zero ? null : GCHandle.FromIntPtr(ud).Target as AurixFmodParticipantVoice;
            if (self == null) return FMOD.RESULT.OK;
            if (type == FMOD.Studio.EVENT_CALLBACK_TYPE.CREATE_PROGRAMMER_SOUND)
            {
                var p = (FMOD.Studio.PROGRAMMER_SOUND_PROPERTIES)Marshal.PtrToStructure(parameterPtr, typeof(FMOD.Studio.PROGRAMMER_SOUND_PROPERTIES));
                p.sound = self._sound.handle; p.subsoundIndex = -1;
                Marshal.StructureToPtr(p, parameterPtr, false);
            }
            return FMOD.RESULT.OK;
        }

        // FMOD stream thread: fill the programmer sound with this participant's decoded voice.
        [AOT.MonoPInvokeCallback(typeof(FMOD.SOUND_PCMREAD_CALLBACK))]
        private static FMOD.RESULT PcmRead(IntPtr soundraw, IntPtr data, uint datalen)
        {
            var snd = new FMOD.Sound(soundraw); snd.getUserData(out var ud);
            var self = ud == IntPtr.Zero ? null : GCHandle.FromIntPtr(ud).Target as AurixFmodParticipantVoice;
            int n = (int)datalen / 4;
            if (self == null || self.Voice == null) { unsafe { float* o = (float*)data; for (int i = 0; i < n; i++) o[i] = 0f; } return FMOD.RESULT.OK; }
            if (self._scratch == null || self._scratch.Length < n) self._scratch = new float[n];
            self.Voice.PullParticipant(self.BoundUserId, self._scratch, 1, self._rate);
            Marshal.Copy(self._scratch, 0, data, n);
            return FMOD.RESULT.OK;
        }
    }
}
#endif
