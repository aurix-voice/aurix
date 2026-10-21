/**
 * What to do about a wire error code (`serverError`, `requestFailed`, `SdkError.code`, REST
 * `error.code`): the "Errors: what to do with them" table of the SDK cookbook, as code. Same text
 * as the native `aurix_error_describe()` and Unity `AurixErrors.Describe`.
 */

const TABLE: ReadonlyArray<readonly [readonly string[], string]> = [
  [
    ['AUTH_FAILED', 'TOKEN_INVALID', 'TOKEN_EXPIRED', 'TOKEN_REUSED'],
    'The voice token is missing, malformed, expired or was already used: mint a new token on your backend and reconnect.',
  ],
  [
    ['AUTH_DENIED', 'ACTION_TOKEN_REQUIRED'],
    "The token's grants do not allow this action (speak, moderate, kick): fix the grants in your token server; do not retry.",
  ],
  [
    ['CHANNEL_NOT_FOUND', 'USER_NOT_FOUND', 'SESSION_NOT_FOUND', 'NOT_FOUND'],
    'Unknown channel, user or session: a stale id after a leave or a fresh session after resume; refresh the roster and retry once.',
  ],
  [
    ['CHANNEL_FULL', 'CHANNEL_LIMIT_EXCEEDED', 'QUOTA_EXCEEDED'],
    'Voice is at its participant ceiling (channel size, plan limit or spending cap): tell the player voice is full and retry with backoff.',
  ],
  [['USER_BANNED'], 'This player is banned from voice: tell them; there is nothing to retry.'],
  [['USER_MUTED'], 'This player is muted by a moderator: tell them; there is nothing to retry.'],
  [['RATE_LIMIT_EXCEEDED'], 'Too many requests or messages: back off before retrying.'],
  [
    ['CONFLICT'],
    'Duplicate or contradictory request (already joined, name mismatch): treat as success when idempotent, otherwise re-read state.',
  ],
  [
    ['VALIDATION_ERROR', 'INVALID_CONFIG'],
    'Malformed request or configuration: a programming error; log it and fix it, do not retry.',
  ],
  [
    ['E2EE_REQUIRED', 'E2EE_MIXED_CHANNELS'],
    'The channel requires end-to-end encryption or E2EE and plaintext channels are mixed in one session: enable E2EE or split the sessions.',
  ],
  [
    ['MEDIA_NODE_UNAVAILABLE', 'TRANSPORT_ERROR', 'STUN_TURN_ERROR'],
    'No media path could be established (QUIC, WebRTC and the WebSocket tunnel were tried): voice is unavailable on this network right now.',
  ],
  [
    ['INTERNAL_ERROR', 'DB_ERROR', 'REDIS_ERROR', 'TIMEOUT'],
    'The voice node failed internally: retry once after a second; if it persists it is a service incident, not your bug.',
  ],
  [
    ['CHAT_DISABLED', 'TTS_DISABLED', 'TRANSLATION_DISABLED'],
    'This feature is disabled for the application or channel: enable it in the console or hide the control.',
  ],
  [
    ['MESSAGE_BLOCKED'],
    "The message was blocked by a filter or the recipient's block list: tell the sender; do not retry.",
  ],
  [['USER_OFFLINE'], 'The recipient is not connected: queue or drop the message.'],
  [
    ['CODEC_ERROR', 'ENCRYPTION_ERROR'],
    'Media could not be encoded, decoded or encrypted: an SDK or configuration problem; log it and reconnect.',
  ],
  [
    [
      'RECORDING_ERROR',
      'MODERATION_ERROR',
      'SAFETY_ERROR',
      'TTS_ERROR',
      'STT_ERROR',
      'TRANSLATION_ERROR',
      'NOISE_SUPPRESSION_UNAVAILABLE',
    ],
    'A server-side feature (recording, moderation, safety, speech or translation) failed: the call itself continues; retry the feature later.',
  ],
  [['NOT_IMPLEMENTED'], 'This node does not implement the request: check the server version.'],
];

const RETRYABLE = new Set([
  'CHANNEL_FULL',
  'CHANNEL_LIMIT_EXCEEDED',
  'QUOTA_EXCEEDED',
  'RATE_LIMIT_EXCEEDED',
  'MEDIA_NODE_UNAVAILABLE',
  'TRANSPORT_ERROR',
  'STUN_TURN_ERROR',
  'INTERNAL_ERROR',
  'DB_ERROR',
  'REDIS_ERROR',
  'TIMEOUT',
  'USER_OFFLINE',
  'RECORDING_ERROR',
  'MODERATION_ERROR',
  'SAFETY_ERROR',
  'TTS_ERROR',
  'STT_ERROR',
  'TRANSLATION_ERROR',
  'NOISE_SUPPRESSION_UNAVAILABLE',
]);

const PLAYER_FACING = new Set([
  'CHANNEL_FULL',
  'CHANNEL_LIMIT_EXCEEDED',
  'QUOTA_EXCEEDED',
  'USER_BANNED',
  'USER_MUTED',
  'MEDIA_NODE_UNAVAILABLE',
  'TRANSPORT_ERROR',
  'STUN_TURN_ERROR',
  'MESSAGE_BLOCKED',
  'USER_OFFLINE',
]);

/** One-sentence explanation of `code` with the recommended action (English). Never empty. */
export function describeError(code: string | undefined | null): string {
  for (const [codes, text] of TABLE) {
    if (code && codes.includes(code)) return text;
  }
  return 'Voice request failed: check the code and message; retry only if isErrorRetryable(code).';
}

/**
 * Whether retrying the same request later can succeed (capacity, rate limit, node-side or
 * transient transport failures). Authorization, validation and moderation outcomes are not retryable.
 */
export function isErrorRetryable(code: string | undefined | null): boolean {
  return !!code && RETRYABLE.has(code);
}

/** Whether the player can do something about it (as opposed to a developer-side bug). */
export function isErrorPlayerFacing(code: string | undefined | null): boolean {
  return !!code && PLAYER_FACING.has(code);
}
