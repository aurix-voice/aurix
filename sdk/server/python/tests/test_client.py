from __future__ import annotations

import asyncio
import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any, Callable, Dict, List, Optional
from urllib.parse import parse_qs, urlsplit

import pytest

from aurix_server import (
    AsyncAurixClient,
    AurixClient,
    AurixError,
    AurixNetworkError,
    RequestOptions,
    WebhookVerificationError,
    event_stream,
    parse_sse,
    parse_webhook,
    sign_webhook,
    verify_webhook_signature,
)

VECTOR = json.loads((Path(__file__).resolve().parents[2] / "vectors" / "webhook_signature.json").read_text())


class Call:
    def __init__(self, method: str, path: str, query: Dict[str, List[str]], headers: Dict[str, str], body: Any):
        self.method, self.path, self.query, self.headers, self.body = method, path, query, headers, body


Handler = Callable[[Call, BaseHTTPRequestHandler, int], None]


class FakeNode:
    """Records requests and answers from a scripted handler."""

    def __init__(self, handler: Handler):
        self.calls: List[Call] = []
        node = self

        class H(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *a: Any) -> None:
                pass

            def _handle(self) -> None:
                length = int(self.headers.get("content-length") or 0)
                raw = self.rfile.read(length) if length else b""
                parts = urlsplit(self.path)
                call = Call(
                    self.command,
                    parts.path,
                    parse_qs(parts.query),
                    {k.lower(): v for k, v in self.headers.items()},
                    json.loads(raw) if raw else None,
                )
                node.calls.append(call)
                handler(call, self, len(node.calls))

            do_GET = do_POST = do_PUT = do_PATCH = do_DELETE = _handle

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), H)
        self.base_url = f"http://127.0.0.1:{self.server.server_port}"
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def close(self) -> None:
        self.server.shutdown()
        self.server.server_close()


def respond(
    h: BaseHTTPRequestHandler, status: int, body: Any, headers: Optional[Dict[str, str]] = None, content_type: str = "application/json"
) -> None:
    data = json.dumps(body).encode() if content_type == "application/json" else body
    h.send_response(status)
    h.send_header("Content-Type", content_type)
    h.send_header("Content-Length", str(len(data)))
    for k, v in (headers or {}).items():
        h.send_header(k, v)
    h.end_headers()
    h.wfile.write(data)


@pytest.fixture
def node() -> Any:
    holder: Dict[str, FakeNode] = {}

    def start(handler: Handler) -> FakeNode:
        holder["n"] = FakeNode(handler)
        return holder["n"]

    yield start
    if "n" in holder:
        holder["n"].close()


def test_issue_token_sends_api_key_and_body(node: Any) -> None:
    n = node(
        lambda c, h, i: respond(
            h,
            200,
            {
                "token": "jwt",
                "user_id": "11111111-0000-4000-8000-000000000001",
                "expires_at": "2030-01-01T00:00:00Z",
                "channels": [],
                "endpoint": {
                    "region": "eu_west",
                    "node_id": "22222222-0000-4000-8000-000000000002",
                    "ws_url": "wss://node/ws",
                    "probe_url": None,
                    "location": None,
                    "distance_km": None,
                },
            },
        )
    )
    client = AurixClient(n.base_url + "/", api_key="ak_test")
    tok = client.issue_token({"external_id": "player-1", "display_name": "Player", "region": "eu_west"})
    assert tok["token"] == "jwt"
    assert tok["endpoint"] is not None and tok["endpoint"]["ws_url"] == "wss://node/ws"
    call = n.calls[0]
    assert (call.method, call.path) == ("POST", "/v1/tokens")
    assert call.headers["x-api-key"] == "ak_test" and "authorization" not in call.headers
    assert call.body == {"external_id": "player-1", "display_name": "Player", "region": "eu_west"}
    assert "aurix-server-sdk-python" in call.headers["user-agent"]


def test_path_and_query_encoding(node: Any) -> None:
    n = node(lambda c, h, i: respond(h, 200, {"items": [], "next_cursor": None}))
    AurixClient(n.base_url, api_key="k").list_channel_messages("ch/1 x", limit=5)
    assert n.calls[0].path == "/v1/channels/ch%2F1%20x/messages"
    assert n.calls[0].query == {"limit": ["5"]}


def test_bearer_tokens_and_per_call_auth(node: Any) -> None:
    n = node(lambda c, h, i: respond(h, 200, {"items": [], "total": 0}))
    client = AurixClient(n.base_url, admin_token="admin-jwt")
    client.list_apps()
    assert n.calls[0].headers["authorization"] == "Bearer admin-jwt"
    from aurix_server import Credentials

    client.list_apps(options=RequestOptions(auth=Credentials(api_key="other")))
    assert "authorization" not in n.calls[1].headers and n.calls[1].headers["x-api-key"] == "other"


def test_error_envelope(node: Any) -> None:
    n = node(lambda c, h, i: respond(h, 403, {"error": {"code": "FORBIDDEN", "message": "app mismatch"}}, {"X-Request-Id": "req-1"}))
    client = AurixClient(n.base_url, api_key="k", max_retries=0)
    with pytest.raises(AurixError) as ei:
        client.get_channel("c1")
    err = ei.value
    assert (err.status, err.code, err.detail, err.request_id) == (403, "FORBIDDEN", "app mismatch", "req-1")
    assert err.is_auth and str(err) == "GET /v1/channels/c1 -> 403 FORBIDDEN: app mismatch"


def test_retries(node: Any) -> None:
    def handler(c: Call, h: BaseHTTPRequestHandler, i: int) -> None:
        if c.path == "/health":
            return respond(h, 503, {"error": {"code": "UNAVAILABLE", "message": "x"}}) if i == 1 else respond(h, 200, {"status": "ok"})
        if i == 3:
            return respond(h, 429, {"error": {"code": "RATE_LIMITED", "message": "slow"}}, {"Retry-After": "0"})
        respond(h, 200, {"token": "t", "user_id": "u", "expires_at": "x", "channels": [], "endpoint": None})

    n = node(handler)
    client = AurixClient(n.base_url, api_key="k", max_retries=2, max_backoff=0.01)
    assert client.health()["status"] == "ok"
    assert len(n.calls) == 2
    assert client.issue_token({"external_id": "u", "display_name": "U"})["token"] == "t"
    assert len(n.calls) == 4


def test_post_not_retried_on_503(node: Any) -> None:
    n = node(lambda c, h, i: respond(h, 503, {"error": {"code": "UNAVAILABLE", "message": "x"}}))
    client = AurixClient(n.base_url, api_key="k", max_retries=3, max_backoff=0.01)
    with pytest.raises(AurixError) as ei:
        client.issue_token({"external_id": "u", "display_name": "U"})
    assert ei.value.status == 503 and len(n.calls) == 1


def test_timeout_is_network_error(node: Any) -> None:
    import time

    n = node(lambda c, h, i: time.sleep(0.5))
    client = AurixClient(n.base_url, api_key="k", timeout=0.05, max_retries=0)
    with pytest.raises(AurixNetworkError):
        client.health()


def test_raw_and_typed_content_type(node: Any) -> None:
    n = node(lambda c, h, i: respond(h, 200, b"bucket_start,minutes\n", content_type="text/csv"))
    client = AurixClient(n.base_url, api_key="k")
    raw = client.export_usage_raw(format="csv")
    assert raw.status == 200 and raw.content_type == "text/csv" and raw.text().startswith("bucket_start")
    with pytest.raises(AurixError) as ei:
        client.export_usage(format="csv")
    assert ei.value.code == "unexpected_content_type"


def test_204_returns_none(node: Any) -> None:
    def handler(c: Call, h: BaseHTTPRequestHandler, i: int) -> None:
        h.send_response(204)
        h.send_header("Content-Length", "0")
        h.end_headers()

    n = node(handler)
    assert AurixClient(n.base_url, api_key="k").delete_channel("c1") is None


def test_async_facade(node: Any) -> None:
    n = node(lambda c, h, i: respond(h, 200, {"status": "ok"}))
    client = AsyncAurixClient(AurixClient(n.base_url, api_key="k"))
    assert asyncio.run(client.health())["status"] == "ok"


def test_webhook_signature_vector() -> None:
    secret, header, body, t = VECTOR["secret"], VECTOR["header"], VECTOR["body"], VECTOR["timestamp"]
    assert sign_webhook(secret, t, body) == header
    assert verify_webhook_signature(secret, header, body, now=t + 10)
    assert verify_webhook_signature(secret, header, body.encode(), now=t - 10)
    assert not verify_webhook_signature(secret, header, body + " ", now=t)
    assert not verify_webhook_signature("whsec_other", header, body, now=t)
    assert not verify_webhook_signature(secret, header, body, now=t + 301)
    assert not verify_webhook_signature(secret, header.replace("v1=", "v1=0"), body, now=t)
    assert not verify_webhook_signature(secret, "garbage", body, now=t)
    assert not verify_webhook_signature(secret, None, body)

    d = parse_webhook(
        secret,
        {"X-Aurix-Signature": header, "x-aurix-event": "participant.joined", "X-Aurix-Delivery-Id": "d1", "X-Aurix-Attempt": "2"},
        body,
        now=t,
    )
    assert d.event["type"] == "participant.joined" and d.event["data"]["user_id"] == "u1"
    assert (d.delivery_id, d.attempt) == ("d1", 2)
    with pytest.raises(WebhookVerificationError):
        parse_webhook(secret, {"X-Aurix-Signature": header, "X-Aurix-Event": "participant.left"}, body, now=t)
    with pytest.raises(WebhookVerificationError):
        parse_webhook("nope", {"X-Aurix-Signature": header}, body, now=t)


def test_sse_parser() -> None:
    raw = b': keepalive\n\nevent: stream.open\ndata: {"ok":true}\n\nid: 42\r\nevent: participant.joined\r\ndata: {"id":"e1",\r\ndata: "type":"x"}\r\n\r\nretry: 250\ndata: tail\n\n'
    events = list(parse_sse(raw.splitlines(keepends=True)))
    assert [(e.type, e.id, e.data, e.retry) for e in events] == [
        ("stream.open", None, '{"ok":true}', None),
        ("participant.joined", "42", '{"id":"e1",\n"type":"x"}', None),
        (None, None, "tail", 250),
    ]


def test_event_stream_resumes_with_last_event_id(node: Any) -> None:
    def handler(c: Call, h: BaseHTTPRequestHandler, i: int) -> None:
        if i == 1:
            data = b'event: stream.open\ndata: {}\n\nid: ev-1\nevent: participant.joined\ndata: {"id":"ev-1","type":"participant.joined","app_id":"a","created_at":"c","data":{}}\n\n'
        else:
            data = b'id: ev-2\nevent: participant.left\ndata: {"id":"ev-2","type":"participant.left","app_id":"a","created_at":"c","data":{}}\n\n'
        h.send_response(200)
        h.send_header("Content-Type", "text/event-stream")
        h.send_header("Content-Length", str(len(data)))
        h.end_headers()
        h.wfile.write(data)

    n = node(handler)
    client = AurixClient(n.base_url, api_key="k")
    seen: List[str] = []
    for ev in event_stream(
        client, types=["participant.joined", "participant.left"], reconnect_delay=0.005, should_stop=lambda: len(seen) >= 3
    ):
        seen.append(ev.type)
    assert seen == ["stream.open", "participant.joined", "participant.left"]
    assert n.calls[0].query == {"types": ["participant.joined,participant.left"]}
    assert n.calls[0].headers["x-api-key"] == "k" and n.calls[0].headers["accept"] == "text/event-stream"
    assert n.calls[1].headers["last-event-id"] == "ev-1"
