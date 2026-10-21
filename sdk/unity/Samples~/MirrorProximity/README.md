# Mirror: voice that follows the player

Requires Mirror (defines `MIRROR`; the sample compiles to nothing without it) and the Aurix Voice package.

1. Add `MirrorVoiceAvatar` to your player prefab next to `NetworkIdentity`; point `Head` at the head bone or camera.
2. Join the positional channel on every client and set the voice component's `Playback` to `PerParticipant`.
3. Set `ChannelId` on the prefab, or call `SetChannel(channelId)` on the local player after the join.

The local player syncs its Aurix user id with a `SyncVar` and reports its pose through `AurixPositionalSource`; remote clients attach that voice to the avatar with `AttachParticipantVoice`.
