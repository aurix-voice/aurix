"""Webhook receiver helpers: verify ``X-Aurix-Signature`` (``t=<unix>,v1=<hex HMAC-SHA256(secret,
"<t>.<raw body>")>``) in constant time with a replay window, and parse the ``EventEnvelope``."""

from __future__ import annotations

import hashlib
import hmac
import json
import time
from dataclasses import dataclass
from typing import Mapping, Optional, Union

from .generated.types import EventEnvelope

SIGNATURE_HEADER = "X-Aurix-Signature"
EVENT_HEADER = "X-Aurix-Event"
WEBHOOK_ID_HEADER = "X-Aurix-Webhook-Id"
DELIVERY_ID_HEADER = "X-Aurix-Delivery-Id"
ATTEMPT_HEADER = "X-Aurix-Attempt"
DEFAULT_TOLERANCE_SEC = 300


def sign_webhook(secret: str, timestamp: int, body: Union[bytes, str]) -> str:
    """Header value for ``body`` at ``timestamp`` (unix seconds); for tests and re-signing proxies."""
    raw = body.encode("utf-8") if isinstance(body, str) else body
    mac = hmac.new(secret.encode("utf-8"), f"{timestamp}.".encode("ascii") + raw, hashlib.sha256)
    return f"t={timestamp},v1={mac.hexdigest()}"


def verify_webhook_signature(
    secret: str,
    header: Optional[str],
    body: Union[bytes, str],
    *,
    tolerance_sec: int = DEFAULT_TOLERANCE_SEC,
    now: Optional[float] = None,
) -> bool:
    """True when ``header`` is a valid signature of the raw ``body`` within the replay window."""
    if not header or not secret:
        return False
    t: Optional[int] = None
    v1: Optional[str] = None
    for part in header.split(","):
        k, _, v = part.strip().partition("=")
        if k == "t" and v.isdigit():
            t = int(v)
        elif k == "v1":
            v1 = v.strip()
    if t is None or v1 is None:
        return False
    if abs((time.time() if now is None else now) - t) > tolerance_sec:
        return False
    expected = sign_webhook(secret, t, body)[len(f"t={t},v1=") :]
    return hmac.compare_digest(expected, v1.lower())


class WebhookVerificationError(Exception):
    pass


@dataclass
class IncomingWebhook:
    event: EventEnvelope
    webhook_id: Optional[str]
    delivery_id: Optional[str]
    attempt: int


def _get(headers: Mapping[str, str], name: str) -> Optional[str]:
    if name in headers:
        return headers[name]
    lname = name.lower()
    for k, v in headers.items():
        if k.lower() == lname:
            return v
    return None


def parse_webhook(
    secret: str,
    headers: Mapping[str, str],
    raw_body: Union[bytes, str],
    *,
    tolerance_sec: int = DEFAULT_TOLERANCE_SEC,
    now: Optional[float] = None,
) -> IncomingWebhook:
    """Verify and parse one webhook POST. ``raw_body`` must be the exact bytes received."""
    if not verify_webhook_signature(secret, _get(headers, SIGNATURE_HEADER), raw_body, tolerance_sec=tolerance_sec, now=now):
        raise WebhookVerificationError("invalid or expired X-Aurix-Signature")
    event: EventEnvelope = json.loads(raw_body)
    header_type = _get(headers, EVENT_HEADER)
    if header_type and header_type != event["type"]:
        raise WebhookVerificationError(f"X-Aurix-Event {header_type} does not match body type {event['type']}")
    attempt_raw = _get(headers, ATTEMPT_HEADER)
    attempt = int(attempt_raw) if attempt_raw and attempt_raw.isdigit() else 1
    return IncomingWebhook(event, _get(headers, WEBHOOK_ID_HEADER), _get(headers, DELIVERY_ID_HEADER), attempt)
