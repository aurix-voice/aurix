# Netcode for GameObjects: voice that follows the player

Requires `com.unity.netcode.gameobjects` (the sample compiles to nothing without it) and the Aurix Voice package.

1. Add `NetcodeVoiceAvatar` to your player prefab next to `NetworkObject`; point `Head` at the head bone or camera.
2. Join the positional channel on every client (`JoinChannelAsync`) and set the voice component's `Playback` to `PerParticipant`.
3. Set `ChannelId` on the prefab, or call `SetChannel(channelId)` on the owner after the join.

The owner publishes its Aurix user id (the `sub` of its player token) over a `NetworkVariable` and reports its pose through `AurixPositionalSource`; other clients attach that voice to the avatar with `AttachParticipantVoice`, so Unity's spatializer, occlusion and reverb zones apply. Players without an attached avatar stay audible through the 2D mix with the node's positional gain.
