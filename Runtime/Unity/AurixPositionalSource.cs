#if UNITY_5_3_OR_NEWER && !(UNITY_WEBGL && !UNITY_EDITOR)
using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading.Tasks;
using Aurix.Protocol;
using UnityEngine;

namespace Aurix.Unity
{
    /// <summary>
    /// Positional voice for the locally controlled player: reports this object's pose to a positional
    /// channel at a fixed rate (only when it moved), so the node applies distance roll-off, radius
    /// visibility and panning to what this player hears — and, optionally, plays each remote talker
    /// from their avatar through Unity's own 3D audio (<see cref="AttachParticipantVoice"/>).
    ///
    /// Put it on the local player's avatar (or its camera via <see cref="PoseSource"/>), set
    /// <see cref="ChannelId"/> or call <see cref="SetChannel"/> after the join; it starts reporting as
    /// soon as the voice component is connected and the channel is joined. Nothing is sent while the
    /// pose is still (within <see cref="MinMoveMeters"/> / <see cref="MinTurnDegrees"/>).
    /// Unity units are metres and left-handed; set the channel's <c>coordinate_system</c> to
    /// <c>left_handed</c> (the default) or convert in <see cref="WorldToMeters"/> if your world is scaled.
    /// </summary>
    [AddComponentMenu("Aurix Voice/Positional Source")]
    [DisallowMultipleComponent]
    public sealed class AurixPositionalSource : MonoBehaviour
    {
        [Tooltip("The voice component to report through; empty = the first one in the scene.")]
        public AurixVoiceBehaviour Voice;
        [Tooltip("Positional channel receiving the pose (GUID). Empty = idle until SetChannel().")]
        public string ChannelId;
        [Tooltip("Pose updates per second (the node rate-limits anyway; 5–10 is plenty).")]
        [Range(1, 30)] public float UpdatesPerSecond = 8f;
        [Tooltip("Skip the update when the object moved less than this (metres) and turned less than MinTurnDegrees.")]
        [Min(0)] public float MinMoveMeters = 0.1f;
        [Min(0)] public float MinTurnDegrees = 2f;
        [Tooltip("Unity units per metre (1 unless your world is scaled).")]
        [Min(0.0001f)] public float WorldToMeters = 1f;
        [Tooltip("Report the pose of this transform instead of the object's own (e.g. the camera for a first-person listener).")]
        public Transform PoseSource;
        [Tooltip("AudioSource settings copied onto voices attached with AttachParticipantVoice (null = sensible 3D defaults).")]
        public AudioSource VoiceTemplate;

        /// <summary>Channel the pose goes to; <see cref="Guid.Empty"/> = idle.</summary>
        public Guid Channel { get; private set; }
        /// <summary>Last pose actually sent, for debugging and tests.</summary>
        public int UpdatesSent { get; private set; }

        private readonly Dictionary<Guid, AurixParticipantAudioSource> _attached = new Dictionary<Guid, AurixParticipantAudioSource>();
        private float _sinceLast;
        private bool _hasLast;
        private Vector3 _lastPos;
        private Quaternion _lastRot;
        private bool _inFlight;

        private void OnEnable()
        {
            if (Voice == null) Voice = FindObjectOfType<AurixVoiceBehaviour>();
            if (Channel == Guid.Empty && !string.IsNullOrEmpty(ChannelId) && Guid.TryParse(ChannelId, out var g)) Channel = g;
            _hasLast = false;
        }

        private void OnValidate()
        {
            if (!string.IsNullOrEmpty(ChannelId) && Guid.TryParse(ChannelId, out var g)) Channel = g;
        }

        /// <summary>Start (non-empty) or stop (<see cref="Guid.Empty"/>) reporting; sends one update right away.</summary>
        public void SetChannel(Guid channel)
        {
            Channel = channel;
            ChannelId = channel == Guid.Empty ? null : channel.ToString();
            _hasLast = false;
            if (channel != Guid.Empty) SendPoseNow();
        }

        /// <summary>Send the current pose now regardless of the rate limit. False when not connected or not joined.</summary>
        public bool SendPoseNow()
        {
            var client = Voice != null ? Voice.Client : null;
            if (client == null || Channel == Guid.Empty || client.State != VoiceConnectionState.MediaBound) return false;
            if (!client.JoinedChannels.Contains(Channel)) return false;
            var self = client.LocalUserId;
            if (self == Guid.Empty) return false;
            ReadPose(out var pos, out var rot);
            _lastPos = pos; _lastRot = rot; _hasLast = true; _sinceLast = 0f;
            var scale = 1f / WorldToMeters;
            var p = pos * scale;
            var fwd = rot * Vector3.forward;
            var up = rot * Vector3.up;
            Fire(client.UpdatePositionAsync(Channel, self,
                new Position3D { X = p.x, Y = p.y, Z = p.z },
                new Orientation3D { ForwardX = fwd.x, ForwardY = fwd.y, ForwardZ = fwd.z, UpX = up.x, UpY = up.y, UpZ = up.z }));
            UpdatesSent++;
            return true;
        }

        private void Update()
        {
            if (Channel == Guid.Empty || _inFlight) return;
            _sinceLast += Time.unscaledDeltaTime;
            if (_sinceLast < 1f / Mathf.Max(1f, UpdatesPerSecond)) return;
            ReadPose(out var pos, out var rot);
            if (_hasLast && (pos - _lastPos).magnitude < MinMoveMeters * WorldToMeters && Quaternion.Angle(rot, _lastRot) < MinTurnDegrees) return;
            SendPoseNow();
        }

        private void ReadPose(out Vector3 pos, out Quaternion rot)
        {
            var t = PoseSource != null ? PoseSource : transform;
            pos = t.position; rot = t.rotation;
        }

        private async void Fire(Task task)
        {
            _inFlight = true;
            try { await task; }
            catch (Exception e) { Debug.LogWarning($"Aurix: position update failed: {e.Message}"); }
            finally { _inFlight = false; }
        }

        /// <summary>
        /// Play <paramref name="userId"/>'s voice from <paramref name="attachTo"/> (typically the remote avatar's head)
        /// through Unity's spatializer; the participant leaves the 2D mix while attached. Requires
        /// <see cref="AurixVoiceBehaviour.Playback"/> = <see cref="VoicePlaybackMode.PerParticipant"/>.
        /// Re-attaching moves the voice to the new transform.
        /// </summary>
        public AurixParticipantAudioSource AttachParticipantVoice(Guid userId, Transform attachTo)
        {
            if (attachTo == null) throw new ArgumentNullException(nameof(attachTo));
            DetachParticipantVoice(userId);
            var go = new GameObject($"Voice {userId.ToString().Substring(0, 8)}");
            go.transform.SetParent(attachTo, false);
            var src = go.AddComponent<AudioSource>();
            if (VoiceTemplate != null)
            {
                src.spatialBlend = VoiceTemplate.spatialBlend; src.rolloffMode = VoiceTemplate.rolloffMode;
                src.minDistance = VoiceTemplate.minDistance; src.maxDistance = VoiceTemplate.maxDistance;
                src.spatialize = VoiceTemplate.spatialize; src.outputAudioMixerGroup = VoiceTemplate.outputAudioMixerGroup;
                src.dopplerLevel = VoiceTemplate.dopplerLevel; src.spread = VoiceTemplate.spread;
            }
            else
            {
                src.spatialBlend = 1f; src.rolloffMode = AudioRolloffMode.Logarithmic;
                src.minDistance = 1.5f; src.maxDistance = 40f; src.dopplerLevel = 0f;
            }
            var p = go.AddComponent<AurixParticipantAudioSource>();
            p.Voice = Voice;
            p.Bind(userId);
            _attached[userId] = p;
            return p;
        }

        /// <summary>Return <paramref name="userId"/> to the 2D mix and destroy the attached source.</summary>
        public void DetachParticipantVoice(Guid userId)
        {
            if (!_attached.TryGetValue(userId, out var p)) return;
            _attached.Remove(userId);
            if (p != null) { p.Unbind(); Destroy(p.gameObject); }
        }

        /// <summary>Detach every voice attached through this component.</summary>
        public void DetachAllParticipantVoices()
        {
            foreach (var id in new List<Guid>(_attached.Keys)) DetachParticipantVoice(id);
        }

        public bool IsParticipantVoiceAttached(Guid userId) => _attached.ContainsKey(userId);

        private void OnDestroy() => DetachAllParticipantVoices();
    }
}
#endif
