export { AurixClient } from './client.js';
export type {
  AurixClientOptions,
  AurixEvents,
  ConnectionState,
  Participant,
  ReceiverPreferences,
  ReconnectPolicy,
  SessionInfo,
} from './client.js';
export type {
  ChannelRole,
  ClientMessage,
  LocalMute,
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
  parseServerMessage,
} from './protocol.js';
