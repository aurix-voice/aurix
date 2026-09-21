"""Hand-written transport under the generated client: auth headers, JSON/raw requests,
timeouts, bounded retries. Standard library only (``urllib``)."""

from __future__ import annotations

import json
import random
import socket
import ssl
import time
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass, field
from typing import Any, Dict, List, Mapping, Optional, Tuple

from .errors import AurixError, AurixNetworkError

SDK_VERSION = "1.2.0"
_IDEMPOTENT = frozenset({"GET", "HEAD", "OPTIONS", "PUT", "DELETE"})
_RETRY_STATUSES = frozenset({502, 503, 504})


@dataclass
class Credentials:
    """Exactly one of these is normally set. Precedence: api_key, admin_token, player_token, bootstrap_token."""

    api_key: Optional[str] = None
    admin_token: Optional[str] = None
    player_token: Optional[str] = None
    bootstrap_token: Optional[str] = None

    def headers(self) -> Dict[str, str]:
        if self.api_key:
            return {"X-API-Key": self.api_key}
        if self.admin_token:
            return {"Authorization": f"Bearer {self.admin_token}"}
        if self.player_token:
            return {"Authorization": f"Bearer {self.player_token}"}
        if self.bootstrap_token:
            return {"X-Bootstrap-Token": self.bootstrap_token}
        return {}


@dataclass
class RequestOptions:
    """Per-call overrides."""

    headers: Dict[str, str] = field(default_factory=dict)
    timeout: Optional[float] = None
    auth: Optional[Credentials] = None


@dataclass
class RawResponse:
    """Undecoded response for binary / CSV / SRT / VTT operations."""

    status: int
    content_type: str
    headers: Dict[str, str]
    body: bytes

    def text(self, encoding: str = "utf-8") -> str:
        return self.body.decode(encoding)

    def json(self) -> Any:
        return json.loads(self.body)


def path_segment(value: Any) -> str:
    return urllib.parse.quote(str(value), safe="")


def _query_items(query: Optional[Mapping[str, Any]]) -> List[Tuple[str, str]]:
    out: List[Tuple[str, str]] = []
    if not query:
        return out
    for key, value in query.items():
        if value is None:
            continue
        if isinstance(value, (list, tuple)):
            out.extend((key, _qs(v)) for v in value if v is not None)
        else:
            out.append((key, _qs(value)))
    return out


def _qs(value: Any) -> str:
    if isinstance(value, bool):
        return "true" if value else "false"
    return str(value)


def retry_after_seconds(headers: Mapping[str, str]) -> Optional[float]:
    for k, v in headers.items():
        if k.lower() == "retry-after":
            try:
                return max(0.0, float(v))
            except ValueError:
                return None
    return None


class BaseClient:
    """Shared behaviour of the generated ``AurixClient``.

    ``base_url`` is the node's HTTP origin (``https://voice.example.com``). Authenticate with
    exactly one credential: ``api_key`` (game backends), ``admin_token`` (operators),
    ``player_token`` (acting on behalf of a player) or ``bootstrap_token`` (first admin).
    """

    def __init__(
        self,
        base_url: str,
        *,
        api_key: Optional[str] = None,
        admin_token: Optional[str] = None,
        player_token: Optional[str] = None,
        bootstrap_token: Optional[str] = None,
        timeout: float = 15.0,
        max_retries: int = 2,
        max_backoff: float = 5.0,
        headers: Optional[Mapping[str, str]] = None,
        user_agent: Optional[str] = None,
        ssl_context: Optional[ssl.SSLContext] = None,
        opener: Optional[urllib.request.OpenerDirector] = None,
    ):
        self.base_url = base_url.rstrip("/")
        self.credentials = Credentials(api_key, admin_token, player_token, bootstrap_token)
        self.timeout = timeout
        self.max_retries = max(0, max_retries)
        self.max_backoff = max_backoff
        self.default_headers = dict(headers or {})
        self.user_agent = user_agent or f"aurix-server-sdk-python/{SDK_VERSION}"
        https = urllib.request.HTTPSHandler(context=ssl_context) if ssl_context else urllib.request.HTTPSHandler()
        self._opener = opener or urllib.request.build_opener(https)

    def url(self, path: str, query: Optional[Mapping[str, Any]] = None) -> str:
        url = self.base_url + path
        items = list(_query_items(query))
        if items:
            url += "?" + urllib.parse.urlencode(items)
        return url

    def _headers(self, options: Optional[RequestOptions], has_body: bool, accept: str) -> Dict[str, str]:
        h: Dict[str, str] = {"Accept": accept, "User-Agent": self.user_agent}
        h.update(self.default_headers)
        h.update((options.auth if options and options.auth else self.credentials).headers())
        if has_body:
            h["Content-Type"] = "application/json"
        if options:
            h.update(options.headers)
        return h

    def _json(
        self,
        method: str,
        path: str,
        *,
        body: Any = None,
        query: Optional[Mapping[str, Any]] = None,
        options: Optional[RequestOptions] = None,
    ) -> Any:
        raw = self._raw(method, path, body=body, query=query, options=options, accept="application/json")
        if raw.status == 204 or not raw.body:
            return None
        if not raw.content_type.lower().startswith("application/json"):
            raise AurixError(
                status=raw.status,
                code="unexpected_content_type",
                message=f"expected application/json, got {raw.content_type or 'no content type'}; use the *_raw variant",
                method=method,
                path=path,
                body=raw.body,
            )
        return json.loads(raw.body)

    def _raw(
        self,
        method: str,
        path: str,
        *,
        body: Any = None,
        query: Optional[Mapping[str, Any]] = None,
        options: Optional[RequestOptions] = None,
        accept: str = "*/*",
    ) -> RawResponse:
        method = method.upper()
        url = self.url(path, query)
        data = json.dumps(body).encode("utf-8") if body is not None else None
        headers = self._headers(options, data is not None, accept)
        timeout = options.timeout if options and options.timeout is not None else self.timeout
        attempt = 0
        while True:
            attempt += 1
            try:
                req = urllib.request.Request(url, data=data, method=method, headers=headers)
                with self._opener.open(req, timeout=timeout) as resp:
                    status = resp.status
                    resp_headers = {k: v for k, v in resp.headers.items()}
                    content = resp.read()
            except urllib.error.HTTPError as e:
                status = e.code
                resp_headers = {k: v for k, v in e.headers.items()}
                content = e.read()
            except (urllib.error.URLError, socket.timeout, TimeoutError, ConnectionError, ssl.SSLError, OSError) as e:
                if attempt <= self.max_retries and method in _IDEMPOTENT:
                    time.sleep(self._backoff(attempt))
                    continue
                raise AurixNetworkError(str(getattr(e, "reason", e) or e), method, path, e) from e

            content_type = next((v for k, v in resp_headers.items() if k.lower() == "content-type"), "")
            if 200 <= status < 300:
                return RawResponse(status, content_type.split(";")[0].strip(), resp_headers, content)
            retryable = status == 429 or (status in _RETRY_STATUSES and method in _IDEMPOTENT)
            if retryable and attempt <= self.max_retries:
                delay = retry_after_seconds(resp_headers)
                time.sleep(self._backoff(attempt) if delay is None else min(delay, self.max_backoff))
                continue
            raise AurixError.from_response(status, content_type, content, resp_headers, method, path)

    def _backoff(self, attempt: int) -> float:
        base = min(self.max_backoff, 0.2 * float(2 ** (attempt - 1)))
        return float(base * (0.5 + random.random() / 2))
