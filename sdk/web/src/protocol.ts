/**
 * Wire types of the Aurix WebSocket control channel.
 *
 * Mirrors `ControlMessage` in `crates/aurix-common/src/protocol.rs`
 * (serde: `{"type": "<Variant>", "data": {...}}`). Only the variants a browser client
 * sends or receives are modelled; unknown variants are surfaced as `UnknownMessage`.
 */

export type ChannelRole = 'listener' | 'speaker' | 'moderator' | 'administrator';

export type RecordingConsent = 'pending' | 'accepted' | 'declined';

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

/** Upper bound the server accepts for `SetParticipantVolume`. */
export const MAX_PARTICIPANT_VOLUME = 2.0;

/** Moderation performed by a player with a one-time action token (`POST /v1/tokens/action`). */
export type ModerationAction = 'kick' | 'mute' | 'unmute';

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
  | { type: 'PositionUpdate'; data: { channel_id: string; positions: UserPosition[] } }
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
      };
    }
  | { type: 'MediaBound'; data: { session_id: string } }
  | { type: 'SessionClose'; data: { session_id: string; reason: string } }
  | { type: 'ChannelJoinAck'; data: { channel_id: string; participants: ParticipantBrief[] } }
  | {
      type: 'ParticipantJoined';
      data: { channel_id: string; user_id: string; display_name: string; ssrc: number };
    }
  | { type: 'ParticipantLeft'; data: { channel_id: string; user_id: string } }
  | {
      type: 'MuteStateChanged';
      data: { channel_id: string; user_id: string; muted: boolean; server_muted: boolean };
    }
  | { type: 'SpeakingStateChanged'; data: { channel_id: string; user_id: string; speaking: boolean } }
  | { type: 'PositionUpdate'; data: { channel_id: string; positions: UserPosition[] } }
  | { type: 'BitrateCommand'; data: { target_bitrate_kbps: number; reason: string } }
  | { type: 'UserBlockChanged'; data: { user_id: string; blocked: boolean } }
  | {
      type: 'ReceiverPreferences';
      data: { blocked_users: string[]; local_mutes: LocalMute[]; volumes: ParticipantVolume[] };
    }
  | {
      type: 'RecordingNotification';
      data: { channel_id: string; recording_id: string; active: boolean; initiated_by: string };
    }
  | { type: 'Error'; data: { code: string; message: string } }
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
