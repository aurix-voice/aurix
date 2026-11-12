# Aurix Voice + FMOD Studio (Unity)

FMOD for Unity replaces Unity's audio pipeline, so the stock `AurixVoiceBehaviour` AudioSource and
`Microphone` capture do not run. These three components move the voice path onto FMOD:

| Component | What it does | Aurix API it uses |
|---|---|---|
| `AurixFmodOutput` | Custom DSP on the master group (or a bus) that adds the remote mix; a second DSP hands FMOD's rendered game audio to the echo canceller | `MixOutput`, `PushRender`, `RenderFedExternally` |
| `AurixFmodCapture` | Records the microphone with `System.recordStart` and pushes it to Aurix | `ExternalCapture`, `PushCapture` |
| `AurixFmodParticipantVoice` | One Studio event (programmer sound + 3D spatializer) per remote speaker | `PullParticipant`, `ReleaseParticipant`, `Playback = PerParticipant` |

Setup: import the FMOD for Unity package, add `AURIX_FMOD` to *Project Settings → Player → Scripting
Define Symbols*, drop `AurixFmodOutput` (and optionally `AurixFmodCapture`) next to the voice component,
and for 3D voice author an event with a Programmer instrument and attach `AurixFmodParticipantVoice` to
each speaker's avatar. Everything else — tokens, channels, push-to-talk, moderation, lip-sync — is unchanged.

Wwise: the same three Aurix calls map onto `AkAudioInputManager` (capture/output via
`AkAudioInputManager.PostAudioInputEvent` with a sample callback that fills from `MixOutput` /
`PullParticipant`) and a Wwise plug-in or `AkAudioInputManager` on the master bus for `PushRender`.
