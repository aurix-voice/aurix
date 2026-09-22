/**
 * Wire types of the Aurix WebSocket control channel.
 *
 * Mirrors `ControlMessage` in `crates/aurix-common/src/protocol.rs`
 * (serde: `{"type": "<Variant>", "data": {...}}`). Only the variants a browser client
 * sends or receives are modelled; unknown variants are surfaced as `UnknownMessage`.
 */

import type { NetworkQualityWire } from './quality.js';

export type ChannelRole = 'listener' | 'speaker' | 'moderator' | 'administrator';

export type RecordingConsent = 'pending' | 'accepted' | 'declined';

/** `AudioPolicy` as serialised by the server (snake_case; `complexity` is `null` without a hint). */
export interface AudioPolicyWire {
  bitrate_bps?: number;
  min_bitrate_bps?: number;
  fec?: boolean;
  dtx?: boolean;
  max_bandwidth?: string;
  complexity?: number | null;
  signal?: string;
  stereo?: boolean;
  /** Frames in this channel are end-to-end encrypted (`e2ee.ts`); the server cannot process them. */
  e2ee?: boolean;
}

export interface ParticipantBrief {
  user_id: string;
  display_name: string;
  ssrc: number;
  role: ChannelRole;
  is_muted: boolean;
  is_speaking: boolean;
  /** Priority speaker (`ChannelConfig.ducking`): their speech attenuates everyone else. */
  is_priority?: boolean;
}

/**
 * Priority-speaker ducking of a channel (`ChannelConfig.ducking`): while a priority member
 * speaks, every other voice is ramped to `gain` over `attack_ms`, held `hold_ms` past their
 * last audible frame and ramped back over `release_ms`. `moderators`: moderators and
 * administrators count as priority speakers too.
 */
export interface DuckingConfigWire {
  gain: number;
  attack_ms: number;
  release_ms: number;
  hold_ms: number;
  moderators: boolean;
}

export interface Position3D {
  x: number;
  y: number;
  z: number;
}

export interface Orientation3D {
  forward_x: number;
  forward_y: number;
  forward_z: number;
  up_x: number;
  up_y: number;
  up_z: number;
}

export interface UserPosition {
  user_id: string;
  position: Position3D;
  orientation: Orientation3D;
}

/** Distance / direction model of a positional channel (`ChannelConfig.positional_config`). */
export interface PositionalConfigWire {
  near_distance: number;
  far_distance: number;
  rolloff: 'linear' | 'logarithmic' | 'custom_spline';
  max_radius: number;
  directional: boolean;
  coordinate_system: 'left_handed' | 'right_handed';
  roster_radius?: number | null;
  text_radius?: number | null;
}

/** One negotiated per-participant downlink track: `user_id` is who it carries right now (`null` = idle). */
export interface ParticipantStreamWire {
  mid: string;
  user_id: string | null;
}

/** Receiver-local mute of one participant; `channel_id: null` means in every channel. */
export interface LocalMute {
  user_id: string;
  channel_id: string | null;
}

/** Receiver-local gain for one participant: `0` silence, `1` as sent, up to `2` (+6 dB). */
export interface ParticipantVolume {
  user_id: string;
  volume: number;
}

/** One entry of a `ChannelEnergy` report: linear audio energy `0..1` (`0` = silent). */
export interface ParticipantEnergy {
  user_id: string;
  energy: number;
}

/** Upper bound the server accepts for `SetParticipantVolume`. */
export const MAX_PARTICIPANT_VOLUME = 2.0;

/**
 * Where this session's outgoing audio goes: nowhere, exactly one joined channel or every
 * joined channel (the default). Enforced by the server before fan-out.
 */
export type TransmissionModeWire =
  | { mode: 'none' }
  | { mode: 'single'; channel_id: string }
  | { mode: 'all' };

/** Moderation performed by a player with a one-time action token (`POST /v1/tokens/action`). */
export type ModerationAction = 'kick' | 'mute' | 'unmute';

export type JsonValue = string | number | boolean | null | JsonValue[] | { [key: string]: JsonValue };

/**
 * One text-chat message as delivered by the server. Exactly one of `channel_id` /
 * `to_user_id` is set. `from_user_id` is the nil UUID for server-injected system messages.
 * `client_ref` is only present on the sender's own echo.
 */
export interface ChatMessageWire {
  id: string;
  channel_id?: string | null;
  from_user_id: string;
  display_name: string;
  to_user_id?: string | null;
  text: string;
  metadata?: JsonValue | null;
  /** RFC 3339 timestamp. */
  sent_at: string;
  client_ref?: string | null;
  /** Directed message that waited for an offline recipient (absent = `false`). */
  offline?: boolean;
  /** RFC 3339 timestamp of the last edit; absent when never edited. */
  edited_at?: string | null;
  /**
   * Tombstone: RFC 3339 timestamp of the deletion. The message keeps its id and position;
   * `text` is empty, `metadata` absent, reactions gone.
   */
  deleted_at?: string | null;
  /** Who deleted it (author / moderator); the nil UUID for the operator (REST). */
  deleted_by?: string | null;
  /** Reaction tallies (history / search only; live messages carry none). */
  reactions?: ChatReactionWire[];
}

export interface ChatReactionWire {
  reaction: string;
  count: number;
  /** At most the first 20 users carrying the reaction. */
  user_ids?: string[];
}

export interface ChatReadMarkerWire {
  user_id: string;
  channel_id?: string | null;
  peer_user_id?: string | null;
  message_id: string;
  /** RFC 3339 timestamps. */
  message_sent_at: string;
  read_at: string;
}

/** `from_user_id` of messages injected through the REST API (`POST .../messages`). */
export const SYSTEM_USER_ID = '00000000-0000-0000-0000-000000000000';

/** Messages the client may send. */
export type ClientMessage =
  | { type: 'ChannelJoin'; data: { channel_id: string; token: string } }
  | { type: 'ChannelLeave'; data: { channel_id: string } }
  | {
      type: 'ModerateParticipant';
      data: {
        channel_id: string;
        user_id: string;
        action: ModerationAction;
        token: string;
        reason?: string | null;
      };
    }
  | {
      type: 'MuteStateChanged';
      data: { channel_id: string; user_id: string; muted: boolean; server_muted: boolean };
    }
  | {
      type: 'SetParticipantMute';
      data: { user_id: string; channel_id: string | null; muted: boolean };
    }
  | { type: 'SetParticipantVolume'; data: { user_id: string; volume: number } }
  | {
      type: 'SetPriority';
      data: { channel_id: string; user_id?: string | null; priority: boolean };
    }
  | { type: 'SetUserBlock'; data: { user_id: string; blocked: boolean } }
  | { type: 'SetTransmission'; data: { mode: TransmissionModeWire } }
  | { type: 'SetChannelFocus'; data: { channel_id?: string | null } }
  | {
      type: 'ChatSend';
      data: { channel_id: string; text: string; metadata?: JsonValue; client_ref?: string };
    }
  | {
      type: 'ChatSendDirect';
      data: { user_id: string; text: string; metadata?: JsonValue; client_ref?: string };
    }
  | { type: 'ChatTyping'; data: { channel_id: string; typing: boolean } }
  | {
      type: 'ChatHistory';
      data: {
        channel_id?: string;
        user_id?: string;
        before?: string;
        after?: string;
        limit?: number;
        client_ref?: string;
      };
    }
  | { type: 'ChatMarkRead'; data: { channel_id?: string; user_id?: string; message_id: string } }
  | { type: 'ChatReadMarkers'; data: { channel_id?: string; user_id?: string } }
  | {
      type: 'ChatEdit';
      data: { message_id: string; text: string; metadata?: JsonValue; client_ref?: string };
    }
  | { type: 'ChatDelete'; data: { message_id: string; client_ref?: string } }
  | { type: 'ChatReact'; data: { message_id: string; reaction: string; add: boolean } }
  | {
      type: 'ChatSearch';
      data: {
        channel_id?: string;
        user_id?: string;
        query: string;
        from_user_id?: string;
        before?: string;
        limit?: number;
        client_ref?: string;
      };
    }
  | { type: 'SetTranscripts'; data: { enabled: boolean } }
  | { type: 'SetNoiseSuppression'; data: { enabled: boolean } }
  | {
      type: 'SetTranslation';
      data: { language?: string | null; spoken_language?: string | null; speech: boolean };
    }
  | {
      type: 'TtsSpeak';
      data: {
        channel_id?: string;
        text: string;
        voice?: string;
        destination?: TtsDestinationWire;
        client_ref?: string;
      };
    }
  | { type: 'TtsCancel'; data?: undefined }
  | { type: 'PositionUpdate'; data: { channel_id: string; positions: UserPosition[] } }
  /** `packet_loss` is a percentage (`0..=100`) of the last report period. */
  | { type: 'QualityReport'; data: { rtt_ms: number; jitter_ms: number; packet_loss: number } }
  | { type: 'RecordingConsentResponse'; data: { recording_id: string; consent: RecordingConsent } }
  | { type: 'WebRtcOffer'; data: { sdp: string } }
  /** Participants to keep on their own downlink track while audible (bounded by `webrtc_participant_streams`). */
  | { type: 'SetParticipantStreams'; data: { pinned: string[] } }
  /** `channel_id` absent: this session can do E2EE (sent once per connection, not relayed). */
  | { type: 'E2eeHello'; data: { channel_id?: string; public_key: string } }
  | { type: 'E2eeSenderKey'; data: { channel_id: string; to: string; public_key: string; generation: number; key: string } }
  | { type: 'Ping'; data: { nonce: number } }
  | { type: 'SessionClose'; data: { session_id: string; reason: string } };

/** Messages the server may send. */
export type ServerMessage =
  | {
      type: 'SessionInitAck';
      data: {
        session_id: string;
        ssrc: number;
        /** Primary native media endpoint (`host:port`, IPv6 hosts bracketed); informational for WebRTC clients. */
        media_addr: string;
        /** Every public media endpoint of the node (IPv4 first, then IPv6); absent from older nodes. */
        media_addrs?: string[];
        media_key: string;
        /** One-time credential for reattaching to this session after a dropped connection. */
        resume_token?: string;
        /** How long the server keeps a dropped session resumable (ms); 0 = resume disabled. */
        resume_grace_ms?: number;
        /** `true` when this ack reattached an existing session (channels are replayed). */
        resumed?: boolean;
        /**
         * `true` when the resumed session was taken over from another node (same session id
         * and SSRC, new media path — the client must renegotiate media).
         */
        migrated?: boolean;
        /** WebSocket URLs of other nodes to try when this one stops answering. */
        failover?: string[];
        /** The node translates transcripts on request; absent when translation is not configured. */
        translation?: TranslationInfoWire;
        /** The node can denoise this session's uplink on request (`SetNoiseSuppression`). */
        noise_suppression?: boolean;
        /** Per-participant WebRTC downlink tracks the node serves at most (absent / 0 = mixed only). */
        webrtc_participant_streams?: number;
        /** Gain the node applies to voices of unfocused channels; the browser mirrors it on per-participant tracks. */
        unfocused_channel_gain?: number;
        /**
         * The node's WebTransport media endpoint (HTTP/3, normally UDP/443): sealed AURX
         * packets as QUIC datagrams instead of WebRTC. `cert_sha256` carries the pins of the
         * node's generated certificate (current and next); absent for a CA certificate.
         */
        webtransport?: WebTransportInfoWire;
      };
    }
  | { type: 'MediaBound'; data: { session_id: string } }
  | { type: 'SessionClose'; data: { session_id: string; reason: string } }
  | {
      type: 'ChannelJoinAck';
      data: {
        channel_id: string;
        participants: ParticipantBrief[];
        transcription?: boolean;
        safety_voice?: boolean;
        audio?: AudioPolicyWire;
        /** Presence is scoped to this distance (positional channel); absent = whole channel. */
        roster_radius?: number;
        /** Channel text reaches only members within this distance; absent = whole channel. */
        text_radius?: number;
        /** Your effective role; `listener` = receive-only (absent on older servers = `speaker`). */
        role?: ChannelRole;
        /**
         * You hold a speaking grant but every `audience.max_speakers` slot is taken; `role` is
         * `listener` until a `RoleChanged` admits you.
         */
        waiting_to_speak?: boolean;
        /** Members across all nodes, including listeners hidden from `participants`. */
        participant_count?: number;
        /** Listeners are hidden from presence in this channel (`audience.hide_listeners`). */
        hidden_listeners?: boolean;
        /** Positional channel: the model the browser applies to per-participant tracks. */
        positional?: PositionalConfigWire | null;
        /** Priority-speaker ducking the node applies to what it mixes; browsers reproduce it on per-participant tracks. */
        ducking?: DuckingConfigWire | null;
        /** You are a priority speaker in this channel. */
        priority?: boolean;
      };
    }
  | {
      type: 'ChannelAudioPolicy';
      data: { channel_id: string; audio: AudioPolicyWire; ducking?: DuckingConfigWire | null };
    }
  | {
      type: 'ParticipantJoined';
      data: {
        channel_id: string;
        user_id: string;
        display_name: string;
        ssrc: number;
        role?: ParticipantBrief['role'];
        is_muted?: boolean;
        is_priority?: boolean;
      };
    }
  | { type: 'ParticipantLeft'; data: { channel_id: string; user_id: string } }
  | { type: 'PriorityChanged'; data: { channel_id: string; user_id: string; priority: boolean } }
  | {
      type: 'RoleChanged';
      data: {
        channel_id: string;
        user_id: string;
        role: ChannelRole;
        reason: 'speaker_demoted' | 'speaker_admitted';
      };
    }
  | {
      type: 'MuteStateChanged';
      data: { channel_id: string; user_id: string; muted: boolean; server_muted: boolean };
    }
  | { type: 'SpeakingStateChanged'; data: { channel_id: string; user_id: string; speaking: boolean } }
  | { type: 'PositionUpdate'; data: { channel_id: string; positions: UserPosition[] } }
  | {
      type: 'BitrateCommand';
      data: { target_bitrate_kbps: number; reason: string; expected_loss_percent?: number };
    }
  | { type: 'NetworkQuality'; data: { quality: NetworkQualityWire } }
  | { type: 'UserBlockChanged'; data: { user_id: string; blocked: boolean } }
  | { type: 'TransmissionChanged'; data: { mode: TransmissionModeWire } }
  | { type: 'ChannelFocusChanged'; data: { channel_id?: string | null } }
  | { type: 'ChatMessageReceived'; data: { message: ChatMessageWire } }
  | {
      type: 'ChatHistoryResult';
      data: {
        channel_id?: string | null;
        user_id?: string | null;
        messages: ChatMessageWire[];
        next_before?: string | null;
        next_after?: string | null;
        client_ref?: string | null;
      };
    }
  | { type: 'ChatReadMarker'; data: { marker: ChatReadMarkerWire } }
  | { type: 'ChatMessageUpdated'; data: { message: ChatMessageWire } }
  | {
      type: 'ChatReactionChanged';
      data: {
        message_id: string;
        channel_id?: string | null;
        message_from_user_id: string;
        message_to_user_id?: string | null;
        user_id: string;
        reaction: string;
        added: boolean;
        count: number;
        /** RFC 3339 timestamp. */
        timestamp: string;
      };
    }
  | {
      type: 'ChatSearchResult';
      data: {
        channel_id?: string | null;
        user_id?: string | null;
        query: string;
        messages: ChatMessageWire[];
        next_before?: string | null;
        client_ref?: string | null;
      };
    }
  | {
      type: 'ChatReadMarkersResult';
      data: {
        channel_id?: string | null;
        user_id?: string | null;
        markers: ChatReadMarkerWire[];
        unread_count: number;
      };
    }
  | { type: 'ChatInboxSynced'; data: { delivered: number; truncated: boolean } }
  | { type: 'ParticipantTyping'; data: { channel_id: string; user_id: string; typing: boolean } }
  | { type: 'ChannelEnergy'; data: { channel_id: string; levels: ParticipantEnergy[] } }
  | { type: 'Transcript'; data: { transcript: TranscriptWire } }
  | {
      type: 'TranslationChanged';
      data: { language?: string | null; spoken_language?: string | null; speech?: boolean };
    }
  | { type: 'NoiseSuppressionChanged'; data: { enabled: boolean } }
  | {
      type: 'TtsStatus';
      data: {
        request_id: string;
        client_ref?: string;
        state: TtsStateWire;
        duration_ms?: number;
        message?: string;
      };
    }
  | {
      type: 'ReceiverPreferences';
      data: {
        blocked_users: string[];
        local_mutes: LocalMute[];
        volumes: ParticipantVolume[];
        /** Absent on servers predating transmission policies (= `all`). */
        transmission?: TransmissionModeWire;
        focus_channel?: string | null;
        /** The node denoises this session's uplink (`SetNoiseSuppression`). */
        noise_suppression?: boolean;
      };
    }
  | {
      type: 'RecordingNotification';
      data: { channel_id: string; recording_id: string; active: boolean; initiated_by: string; live?: boolean };
    }
  | { type: 'Error'; data: { code: string; message: string; client_ref?: string } }
  | { type: 'Kick'; data: { channel_id: string; user_id: string; reason: string } }
  | {
      type: 'ModerateParticipantAck';
      data: { channel_id: string; user_id: string; action: ModerationAction };
    }
  | { type: 'WebRtcAnswer'; data: { sdp: string } }
  /** Current `mid → participant` layout of the per-participant downlink tracks (full snapshot). */
  | { type: 'ParticipantStreams'; data: { streams: ParticipantStreamWire[] } }
  | { type: 'E2eeHello'; data: { channel_id?: string; user_id?: string; public_key: string } }
  | {
      type: 'E2eeSenderKey';
      data: { channel_id: string; from?: string; to: string; public_key: string; generation: number; key: string };
    }
  | { type: 'Pong'; data: { nonce: number } };

export interface UnknownMessage {
  type: string;
  data: unknown;
}

export type TtsDestinationWire = 'channel' | 'local' | 'both';
export type TtsStateWire = 'queued' | 'playing' | 'finished' | 'cancelled' | 'failed';

export interface TranscriptWordWire {
  word: string;
  start_ms: number;
  end_ms: number;
}

export interface TranscriptWire {
  id: string;
  channel_id: string;
  user_id: string;
  text: string;
  language?: string;
  started_at: string;
  duration_ms: number;
  words?: TranscriptWordWire[];
  /** Present when `text` is a translation: the segment as transcribed. */
  original?: { text: string; language?: string | null } | null;
}

export interface TranslationInfoWire {
  speech?: boolean;
  /** Target languages listeners may request; empty/absent = any tag. */
  languages?: string[];
}

export interface WebTransportInfoWire {
  /** `https://host:port/aurix` URLs to try in order (IPv4 first, then IPv6). */
  urls: string[];
  /** SHA-256 fingerprints (hex) of the node's generated certificate: current, then next. */
  cert_sha256?: string[];
}

/**
 * Synthesized-voice streams (server TTS) carry SSRCs with the top bit set; a participant's
 * synthesized voice is `ssrc | SYNTH_SSRC_FLAG`, announcements use a per-channel SSRC.
 */
export const SYNTH_SSRC_FLAG = 0x80000000;

export interface TurnCredentials {
  username: string;
  password: string;
  ttl: number;
  expires_at: number;
  uris: string[];
}

/** Sub-protocol names understood by the server's WebSocket upgrade handler. */
export const AURIX_SUBPROTOCOL = 'aurix';
export const BEARER_SUBPROTOCOL_PREFIX = 'bearer.';
/** `resume.<session_id>.<resume_token>` sub-protocol carries the resume credential. */
export const RESUME_SUBPROTOCOL_PREFIX = 'resume.';

export function parseServerMessage(raw: string): ServerMessage | UnknownMessage {
  const value: unknown = JSON.parse(raw);
  if (typeof value !== 'object' || value === null || !('type' in value)) {
    throw new Error('malformed control message');
  }
  const typed = value as { type: unknown; data?: unknown };
  if (typeof typed.type !== 'string') {
    throw new Error('malformed control message type');
  }
  return { type: typed.type, data: typed.data ?? {} } as ServerMessage | UnknownMessage;
}
