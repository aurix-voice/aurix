export { AurixClient } from './client.js';
export type {
  AurixClientOptions,
  AurixEvents,
  ConnectionState,
  Participant,
  SessionInfo,
} from './client.js';
export type {
  ChannelRole,
  ClientMessage,
  Orientation3D,
  ParticipantBrief,
  Position3D,
  RecordingConsent,
  ServerMessage,
  TurnCredentials,
  UnknownMessage,
  UserPosition,
} from './protocol.js';
export { AURIX_SUBPROTOCOL, BEARER_SUBPROTOCOL_PREFIX, parseServerMessage } from './protocol.js';
