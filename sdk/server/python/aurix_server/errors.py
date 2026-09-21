from __future__ import annotations

import json
from typing import Any, Mapping, Optional


class AurixError(Exception):
    """Non-2xx response. ``code``/``message`` come from the API's ``{"error": {code, message}}`` envelope."""

    def __init__(
        self,
        *,
        status: int,
        code: str,
        message: str,
        method: str,
        path: str,
        request_id: Optional[str] = None,
        retry_after: Optional[float] = None,
        body: Any = None,
    ):
        super().__init__(f"{method} {path} -> {status} {code}: {message}")
        self.status = status
        self.code = code
        self.detail = message
        self.method = method
        self.path = path
        self.request_id = request_id
        self.retry_after = retry_after
        self.body = body

    @property
    def is_auth(self) -> bool:
        return self.status in (401, 403)

    @property
    def is_not_found(self) -> bool:
        return self.status == 404

    @property
    def is_rate_limited(self) -> bool:
        return self.status == 429

    @classmethod
    def from_response(
        cls, status: int, content_type: str, content: bytes, headers: Mapping[str, str], method: str, path: str
    ) -> "AurixError":
        code = f"http_{status}"
        text = content.decode("utf-8", errors="replace")
        message = text[:512] or f"HTTP {status}"
        body: Any = text
        if content_type.lower().startswith("application/json"):
            try:
                body = json.loads(content)
                err = body.get("error") if isinstance(body, dict) else None
                if isinstance(err, dict):
                    if isinstance(err.get("code"), str):
                        code = err["code"]
                    if isinstance(err.get("message"), str):
                        message = err["message"]
            except ValueError:
                pass
        lower = {k.lower(): v for k, v in headers.items()}
        retry_after: Optional[float] = None
        if "retry-after" in lower:
            try:
                retry_after = max(0.0, float(lower["retry-after"]))
            except ValueError:
                retry_after = None
        return cls(
            status=status,
            code=code,
            message=message,
            method=method,
            path=path,
            request_id=lower.get("x-request-id"),
            retry_after=retry_after,
            body=body,
        )


class AurixNetworkError(Exception):
    """Transport failure (DNS, refused connection, timeout) - no HTTP response was received."""

    def __init__(self, message: str, method: str, path: str, cause: Optional[BaseException] = None):
        super().__init__(f"{method} {path}: {message}")
        self.method = method
        self.path = path
        self.cause = cause
