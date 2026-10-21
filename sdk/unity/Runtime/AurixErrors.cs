using System;

namespace Aurix
{
    /// <summary>
    /// What to do about a wire error code (<c>RequestFailed</c>, <c>ServerError</c>, <c>RejoinFailed</c>,
    /// <see cref="AurixApiException"/>): the "Errors: what to do with them" table of the SDK cookbook, as code.
    /// Same text as the native <c>aurix_error_describe()</c>.
    /// </summary>
    public static class AurixErrors
    {
        /// <summary>One-sentence explanation of <paramref name="code"/> with the recommended action (English). Never null.</summary>
        public static string Describe(string code)
        {
            switch (code ?? string.Empty)
            {
                case "AUTH_FAILED":
                case "TOKEN_INVALID":
                case "TOKEN_EXPIRED":
                case "TOKEN_REUSED":
                    return "The voice token is missing, malformed, expired or was already used: mint a new token on your backend and reconnect.";
                case "AUTH_DENIED":
                case "ACTION_TOKEN_REQUIRED":
                    return "The token's grants do not allow this action (speak, moderate, kick): fix the grants in your token server; do not retry.";
                case "CHANNEL_NOT_FOUND":
                case "USER_NOT_FOUND":
                case "SESSION_NOT_FOUND":
                case "NOT_FOUND":
                    return "Unknown channel, user or session: a stale id after a leave or a fresh session after resume; refresh the roster and retry once.";
                case "CHANNEL_FULL":
                case "CHANNEL_LIMIT_EXCEEDED":
                case "QUOTA_EXCEEDED":
                    return "Voice is at its participant ceiling (channel size, plan limit or spending cap): tell the player voice is full and retry with backoff.";
                case "USER_BANNED":
                    return "This player is banned from voice: tell them; there is nothing to retry.";
                case "USER_MUTED":
                    return "This player is muted by a moderator: tell them; there is nothing to retry.";
                case "RATE_LIMIT_EXCEEDED":
                    return "Too many requests or messages: back off before retrying.";
                case "CONFLICT":
                    return "Duplicate or contradictory request (already joined, name mismatch): treat as success when idempotent, otherwise re-read state.";
                case "VALIDATION_ERROR":
                case "INVALID_CONFIG":
                    return "Malformed request or configuration: a programming error; log it and fix it, do not retry.";
                case "E2EE_REQUIRED":
                case "E2EE_MIXED_CHANNELS":
                    return "The channel requires end-to-end encryption or E2EE and plaintext channels are mixed in one session: enable E2EE or split the sessions.";
                case "MEDIA_NODE_UNAVAILABLE":
                case "TRANSPORT_ERROR":
                case "STUN_TURN_ERROR":
                    return "No media path could be established (QUIC, WebRTC and the WebSocket tunnel were tried): voice is unavailable on this network right now.";
                case "INTERNAL_ERROR":
                case "DB_ERROR":
                case "REDIS_ERROR":
                case "TIMEOUT":
                    return "The voice node failed internally: retry once after a second; if it persists it is a service incident, not your bug.";
                case "CHAT_DISABLED":
                case "TTS_DISABLED":
                case "TRANSLATION_DISABLED":
                    return "This feature is disabled for the application or channel: enable it in the console or hide the control.";
                case "MESSAGE_BLOCKED":
                    return "The message was blocked by a filter or the recipient's block list: tell the sender; do not retry.";
                case "USER_OFFLINE":
                    return "The recipient is not connected: queue or drop the message.";
                case "CODEC_ERROR":
                case "ENCRYPTION_ERROR":
                    return "Media could not be encoded, decoded or encrypted: an SDK or configuration problem; log it and reconnect.";
                case "RECORDING_ERROR":
                case "MODERATION_ERROR":
                case "SAFETY_ERROR":
                case "TTS_ERROR":
                case "STT_ERROR":
                case "TRANSLATION_ERROR":
                case "NOISE_SUPPRESSION_UNAVAILABLE":
                    return "A server-side feature (recording, moderation, safety, speech or translation) failed: the call itself continues; retry the feature later.";
                case "NOT_IMPLEMENTED":
                    return "This node does not implement the request: check the server version.";
                default:
                    return "Voice request failed: check the code and message; retry only if IsRetryable(code).";
            }
        }

        /// <summary>
        /// Whether retrying the same request later can succeed (capacity, rate limit, node-side or transient transport
        /// failures). Authorization, validation and moderation outcomes are not retryable.
        /// </summary>
        public static bool IsRetryable(string code)
        {
            switch (code)
            {
                case "CHANNEL_FULL":
                case "CHANNEL_LIMIT_EXCEEDED":
                case "QUOTA_EXCEEDED":
                case "RATE_LIMIT_EXCEEDED":
                case "MEDIA_NODE_UNAVAILABLE":
                case "TRANSPORT_ERROR":
                case "STUN_TURN_ERROR":
                case "INTERNAL_ERROR":
                case "DB_ERROR":
                case "REDIS_ERROR":
                case "TIMEOUT":
                case "USER_OFFLINE":
                case "RECORDING_ERROR":
                case "MODERATION_ERROR":
                case "SAFETY_ERROR":
                case "TTS_ERROR":
                case "STT_ERROR":
                case "TRANSLATION_ERROR":
                case "NOISE_SUPPRESSION_UNAVAILABLE":
                    return true;
                default:
                    return false;
            }
        }

        /// <summary>Whether the player can do something about it (as opposed to a developer-side bug).</summary>
        public static bool IsPlayerFacing(string code)
        {
            switch (code)
            {
                case "CHANNEL_FULL":
                case "CHANNEL_LIMIT_EXCEEDED":
                case "QUOTA_EXCEEDED":
                case "USER_BANNED":
                case "USER_MUTED":
                case "MEDIA_NODE_UNAVAILABLE":
                case "TRANSPORT_ERROR":
                case "STUN_TURN_ERROR":
                case "MESSAGE_BLOCKED":
                case "USER_OFFLINE":
                    return true;
                default:
                    return false;
            }
        }
    }
}
