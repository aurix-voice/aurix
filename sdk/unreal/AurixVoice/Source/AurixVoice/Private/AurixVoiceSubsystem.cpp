#include "AurixVoiceSubsystem.h"

#include "AurixAudioCapture.h"
#include "AurixNativeConversions.h"
#include "AurixParticipantSoundWave.h"
#include "AurixRegionDiscovery.h"
#include "AurixVoiceLog.h"
#include "AurixVoiceSoundWave.h"
#include "Components/AudioComponent.h"
#include "Components/SceneComponent.h"
#include "Dom/JsonObject.h"
#include "Engine/GameInstance.h"
#include "Engine/World.h"
#include "GameFramework/Actor.h"
#include "Kismet/GameplayStatics.h"
#include "Serialization/JsonReader.h"
#include "Serialization/JsonSerializer.h"
#include "Sound/SoundAttenuation.h"
#include "Sound/SoundClass.h"

#include "aurix_client.hpp"

struct FAurixNativeClient
{
	aurix::Client Client;
};

namespace
{
constexpr int32 MaxEventsPerTick = 256;

FDateTime FromUnixMs(int64 Ms)
{
	return FDateTime::FromUnixTimestamp(Ms / 1000) + FTimespan::FromMilliseconds(static_cast<double>(Ms % 1000));
}

EAurixConnectionState ToState(AurixConnectionState S)
{
	switch (S)
	{
	case AURIX_STATE_CONNECTING: return EAurixConnectionState::Connecting;
	case AURIX_STATE_CONNECTED: return EAurixConnectionState::Connected;
	case AURIX_STATE_MEDIA_BOUND: return EAurixConnectionState::MediaBound;
	case AURIX_STATE_RECONNECTING: return EAurixConnectionState::Reconnecting;
	case AURIX_STATE_FAILED: return EAurixConnectionState::Failed;
	case AURIX_STATE_DISCONNECTED:
	default: return EAurixConnectionState::Disconnected;
	}
}

EAurixRole ToRole(AurixRole R)
{
	switch (R)
	{
	case AURIX_ROLE_SPEAKER: return EAurixRole::Speaker;
	case AURIX_ROLE_MODERATOR: return EAurixRole::Moderator;
	case AURIX_ROLE_ADMINISTRATOR: return EAurixRole::Administrator;
	case AURIX_ROLE_LISTENER:
	default: return EAurixRole::Listener;
	}
}

EAurixTransmissionMode ToTransmission(AurixTransmissionMode M)
{
	switch (M)
	{
	case AURIX_TRANSMIT_SINGLE: return EAurixTransmissionMode::Single;
	case AURIX_TRANSMIT_ALL: return EAurixTransmissionMode::All;
	case AURIX_TRANSMIT_NONE:
	default: return EAurixTransmissionMode::None;
	}
}

AurixTransmissionMode FromTransmission(EAurixTransmissionMode M)
{
	switch (M)
	{
	case EAurixTransmissionMode::Single: return AURIX_TRANSMIT_SINGLE;
	case EAurixTransmissionMode::All: return AURIX_TRANSMIT_ALL;
	case EAurixTransmissionMode::None:
	default: return AURIX_TRANSMIT_NONE;
	}
}

EAurixAudioCodec ToCodec(AurixAudioCodec C)
{
	return C == AURIX_CODEC_PCMU ? EAurixAudioCodec::Pcmu : EAurixAudioCodec::Opus;
}

AurixAudioCodec FromCodec(EAurixAudioCodec C)
{
	return C == EAurixAudioCodec::Pcmu ? AURIX_CODEC_PCMU : AURIX_CODEC_OPUS;
}

EAurixDownlinkMode ToDownlinkMode(AurixDownlinkMode M)
{
	return M == AURIX_DOWNLINK_MIXED ? EAurixDownlinkMode::Mixed : EAurixDownlinkMode::Streams;
}

AurixDownlinkMode FromDownlinkMode(EAurixDownlinkMode M)
{
	return M == EAurixDownlinkMode::Mixed ? AURIX_DOWNLINK_MIXED : AURIX_DOWNLINK_STREAMS;
}

FAurixDucking ToDucking(const AurixDucking& D)
{
	FAurixDucking Out;
	Out.bEnabled = D.enabled;
	Out.Gain = D.gain;
	Out.AttackMs = static_cast<int32>(D.attack_ms);
	Out.ReleaseMs = static_cast<int32>(D.release_ms);
	Out.HoldMs = static_cast<int32>(D.hold_ms);
	Out.bModerators = D.moderators;
	return Out;
}

FAurixChannelInfo ToChannelInfo(const AurixChannelInfo& I)
{
	FAurixChannelInfo Out;
	Out.Role = ToRole(I.role);
	Out.ParticipantCount = static_cast<int32>(I.participant_count);
	Out.bHiddenListeners = I.hidden_listeners;
	Out.bTranscription = I.transcription;
	Out.bSafetyVoice = I.safety_voice;
	Out.bPriority = I.priority;
	Out.Ducking = ToDucking(I.ducking);
	return Out;
}

AurixVoiceEffects ToRawEffects(const FAurixVoiceEffects& E)
{
	AurixVoiceEffects Raw;
	Raw.highpass_hz = E.HighpassHz;
	Raw.lowpass_hz = E.LowpassHz;
	Raw.formant_semitones = E.FormantSemitones;
	Raw.pitch_semitones = E.PitchSemitones;
	Raw.ring_mod_hz = E.RingModHz;
	Raw.distortion_drive = E.DistortionDrive;
	Raw.tremolo_hz = E.TremoloHz;
	Raw.tremolo_depth = E.TremoloDepth;
	Raw.static_level = E.StaticLevel;
	Raw.reverb_mix = E.ReverbMix;
	Raw.reverb_size = E.ReverbSize;
	Raw.reverb_damping = E.ReverbDamping;
	return Raw;
}

FAurixVoiceEffects ToEffects(const AurixVoiceEffects& Raw)
{
	FAurixVoiceEffects Out;
	Out.HighpassHz = Raw.highpass_hz;
	Out.LowpassHz = Raw.lowpass_hz;
	Out.FormantSemitones = Raw.formant_semitones;
	Out.PitchSemitones = Raw.pitch_semitones;
	Out.RingModHz = Raw.ring_mod_hz;
	Out.DistortionDrive = Raw.distortion_drive;
	Out.TremoloHz = Raw.tremolo_hz;
	Out.TremoloDepth = Raw.tremolo_depth;
	Out.StaticLevel = Raw.static_level;
	Out.ReverbMix = Raw.reverb_mix;
	Out.ReverbSize = Raw.reverb_size;
	Out.ReverbDamping = Raw.reverb_damping;
	return Out;
}

AurixVoicePreset ToRawPreset(EAurixVoicePreset P)
{
	switch (P)
	{
	case EAurixVoicePreset::Monster: return AURIX_VOICE_PRESET_MONSTER;
	case EAurixVoicePreset::Radio: return AURIX_VOICE_PRESET_RADIO;
	case EAurixVoicePreset::Helium: return AURIX_VOICE_PRESET_HELIUM;
	case EAurixVoicePreset::Ghost: return AURIX_VOICE_PRESET_GHOST;
	case EAurixVoicePreset::Robot:
	default: return AURIX_VOICE_PRESET_ROBOT;
	}
}

FAurixVisemeFrame ToVisemeFrame(const AurixVisemeFrame& F)
{
	FAurixVisemeFrame Out;
	Out.Weights.SetNumUninitialized(AURIX_VISEME_COUNT);
	for (int32 i = 0; i < AURIX_VISEME_COUNT; ++i)
	{
		Out.Weights[i] = F.weights[i];
	}
	Out.Dominant = static_cast<EAurixViseme>(static_cast<uint8>(F.dominant));
	Out.MouthOpen = F.mouth_open;
	Out.Energy = F.energy;
	Out.Confidence = F.confidence;
	Out.Sequence = static_cast<int64>(F.sequence);
	return Out;
}

EAurixMediaPath ToMediaPath(AurixMediaPath P)
{
	switch (P)
	{
	case AURIX_MEDIA_UDP: return EAurixMediaPath::Udp;
	case AURIX_MEDIA_TUNNEL: return EAurixMediaPath::Tunnel;
	case AURIX_MEDIA_QUIC: return EAurixMediaPath::Quic;
	case AURIX_MEDIA_TLS: return EAurixMediaPath::Tls;
	case AURIX_MEDIA_NONE:
	default: return EAurixMediaPath::None;
	}
}

AurixMediaPathPolicy FromMediaPathPolicy(EAurixMediaPathPolicy P)
{
	switch (P)
	{
	case EAurixMediaPathPolicy::UdpOnly: return AURIX_MEDIA_PATH_UDP_ONLY;
	case EAurixMediaPathPolicy::TunnelOnly: return AURIX_MEDIA_PATH_TUNNEL_ONLY;
	case EAurixMediaPathPolicy::QuicOnly: return AURIX_MEDIA_PATH_QUIC_ONLY;
	case EAurixMediaPathPolicy::TlsOnly: return AURIX_MEDIA_PATH_TLS_ONLY;
	case EAurixMediaPathPolicy::Auto:
	default: return AURIX_MEDIA_PATH_AUTO;
	}
}

EAurixModerationAction ToModeration(AurixModerationAction A)
{
	switch (A)
	{
	case AURIX_MODERATION_MUTE: return EAurixModerationAction::Mute;
	case AURIX_MODERATION_UNMUTE: return EAurixModerationAction::Unmute;
	case AURIX_MODERATION_KICK:
	default: return EAurixModerationAction::Kick;
	}
}

AurixModerationAction FromModeration(EAurixModerationAction A)
{
	switch (A)
	{
	case EAurixModerationAction::Mute: return AURIX_MODERATION_MUTE;
	case EAurixModerationAction::Unmute: return AURIX_MODERATION_UNMUTE;
	case EAurixModerationAction::Kick:
	default: return AURIX_MODERATION_KICK;
	}
}

AurixTtsDestination FromTtsDestination(EAurixTtsDestination D)
{
	switch (D)
	{
	case EAurixTtsDestination::Channel: return AURIX_TTS_CHANNEL;
	case EAurixTtsDestination::Local: return AURIX_TTS_LOCAL;
	case EAurixTtsDestination::Both:
	default: return AURIX_TTS_BOTH;
	}
}

EAurixTtsState ToTtsState(AurixTtsState S)
{
	switch (S)
	{
	case AURIX_TTS_PLAYING: return EAurixTtsState::Playing;
	case AURIX_TTS_FINISHED: return EAurixTtsState::Finished;
	case AURIX_TTS_CANCELLED: return EAurixTtsState::Cancelled;
	case AURIX_TTS_FAILED: return EAurixTtsState::Failed;
	case AURIX_TTS_QUEUED:
	default: return EAurixTtsState::Queued;
	}
}

FAurixSessionInfo ToSession(const AurixSessionInfo& S)
{
	FAurixSessionInfo Out;
	Out.SessionId = ToGuid(S.session_id);
	Out.UserId = ToGuid(S.user_id);
	Out.Ssrc = static_cast<int64>(S.ssrc);
	Out.ResumeGraceMs = static_cast<int32>(S.resume_grace_ms);
	Out.bResumed = S.resumed;
	Out.bMediaTunnel = S.media_tunnel;
	Out.bMediaQuic = S.media_quic;
	Out.bMediaTls = S.media_tls;
	Out.bDownlinkMix = S.downlink_mix;
	Out.bTranslation = S.translation;
	Out.bTranslationSpeech = S.translation_speech;
	Out.bMigrated = S.migrated;
	return Out;
}

TArray<FAurixChatReactionTally> ParseReactions(const char* Json)
{
	TArray<FAurixChatReactionTally> Out;
	if (!Json || !*Json)
	{
		return Out;
	}
	TArray<TSharedPtr<FJsonValue>> Items;
	const TSharedRef<TJsonReader<>> Reader = TJsonReaderFactory<>::Create(FromUtf8(Json));
	if (!FJsonSerializer::Deserialize(Reader, Items))
	{
		return Out;
	}
	for (const TSharedPtr<FJsonValue>& Item : Items)
	{
		const TSharedPtr<FJsonObject>* Obj = nullptr;
		if (!Item.IsValid() || !Item->TryGetObject(Obj) || !Obj || !Obj->IsValid())
		{
			continue;
		}
		FAurixChatReactionTally Tally;
		(*Obj)->TryGetStringField(TEXT("reaction"), Tally.Reaction);
		(*Obj)->TryGetNumberField(TEXT("count"), Tally.Count);
		const TArray<TSharedPtr<FJsonValue>>* Users = nullptr;
		if ((*Obj)->TryGetArrayField(TEXT("user_ids"), Users) && Users)
		{
			for (const TSharedPtr<FJsonValue>& U : *Users)
			{
				FString Id;
				FGuid Guid;
				if (U.IsValid() && U->TryGetString(Id) && FGuid::Parse(Id, Guid))
				{
					Tally.UserIds.Add(Guid);
				}
			}
		}
		Out.Add(MoveTemp(Tally));
	}
	return Out;
}

FAurixChatMessage ToChatMessage(const AurixChatMessage& M)
{
	FAurixChatMessage Out;
	Out.MessageId = ToGuid(M.message_id);
	Out.ChannelId = ToGuid(M.channel_id);
	Out.SenderId = ToGuid(M.sender_id);
	Out.RecipientId = ToGuid(M.recipient_id);
	Out.SenderName = FromUtf8(M.sender_name);
	Out.Text = FromUtf8(M.text);
	Out.MetadataJson = FromUtf8(M.metadata_json);
	Out.SentAt = FromUnixMs(M.sent_at_ms);
	Out.RequestId = static_cast<int64>(M.request_id);
	Out.bOffline = M.offline;
	Out.Cursor = FromUtf8(M.cursor);
	Out.bEdited = M.edited_at_ms != 0;
	Out.EditedAt = FromUnixMs(M.edited_at_ms);
	Out.bDeleted = M.deleted_at_ms != 0;
	Out.DeletedAt = FromUnixMs(M.deleted_at_ms);
	Out.DeletedBy = ToGuid(M.deleted_by);
	Out.Reactions = ParseReactions(M.reactions_json);
	return Out;
}

FAurixChatReactionChange ToReactionChange(const AurixChatReaction& R)
{
	FAurixChatReactionChange Out;
	Out.MessageId = ToGuid(R.message_id);
	Out.ChannelId = ToGuid(R.channel_id);
	Out.MessageSenderId = ToGuid(R.message_sender_id);
	Out.MessageRecipientId = ToGuid(R.message_recipient_id);
	Out.UserId = ToGuid(R.user_id);
	Out.Reaction = FromUtf8(R.reaction);
	Out.bAdded = R.added;
	Out.Count = static_cast<int32>(R.count);
	Out.Timestamp = FromUnixMs(R.timestamp_ms);
	return Out;
}

FAurixChatHistoryPage ReadHistoryPage(const AurixEvent* Raw, const AurixChatHistory& H, FGuid ChannelId, FGuid UserId)
{
	FAurixChatHistoryPage Page;
	Page.ChannelId = ChannelId;
	Page.PeerUserId = UserId;
	Page.NextBefore = FromUtf8(H.next_before);
	Page.NextAfter = FromUtf8(H.next_after);
	Page.Messages.Reserve(static_cast<int32>(H.count));
	for (size_t i = 0; i < H.count; ++i)
	{
		AurixChatMessage M;
		if (aurix_event_chat_history_message(Raw, i, &M))
		{
			Page.Messages.Add(ToChatMessage(M));
		}
	}
	return Page;
}

FAurixReadMarker ToReadMarker(const AurixReadMarker& M)
{
	FAurixReadMarker Out;
	Out.UserId = ToGuid(M.user_id);
	Out.ChannelId = ToGuid(M.channel_id);
	Out.PeerUserId = ToGuid(M.peer_user_id);
	Out.MessageId = ToGuid(M.message_id);
	Out.MessageSentAt = FromUnixMs(M.message_sent_at_ms);
	Out.ReadAt = FromUnixMs(M.read_at_ms);
	return Out;
}

FAurixParticipant ToParticipant(const AurixParticipant& P)
{
	FAurixParticipant Out;
	Out.UserId = ToGuid(P.user_id);
	Out.DisplayName = FromUtf8(P.display_name);
	Out.Ssrc = static_cast<int64>(P.ssrc);
	Out.Role = ToRole(P.role);
	Out.bMuted = P.muted;
	Out.bServerMuted = P.server_muted;
	Out.bSpeaking = P.speaking;
	Out.Energy = P.energy;
	Out.bPriority = P.priority;
	return Out;
}

TArray<FAurixParticipant> ToParticipants(const std::vector<AurixParticipant>& In)
{
	TArray<FAurixParticipant> Out;
	Out.Reserve(static_cast<int32>(In.size()));
	for (const AurixParticipant& P : In)
	{
		Out.Add(ToParticipant(P));
	}
	return Out;
}

FAurixNetworkQuality ToNetworkQuality(const AurixNetworkQuality& Q)
{
	FAurixNetworkQuality Out;
	Out.Bars = static_cast<int32>(Q.bars);
	Out.RFactor = Q.r_factor;
	Out.Mos = Q.mos;
	Out.RttMs = Q.rtt_ms;
	Out.DownlinkJitterMs = Q.downlink_jitter_ms;
	Out.DownlinkLossPercent = Q.downlink_loss_percent;
	Out.UplinkJitterMs = Q.uplink_jitter_ms;
	Out.UplinkLossPercent = Q.uplink_loss_percent;
	Out.ReceiversLossPercent = Q.receivers_loss_percent;
	Out.UplinkBitrateKbps = static_cast<int32>(Q.uplink_bitrate_kbps);
	Out.UplinkPacketsReceived = static_cast<int64>(Q.uplink_packets_received);
	Out.UplinkPacketsLost = static_cast<int64>(Q.uplink_packets_lost);
	return Out;
}

AurixOpusBandwidth ToNativeBandwidth(EAurixOpusBandwidth B)
{
	switch (B)
	{
	case EAurixOpusBandwidth::Narrowband: return AURIX_BANDWIDTH_NARROWBAND;
	case EAurixOpusBandwidth::Mediumband: return AURIX_BANDWIDTH_MEDIUMBAND;
	case EAurixOpusBandwidth::Wideband: return AURIX_BANDWIDTH_WIDEBAND;
	case EAurixOpusBandwidth::Superwideband: return AURIX_BANDWIDTH_SUPERWIDEBAND;
	default: return AURIX_BANDWIDTH_FULLBAND;
	}
}

EAurixOpusBandwidth FromNativeBandwidth(AurixOpusBandwidth B)
{
	switch (B)
	{
	case AURIX_BANDWIDTH_NARROWBAND: return EAurixOpusBandwidth::Narrowband;
	case AURIX_BANDWIDTH_MEDIUMBAND: return EAurixOpusBandwidth::Mediumband;
	case AURIX_BANDWIDTH_WIDEBAND: return EAurixOpusBandwidth::Wideband;
	case AURIX_BANDWIDTH_SUPERWIDEBAND: return EAurixOpusBandwidth::Superwideband;
	default: return EAurixOpusBandwidth::Fullband;
	}
}

AurixOpusSignal ToNativeSignal(EAurixOpusSignal S)
{
	switch (S)
	{
	case EAurixOpusSignal::Voice: return AURIX_SIGNAL_VOICE;
	case EAurixOpusSignal::Music: return AURIX_SIGNAL_MUSIC;
	default: return AURIX_SIGNAL_AUTO;
	}
}

EAurixOpusSignal FromNativeSignal(AurixOpusSignal S)
{
	switch (S)
	{
	case AURIX_SIGNAL_VOICE: return EAurixOpusSignal::Voice;
	case AURIX_SIGNAL_MUSIC: return EAurixOpusSignal::Music;
	default: return EAurixOpusSignal::Auto;
	}
}

AurixEncoderSettings ToNativeEncoderSettings(const FAurixEncoderSettings& S)
{
	AurixEncoderSettings Out;
	Out.bitrate_bps = static_cast<uint32_t>(FMath::Max(1, S.BitrateBps));
	Out.complexity = static_cast<uint8_t>(FMath::Clamp(S.Complexity, 0, 10));
	Out.max_bandwidth = ToNativeBandwidth(S.MaxBandwidth);
	Out.signal = ToNativeSignal(S.Signal);
	Out.vbr = S.bVbr;
	Out.constrained_vbr = S.bConstrainedVbr;
	Out.fec = S.bFec;
	Out.expected_loss_percent = static_cast<uint8_t>(FMath::Clamp(S.ExpectedLossPercent, 0, 100));
	Out.dtx = S.bDtx;
	Out.channels = S.bStereo ? 2 : 1;
	Out.dred_duration_ms = static_cast<uint16_t>(FMath::Clamp(S.DredDurationMs, 0, 1040));
	return Out;
}

FAurixEncoderSettings FromNativeEncoderSettings(const AurixEncoderSettings& S)
{
	FAurixEncoderSettings Out;
	Out.BitrateBps = static_cast<int32>(S.bitrate_bps);
	Out.Complexity = static_cast<int32>(S.complexity);
	Out.MaxBandwidth = FromNativeBandwidth(S.max_bandwidth);
	Out.Signal = FromNativeSignal(S.signal);
	Out.bVbr = S.vbr;
	Out.bConstrainedVbr = S.constrained_vbr;
	Out.bFec = S.fec;
	Out.ExpectedLossPercent = static_cast<int32>(S.expected_loss_percent);
	Out.bDtx = S.dtx;
	Out.bStereo = S.channels == 2;
	Out.DredDurationMs = static_cast<int32>(S.dred_duration_ms);
	return Out;
}

AurixLossAdaptation ToNativeLossAdaptation(EAurixLossAdaptation A)
{
	switch (A)
	{
	case EAurixLossAdaptation::FixedLow: return AURIX_LOSS_ADAPTATION_FIXED_LOW;
	case EAurixLossAdaptation::FixedModerate: return AURIX_LOSS_ADAPTATION_FIXED_MODERATE;
	case EAurixLossAdaptation::FixedHigh: return AURIX_LOSS_ADAPTATION_FIXED_HIGH;
	default: return AURIX_LOSS_ADAPTATION_AUTO;
	}
}

EAurixLossAdaptation FromNativeLossAdaptation(AurixLossAdaptation A)
{
	switch (A)
	{
	case AURIX_LOSS_ADAPTATION_FIXED_LOW: return EAurixLossAdaptation::FixedLow;
	case AURIX_LOSS_ADAPTATION_FIXED_MODERATE: return EAurixLossAdaptation::FixedModerate;
	case AURIX_LOSS_ADAPTATION_FIXED_HIGH: return EAurixLossAdaptation::FixedHigh;
	default: return EAurixLossAdaptation::Auto;
	}
}

EAurixLossProfile FromNativeLossProfile(AurixLossProfile P)
{
	switch (P)
	{
	case AURIX_LOSS_PROFILE_MODERATE: return EAurixLossProfile::Moderate;
	case AURIX_LOSS_PROFILE_HIGH: return EAurixLossProfile::High;
	default: return EAurixLossProfile::Low;
	}
}

AurixDecoderSettings ToNativeDecoderSettings(const FAurixDecoderSettings& S)
{
	AurixDecoderSettings Out;
	Out.complexity = static_cast<uint8_t>(FMath::Clamp(S.Complexity, 0, 10));
	Out.osce_bwe = S.bOsceBwe;
	return Out;
}

FAurixDecoderSettings FromNativeDecoderSettings(const AurixDecoderSettings& S)
{
	FAurixDecoderSettings Out;
	Out.Complexity = static_cast<int32>(S.complexity);
	Out.bOsceBwe = S.osce_bwe;
	return Out;
}

AurixNoiseSuppression ToNativeNoiseSuppression(EAurixNoiseSuppression N)
{
	switch (N)
	{
	case EAurixNoiseSuppression::Off: return AURIX_NOISE_SUPPRESSION_OFF;
	case EAurixNoiseSuppression::Low: return AURIX_NOISE_SUPPRESSION_LOW;
	case EAurixNoiseSuppression::Moderate: return AURIX_NOISE_SUPPRESSION_MODERATE;
	default: return AURIX_NOISE_SUPPRESSION_HIGH;
	}
}

EAurixNoiseSuppression FromNativeNoiseSuppression(AurixNoiseSuppression N)
{
	switch (N)
	{
	case AURIX_NOISE_SUPPRESSION_OFF: return EAurixNoiseSuppression::Off;
	case AURIX_NOISE_SUPPRESSION_LOW: return EAurixNoiseSuppression::Low;
	case AURIX_NOISE_SUPPRESSION_MODERATE: return EAurixNoiseSuppression::Moderate;
	default: return EAurixNoiseSuppression::High;
	}
}

AurixDspConfig ToNativeDspConfig(const FAurixDspSettings& S)
{
	AurixDspConfig Out;
	aurix_dsp_config_default(&Out);
	Out.high_pass = S.bHighPass;
	Out.echo_cancellation = S.bEchoCancellation;
	Out.echo_tail_ms = static_cast<uint32_t>(FMath::Max(0, S.EchoTailMs));
	Out.stream_delay_ms = static_cast<uint32_t>(FMath::Max(0, S.StreamDelayMs));
	Out.noise_suppression = ToNativeNoiseSuppression(S.NoiseSuppression);
	Out.agc = S.bAgc;
	Out.agc_target_dbfs = S.AgcTargetDbfs;
	Out.agc_max_gain_db = S.AgcMaxGainDb;
	return Out;
}

FAurixDspSettings FromNativeDspConfig(const AurixDspConfig& C)
{
	FAurixDspSettings Out;
	Out.bHighPass = C.high_pass;
	Out.bEchoCancellation = C.echo_cancellation;
	Out.EchoTailMs = static_cast<int32>(C.echo_tail_ms);
	Out.StreamDelayMs = static_cast<int32>(C.stream_delay_ms);
	Out.NoiseSuppression = FromNativeNoiseSuppression(C.noise_suppression);
	Out.bAgc = C.agc;
	Out.AgcTargetDbfs = C.agc_target_dbfs;
	Out.AgcMaxGainDb = C.agc_max_gain_db;
	return Out;
}

FAurixDspStats ToDspStats(const AurixDspStats& S)
{
	FAurixDspStats Out;
	Out.ErleDb = S.erle_db;
	Out.EchoDelayMs = static_cast<int32>(S.echo_delay_ms);
	Out.bEchoConverged = S.echo_converged;
	Out.bFarEndActive = S.far_end_active;
	Out.SpeechProbability = S.speech_probability;
	Out.AgcGainDb = S.agc_gain_db;
	Out.FarEndUnderruns = static_cast<int64>(S.far_end_underruns);
	return Out;
}

FAurixAudioPolicy ToAudioPolicy(const AurixAudioPolicy& P)
{
	FAurixAudioPolicy Out;
	Out.BitrateBps = static_cast<int32>(P.bitrate_bps);
	Out.MinBitrateBps = static_cast<int32>(P.min_bitrate_bps);
	Out.bFec = P.fec;
	Out.bDtx = P.dtx;
	Out.MaxBandwidth = FromNativeBandwidth(P.max_bandwidth);
	Out.Complexity = static_cast<int32>(P.complexity);
	Out.Signal = FromNativeSignal(P.signal);
	Out.bStereo = P.stereo;
	return Out;
}

FAurixStats ToStats(const AurixStats& S)
{
	FAurixStats Out;
	Out.PacketsSent = static_cast<int64>(S.packets_sent);
	Out.BytesSent = static_cast<int64>(S.bytes_sent);
	Out.PacketsReceived = static_cast<int64>(S.packets_received);
	Out.BytesReceived = static_cast<int64>(S.bytes_received);
	Out.AudioFramesReceived = static_cast<int64>(S.audio_frames_received);
	Out.BadAuth = static_cast<int64>(S.bad_auth);
	Out.Replayed = static_cast<int64>(S.replayed);
	Out.HeartbeatsLost = static_cast<int64>(S.heartbeats_lost);
	Out.HeartbeatsLostConsecutive = static_cast<int32>(S.heartbeats_lost_consecutive);
	Out.MediaPath = ToMediaPath(S.media_path);
	Out.UplinkDropped = static_cast<int64>(S.uplink_dropped);
	Out.FramesE2ee = static_cast<int64>(S.frames_e2ee);
	Out.E2eeUndecryptable = static_cast<int64>(S.e2ee_undecryptable);
	Out.FramesLost = static_cast<int64>(S.frames_lost);
	Out.FramesLate = static_cast<int64>(S.frames_late);
	Out.Underruns = static_cast<int64>(S.underruns);
	Out.RttMs = S.rtt_ms;
	Out.RttMinMs = S.rtt_min_ms;
	Out.RttAvgMs = S.rtt_avg_ms;
	Out.RttMaxMs = S.rtt_max_ms;
	Out.JitterMs = S.jitter_ms;
	Out.LossPercent = S.loss_percent;
	Out.RFactor = S.r_factor;
	Out.Mos = S.mos;
	Out.Bars = static_cast<int32>(S.bars);
	Out.bHasServer = S.has_server;
	Out.Server = ToNetworkQuality(S.server);
	Out.FramesEncoded = static_cast<int64>(S.frames_encoded);
	Out.FramesSent = static_cast<int64>(S.frames_sent);
	Out.FramesGated = static_cast<int64>(S.frames_gated);
	Out.ActiveStreams = static_cast<int32>(S.active_streams);
	return Out;
}

const TCHAR* ResultName(AurixResult R)
{
	switch (R)
	{
	case AURIX_OK: return TEXT("ok");
	case AURIX_NULL_POINTER: return TEXT("null pointer");
	case AURIX_INVALID_ARGUMENT: return TEXT("invalid argument");
	case AURIX_NOT_CONNECTED: return TEXT("not connected");
	case AURIX_CLOSED: return TEXT("closed");
	case AURIX_TRANSPORT: return TEXT("transport");
	case AURIX_UNAUTHORIZED: return TEXT("unauthorized");
	case AURIX_TIMEOUT: return TEXT("timeout");
	case AURIX_SERVER_REJECTED: return TEXT("server rejected");
	case AURIX_CODEC: return TEXT("codec");
	case AURIX_PROTOCOL: return TEXT("protocol");
	default: return TEXT("unknown");
	}
}

bool Check(AurixResult R, const TCHAR* What)
{
	if (R == AURIX_OK)
	{
		return true;
	}
	UE_LOG(LogAurixVoice, Warning, TEXT("%s failed (%s): %s"), What, ResultName(R), *FromUtf8(aurix_last_error()));
	return false;
}
} // namespace

UAurixVoiceSubsystem::UAurixVoiceSubsystem() = default;
UAurixVoiceSubsystem::~UAurixVoiceSubsystem() = default;

void UAurixVoiceSubsystem::Initialize(FSubsystemCollectionBase& Collection)
{
	Super::Initialize(Collection);
	PostLoadMapHandle = FCoreUObjectDelegates::PostLoadMapWithWorld.AddUObject(this, &UAurixVoiceSubsystem::OnPostLoadMap);
}

void UAurixVoiceSubsystem::Deinitialize()
{
	FCoreUObjectDelegates::PostLoadMapWithWorld.Remove(PostLoadMapHandle);
	CancelRegionDiscovery();
	ReleaseNative();
	Super::Deinitialize();
}

bool UAurixVoiceSubsystem::IsTickable() const
{
	return !IsTemplate() && Native.IsValid();
}

void UAurixVoiceSubsystem::Tick(float /*DeltaTime*/)
{
	PumpEvents();
}

// ---- lifecycle -----------------------------------------------------------------------------

bool UAurixVoiceSubsystem::Connect(const FAurixVoiceSettings& Settings)
{
	ReleaseNative();
	ActiveSettings = Settings;

	aurix::Config Cfg(ToUtf8(Settings.WebSocketUrl), ToUtf8(Settings.Token));
	Cfg.raw.auto_reconnect = Settings.bAutoReconnect;
	Cfg.raw.reconnect_max_attempts = static_cast<uint32_t>(FMath::Max(0, Settings.ReconnectMaxAttempts));
	Cfg.raw.reconnect_initial_delay_ms = static_cast<uint32_t>(FMath::Max(1, Settings.ReconnectInitialDelayMs));
	Cfg.raw.reconnect_max_delay_ms = static_cast<uint32_t>(FMath::Max(1, Settings.ReconnectMaxDelayMs));
	Cfg.raw.request_timeout_ms = static_cast<uint32_t>(FMath::Max(1, Settings.RequestTimeoutMs));
	Cfg.raw.encoder = ToNativeEncoderSettings(Settings.Encoder);
	Cfg.raw.dsp = ToNativeDspConfig(Settings.Dsp);
	Cfg.raw.decoder = ToNativeDecoderSettings(Settings.Decoder);
	Cfg.raw.loss_adaptation = ToNativeLossAdaptation(Settings.LossAdaptation);
	Cfg.raw.follow_channel_policy = Settings.bFollowChannelPolicy;
	Cfg.raw.jitter_target_frames = static_cast<uint32_t>(FMath::Max(1, Settings.JitterTargetFrames));
	Cfg.raw.jitter_max_frames = static_cast<uint32_t>(FMath::Max(1, Settings.JitterMaxFrames));
	Cfg.raw.vad_gate = Settings.bVadGate;
	Cfg.raw.media_path = FromMediaPathPolicy(Settings.MediaPath);
	Cfg.raw.quic = Settings.bQuic;
	Cfg.raw.tls_tunnel = Settings.bTlsTunnel;
	Cfg.raw.udp_fallback_lost_heartbeats = static_cast<uint32_t>(FMath::Max(0, Settings.UdpFallbackLostHeartbeats));
	Cfg.raw.udp_reprobe_interval_ms = static_cast<uint32_t>(FMath::Max(0, Settings.UdpReprobeIntervalMs));
	Cfg.raw.e2ee = Settings.bE2ee;
	Cfg.raw.has_e2ee_identity = false;
	if (!Settings.E2eeIdentityHex.IsEmpty())
	{
		uint8 Secret[32] = {};
		if (DecodeHex(Settings.E2eeIdentityHex, Secret, sizeof(Secret)))
		{
			FMemory::Memcpy(Cfg.raw.e2ee_identity, Secret, sizeof(Secret));
			Cfg.raw.has_e2ee_identity = true;
		}
		else
		{
			UE_LOG(LogAurixVoice, Warning, TEXT("E2eeIdentityHex must be 64 hex characters; using a fresh identity"));
		}
	}

	TUniquePtr<FAurixNativeClient> Created = MakeUnique<FAurixNativeClient>();
	Created->Client = aurix::Client::create(Cfg);
	if (!Created->Client)
	{
		UE_LOG(LogAurixVoice, Error, TEXT("aurix_client_create failed: %s"), *FromUtf8(aurix_last_error()));
		return false;
	}
	if (!Check(Created->Client.connect(), TEXT("connect")))
	{
		return false;
	}
	Native = MoveTemp(Created);

	// Participant sounds and claims outlive the native client (reconnect with a new token).
	for (const TPair<FGuid, TObjectPtr<UAurixParticipantSoundWave>>& Pair : ParticipantSounds)
	{
		if (Pair.Value)
		{
			Pair.Value->SetSource(Native->Client.raw(), Pair.Key);
		}
		Native->Client.set_participant_claimed(ToUuid(Pair.Key), true);
	}
	for (const FGuid& UserId : ManualClaims)
	{
		Native->Client.set_participant_claimed(ToUuid(UserId), true);
	}

	if (Settings.bAutoStartPlayback)
	{
		StartPlayback();
	}
	return true;
}

void UAurixVoiceSubsystem::Disconnect()
{
	ReleaseNative();
}

void UAurixVoiceSubsystem::ReleaseNative()
{
	bPlaybackRequested = false;
	if (Capture)
	{
		Capture->Stop();
	}
	if (SoundWave)
	{
		SoundWave->SetClient(nullptr);
	}
	for (const TPair<FGuid, TObjectPtr<UAurixParticipantSoundWave>>& Pair : ParticipantSounds)
	{
		if (Pair.Value)
		{
			Pair.Value->SetSource(nullptr, Pair.Key);
		}
	}
	if (PlaybackComponent)
	{
		PlaybackComponent->Stop();
	}
	if (Native)
	{
		// Destroying the wrapper disconnects (brief wait for the server's ack) and frees the client.
		Native.Reset();
	}
}

bool UAurixVoiceSubsystem::SetToken(const FString& Token)
{
	ActiveSettings.Token = Token;
	if (!Native)
	{
		return true;
	}
	return Check(Native->Client.set_token(ToUtf8(Token)), TEXT("set_token"));
}

EAurixConnectionState UAurixVoiceSubsystem::GetConnectionState() const
{
	return Native ? ToState(Native->Client.state()) : EAurixConnectionState::Disconnected;
}

bool UAurixVoiceSubsystem::IsConnected() const
{
	const EAurixConnectionState S = GetConnectionState();
	return S == EAurixConnectionState::Connected || S == EAurixConnectionState::MediaBound;
}

bool UAurixVoiceSubsystem::GetSession(FAurixSessionInfo& OutSession) const
{
	AurixSessionInfo Raw;
	if (!Native || !Native->Client.session(Raw))
	{
		OutSession = FAurixSessionInfo();
		return false;
	}
	OutSession = ToSession(Raw);
	return true;
}

FString UAurixVoiceSubsystem::GetLastError() const
{
	return FromUtf8(aurix_last_error());
}

FString UAurixVoiceSubsystem::GetNativeVersion()
{
	return FromUtf8(aurix_version());
}

// ---- channels ------------------------------------------------------------------------------

bool UAurixVoiceSubsystem::JoinChannel(FGuid ChannelId, const FString& JoinToken, int64& RequestId)
{
	RequestId = 0;
	if (!Native)
	{
		return false;
	}
	uint64_t Id = 0;
	const std::string Token = ToUtf8(JoinToken);
	const bool bOk = Check(Native->Client.join_channel(ToUuid(ChannelId), JoinToken.IsEmpty() ? nullptr : Token.c_str(), &Id), TEXT("join_channel"));
	RequestId = static_cast<int64>(Id);
	return bOk;
}

bool UAurixVoiceSubsystem::LeaveChannel(FGuid ChannelId)
{
	return Native && Check(Native->Client.leave_channel(ToUuid(ChannelId)), TEXT("leave_channel"));
}

TArray<FGuid> UAurixVoiceSubsystem::GetJoinedChannels() const
{
	TArray<FGuid> Out;
	if (Native)
	{
		for (const aurix::Uuid& U : Native->Client.joined_channels())
		{
			Out.Add(ToGuid(U.raw));
		}
	}
	return Out;
}

TArray<FAurixParticipant> UAurixVoiceSubsystem::GetParticipants(FGuid ChannelId) const
{
	return Native ? ToParticipants(Native->Client.participants(ToUuid(ChannelId))) : TArray<FAurixParticipant>();
}

bool UAurixVoiceSubsystem::IsChannelTranscribed(FGuid ChannelId) const
{
	return Native && Native->Client.channel_transcribes(ToUuid(ChannelId));
}

bool UAurixVoiceSubsystem::IsChannelMonitored(FGuid ChannelId) const
{
	return Native && Native->Client.channel_monitored(ToUuid(ChannelId));
}

bool UAurixVoiceSubsystem::GetChannelScope(FGuid ChannelId, FAurixChannelScope& OutScope) const
{
	AurixChannelScope Raw;
	if (!Native || !Native->Client.channel_scope(ToUuid(ChannelId), Raw))
	{
		OutScope = FAurixChannelScope();
		return false;
	}
	OutScope.RosterRadius = Raw.roster_radius;
	OutScope.TextRadius = Raw.text_radius;
	return true;
}

bool UAurixVoiceSubsystem::GetChannelInfo(FGuid ChannelId, FAurixChannelInfo& OutInfo) const
{
	AurixChannelInfo Raw;
	if (!Native || !Native->Client.channel_info(ToUuid(ChannelId), Raw))
	{
		OutInfo = FAurixChannelInfo();
		return false;
	}
	OutInfo = ToChannelInfo(Raw);
	return true;
}

bool UAurixVoiceSubsystem::CanSpeakIn(FGuid ChannelId) const
{
	return Native && Native->Client.can_speak_in(ToUuid(ChannelId));
}

bool UAurixVoiceSubsystem::GetUserForSsrc(int64 Ssrc, FGuid& OutUserId) const
{
	OutUserId.Invalidate();
	aurix::Uuid U;
	if (!Native || Ssrc < 0 || Ssrc > static_cast<int64>(MAX_uint32) || !Native->Client.user_for_ssrc(static_cast<uint32_t>(Ssrc), U))
	{
		return false;
	}
	OutUserId = ToGuid(U.raw);
	return true;
}

// ---- microphone ----------------------------------------------------------------------------

TArray<FString> UAurixVoiceSubsystem::GetCaptureDevices()
{
	return FAurixAudioCapture::ListDevices();
}

bool UAurixVoiceSubsystem::StartCapture(int32 DeviceIndex)
{
	if (!Native)
	{
		UE_LOG(LogAurixVoice, Warning, TEXT("StartCapture: not connected"));
		return false;
	}
	if (!Capture)
	{
		Capture = MakeUnique<FAurixAudioCapture>();
	}
	Native->Client.reset_capture();
	return Capture->Start(Native->Client.raw(), DeviceIndex);
}

void UAurixVoiceSubsystem::StopCapture()
{
	if (Capture)
	{
		Capture->Stop();
	}
}

bool UAurixVoiceSubsystem::IsCapturing() const
{
	return Capture && Capture->IsCapturing();
}

void UAurixVoiceSubsystem::PushCaptureAudio(const TArray<float>& InterleavedPcm, int32 SampleRate, int32 Channels)
{
	if (Native && SampleRate > 0 && Channels > 0 && Channels <= 255 && InterleavedPcm.Num() > 0)
	{
		Native->Client.push_capture(InterleavedPcm.GetData(), static_cast<size_t>(InterleavedPcm.Num()), static_cast<uint32_t>(SampleRate), static_cast<uint8_t>(Channels));
	}
}

void UAurixVoiceSubsystem::SetMuted(bool bMuted)
{
	if (Native)
	{
		Native->Client.set_muted(bMuted);
	}
}

bool UAurixVoiceSubsystem::IsMuted() const
{
	return Native && Native->Client.is_muted();
}

bool UAurixVoiceSubsystem::IsSpeaking() const
{
	return Native && Native->Client.is_speaking();
}

void UAurixVoiceSubsystem::SetInputGain(float Gain)
{
	if (Native)
	{
		Native->Client.set_input_gain(Gain);
	}
}

float UAurixVoiceSubsystem::GetInputEnergy() const
{
	return Native ? Native->Client.input_energy() : 0.f;
}

void UAurixVoiceSubsystem::SetVoiceActivityDetector(float Threshold, int32 HangoverFrames)
{
	if (Native)
	{
		Native->Client.set_vad(Threshold, static_cast<uint32_t>(FMath::Max(0, HangoverFrames)));
	}
}

void UAurixVoiceSubsystem::SetVadGate(bool bEnabled)
{
	if (Native)
	{
		Native->Client.set_vad_gate(bEnabled);
	}
}

bool UAurixVoiceSubsystem::SetBitrate(int32 BitrateBps)
{
	return Native && BitrateBps > 0 && Check(Native->Client.set_bitrate(static_cast<uint32_t>(BitrateBps)), TEXT("set_bitrate"));
}

bool UAurixVoiceSubsystem::SetEncoderSettings(const FAurixEncoderSettings& Settings)
{
	return Native && Check(Native->Client.set_encoder_settings(ToNativeEncoderSettings(Settings)), TEXT("set_encoder_settings"));
}

bool UAurixVoiceSubsystem::GetEncoderSettings(FAurixEncoderSettings& OutSettings) const
{
	AurixEncoderSettings Raw;
	if (!Native || !Native->Client.encoder_settings(Raw))
	{
		OutSettings = FAurixEncoderSettings();
		return false;
	}
	OutSettings = FromNativeEncoderSettings(Raw);
	return true;
}

bool UAurixVoiceSubsystem::SetLossAdaptation(EAurixLossAdaptation Adaptation)
{
	return Native && Check(Native->Client.set_loss_adaptation(ToNativeLossAdaptation(Adaptation)), TEXT("set_loss_adaptation"));
}

EAurixLossAdaptation UAurixVoiceSubsystem::GetLossAdaptation() const
{
	return Native ? FromNativeLossAdaptation(Native->Client.loss_adaptation()) : EAurixLossAdaptation::Auto;
}

EAurixLossProfile UAurixVoiceSubsystem::GetLossProfile() const
{
	return Native ? FromNativeLossProfile(Native->Client.loss_profile()) : EAurixLossProfile::Low;
}

bool UAurixVoiceSubsystem::SetDecoderSettings(const FAurixDecoderSettings& Settings)
{
	return Native && Check(Native->Client.set_decoder_settings(ToNativeDecoderSettings(Settings)), TEXT("set_decoder_settings"));
}

bool UAurixVoiceSubsystem::GetDecoderSettings(FAurixDecoderSettings& OutSettings) const
{
	AurixDecoderSettings Raw;
	if (!Native || !Native->Client.decoder_settings(Raw))
	{
		OutSettings = FAurixDecoderSettings();
		return false;
	}
	OutSettings = FromNativeDecoderSettings(Raw);
	return true;
}

bool UAurixVoiceSubsystem::IsDredSupported()
{
	return aurix::Client::dred_supported();
}

bool UAurixVoiceSubsystem::SetComplexity(int32 Complexity)
{
	const int8_t Pinned = Complexity < 0 ? int8_t(-1) : static_cast<int8_t>(FMath::Min(Complexity, 10));
	return Native && Check(Native->Client.set_complexity(Pinned), TEXT("set_complexity"));
}

bool UAurixVoiceSubsystem::SetDspSettings(const FAurixDspSettings& Settings)
{
	return Native && Check(Native->Client.set_dsp(ToNativeDspConfig(Settings)), TEXT("set_dsp"));
}

bool UAurixVoiceSubsystem::GetDspSettings(FAurixDspSettings& OutSettings) const
{
	AurixDspConfig Raw;
	if (!Native || !Native->Client.dsp(Raw))
	{
		OutSettings = FAurixDspSettings();
		return false;
	}
	OutSettings = FromNativeDspConfig(Raw);
	return true;
}

bool UAurixVoiceSubsystem::GetDspStats(FAurixDspStats& OutStats) const
{
	AurixDspStats Raw;
	if (!Native || !Native->Client.dsp_stats(Raw))
	{
		OutStats = FAurixDspStats();
		return false;
	}
	OutStats = ToDspStats(Raw);
	return true;
}

bool UAurixVoiceSubsystem::SetVoiceEffects(const FAurixVoiceEffects& Effects)
{
	return Native && Check(Native->Client.set_voice_effects(ToRawEffects(Effects)), TEXT("set_voice_effects"));
}

bool UAurixVoiceSubsystem::GetVoiceEffects(FAurixVoiceEffects& OutEffects) const
{
	AurixVoiceEffects Raw;
	if (!Native || !Native->Client.voice_effects(Raw))
	{
		OutEffects = FAurixVoiceEffects();
		return false;
	}
	OutEffects = ToEffects(Raw);
	return true;
}

FAurixVoiceEffects UAurixVoiceSubsystem::MakeVoicePreset(EAurixVoicePreset Preset)
{
	return ToEffects(aurix::Client::voice_preset(ToRawPreset(Preset)));
}

bool UAurixVoiceSubsystem::SetVoicePreset(EAurixVoicePreset Preset)
{
	return Native && Check(Native->Client.set_voice_preset(ToRawPreset(Preset)), TEXT("set_voice_preset"));
}

bool UAurixVoiceSubsystem::SetVisemesEnabled(bool bEnabled)
{
	return Native && Check(Native->Client.set_visemes(bEnabled), TEXT("set_visemes"));
}

bool UAurixVoiceSubsystem::AreVisemesEnabled() const
{
	return Native && Native->Client.visemes_enabled();
}

bool UAurixVoiceSubsystem::GetParticipantVisemes(FGuid UserId, FAurixVisemeFrame& OutFrame) const
{
	AurixVisemeFrame Raw;
	if (!Native || !UserId.IsValid() || !Native->Client.participant_visemes(ToUuid(UserId), Raw))
	{
		OutFrame = FAurixVisemeFrame();
		return false;
	}
	OutFrame = ToVisemeFrame(Raw);
	return true;
}

bool UAurixVoiceSubsystem::GetLocalVisemes(FAurixVisemeFrame& OutFrame) const
{
	AurixVisemeFrame Raw;
	if (!Native || !Native->Client.local_visemes(Raw))
	{
		OutFrame = FAurixVisemeFrame();
		return false;
	}
	OutFrame = ToVisemeFrame(Raw);
	return true;
}

bool UAurixVoiceSubsystem::SetVoiceEffectCallback(FAurixVoiceEffectFn Callback, void* UserData)
{
	return Native && Check(Native->Client.set_voice_effect_callback(reinterpret_cast<AurixVoiceEffectFn>(Callback), UserData), TEXT("set_voice_effect_callback"));
}

void UAurixVoiceSubsystem::PushRenderAudio(const TArray<float>& InterleavedPcm, int32 Channels)
{
	if (!Native || Channels < 1 || Channels > 2 || InterleavedPcm.Num() == 0)
	{
		return;
	}
	Native->Client.push_render(InterleavedPcm.GetData(), static_cast<size_t>(InterleavedPcm.Num()), static_cast<uint8_t>(Channels));
}

bool UAurixVoiceSubsystem::GetAudioPolicy(FAurixAudioPolicy& OutPolicy) const
{
	AurixAudioPolicy Raw;
	if (!Native || !Native->Client.audio_policy(Raw))
	{
		OutPolicy = FAurixAudioPolicy();
		return false;
	}
	OutPolicy = ToAudioPolicy(Raw);
	return true;
}

// ---- playback ------------------------------------------------------------------------------

bool UAurixVoiceSubsystem::StartPlayback()
{
	bPlaybackRequested = true;
	if (!Native)
	{
		return false;
	}
	UGameInstance* GameInstance = GetGameInstance();
	UWorld* World = GameInstance ? GameInstance->GetWorld() : nullptr;
	if (!World)
	{
		// No world yet (Connect called during startup): OnPostLoadMap retries.
		return false;
	}
	if (!SoundWave)
	{
		SoundWave = NewObject<UAurixVoiceSoundWave>(this, TEXT("AurixVoiceMix"));
	}
	SoundWave->SoundClassObject = ActiveSettings.PlaybackSoundClass;
	SoundWave->SetClient(Native->Client.raw());

	if (!IsValid(PlaybackComponent))
	{
		PlaybackComponent = nullptr;
		PlaybackComponent = UGameplayStatics::CreateSound2D(World, SoundWave, 1.f, 1.f, 0.f, nullptr, /*bPersistAcrossLevelTransition*/ true, /*bAutoDestroy*/ false);
		if (!PlaybackComponent)
		{
			UE_LOG(LogAurixVoice, Warning, TEXT("StartPlayback: CreateSound2D failed (no audio device?)"));
			return false;
		}
		PlaybackComponent->bIsUISound = true;
		PlaybackComponent->bAllowSpatialization = false;
		PlaybackComponent->bAutoDestroy = false;
	}
	if (!PlaybackComponent->IsPlaying())
	{
		PlaybackComponent->Play();
	}
	return true;
}

void UAurixVoiceSubsystem::StopPlayback()
{
	bPlaybackRequested = false;
	if (PlaybackComponent)
	{
		PlaybackComponent->Stop();
	}
	if (SoundWave)
	{
		SoundWave->SetClient(nullptr);
	}
}

void UAurixVoiceSubsystem::OnPostLoadMap(UWorld* /*LoadedWorld*/)
{
	if (bPlaybackRequested && Native)
	{
		StartPlayback();
	}
}

int32 UAurixVoiceSubsystem::MixOutputAudio(TArray<float>& InterleavedPcm, int32 Channels)
{
	if (!Native || Channels < 1 || Channels > 2 || InterleavedPcm.Num() == 0)
	{
		return 0;
	}
	return static_cast<int32>(Native->Client.mix_output(InterleavedPcm.GetData(), static_cast<size_t>(InterleavedPcm.Num()), static_cast<uint8_t>(Channels)));
}

// ---- per-participant playback --------------------------------------------------------------

UAurixParticipantSoundWave* UAurixVoiceSubsystem::CreateParticipantSound(FGuid UserId, bool bStereo)
{
	if (!UserId.IsValid())
	{
		return nullptr;
	}
	if (const TObjectPtr<UAurixParticipantSoundWave>* Existing = ParticipantSounds.Find(UserId))
	{
		if (*Existing)
		{
			return *Existing;
		}
	}
	UAurixParticipantSoundWave* Wave = NewObject<UAurixParticipantSoundWave>(this, *FString::Printf(TEXT("AurixVoice_%s"), *UserId.ToString(EGuidFormats::Digits)));
	Wave->SetStereo(bStereo);
	Wave->SoundClassObject = ActiveSettings.PlaybackSoundClass;
	Wave->SetSource(Native ? Native->Client.raw() : nullptr, UserId);
	ParticipantSounds.Add(UserId, Wave);
	if (Native)
	{
		Native->Client.set_participant_claimed(ToUuid(UserId), true);
	}
	return Wave;
}

UAudioComponent* UAurixVoiceSubsystem::SpawnParticipantAudioComponent(FGuid UserId, USceneComponent* AttachTo, USoundAttenuation* Attenuation, bool bStereo)
{
	UAurixParticipantSoundWave* Wave = CreateParticipantSound(UserId, bStereo);
	if (!Wave || !AttachTo)
	{
		return nullptr;
	}
	UAudioComponent* Component = UGameplayStatics::SpawnSoundAttached(Wave, AttachTo, NAME_None, FVector::ZeroVector, FRotator::ZeroRotator, EAttachLocation::KeepRelativeOffset, /*bStopWhenAttachedToDestroyed*/ true, 1.f, 1.f, 0.f, Attenuation, nullptr, /*bAutoDestroy*/ false);
	if (!Component)
	{
		UE_LOG(LogAurixVoice, Warning, TEXT("SpawnParticipantAudioComponent: SpawnSoundAttached failed (no audio device?)"));
		return nullptr;
	}
	Component->bAllowSpatialization = true;
	Component->bIsUISound = false;
	if (!Component->IsPlaying())
	{
		Component->Play();
	}
	return Component;
}

void UAurixVoiceSubsystem::ReleaseParticipantSound(FGuid UserId)
{
	TObjectPtr<UAurixParticipantSoundWave> Wave;
	if (ParticipantSounds.RemoveAndCopyValue(UserId, Wave) && Wave)
	{
		Wave->SetSource(nullptr, UserId);
	}
	if (Native && !ManualClaims.Contains(UserId))
	{
		Native->Client.set_participant_claimed(ToUuid(UserId), false);
	}
}

UAurixParticipantSoundWave* UAurixVoiceSubsystem::GetParticipantSound(FGuid UserId) const
{
	const TObjectPtr<UAurixParticipantSoundWave>* Found = ParticipantSounds.Find(UserId);
	return Found ? Found->Get() : nullptr;
}

int32 UAurixVoiceSubsystem::PullParticipantAudio(FGuid UserId, TArray<float>& InterleavedPcm, int32 Channels)
{
	if (!Native || Channels < 1 || Channels > 2 || InterleavedPcm.Num() == 0)
	{
		return 0;
	}
	return static_cast<int32>(Native->Client.pull_participant(ToUuid(UserId), InterleavedPcm.GetData(), static_cast<size_t>(InterleavedPcm.Num()), static_cast<uint8_t>(Channels)));
}

void UAurixVoiceSubsystem::SetParticipantClaimed(FGuid UserId, bool bClaimed)
{
	if (!UserId.IsValid())
	{
		return;
	}
	if (bClaimed)
	{
		ManualClaims.Add(UserId);
	}
	else
	{
		ManualClaims.Remove(UserId);
	}
	if (Native)
	{
		// A participant sound keeps its own claim.
		Native->Client.set_participant_claimed(ToUuid(UserId), bClaimed || ParticipantSounds.Contains(UserId));
	}
}

TArray<FAurixParticipantStream> UAurixVoiceSubsystem::GetParticipantStreams() const
{
	TArray<FAurixParticipantStream> Out;
	if (!Native)
	{
		return Out;
	}
	for (const AurixParticipantStream& S : Native->Client.participant_streams())
	{
		FAurixParticipantStream Info;
		Info.Ssrc = static_cast<int64>(S.ssrc);
		Info.UserId = ToGuid(S.user_id);
		Info.bSynthesized = S.synthesized;
		Info.bMixed = S.mixed;
		Info.bStereo = S.stereo;
		Info.bActive = S.active;
		Info.BufferedFrames = static_cast<int32>(S.buffered_frames);
		Info.bClaimed = Info.UserId.IsValid() && (ParticipantSounds.Contains(Info.UserId) || ManualClaims.Contains(Info.UserId));
		Out.Add(Info);
	}
	return Out;
}

void UAurixVoiceSubsystem::SetOutputVolume(float Volume)
{
	if (Native)
	{
		Native->Client.set_output_volume(Volume);
	}
}

void UAurixVoiceSubsystem::SetOutputMuted(bool bMuted)
{
	if (Native)
	{
		Native->Client.set_output_muted(bMuted);
	}
}

// ---- receiver preferences ------------------------------------------------------------------

bool UAurixVoiceSubsystem::SetParticipantMuted(FGuid UserId, FGuid ChannelId, bool bMuted)
{
	if (!Native)
	{
		return false;
	}
	const aurix::Uuid Channel = ToUuid(ChannelId);
	return Check(Native->Client.set_participant_mute(ToUuid(UserId), ChannelId.IsValid() ? &Channel : nullptr, bMuted), TEXT("set_participant_mute"));
}

bool UAurixVoiceSubsystem::SetParticipantVolume(FGuid UserId, float Volume)
{
	return Native && Check(Native->Client.set_participant_volume(ToUuid(UserId), Volume), TEXT("set_participant_volume"));
}

bool UAurixVoiceSubsystem::SetUserBlocked(FGuid UserId, bool bBlocked)
{
	return Native && Check(Native->Client.set_user_block(ToUuid(UserId), bBlocked), TEXT("set_user_block"));
}

bool UAurixVoiceSubsystem::SetPriority(FGuid ChannelId, FGuid UserId, bool bPriority)
{
	if (!Native)
	{
		return false;
	}
	const aurix::Uuid User = ToUuid(UserId);
	return Check(Native->Client.set_priority(ToUuid(ChannelId), UserId.IsValid() ? &User : nullptr, bPriority), TEXT("set_priority"));
}

bool UAurixVoiceSubsystem::IsDuckingActive(FGuid ChannelId) const
{
	return Native && Native->Client.ducking_active(ToUuid(ChannelId));
}

bool UAurixVoiceSubsystem::SetTransmission(EAurixTransmissionMode Mode, FGuid ChannelId)
{
	if (!Native)
	{
		return false;
	}
	const aurix::Uuid Channel = ToUuid(ChannelId);
	return Check(Native->Client.set_transmission(FromTransmission(Mode), ChannelId.IsValid() ? &Channel : nullptr), TEXT("set_transmission"));
}

bool UAurixVoiceSubsystem::SetChannelFocus(FGuid ChannelId)
{
	if (!Native)
	{
		return false;
	}
	const aurix::Uuid Channel = ToUuid(ChannelId);
	return Check(Native->Client.set_channel_focus(ChannelId.IsValid() ? &Channel : nullptr), TEXT("set_channel_focus"));
}

bool UAurixVoiceSubsystem::SetAudioCodec(EAurixAudioCodec Codec)
{
	return Native && Check(Native->Client.set_audio_codec(FromCodec(Codec)), TEXT("set_audio_codec"));
}

EAurixAudioCodec UAurixVoiceSubsystem::GetAudioCodec() const
{
	return Native ? ToCodec(Native->Client.audio_codec()) : EAurixAudioCodec::Opus;
}

bool UAurixVoiceSubsystem::SetDownlinkMode(EAurixDownlinkMode Mode)
{
	return Native && Check(Native->Client.set_downlink_mode(FromDownlinkMode(Mode)), TEXT("set_downlink_mode"));
}

EAurixDownlinkMode UAurixVoiceSubsystem::GetDownlinkMode() const
{
	return Native ? ToDownlinkMode(Native->Client.downlink_mode()) : EAurixDownlinkMode::Streams;
}

EAurixMediaPath UAurixVoiceSubsystem::GetMediaPath() const
{
	return Native ? ToMediaPath(Native->Client.media_path()) : EAurixMediaPath::None;
}

bool UAurixVoiceSubsystem::NetworkChanged()
{
	return Native && Native->Client.network_changed();
}

FString UAurixVoiceSubsystem::GetEndpoint() const
{
	return Native ? FromUtf8(Native->Client.endpoint().c_str()) : FString();
}

TArray<FString> UAurixVoiceSubsystem::GetFailoverEndpoints() const
{
	TArray<FString> Out;
	if (Native)
	{
		for (const std::string& Url : Native->Client.failover_endpoints())
		{
			Out.Add(FromUtf8(Url.c_str()));
		}
	}
	return Out;
}

FString UAurixVoiceSubsystem::GetE2eeFingerprint() const
{
	return Native ? FromUtf8(Native->Client.e2ee_fingerprint().c_str()) : FString();
}

FString UAurixVoiceSubsystem::GetE2eePeerFingerprint(FGuid UserId) const
{
	return Native ? FromUtf8(Native->Client.e2ee_peer_fingerprint(ToUuid(UserId)).c_str()) : FString();
}

bool UAurixVoiceSubsystem::IsE2eePeerDecryptable(FGuid UserId) const
{
	return Native && Native->Client.e2ee_peer_decryptable(ToUuid(UserId));
}

bool UAurixVoiceSubsystem::SetTranscripts(bool bEnabled)
{
	return Native && Check(Native->Client.set_transcripts(bEnabled), TEXT("set_transcripts"));
}

bool UAurixVoiceSubsystem::SetTranslation(const FString& Language, const FString& SpokenLanguage, bool bSpeech)
{
	if (!Native)
	{
		return false;
	}
	const std::string LanguageUtf8 = ToUtf8(Language);
	const std::string SpokenUtf8 = ToUtf8(SpokenLanguage);
	return Check(
		Native->Client.set_translation(Language.IsEmpty() ? nullptr : LanguageUtf8.c_str(), SpokenLanguage.IsEmpty() ? nullptr : SpokenUtf8.c_str(), bSpeech),
		TEXT("set_translation"));
}

// ---- positional audio ----------------------------------------------------------------------

bool UAurixVoiceSubsystem::UpdatePositions(FGuid ChannelId, const TArray<FAurixPosition>& Positions)
{
	if (!Native || Positions.Num() == 0)
	{
		return false;
	}
	std::vector<AurixPosition> Raw;
	Raw.reserve(static_cast<size_t>(Positions.Num()));
	for (const FAurixPosition& P : Positions)
	{
		AurixPosition R;
		R.user_id = ToUuid(P.UserId).raw;
		R.x = static_cast<float>(P.Location.X);
		R.y = static_cast<float>(P.Location.Y);
		R.z = static_cast<float>(P.Location.Z);
		R.forward_x = static_cast<float>(P.Forward.X);
		R.forward_y = static_cast<float>(P.Forward.Y);
		R.forward_z = static_cast<float>(P.Forward.Z);
		R.up_x = static_cast<float>(P.Up.X);
		R.up_y = static_cast<float>(P.Up.Y);
		R.up_z = static_cast<float>(P.Up.Z);
		Raw.push_back(R);
	}
	return Check(Native->Client.update_positions(ToUuid(ChannelId), Raw.data(), Raw.size()), TEXT("update_positions"));
}

bool UAurixVoiceSubsystem::UpdateOwnPosition(FGuid ChannelId, FVector Location, FRotator Rotation, float WorldToMeters)
{
	FAurixSessionInfo Session;
	if (!GetSession(Session) || !Session.UserId.IsValid() || WorldToMeters <= 0.f)
	{
		return false;
	}
	FAurixPosition P;
	P.UserId = Session.UserId;
	P.Location = Location / WorldToMeters;
	P.Forward = Rotation.Vector();
	P.Up = Rotation.RotateVector(FVector::UpVector);
	return UpdatePositions(ChannelId, {P});
}

// ---- recording / raw -----------------------------------------------------------------------

bool UAurixVoiceSubsystem::RespondRecordingConsent(FGuid RecordingId, bool bAccept)
{
	return Native && Check(Native->Client.respond_recording_consent(ToUuid(RecordingId), bAccept ? AURIX_CONSENT_ACCEPTED : AURIX_CONSENT_DECLINED), TEXT("respond_recording_consent"));
}

bool UAurixVoiceSubsystem::SendControlJson(const FString& Json)
{
	return Native && Check(Native->Client.send_control_json(ToUtf8(Json)), TEXT("send_control_json"));
}

// ---- chat / moderation / speech ------------------------------------------------------------

bool UAurixVoiceSubsystem::Moderate(FGuid ChannelId, FGuid UserId, EAurixModerationAction Action, const FString& ActionToken, const FString& Reason, int64& RequestId)
{
	RequestId = 0;
	if (!Native)
	{
		return false;
	}
	uint64_t Id = 0;
	const std::string ReasonUtf8 = ToUtf8(Reason);
	const bool bOk = Check(
		Native->Client.moderate(ToUuid(ChannelId), ToUuid(UserId), FromModeration(Action), ToUtf8(ActionToken), Reason.IsEmpty() ? nullptr : ReasonUtf8.c_str(), &Id),
		TEXT("moderate"));
	RequestId = static_cast<int64>(Id);
	return bOk;
}

bool UAurixVoiceSubsystem::SendChat(FGuid ChannelId, const FString& Text, const FString& MetadataJson, int64& RequestId)
{
	RequestId = 0;
	if (!Native)
	{
		return false;
	}
	uint64_t Id = 0;
	const std::string Meta = ToUtf8(MetadataJson);
	const bool bOk = Check(Native->Client.send_chat(ToUuid(ChannelId), ToUtf8(Text), MetadataJson.IsEmpty() ? nullptr : Meta.c_str(), &Id), TEXT("send_chat"));
	RequestId = static_cast<int64>(Id);
	return bOk;
}

bool UAurixVoiceSubsystem::SendDirectChat(FGuid UserId, const FString& Text, const FString& MetadataJson, int64& RequestId)
{
	RequestId = 0;
	if (!Native)
	{
		return false;
	}
	uint64_t Id = 0;
	const std::string Meta = ToUtf8(MetadataJson);
	const bool bOk = Check(Native->Client.send_direct_chat(ToUuid(UserId), ToUtf8(Text), MetadataJson.IsEmpty() ? nullptr : Meta.c_str(), &Id), TEXT("send_direct_chat"));
	RequestId = static_cast<int64>(Id);
	return bOk;
}

bool UAurixVoiceSubsystem::SetTyping(FGuid ChannelId, bool bTyping)
{
	return Native && Check(Native->Client.set_typing(ToUuid(ChannelId), bTyping), TEXT("set_typing"));
}

bool UAurixVoiceSubsystem::ChannelHistory(FGuid ChannelId, const FString& Before, const FString& After, int32 Limit, int64& RequestId)
{
	RequestId = 0;
	if (!Native)
	{
		return false;
	}
	uint64_t Id = 0;
	const std::string BeforeUtf8 = ToUtf8(Before);
	const std::string AfterUtf8 = ToUtf8(After);
	const bool bOk = Check(Native->Client.channel_history(ToUuid(ChannelId), Before.IsEmpty() ? nullptr : BeforeUtf8.c_str(), After.IsEmpty() ? nullptr : AfterUtf8.c_str(), static_cast<uint32_t>(FMath::Max(0, Limit)), &Id), TEXT("channel_history"));
	RequestId = static_cast<int64>(Id);
	return bOk;
}

bool UAurixVoiceSubsystem::DirectHistory(FGuid UserId, const FString& Before, const FString& After, int32 Limit, int64& RequestId)
{
	RequestId = 0;
	if (!Native)
	{
		return false;
	}
	uint64_t Id = 0;
	const std::string BeforeUtf8 = ToUtf8(Before);
	const std::string AfterUtf8 = ToUtf8(After);
	const bool bOk = Check(Native->Client.direct_history(ToUuid(UserId), Before.IsEmpty() ? nullptr : BeforeUtf8.c_str(), After.IsEmpty() ? nullptr : AfterUtf8.c_str(), static_cast<uint32_t>(FMath::Max(0, Limit)), &Id), TEXT("direct_history"));
	RequestId = static_cast<int64>(Id);
	return bOk;
}

bool UAurixVoiceSubsystem::MarkChannelRead(FGuid ChannelId, FGuid MessageId)
{
	return Native && Check(Native->Client.mark_channel_read(ToUuid(ChannelId), ToUuid(MessageId)), TEXT("mark_channel_read"));
}

bool UAurixVoiceSubsystem::MarkDirectRead(FGuid UserId, FGuid MessageId)
{
	return Native && Check(Native->Client.mark_direct_read(ToUuid(UserId), ToUuid(MessageId)), TEXT("mark_direct_read"));
}

bool UAurixVoiceSubsystem::ChannelReadMarkers(FGuid ChannelId)
{
	return Native && Check(Native->Client.channel_read_markers(ToUuid(ChannelId)), TEXT("channel_read_markers"));
}

bool UAurixVoiceSubsystem::DirectReadMarkers(FGuid UserId)
{
	return Native && Check(Native->Client.direct_read_markers(ToUuid(UserId)), TEXT("direct_read_markers"));
}

bool UAurixVoiceSubsystem::EditChat(FGuid MessageId, const FString& Text, const FString& MetadataJson, int64& RequestId)
{
	RequestId = 0;
	if (!Native)
	{
		return false;
	}
	uint64_t Id = 0;
	const std::string Meta = ToUtf8(MetadataJson);
	const bool bOk = Check(Native->Client.edit_chat(ToUuid(MessageId), ToUtf8(Text), MetadataJson.IsEmpty() ? nullptr : Meta.c_str(), &Id), TEXT("edit_chat"));
	RequestId = static_cast<int64>(Id);
	return bOk;
}

bool UAurixVoiceSubsystem::DeleteChat(FGuid MessageId, int64& RequestId)
{
	RequestId = 0;
	if (!Native)
	{
		return false;
	}
	uint64_t Id = 0;
	const bool bOk = Check(Native->Client.delete_chat(ToUuid(MessageId), &Id), TEXT("delete_chat"));
	RequestId = static_cast<int64>(Id);
	return bOk;
}

bool UAurixVoiceSubsystem::ReactChat(FGuid MessageId, const FString& Reaction, bool bAdd)
{
	return Native && Check(Native->Client.react_chat(ToUuid(MessageId), ToUtf8(Reaction), bAdd), TEXT("react_chat"));
}

bool UAurixVoiceSubsystem::SearchChannelChat(FGuid ChannelId, const FString& Query, FGuid FromUserId, const FString& Before, int32 Limit, int64& RequestId)
{
	RequestId = 0;
	if (!Native)
	{
		return false;
	}
	uint64_t Id = 0;
	const std::string BeforeUtf8 = ToUtf8(Before);
	const aurix::Uuid From = ToUuid(FromUserId);
	const bool bOk = Check(Native->Client.search_channel_chat(ToUuid(ChannelId), ToUtf8(Query), FromUserId.IsValid() ? &From : nullptr, Before.IsEmpty() ? nullptr : BeforeUtf8.c_str(), static_cast<uint32_t>(FMath::Max(0, Limit)), &Id), TEXT("search_channel_chat"));
	RequestId = static_cast<int64>(Id);
	return bOk;
}

bool UAurixVoiceSubsystem::SearchDirectChat(FGuid UserId, const FString& Query, FGuid FromUserId, const FString& Before, int32 Limit, int64& RequestId)
{
	RequestId = 0;
	if (!Native)
	{
		return false;
	}
	uint64_t Id = 0;
	const std::string BeforeUtf8 = ToUtf8(Before);
	const aurix::Uuid User = ToUuid(UserId);
	const aurix::Uuid From = ToUuid(FromUserId);
	const bool bOk = Check(Native->Client.search_direct_chat(UserId.IsValid() ? &User : nullptr, ToUtf8(Query), FromUserId.IsValid() ? &From : nullptr, Before.IsEmpty() ? nullptr : BeforeUtf8.c_str(), static_cast<uint32_t>(FMath::Max(0, Limit)), &Id), TEXT("search_direct_chat"));
	RequestId = static_cast<int64>(Id);
	return bOk;
}

bool UAurixVoiceSubsystem::Speak(const FString& Text, FGuid ChannelId, EAurixTtsDestination Destination, const FString& Voice, int64& RequestId)
{
	RequestId = 0;
	if (!Native)
	{
		return false;
	}
	uint64_t Id = 0;
	const aurix::Uuid Channel = ToUuid(ChannelId);
	const std::string VoiceUtf8 = ToUtf8(Voice);
	const bool bOk = Check(
		Native->Client.speak(ToUtf8(Text), ChannelId.IsValid() ? &Channel : nullptr, FromTtsDestination(Destination), Voice.IsEmpty() ? nullptr : VoiceUtf8.c_str(), &Id),
		TEXT("speak"));
	RequestId = static_cast<int64>(Id);
	return bOk;
}

bool UAurixVoiceSubsystem::CancelSpeech()
{
	return Native && Check(Native->Client.cancel_speech(), TEXT("cancel_speech"));
}

bool UAurixVoiceSubsystem::GetStats(FAurixStats& OutStats) const
{
	AurixStats Raw;
	if (!Native || !Native->Client.stats(Raw))
	{
		OutStats = FAurixStats();
		return false;
	}
	OutStats = ToStats(Raw);
	return true;
}

bool UAurixVoiceSubsystem::GetNetworkQuality(FAurixNetworkQuality& OutQuality) const
{
	AurixNetworkQuality Raw;
	if (!Native || !Native->Client.network_quality(Raw))
	{
		OutQuality = FAurixNetworkQuality();
		return false;
	}
	OutQuality = ToNetworkQuality(Raw);
	return true;
}

// ---- regions ---------------------------------------------------------------------------------

void UAurixVoiceSubsystem::DiscoverRegions(const FAurixRegionDiscoveryRequest& Request, FAurixRegionsDiscovered OnComplete)
{
	CancelRegionDiscovery();

	TWeakObjectPtr<UAurixVoiceSubsystem> WeakThis(this);
	TSharedPtr<FAurixRegionDiscovery> Discovery = MakeShared<FAurixRegionDiscovery>(
		Request,
		FAurixRegionDiscovery::FOnComplete::CreateLambda(
			[WeakThis, OnComplete](bool bSuccess, const TArray<FAurixRegionEndpoint>& Regions, const FString& Error) {
				if (UAurixVoiceSubsystem* Self = WeakThis.Get())
				{
					Self->RegionDiscovery.Reset();
				}
				OnComplete.ExecuteIfBound(bSuccess, Regions, Error);
			}));

	FString Error;
	if (!Discovery->Start(Error))
	{
		UE_LOG(LogAurixVoice, Warning, TEXT("DiscoverRegions: %s"), *Error);
		OnComplete.ExecuteIfBound(false, TArray<FAurixRegionEndpoint>(), Error);
		return;
	}
	RegionDiscovery = MoveTemp(Discovery);
}

void UAurixVoiceSubsystem::CancelRegionDiscovery()
{
	if (RegionDiscovery.IsValid())
	{
		RegionDiscovery->Cancel();
		RegionDiscovery.Reset();
	}
}

// ---- conversions ---------------------------------------------------------------------------

bool UAurixVoiceSubsystem::ParseUuid(const FString& Text, FGuid& OutGuid)
{
	aurix::Uuid U;
	if (!aurix::Uuid::parse(ToUtf8(Text), U))
	{
		OutGuid.Invalidate();
		return false;
	}
	OutGuid = ToGuid(U.raw);
	return true;
}

FString UAurixVoiceSubsystem::FormatUuid(FGuid Guid)
{
	return FromUtf8(ToUuid(Guid).str().c_str());
}

// ---- events --------------------------------------------------------------------------------

void UAurixVoiceSubsystem::PumpEvents()
{
	for (int32 i = 0; i < MaxEventsPerTick && Native; ++i)
	{
		aurix::Event Event = Native->Client.poll_event();
		if (!Event)
		{
			break;
		}
		// A handler may call Disconnect(); `Event` owns its memory independently of the client.
		DispatchEvent(Event.raw());
	}
}

void UAurixVoiceSubsystem::DispatchEvent(const AurixEvent* Raw)
{
	const AurixEventType Type = aurix_event_type(Raw);
	const FGuid ChannelId = ToGuid(aurix_event_channel_id(Raw));
	const FGuid UserId = ToGuid(aurix_event_user_id(Raw));

	if (OnRawEvent.IsBound())
	{
		OnRawEvent.Broadcast(FromUtf8(aurix_event_json(Raw)));
	}

	switch (Type)
	{
	case AURIX_EVENT_STATE_CHANGED:
		OnConnectionStateChanged.Broadcast(ToState(aurix_event_state(Raw)));
		break;

	case AURIX_EVENT_SESSION_READY:
	{
		AurixSessionInfo Info;
		if (aurix_event_session(Raw, &Info))
		{
			if (ActiveSettings.bAutoStartCapture && !IsCapturing())
			{
				StartCapture(ActiveSettings.CaptureDeviceIndex);
			}
			if (bPlaybackRequested && (!PlaybackComponent || !PlaybackComponent->IsPlaying()))
			{
				StartPlayback();
			}
			OnSessionReady.Broadcast(ToSession(Info));
		}
		break;
	}

	case AURIX_EVENT_MEDIA_BOUND:
		OnMediaBound.Broadcast();
		break;

	case AURIX_EVENT_CHANNEL_JOINED:
	{
		const size_t Count = aurix_event_participant_count(Raw);
		TArray<FAurixParticipant> Participants;
		Participants.Reserve(static_cast<int32>(Count));
		for (size_t i = 0; i < Count; ++i)
		{
			AurixParticipant P;
			if (aurix_event_participant(Raw, i, &P))
			{
				Participants.Add(ToParticipant(P));
			}
		}
		OnChannelJoined.Broadcast(static_cast<int64>(aurix_event_request_id(Raw)), ChannelId, Participants, aurix_event_flag(Raw));
		break;
	}

	case AURIX_EVENT_CHANNEL_LEFT:
		OnChannelLeft.Broadcast(ChannelId);
		break;

	case AURIX_EVENT_PARTICIPANT_JOINED:
	{
		AurixParticipant P;
		if (aurix_event_participant(Raw, 0, &P))
		{
			OnParticipantJoined.Broadcast(ChannelId, ToParticipant(P));
		}
		break;
	}

	case AURIX_EVENT_PARTICIPANT_LEFT:
		OnParticipantLeft.Broadcast(ChannelId, UserId);
		break;

	case AURIX_EVENT_PARTICIPANT_MUTE_CHANGED:
		OnParticipantMuteChanged.Broadcast(ChannelId, UserId, aurix_event_flag(Raw), aurix_event_flag2(Raw));
		break;

	case AURIX_EVENT_PARTICIPANT_SPEAKING:
		OnParticipantSpeaking.Broadcast(ChannelId, UserId, aurix_event_flag(Raw));
		break;

	case AURIX_EVENT_PARTICIPANT_PRIORITY_CHANGED:
		OnParticipantPriorityChanged.Broadcast(ChannelId, UserId, aurix_event_flag(Raw));
		break;

	case AURIX_EVENT_DUCKING_CHANGED:
		OnDuckingChanged.Broadcast(ChannelId, aurix_event_flag(Raw), ToDucking(aurix_event_ducking(Raw)));
		break;

	case AURIX_EVENT_CHANNEL_ENERGY:
	{
		const size_t Count = aurix_event_participant_count(Raw);
		TArray<FAurixParticipant> Levels;
		Levels.Reserve(static_cast<int32>(Count));
		for (size_t i = 0; i < Count; ++i)
		{
			AurixParticipant P;
			if (aurix_event_participant(Raw, i, &P))
			{
				FAurixParticipant Level;
				Level.UserId = ToGuid(P.user_id);
				Level.Energy = P.energy;
				Levels.Add(Level);
			}
		}
		OnChannelEnergy.Broadcast(ChannelId, Levels);
		break;
	}

	case AURIX_EVENT_LOCAL_SPEAKING:
		OnLocalSpeaking.Broadcast(aurix_event_flag(Raw));
		break;

	case AURIX_EVENT_TRANSMISSION_CHANGED:
		OnTransmissionChanged.Broadcast(ToTransmission(aurix_event_transmission(Raw)), ChannelId);
		break;

	case AURIX_EVENT_CHANNEL_FOCUS_CHANGED:
		OnChannelFocusChanged.Broadcast(ChannelId);
		break;

	case AURIX_EVENT_AUDIO_CODEC_CHANGED:
		OnAudioCodecChanged.Broadcast(ToCodec(aurix_event_audio_codec(Raw)));
		break;

	case AURIX_EVENT_DOWNLINK_MODE_CHANGED:
		OnDownlinkModeChanged.Broadcast(ToDownlinkMode(aurix_event_downlink_mode(Raw)));
		break;

	case AURIX_EVENT_MEDIA_PATH_CHANGED:
		OnMediaPathChanged.Broadcast(ToMediaPath(aurix_event_media_path(Raw)), FromUtf8(aurix_event_message(Raw)));
		break;

	case AURIX_EVENT_USER_BLOCK_CHANGED:
		OnUserBlockChanged.Broadcast(UserId, aurix_event_flag(Raw));
		break;

	case AURIX_EVENT_RECORDING:
		OnRecording.Broadcast(ChannelId, ToGuid(aurix_event_object_id(Raw)), aurix_event_flag(Raw), UserId);
		break;

	case AURIX_EVENT_BITRATE_CHANGED:
		OnBitrateChanged.Broadcast(static_cast<int32>(aurix_event_number(Raw)), FromUtf8(aurix_event_message(Raw)));
		break;

	case AURIX_EVENT_KICKED:
		OnKicked.Broadcast(ChannelId, FromUtf8(aurix_event_message(Raw)));
		break;

	case AURIX_EVENT_MODERATION_APPLIED:
		OnModerationApplied.Broadcast(static_cast<int64>(aurix_event_request_id(Raw)), ChannelId, UserId, ToModeration(aurix_event_moderation_action(Raw)));
		break;

	case AURIX_EVENT_CHAT_MESSAGE:
	{
		AurixChatMessage M;
		if (aurix_event_chat(Raw, &M))
		{
			OnChatMessage.Broadcast(ToChatMessage(M));
		}
		break;
	}

	case AURIX_EVENT_CHAT_HISTORY:
	{
		AurixChatHistory H;
		if (aurix_event_chat_history(Raw, &H))
		{
			OnChatHistory.Broadcast(static_cast<int64>(aurix_event_request_id(Raw)), ReadHistoryPage(Raw, H, ChannelId, UserId));
		}
		break;
	}

	case AURIX_EVENT_CHAT_SEARCH_RESULT:
	{
		AurixChatHistory H;
		if (aurix_event_chat_history(Raw, &H))
		{
			FAurixChatHistoryPage Page = ReadHistoryPage(Raw, H, ChannelId, UserId);
			Page.Query = FromUtf8(aurix_event_message(Raw));
			OnChatSearchResult.Broadcast(static_cast<int64>(aurix_event_request_id(Raw)), Page);
		}
		break;
	}

	case AURIX_EVENT_CHAT_MESSAGE_UPDATED:
	{
		AurixChatMessage M;
		if (aurix_event_chat(Raw, &M))
		{
			OnChatMessageUpdated.Broadcast(static_cast<int64>(aurix_event_request_id(Raw)), ToChatMessage(M));
		}
		break;
	}

	case AURIX_EVENT_CHAT_REACTION_CHANGED:
	{
		AurixChatReaction R;
		if (aurix_event_reaction(Raw, &R))
		{
			OnChatReactionChanged.Broadcast(ToReactionChange(R));
		}
		break;
	}

	case AURIX_EVENT_CHAT_READ_MARKER:
	{
		AurixReadMarker M;
		if (aurix_event_read_marker(Raw, &M))
		{
			OnChatReadMarker.Broadcast(ToReadMarker(M));
		}
		break;
	}

	case AURIX_EVENT_CHAT_READ_MARKERS:
	{
		TArray<FAurixReadMarker> Markers;
		const uint64_t Count = aurix_event_number2(Raw);
		Markers.Reserve(static_cast<int32>(Count));
		for (size_t i = 0; i < Count; ++i)
		{
			AurixReadMarker M;
			if (aurix_event_read_marker_at(Raw, i, &M))
			{
				Markers.Add(ToReadMarker(M));
			}
		}
		OnChatReadMarkers.Broadcast(ChannelId, UserId, Markers, static_cast<int32>(aurix_event_number(Raw)));
		break;
	}

	case AURIX_EVENT_CHAT_INBOX_SYNCED:
		OnChatInboxSynced.Broadcast(static_cast<int32>(aurix_event_number(Raw)), aurix_event_flag(Raw));
		break;

	case AURIX_EVENT_PARTICIPANT_TYPING:
		OnParticipantTyping.Broadcast(ChannelId, UserId, aurix_event_flag(Raw));
		break;

	case AURIX_EVENT_TRANSCRIPT:
	{
		AurixTranscript T;
		if (aurix_event_transcript(Raw, &T))
		{
			FAurixTranscript Out;
			Out.ChannelId = ToGuid(T.channel_id);
			Out.UserId = ToGuid(T.user_id);
			Out.Text = FromUtf8(T.text);
			Out.Language = FromUtf8(T.language);
			Out.StartedAt = FromUnixMs(T.started_at_ms);
			Out.DurationMs = static_cast<int32>(T.duration_ms);
			Out.bTranslated = T.original_text != nullptr;
			Out.OriginalText = FromUtf8(T.original_text);
			Out.OriginalLanguage = FromUtf8(T.original_language);
			OnTranscript.Broadcast(Out);
		}
		break;
	}

	case AURIX_EVENT_TRANSLATION_CHANGED:
	{
		AurixTranslation T;
		if (aurix_event_translation(Raw, &T))
		{
			OnTranslationChanged.Broadcast(FromUtf8(T.language), FromUtf8(T.spoken_language), T.speech);
		}
		break;
	}

	case AURIX_EVENT_TTS_STATUS:
	{
		AurixTtsStatus S;
		if (aurix_event_tts(Raw, &S))
		{
			FAurixTtsStatus Out;
			Out.RequestId = static_cast<int64>(S.request_id);
			Out.ServerRequestId = ToGuid(S.server_request_id);
			Out.State = ToTtsState(S.state);
			Out.DurationMs = static_cast<int32>(S.duration_ms);
			Out.Message = FromUtf8(S.message);
			OnTtsStatus.Broadcast(Out);
		}
		break;
	}

	case AURIX_EVENT_POSITIONS:
		// Only exposed through OnRawEvent (JSON); games already know their own positions.
		break;

	case AURIX_EVENT_REJOIN_FAILED:
		OnRejoinFailed.Broadcast(ChannelId, FromUtf8(aurix_event_code(Raw)), FromUtf8(aurix_event_message(Raw)));
		break;

	case AURIX_EVENT_REQUEST_FAILED:
		OnRequestFailed.Broadcast(static_cast<int64>(aurix_event_request_id(Raw)), FromUtf8(aurix_event_code(Raw)), FromUtf8(aurix_event_message(Raw)));
		break;

	case AURIX_EVENT_SERVER_ERROR:
		OnServerError.Broadcast(FromUtf8(aurix_event_code(Raw)), FromUtf8(aurix_event_message(Raw)));
		break;

	case AURIX_EVENT_RECOVERING:
		OnRecovering.Broadcast(static_cast<int32>(aurix_event_number(Raw)), static_cast<int32>(aurix_event_number2(Raw)), FromUtf8(aurix_event_message(Raw)));
		break;

	case AURIX_EVENT_RECOVERED:
		OnRecovered.Broadcast(aurix_event_flag(Raw), aurix_event_flag2(Raw));
		break;

	case AURIX_EVENT_ENDPOINT_CHANGED:
		OnEndpointChanged.Broadcast(FromUtf8(aurix_event_message(Raw)));
		break;

	case AURIX_EVENT_E2EE_PEER_KEY:
		OnE2eePeerKey.Broadcast(ToGuid(aurix_event_user_id(Raw)), FromUtf8(aurix_event_message(Raw)), FromUtf8(aurix_event_code(Raw)));
		break;

	case AURIX_EVENT_E2EE_PEER_DECRYPTABLE:
		OnE2eePeerDecryptable.Broadcast(ToGuid(aurix_event_user_id(Raw)), aurix_event_flag(Raw));
		break;

	case AURIX_EVENT_E2EE_KEY_ROTATED:
		OnE2eeKeyRotated.Broadcast(static_cast<int32>(aurix_event_number(Raw)));
		break;

	case AURIX_EVENT_FAILED_TO_RECOVER:
		OnFailedToRecover.Broadcast(FromUtf8(aurix_event_message(Raw)));
		break;

	case AURIX_EVENT_DISCONNECTED:
		StopCapture();
		OnDisconnected.Broadcast(FromUtf8(aurix_event_message(Raw)));
		break;

	case AURIX_EVENT_NETWORK_QUALITY:
	{
		AurixNetworkQuality Q;
		if (aurix_event_network_quality(Raw, &Q))
		{
			OnNetworkQuality.Broadcast(ToNetworkQuality(Q));
		}
		break;
	}

	case AURIX_EVENT_AUDIO_POLICY_CHANGED:
	{
		AurixAudioPolicy P;
		if (aurix_event_audio_policy(Raw, &P))
		{
			OnAudioPolicyChanged.Broadcast(ToAudioPolicy(P));
		}
		break;
	}

	case AURIX_EVENT_LOSS_PROFILE_CHANGED:
		OnLossProfileChanged.Broadcast(FromNativeLossProfile(aurix_event_loss_profile(Raw)), static_cast<int32>(aurix_event_number2(Raw)));
		break;

	default:
		UE_LOG(LogAurixVoice, Verbose, TEXT("unhandled native event %d"), static_cast<int32>(Type));
		break;
	}
}
