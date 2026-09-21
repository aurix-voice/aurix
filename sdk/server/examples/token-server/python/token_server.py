"""Backend-only Aurix token server (Python >= 3.9, stdlib http.server + aurix_server).

    game client --(game session)--> POST /voice/token --> this server --(API key)--> Aurix POST /v1/tokens
    game client <-- { token, user_id, expires_at, endpoint } <---------------------------'

The API key lives only in this process' environment. Clients never see it, never choose their own
player id (that comes from the game session) and never choose their grants.
"""

from __future__ import annotations

import base64
import hashlib
import hmac
import json
import logging
import os
import re
import sys
import time
from dataclasses import dataclass
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any, Dict, Mapping, Optional, cast

from aurix_server import AurixClient, AurixError, AurixNetworkError
from aurix_server.types import ChannelGrant, GenerateTokenRequest, Region

REGIONS = ("us_east", "us_west", "eu_west", "eu_central", "asia_pacific", "south_america", "australia", "middle_east", "africa")
MAX_BODY = 4096
MATCH_ID = re.compile(r"^[A-Za-z0-9_-]{1,64}$")
log = logging.getLogger("token_server")


@dataclass(frozen=True)
class Config:
    aurix_url: str
    api_key: str
    session_secret: str
    region: Optional[Region]
    port: int
    allow_dev_login: bool


def load_config(env: Mapping[str, str] = os.environ) -> Config:
    """Reads configuration from the environment; raises on anything unsafe or missing."""
    key_file = env.get("AURIX_API_KEY_FILE")
    if key_file:
        with open(key_file, encoding="utf-8") as f:
            api_key = f.read().strip()
    else:
        api_key = env.get("AURIX_API_KEY", "").strip()
    if not api_key:
        raise ValueError("set AURIX_API_KEY or AURIX_API_KEY_FILE (backend environment only)")
    secret = env.get("GAME_SESSION_SECRET", "")
    if len(secret) < 32:
        raise ValueError("GAME_SESSION_SECRET must be >= 32 characters")
    region = env.get("AURIX_REGION") or None
    if region is not None and region not in REGIONS:
        raise ValueError(f"AURIX_REGION must be one of {', '.join(REGIONS)}")
    return Config(
        aurix_url=env.get("AURIX_URL", "http://localhost:8080"),
        api_key=api_key,
        session_secret=secret,
        region=cast(Optional[Region], region),
        port=int(env.get("PORT", "3000")),
        allow_dev_login=env.get("ALLOW_DEV_LOGIN") == "1",
    )


# -------------------------------------------------------------------------------------------------
# Game session - stand-in for your real login. Replace `authenticate_player` with your own
# session/JWT validation; what matters is that the player id comes from *your* auth, not the body.
# -------------------------------------------------------------------------------------------------


@dataclass(frozen=True)
class Player:
    player_id: str
    display_name: str


def _b64u(data: bytes) -> str:
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode("ascii")


def _unb64u(s: str) -> bytes:
    return base64.urlsafe_b64decode(s + "=" * (-len(s) % 4))


def mint_dev_session(secret: str, player_id: str, display_name: str, ttl_sec: int = 3600) -> str:
    payload = _b64u(json.dumps({"pid": player_id, "name": display_name, "exp": int(time.time()) + ttl_sec}).encode())
    sig = hmac.new(secret.encode(), payload.encode(), hashlib.sha256).digest()
    return f"{payload}.{_b64u(sig)}"


def authenticate_player(secret: str, authorization: Optional[str]) -> Optional[Player]:
    m = re.match(r"^Bearer\s+([A-Za-z0-9_-]+)\.([A-Za-z0-9_-]+)$", authorization or "")
    if not m:
        return None
    expected = hmac.new(secret.encode(), m.group(1).encode(), hashlib.sha256).digest()
    try:
        given = _unb64u(m.group(2))
    except ValueError:
        return None
    if not hmac.compare_digest(given, expected):
        return None
    try:
        claims = json.loads(_unb64u(m.group(1)))
    except ValueError:
        return None
    pid, name, exp = claims.get("pid"), claims.get("name"), claims.get("exp")
    if not isinstance(pid, str) or not isinstance(name, str) or not isinstance(exp, int):
        return None
    if exp <= int(time.time()):
        return None
    return Player(player_id=pid, display_name=name)


# -------------------------------------------------------------------------------------------------
# Token issuance
# -------------------------------------------------------------------------------------------------


def player_may_join(_player: Player, match_id: str) -> bool:
    """Game-side authorisation: may this player join this match's voice? (stub: every match)"""
    return bool(MATCH_ID.match(match_id))


def issue_voice_token(aurix: AurixClient, player: Player, match_id: str, region: Optional[Region]) -> Dict[str, Any]:
    """Calls Aurix and returns ONLY what the game client needs (new server fields must be opted in here)."""
    grant: ChannelGrant = {"ad_hoc": {"name": f"match-{match_id}", "channel_type": "team"}, "join": True, "speak": True, "receive": True}
    body: GenerateTokenRequest = {"external_id": player.player_id, "display_name": player.display_name, "channels": [grant]}
    if region is not None:
        body["region"] = region
    res = aurix.issue_token(body)
    endpoint = res["endpoint"]
    return {
        "token": res["token"],
        "user_id": res["user_id"],
        "expires_at": res["expires_at"],
        "endpoint": {"ws_url": endpoint["ws_url"], "region": endpoint["region"]} if endpoint else None,
    }


# -------------------------------------------------------------------------------------------------
# HTTP
# -------------------------------------------------------------------------------------------------


class HttpError(Exception):
    def __init__(self, status: int, message: str):
        super().__init__(message)
        self.status = status


def make_handler(cfg: Config, aurix: AurixClient) -> type:
    class Handler(BaseHTTPRequestHandler):
        server_version = "aurix-token-server/1.2"

        def log_message(self, fmt: str, *args: Any) -> None:  # quiet default access log
            log.debug(fmt, *args)

        def _send(self, status: int, body: Dict[str, Any]) -> None:
            data = json.dumps(body).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Cache-Control", "no-store")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

        def _read_json(self) -> Dict[str, Any]:
            length = int(self.headers.get("Content-Length") or 0)
            if length > MAX_BODY:
                raise HttpError(413, "body too large")
            raw = self.rfile.read(length) if length else b"{}"
            try:
                parsed = json.loads(raw or b"{}")
            except ValueError:
                raise HttpError(400, "invalid JSON") from None
            if not isinstance(parsed, dict):
                raise HttpError(400, "invalid JSON")
            return parsed

        def do_GET(self) -> None:  # noqa: N802 - http.server naming
            if self.path == "/healthz":
                self._send(200, {"ok": True})
            else:
                self._send(404, {"error": "not found"})

        def do_POST(self) -> None:  # noqa: N802 - http.server naming
            try:
                if self.path == "/dev/login" and cfg.allow_dev_login:
                    body = self._read_json()
                    pid, name = body.get("player_id"), body.get("display_name")
                    if not isinstance(pid, str) or not isinstance(name, str):
                        raise HttpError(400, "player_id and display_name required")
                    self._send(200, {"session": mint_dev_session(cfg.session_secret, pid, name)})
                    return
                if self.path == "/voice/token":
                    player = authenticate_player(cfg.session_secret, self.headers.get("Authorization"))
                    if player is None:
                        raise HttpError(401, "not logged in")
                    match_id = self._read_json().get("match_id")
                    if not isinstance(match_id, str) or not player_may_join(player, match_id):
                        raise HttpError(403, "not allowed to join this match")
                    self._send(200, issue_voice_token(aurix, player, match_id, cfg.region))
                    return
                raise HttpError(404, "not found")
            except HttpError as e:
                self._send(e.status, {"error": str(e)})
            except AurixError as e:
                # Aurix' message may describe our request; log it server-side, never forward it verbatim.
                log.error("aurix %s %s (request %s)", e.status, e.code, e.request_id or "-")
                self._send(503 if e.status == 429 else 502, {"error": "voice service unavailable"})
            except AurixNetworkError as e:
                log.error("aurix unreachable: %s", e)
                self._send(503, {"error": "voice service unavailable"})
            except Exception:
                log.exception("unhandled")
                self._send(500, {"error": "internal error"})

    return Handler


def create_token_server(
    cfg: Config, aurix: Optional[AurixClient] = None, host: str = "0.0.0.0", port: Optional[int] = None
) -> ThreadingHTTPServer:
    client = aurix or AurixClient(cfg.aurix_url, api_key=cfg.api_key)
    return ThreadingHTTPServer((host, cfg.port if port is None else port), make_handler(cfg, client))


def main() -> int:
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
    cfg = load_config()
    srv = create_token_server(cfg)
    log.info("token server on :%d -> %s%s", cfg.port, cfg.aurix_url, " (dev login enabled)" if cfg.allow_dev_login else "")
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
