#if UNITY_5_3_OR_NEWER && MIRROR && !(UNITY_WEBGL && !UNITY_EDITOR)
using System;
using Aurix.Unity;
using Mirror;
using UnityEngine;

namespace Aurix.Samples
{
    /// <summary>
    /// Voice that follows the networked player (Mirror). On the player prefab: the local player reports
    /// its pose to the positional channel through <see cref="AurixPositionalSource"/> and syncs its Aurix
    /// user id; remote clients attach that voice to <see cref="Head"/> for in-engine 3D audio.
    /// Set the voice component's <c>Playback</c> to <c>PerParticipant</c>.
    /// </summary>
    [DisallowMultipleComponent]
    public sealed class MirrorVoiceAvatar : NetworkBehaviour
    {
        [Tooltip("Positional channel (GUID) the players are joined to; can be set later with SetChannel().")]
        public string ChannelId;
        [Tooltip("Where the remote voice is played from (empty = this object).")]
        public Transform Head;
        [Tooltip("The voice component; empty = the first one in the scene.")]
        public AurixVoiceBehaviour Voice;

        [SyncVar(hook = nameof(OnUserIdChanged))] private string _userId = "";
        private AurixPositionalSource _source;
        private AurixPositionalSource _attacher;
        private Guid _attachedAs;

        public override void OnStartLocalPlayer()
        {
            if (Voice == null) Voice = FindObjectOfType<AurixVoiceBehaviour>();
            _source = gameObject.GetComponent<AurixPositionalSource>() ?? gameObject.AddComponent<AurixPositionalSource>();
            _source.Voice = Voice;
            _source.PoseSource = Head;
            if (Guid.TryParse(ChannelId, out var ch)) _source.SetChannel(ch);
            PublishUserId();
        }

        public override void OnStartClient()
        {
            if (!isLocalPlayer) Attach(_userId);
        }

        public override void OnStopClient()
        {
            if (_attacher != null && _attachedAs != Guid.Empty) _attacher.DetachParticipantVoice(_attachedAs);
            _attachedAs = Guid.Empty;
        }

        /// <summary>Local player: start reporting to <paramref name="channel"/>; call after the channel join.</summary>
        public void SetChannel(Guid channel)
        {
            ChannelId = channel.ToString();
            if (_source != null) _source.SetChannel(channel);
        }

        private void Update()
        {
            if (isLocalPlayer && string.IsNullOrEmpty(_userId)) PublishUserId();
        }

        private void PublishUserId()
        {
            var id = Voice != null && Voice.Client != null ? Voice.Client.LocalUserId : Guid.Empty;
            if (id != Guid.Empty) CmdSetUserId(id.ToString());
        }

        [Command] private void CmdSetUserId(string id) => _userId = id;

        private void OnUserIdChanged(string _, string now)
        {
            if (!isLocalPlayer) Attach(now);
        }

        private void Attach(string value)
        {
            if (!Guid.TryParse(value, out var user) || user == Guid.Empty || user == _attachedAs) return;
            if (_attacher == null)
            {
                _attacher = FindObjectOfType<AurixPositionalSource>();
                if (_attacher == null) return;
            }
            if (_attachedAs != Guid.Empty) _attacher.DetachParticipantVoice(_attachedAs);
            _attacher.AttachParticipantVoice(user, Head != null ? Head : transform);
            _attachedAs = user;
        }
    }
}
#endif
