export { AurixClient } from './client.js';
export type {
  AurixClientOptions,
  AurixEvents,
  ChatMessage,
  ConnectionState,
  Participant,
  ReceiverPreferences,
  ReconnectPolicy,
  SendMessageOptions,
  SessionInfo,
} from './client.js';
export type {
  ChannelRole,
  ChatMessageWire,
  ClientMessage,
  JsonValue,
  LocalMute,
  ModerationAction,
  Orientation3D,
  ParticipantBrief,
  ParticipantVolume,
  Position3D,
  RecordingConsent,
  ServerMessage,
  TurnCredentials,
  UnknownMessage,
  UserPosition,
} from './protocol.js';
export {
  AURIX_SUBPROTOCOL,
  BEARER_SUBPROTOCOL_PREFIX,
  MAX_PARTICIPANT_VOLUME,
  RESUME_SUBPROTOCOL_PREFIX,
  SYSTEM_USER_ID,
  parseServerMessage,
} from './protocol.js';
