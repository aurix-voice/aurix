# Aurix Voice + FMOD Studio (Unreal)

`UAurixFmodVoiceComponent` is drop-in sample code, not a plugin module: copy both files into a game
module whose `Build.cs` lists `"AurixVoice"` and `"FMODStudio"`, add the component to a persistent
actor, and the voice path moves onto FMOD:

| Need | What the component does | AurixVoice API |
|---|---|---|
| Hear the team | custom DSP at the tail of the master group (or `VoiceBus`, e.g. `bus:/Voice`) adds the remote mix; the plugin's own 2D playback is stopped (clear `bAutoStartPlayback` in the plugin settings too) | `MixOutputAudio`, `StopPlayback` |
| Echo cancellation | pass-through DSP tapped upstream of the voice DSP on the master group hands FMOD's game mix to the AEC (the core already references what `MixOutputAudio` returned, so voice must not be pushed twice; `VoiceBus` is therefore ignored while this is on) | `PushRenderAudio` |
| FMOD owns the microphone | `bCaptureWithFmod`: `System::recordStart` ring buffer → Aurix every tick | `StopCapture`, `PushCaptureAudio` |
| 3D voice per speaker | `BindParticipantToEvent(UserId, FMODAudioComponent)`: programmer sound streaming that participant into an event with a Programmer instrument + spatializer | `PullParticipantAudio`, `SetParticipantClaimed` |

Format contract: `MixOutputAudio`, `PullParticipantAudio` and `PushRenderAudio` are 48 kHz interleaved
float, 1 or 2 channels — set *Project Settings → FMOD Studio → Sample Rate* to 48000 (the component
refuses to start otherwise and logs why). `PushCaptureAudio` takes any rate / channel count.

Blueprint users can do the same without C++: `MixOutputAudio`, `PullParticipantAudio`, `PushCaptureAudio`,
`PushRenderAudio`, `SetParticipantClaimed`, `StopPlayback` are all BlueprintCallable; only the DSP
callbacks themselves need native code (FMOD's DSP/PCM-read callbacks are C functions).

This sample is compiled against the FMOD Studio UE integration (2.02/2.03 API: `IFMODStudioModule`,
`UFMODAudioComponent::SetProgrammerSound`). It is not built by the repository CI, which has no FMOD
licence; treat it as a reviewed starting point and keep it in your game module.
