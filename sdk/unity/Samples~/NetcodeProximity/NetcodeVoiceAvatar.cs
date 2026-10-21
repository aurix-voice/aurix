#if UNITY_5_3_OR_NEWER && AURIX_NETCODE && !(UNITY_WEBGL && !UNITY_EDITOR)
using System;
using Aurix.Unity;
using Unity.Collections;
using Unity.Netcode;
using UnityEngine;

namespace Aurix.Samples
{
    /// <summary>
    /// Voice that follows the networked player (Netcode for GameObjects). Put it on the player prefab:
    /// the owner publishes its Aurix user id and reports its pose to the positional channel through
    /// <see cref="AurixPositionalSource"/>; every other client attaches that player's voice to this
    /// avatar (<see cref="Head"/>) so Unity's 3D audio renders it in-world. Set <see cref="ChannelId"/>
    /// (or call <see cref="SetChannel"/>) to the positional channel the players joined, and the voice
    /// component's <c>Playback</c> to <c>PerParticipant</c>.
    /// </summary>
    [DisallowMultipleComponent]
    public sealed class NetcodeVoiceAvatar : NetworkBehaviour
    {
        [Tooltip("Positional channel (GUID) the players are joined to; can be set later with SetChannel().")]
        public string ChannelId;
        [Tooltip("Where the remote voice is played from (empty = this object).")]
        public Transform Head;
        [Tooltip("The voice component; empty = the first one in the scene.")]
        public AurixVoiceBehaviour Voice;

        private readonly NetworkVariable<FixedString64Bytes> _userId = new NetworkVariable<FixedString64Bytes>(
            default, NetworkVariableReadPermission.Everyone, NetworkVariableWritePermission.Owner);
        private AurixPositionalSource _source;
        private AurixPositionalSource _attacher;
        private Guid _attachedAs;

        public override void OnNetworkSpawn()
        {
            if (Voice == null) Voice = FindObjectOfType<AurixVoiceBehaviour>();
            if (IsOwner)
            {
                _source = gameObject.GetComponent<AurixPositionalSource>() ?? gameObject.AddComponent<AurixPositionalSource>();
                _source.Voice = Voice;
                _source.PoseSource = Head;
                if (Guid.TryParse(ChannelId, out var ch)) _source.SetChannel(ch);
                PublishUserId();
            }
            else
            {
                _userId.OnValueChanged += (_, __) => Attach();
                Attach();
            }
        }

        public override void OnNetworkDespawn()
        {
            if (_attacher != null && _attachedAs != Guid.Empty) _attacher.DetachParticipantVoice(_attachedAs);
            _attachedAs = Guid.Empty;
        }

        /// <summary>Owner: start reporting to <paramref name="channel"/>; call after the channel join.</summary>
        public void SetChannel(Guid channel)
        {
            ChannelId = channel.ToString();
            if (_source != null) _source.SetChannel(channel);
        }

        private void Update()
        {
            if (IsOwner && _userId.Value.IsEmpty) PublishUserId();
        }

        private void PublishUserId()
        {
            var id = Voice != null && Voice.Client != null ? Voice.Client.LocalUserId : Guid.Empty;
            if (id != Guid.Empty) _userId.Value = new FixedString64Bytes(id.ToString());
        }

        private void Attach()
        {
            if (!Guid.TryParse(_userId.Value.ToString(), out var user) || user == Guid.Empty || user == _attachedAs) return;
            if (_attacher == null)
            {
                _attacher = FindObjectOfType<AurixPositionalSource>();
                if (_attacher == null) return; // the local player has not spawned yet; retried on the next value change
            }
            if (_attachedAs != Guid.Empty) _attacher.DetachParticipantVoice(_attachedAs);
            _attacher.AttachParticipantVoice(user, Head != null ? Head : transform);
            _attachedAs = user;
        }
    }
}
#endif
