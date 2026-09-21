"""Aurix server-side SDK: typed REST client for game backends (token issuance, channels,
moderation, analytics, webhooks, SSE). Generated from api/openapi.json; transport is stdlib-only."""

from . import types
from ._http import SDK_VERSION, BaseClient, Credentials, RawResponse, RequestOptions
from .errors import AurixError, AurixNetworkError
from .generated.client import AsyncAurixClient, AurixClient
from .sse import SseEvent, event_stream, parse_sse
from .webhooks import (
    ATTEMPT_HEADER,
    DEFAULT_TOLERANCE_SEC,
    DELIVERY_ID_HEADER,
    EVENT_HEADER,
    SIGNATURE_HEADER,
    WEBHOOK_ID_HEADER,
    IncomingWebhook,
    WebhookVerificationError,
    parse_webhook,
    sign_webhook,
    verify_webhook_signature,
)

__version__ = SDK_VERSION

__all__ = [
    "ATTEMPT_HEADER",
    "AsyncAurixClient",
    "AurixClient",
    "AurixError",
    "AurixNetworkError",
    "BaseClient",
    "Credentials",
    "DEFAULT_TOLERANCE_SEC",
    "DELIVERY_ID_HEADER",
    "EVENT_HEADER",
    "RawResponse",
    "RequestOptions",
    "SDK_VERSION",
    "SIGNATURE_HEADER",
    "SseEvent",
    "WEBHOOK_ID_HEADER",
    "IncomingWebhook",
    "WebhookVerificationError",
    "event_stream",
    "parse_sse",
    "parse_webhook",
    "sign_webhook",
    "types",
    "verify_webhook_signature",
]
