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
}

export interface ParticipantBrief {
  user_id: string;
  display_name: string;
  ssrc: number;
  role: ChannelRole;
  is_muted: boolean;
  is_speaking: boolean;
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
  | { type: 'SetTranscripts'; data: { enabled: boolean } }
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
  | { type: 'Ping'; data: { nonce: number } }
  | { type: 'SessionClose'; data: { session_id: string; reason: string } };

/** Messages the server may send. */
export type ServerMessage =
  | {
      type: 'SessionInitAck';
      data: {
        session_id: string;
        ssrc: number;
        media_addr: string;
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
        /** Your role; `listener` = receive-only (absent on older servers = `speaker`). */
        role?: ChannelRole;
        /** Members across all nodes, including listeners hidden from `participants`. */
        participant_count?: number;
        /** Listeners are hidden from presence in this channel (`audience.hide_listeners`). */
        hidden_listeners?: boolean;
      };
    }
  | { type: 'ChannelAudioPolicy'; data: { channel_id: string; audio: AudioPolicyWire } }
  | {
      type: 'ParticipantJoined';
      data: {
        channel_id: string;
        user_id: string;
        display_name: string;
        ssrc: number;
        role?: ParticipantBrief['role'];
        is_muted?: boolean;
      };
    }
  | { type: 'ParticipantLeft'; data: { channel_id: string; user_id: string } }
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
  | { type: 'ParticipantTyping'; data: { channel_id: string; user_id: string; typing: boolean } }
  | { type: 'ChannelEnergy'; data: { channel_id: string; levels: ParticipantEnergy[] } }
  | { type: 'Transcript'; data: { transcript: TranscriptWire } }
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
