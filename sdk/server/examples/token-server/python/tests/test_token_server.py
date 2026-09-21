import json
import logging
import threading
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any, Dict, Iterator, List, Optional, Tuple

import pytest

from token_server import authenticate_player, create_token_server, load_config, mint_dev_session

API_KEY = "aurx_test_SECRET_KEY_never_in_client_payload"
SECRET = "0123456789abcdef0123456789abcdef"


class FakeAurix(ThreadingHTTPServer):
    seen: List[Dict[str, Any]]


def _fake_aurix() -> FakeAurix:
    class H(BaseHTTPRequestHandler):
        def log_message(self, *_: Any) -> None:
            pass

        def do_POST(self) -> None:  # noqa: N802
            raw = self.rfile.read(int(self.headers.get("Content-Length") or 0))
            srv: FakeAurix = self.server  # type: ignore[assignment]
            srv.seen.append({"path": self.path, "api_key": self.headers.get("X-API-Key"), "body": json.loads(raw or b"null")})
            status: int
            body: Dict[str, Any]
            if self.path != "/v1/tokens":
                status, body = 404, {"error": {"code": "NOT_FOUND", "message": "nope"}}
            elif self.headers.get("X-API-Key") != API_KEY:
                status, body = 401, {"error": {"code": "AUTH_FAILED", "message": "bad key"}}
            else:
                status, body = (
                    200,
                    {
                        "token": "player.jwt",
                        "user_id": "u-1",
                        "expires_at": "2030-01-01T00:00:00Z",
                        "channels": [],
                        "endpoint": {
                            "region": "eu_west",
                            "node_id": "n1",
                            "ws_url": "wss://eu1.example/ws",
                            "nodes": 1,
                            "load_factor": 0.1,
                        },
                        "api_key_echo": API_KEY,
                    },
                )
            data = json.dumps(body).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

    srv = FakeAurix(("127.0.0.1", 0), H)
    srv.seen = []
    return srv


@pytest.fixture()
def stack() -> Iterator[Tuple[FakeAurix, str]]:
    aurix = _fake_aurix()
    cfg = load_config(
        {
            "AURIX_URL": f"http://127.0.0.1:{aurix.server_address[1]}",
            "AURIX_API_KEY": API_KEY,
            "GAME_SESSION_SECRET": SECRET,
            "AURIX_REGION": "eu_west",
            "ALLOW_DEV_LOGIN": "1",
        }
    )
    ts = create_token_server(cfg, host="127.0.0.1", port=0)
    threads = [threading.Thread(target=s.serve_forever, daemon=True) for s in (aurix, ts)]
    for t in threads:
        t.start()
    try:
        yield aurix, f"http://127.0.0.1:{ts.server_address[1]}"
    finally:
        for s in (aurix, ts):
            s.shutdown()
            s.server_close()


def post(url: str, body: Dict[str, Any], session: Optional[str] = None) -> Tuple[int, Dict[str, Any]]:
    headers = {"Content-Type": "application/json"}
    if session:
        headers["Authorization"] = f"Bearer {session}"
    req = urllib.request.Request(url, data=json.dumps(body).encode(), headers=headers, method="POST")
    try:
        with urllib.request.urlopen(req, timeout=5) as r:
            return r.status, json.loads(r.read())
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read())


def test_config_refuses_unsafe_startup() -> None:
    with pytest.raises(ValueError, match="AURIX_API_KEY"):
        load_config({"GAME_SESSION_SECRET": SECRET})
    with pytest.raises(ValueError, match="GAME_SESSION_SECRET"):
        load_config({"AURIX_API_KEY": API_KEY, "GAME_SESSION_SECRET": "short"})
    with pytest.raises(ValueError, match="AURIX_REGION"):
        load_config({"AURIX_API_KEY": API_KEY, "GAME_SESSION_SECRET": SECRET, "AURIX_REGION": "eu"})


def test_game_session_rejects_forged_and_expired() -> None:
    good = mint_dev_session(SECRET, "p1", "Alice")
    p = authenticate_player(SECRET, f"Bearer {good}")
    assert p is not None and (p.player_id, p.display_name) == ("p1", "Alice")
    assert authenticate_player("x" * 32, f"Bearer {good}") is None
    assert authenticate_player(SECRET, f"Bearer {good.split('.')[0]}.AAAA") is None
    assert authenticate_player(SECRET, f"Bearer {mint_dev_session(SECRET, 'p1', 'Alice', ttl_sec=-1)}") is None
    assert authenticate_player(SECRET, None) is None


def test_token_requires_session(stack: Tuple[FakeAurix, str]) -> None:
    aurix, url = stack
    status, _ = post(f"{url}/voice/token", {"match_id": "m1", "external_id": "admin"})
    assert status == 401
    assert aurix.seen == []


def test_happy_path_hides_api_key(stack: Tuple[FakeAurix, str]) -> None:
    aurix, url = stack
    status, login = post(f"{url}/dev/login", {"player_id": "p1", "display_name": "Alice"})
    assert status == 200
    status, body = post(f"{url}/voice/token", {"match_id": "m1", "external_id": "spoof", "channels": ["*"]}, login["session"])
    assert status == 200
    assert body == {
        "token": "player.jwt",
        "user_id": "u-1",
        "expires_at": "2030-01-01T00:00:00Z",
        "endpoint": {"ws_url": "wss://eu1.example/ws", "region": "eu_west"},
    }
    assert API_KEY not in json.dumps(body)
    seen = aurix.seen[-1]
    assert seen["path"] == "/v1/tokens"
    assert seen["api_key"] == API_KEY
    assert seen["body"]["external_id"] == "p1"
    assert seen["body"]["display_name"] == "Alice"
    assert seen["body"]["region"] == "eu_west"
    assert seen["body"]["channels"] == [
        {"ad_hoc": {"name": "match-m1", "channel_type": "team"}, "join": True, "speak": True, "receive": True}
    ]


def test_invalid_match_refused_before_aurix(stack: Tuple[FakeAurix, str]) -> None:
    aurix, url = stack
    status, _ = post(f"{url}/voice/token", {"match_id": "../etc"}, mint_dev_session(SECRET, "p1", "Alice"))
    assert status == 403
    assert aurix.seen == []


def test_aurix_error_is_generic_and_logged(caplog: pytest.LogCaptureFixture) -> None:
    aurix = _fake_aurix()
    cfg = load_config(
        {"AURIX_URL": f"http://127.0.0.1:{aurix.server_address[1]}", "AURIX_API_KEY": "aurx_wrong_key", "GAME_SESSION_SECRET": SECRET}
    )
    ts = create_token_server(cfg, host="127.0.0.1", port=0)
    for s in (aurix, ts):
        threading.Thread(target=s.serve_forever, daemon=True).start()
    try:
        with caplog.at_level(logging.ERROR, logger="token_server"):
            status, body = post(
                f"http://127.0.0.1:{ts.server_address[1]}/voice/token", {"match_id": "m1"}, mint_dev_session(SECRET, "p1", "A")
            )
        assert (status, body) == (502, {"error": "voice service unavailable"})
        assert "aurix 401 AUTH_FAILED" in caplog.text
        assert "aurx_wrong_key" not in caplog.text
    finally:
        for s in (aurix, ts):
            s.shutdown()
            s.server_close()
