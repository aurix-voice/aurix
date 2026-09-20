#if UNITY_5_3_OR_NEWER
using System;
using Aurix.Audio;
using UnityEngine;

namespace Aurix.Unity
{
    /// <summary>
    /// Lip-sync for one voice — a remote participant (<see cref="Bind"/>) or the local microphone
    /// (<see cref="BindLocal"/>) — from the SDK's viseme analysis (native core, or the browser worklet
    /// in WebGL). Every frame it pulls the latest <see cref="VisemeFrame"/>, smooths it, drives the
    /// blend shapes of <see cref="Target"/> (names in <see cref="BlendShapes"/>, <see cref="Viseme"/>
    /// order; jaw from <see cref="JawBlendShape"/>) and raises <see cref="OnFrame"/> for custom rigs.
    /// Turns the analysis on for the whole client when <see cref="EnableAnalysis"/> is set (it is a
    /// client-wide switch: <see cref="IAurixVoiceClient.SetVisemesAsync"/>). Nothing about the mouth
    /// shapes leaves the device: they are derived from decoded audio after E2EE.
    /// </summary>
    public sealed class AurixLipSync : MonoBehaviour
    {
        [Tooltip("The voice component (AurixVoiceBehaviour / AurixWebGLVoiceBehaviour) to read from; empty = one on this GameObject or the first in the scene.")]
        public MonoBehaviour Voice;
        [Tooltip("Participant to animate (user id GUID); empty = the local microphone. Usually set from code with Bind().")]
        public string UserId;
        [Tooltip("Switch the client's viseme analysis on when this component starts (all heard voices + the microphone).")]
        public bool EnableAnalysis = true;

        [Header("Rig")]
        [Tooltip("Optional: the face mesh whose blend shapes are driven. Leave empty and subscribe to OnFrame / read Current for other rigs.")]
        public SkinnedMeshRenderer Target;
        [Tooltip("Blend-shape names per viseme, in Viseme order (sil, PP, FF, SS, aa, E, ih, oh, ou); empty = not driven.")]
        public string[] BlendShapes = { "", "viseme_PP", "viseme_FF", "viseme_SS", "viseme_aa", "viseme_E", "viseme_I", "viseme_O", "viseme_U" };
        [Tooltip("Optional blend shape driven by MouthOpen (jaw).")]
        public string JawBlendShape = "";
        [Tooltip("Blend-shape weight for a fully active viseme (Unity uses 0..100).")]
        public float BlendShapeScale = 100f;
        [Tooltip("Smoothing time constant in seconds (0 = raw 20 ms frames).")]
        [Range(0f, 0.3f)] public float Smoothing = 0.04f;

        /// <summary>The bound participant, <see cref="Guid.Empty"/> for the local microphone.</summary>
        public Guid BoundUserId { get; private set; }
        public bool IsLocal => BoundUserId == Guid.Empty;

        /// <summary>Smoothed mouth state as applied this frame (silence when nothing is heard / analysis is off).</summary>
        public VisemeFrame Current => _current;
        /// <summary>The last raw frame the SDK produced for this voice (null before the first).</summary>
        public VisemeFrame? Raw { get; private set; }
        /// <summary>New raw frames are arriving (the voice is being analysed and audio is flowing).</summary>
        public bool IsAnalysing { get; private set; }

        /// <summary>Fired on the main thread after each smoothing step with <see cref="Current"/>.</summary>
        public event Action<VisemeFrame> OnFrame;

        private IAurixVoiceHost _host;
        private IAurixVoiceClient _enabledOn;
        private VisemeFrame _current = VisemeFrame.Silent();
        private ulong _lastSequence;
        private float _staleFor;
        private int[] _shapeIndices;
        private int _jawIndex = -1;
        private Mesh _indexedMesh;

        /// <summary>Animate <paramref name="userId"/>'s voice from now on.</summary>
        public void Bind(Guid userId)
        {
            BoundUserId = userId;
            UserId = userId == Guid.Empty ? null : userId.ToString();
            ResetState();
        }

        /// <summary>Animate the local microphone (as sent, after voice effects).</summary>
        public void BindLocal() => Bind(Guid.Empty);

        private void OnEnable()
        {
            if (!string.IsNullOrEmpty(UserId) && Guid.TryParse(UserId, out var id)) BoundUserId = id;
            ResetState();
        }

        private void OnDisable()
        {
            ResetState();
            Apply(VisemeFrame.Silent());
        }

        private void ResetState()
        {
            _current = VisemeFrame.Silent();
            Raw = null;
            _lastSequence = 0;
            _staleFor = 0f;
            IsAnalysing = false;
        }

        private void Update()
        {
            if (_host == null) _host = AurixVoiceHost.Resolve(Voice, this);
            var client = _host?.VoiceClient;
            if (client == null)
            {
                _enabledOn = null;
                Step(null);
                return;
            }
            if (EnableAnalysis && !ReferenceEquals(_enabledOn, client))
            {
                _enabledOn = client;
                if (!client.VisemesEnabled)
                {
                    if (client.SupportsVisemes) _ = client.SetVisemesAsync(true);
                    else Debug.LogWarning("Aurix: lip-sync is not available on this platform (no aurix_client native library)");
                }
            }
            Step(IsLocal ? client.GetLocalVisemes() : client.GetParticipantVisemes(BoundUserId));
        }

        private void Step(VisemeFrame? frame)
        {
            float dt = Time.deltaTime;
            if (frame.HasValue && (Raw == null || frame.Value.Sequence != _lastSequence))
            {
                Raw = frame;
                _lastSequence = frame.Value.Sequence;
                _staleFor = 0f;
                IsAnalysing = true;
            }
            else
            {
                _staleFor += dt;
                // A voice that stopped (or left) produces no frames: fall back to silence after ~5 missing frames.
                if (_staleFor > 0.1f) IsAnalysing = false;
            }
            var target = IsAnalysing && Raw.HasValue ? Raw.Value : VisemeFrame.Silent(_lastSequence);
            Smooth(ref _current, target, dt);
            Apply(_current);
            OnFrame?.Invoke(_current);
        }

        private void Smooth(ref VisemeFrame current, VisemeFrame target, float dt)
        {
            float k = Smoothing <= 0f || dt <= 0f ? 1f : 1f - (float)Math.Exp(-dt / Smoothing);
            for (int i = 0; i < VisemeFrame.Count; i++) current[i] += (target[i] - current[i]) * k;
            current.MouthOpen += (target.MouthOpen - current.MouthOpen) * k;
            current.Energy += (target.Energy - current.Energy) * k;
            current.Confidence = target.Confidence;
            current.Dominant = target.Dominant;
            current.Sequence = target.Sequence;
        }

        private void Apply(VisemeFrame frame)
        {
            var target = Target;
            if (target == null) return;
            var mesh = target.sharedMesh;
            if (mesh == null) return;
            if (!ReferenceEquals(mesh, _indexedMesh) || _shapeIndices == null) IndexBlendShapes(mesh);
            for (int i = 0; i < VisemeFrame.Count; i++)
                if (_shapeIndices[i] >= 0) target.SetBlendShapeWeight(_shapeIndices[i], frame[i] * BlendShapeScale);
            if (_jawIndex >= 0) target.SetBlendShapeWeight(_jawIndex, frame.MouthOpen * BlendShapeScale);
        }

        private void IndexBlendShapes(Mesh mesh)
        {
            _indexedMesh = mesh;
            _shapeIndices = new int[VisemeFrame.Count];
            for (int i = 0; i < VisemeFrame.Count; i++)
            {
                string name = BlendShapes != null && i < BlendShapes.Length ? BlendShapes[i] : null;
                _shapeIndices[i] = string.IsNullOrEmpty(name) ? -1 : mesh.GetBlendShapeIndex(name);
            }
            _jawIndex = string.IsNullOrEmpty(JawBlendShape) ? -1 : mesh.GetBlendShapeIndex(JawBlendShape);
        }
    }
}
#endif
