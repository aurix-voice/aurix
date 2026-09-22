#!/usr/bin/env python3
"""Browser test for the Godot Web export (AurixWebVoiceClient over the Aurix Web SDK).

Runs the exported lobby (sdk/godot/build/web, see scripts/build_web.sh) in headless Chromium via
Playwright. Two layers:

  * always: the Godot runtime boots, the GDScript client installs its JavaScriptBridge glue, loads
    aurix-web-sdk.js, reports SDK_READY and fails a connect to a dead endpoint with a clean
    error instead of hanging or crashing the engine;
  * with AURIX_API_URL / AURIX_WS_URL / AURIX_API_KEY: a live node — the Godot lobby joins a
    channel next to a plain Web SDK peer (same bundle, oscillator uplink); roster, speaking,
    chat both ways, per-participant stream layout, network quality, mute, leave and disconnect
    are checked end to end.

    python3 sdk/godot/tests/web/godot_web_e2e.py [--build DIR] [--headed]

Exit code 0 on success, 1 on the first failed check, 3 when the export is missing.
"""
import argparse
import http.server
import json
import os
import socketserver
import sys
import threading
import time
import urllib.request
from pathlib import Path

from playwright.sync_api import Page, sync_playwright

ROOT = Path(__file__).resolve().parents[4]
DEFAULT_BUILD = ROOT / "sdk" / "godot" / "build" / "web"

failures = 0


def check(cond: bool, what: str) -> None:
    global failures
    print(("  ok   " if cond else "  FAIL ") + what, flush=True)
    if not cond:
        failures += 1


class Quiet(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *_args) -> None:  # noqa: D401
        pass

    def end_headers(self) -> None:
        self.send_header("Cache-Control", "no-store")
        super().end_headers()


def serve(directory: Path) -> tuple[socketserver.TCPServer, int]:
    handler = lambda *a, **kw: Quiet(*a, directory=str(directory), **kw)  # noqa: E731
    httpd = socketserver.ThreadingTCPServer(("127.0.0.1", 0), handler)
    httpd.daemon_threads = True
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    return httpd, httpd.server_address[1]


def snapshot(page: Page) -> dict:
    raw = page.evaluate("() => globalThis.__aurixGodot ? JSON.stringify(globalThis.__aurixGodot) : null")
    return json.loads(raw) if raw else {}


def wait_for(page: Page, pred, what: str, timeout: float = 30.0) -> dict:
    deadline = time.time() + timeout
    snap: dict = {}
    while time.time() < deadline:
        snap = snapshot(page)
        if snap and pred(snap):
            check(True, what)
            return snap
        time.sleep(0.2)
    check(False, f"{what} (timeout; last snapshot: {json.dumps(snap)[:600]})")
    return snap


_cmd_id = 0


def command(page: Page, op: str, **kw) -> dict:
    global _cmd_id
    _cmd_id += 1
    cid = _cmd_id
    page.evaluate("(c) => { (globalThis.__aurixGodotCommands ||= []).push(c); }", {"op": op, "id": cid, **kw})
    snap = wait_for(page, lambda s: any(e["kind"] == "command" and e["data"].get("id") == cid for e in s["events"]), f"command {op} executed", 10)
    for e in snap.get("events", []):
        if e["kind"] == "command" and e["data"].get("id") == cid:
            return e["data"]
    return {}


def events(snap: dict, kind: str) -> list:
    return [e["data"] for e in snap.get("events", []) if e["kind"] == kind]


class Api:
    def __init__(self) -> None:
        self.url = os.environ["AURIX_API_URL"].rstrip("/")
        self.key = os.environ["AURIX_API_KEY"]

    def post(self, path: str, body: dict) -> dict:
        req = urllib.request.Request(
            self.url + path,
            data=json.dumps(body).encode(),
            headers={"content-type": "application/json", "authorization": "Bearer " + self.key},
        )
        with urllib.request.urlopen(req, timeout=10) as resp:
            return json.load(resp)

    def token(self, channel: str, name: str) -> tuple[str, str]:
        t = self.post(
            "/v1/tokens",
            {"external_id": f"godot-web-{name}-{int(time.time())}", "display_name": name,
             "channels": [{"channel_id": channel, "join": True, "speak": True, "receive": True}]},
        )
        return t["token"], t["user_id"]


PEER_BOOT = """
async ([token, channel, apiUrl, wsUrl]) => {
  const { AurixClient } = globalThis.AurixWebSdk;
  const ctx = new AudioContext(); await ctx.resume();
  const dst = ctx.createMediaStreamDestination();
  const osc = ctx.createOscillator(); osc.frequency.value = 330;
  const g = ctx.createGain(); g.gain.value = 0; osc.connect(g).connect(dst); osc.start();
  globalThis.__tone = (on) => { g.gain.value = on ? 0.6 : 0; };
  const c = new AurixClient({ apiUrl, wsUrl, token, localStream: dst.stream, participantStreams: 4 });
  globalThis.__peer = c; globalThis.__chat = []; globalThis.__speaking = [];
  c.on('chatMessage', (m) => globalThis.__chat.push({ from: m.fromUserId, text: m.text }));
  c.on('speaking', (channelId, userId, speaking) => globalThis.__speaking.push({ userId, speaking }));
  const info = await c.connect();
  const roster = await c.joinChannel(channel);
  return { userId: info.userId, roster: roster.map((p) => p.userId) };
}
"""


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--build", default=str(DEFAULT_BUILD))
    ap.add_argument("--headed", action="store_true")
    args = ap.parse_args()
    build = Path(args.build)
    if not (build / "index.html").exists() or not (build / "aurix-web-sdk.js").exists():
        print(f"export not found in {build}; run sdk/godot/scripts/build_web.sh first", file=sys.stderr)
        return 3

    live = all(os.environ.get(k) for k in ("AURIX_API_URL", "AURIX_WS_URL", "AURIX_API_KEY"))
    httpd, port = serve(build)
    base = f"http://127.0.0.1:{port}"
    console: list[str] = []
    try:
        with sync_playwright() as pw:
            browser = pw.chromium.launch(
                headless=not args.headed,
                args=[
                    "--autoplay-policy=no-user-gesture-required",
                    "--use-fake-device-for-media-stream",
                    "--use-fake-ui-for-media-stream",
                    "--enable-unsafe-swiftshader",
                ],
            )
            context = browser.new_context(permissions=["microphone"])
            godot = context.new_page()
            godot.on("console", lambda m: console.append(m.text))
            godot.on("pageerror", lambda e: console.append(f"PAGEERROR {e}"))

            # --- layer 1: boot + glue + SDK load + dead endpoint
            print("Godot Web export smoke")
            godot.goto(f"{base}/index.html?harness=1&ws=ws://127.0.0.1:9/ws&token=dead-token", wait_until="load")
            snap = wait_for(godot, lambda s: s.get("sdk") == 2, "Godot runtime booted and Web SDK loaded (SDK_READY)", 90)
            check(bool(snap.get("sdk_version")), f"SDK version visible to GDScript: {snap.get('sdk_version')}")
            snap = wait_for(godot, lambda s: s["state"] in (5, 0), "connect to a dead endpoint settles (failed/disconnected)", 60)
            check(snap["state"] == 5, f"state is STATE_FAILED with a reason: {snap.get('error')!r}")
            check(any(e["kind"] in ("failed", "request_failed") for e in snap["events"]), "failure surfaced as a signal")
            check(not any("PAGEERROR" in c for c in console), "no uncaught page errors")
            check(not any("Failed to load script" in c or "Parse Error" in c for c in console), "no GDScript load/parse errors in console")

            if not live:
                print("live checks skipped: set AURIX_API_URL, AURIX_WS_URL, AURIX_API_KEY to run against a node")
                return 0 if failures == 0 else 1

            # --- layer 2: live node
            print("Godot Web export against live node")
            api = Api()
            ws_url = os.environ["AURIX_WS_URL"]
            channel = api.post("/v1/channels", {"name": f"godot-web-{int(time.time())}", "config": {"channel_type": "team"}})["id"]
            godot_token, godot_uid = api.token(channel, "Godot")
            peer_token, peer_uid = api.token(channel, "Peer")

            godot.goto(
                f"{base}/index.html?harness=1&ws={ws_url}&api={api.url}&token={godot_token}&channel={channel}",
                wait_until="load",
            )
            snap = wait_for(godot, lambda s: s["state"] == 3, "Godot lobby connected + media bound", 90)
            check(snap["session"].get("user_id") == godot_uid, "session user id matches the issued token")
            check(snap["session"].get("media_webrtc") is True and snap["session"].get("participant_stream_cap", 0) >= 1, "session advertises WebRTC + stream cap")
            expected_transport = "webtransport" if snap["session"].get("media_webtransport") else "webrtc"
            res = command(godot, "join", channel=channel)
            check(isinstance(res.get("result"), (int, float)) and res["result"] > 0, "join_channel returned a request id")
            snap = wait_for(godot, lambda s: s["channel"] == channel, "channel_joined")
            joined = events(snap, "joined")
            check(bool(joined) and joined[-1]["role"] == 1, f"joined as speaker: {joined[-1] if joined else None}")

            peer = context.new_page()
            peer.goto(f"{base}/index.html", wait_until="load")
            peer.add_script_tag(url=f"{base}/aurix-web-sdk.js")
            peer.wait_for_function("() => globalThis.AurixWebSdk && globalThis.AurixWebSdk.AurixClient")
            boot = peer.evaluate(PEER_BOOT, [peer_token, channel, api.url, ws_url])
            check(godot_uid in boot["roster"], "peer sees the Godot lobby in the roster")
            snap = wait_for(godot, lambda s: any(p["user_id"] == peer_uid for p in s["participants"]), "Godot roster shows the peer (participant_joined)")
            peer_entry = next(p for p in snap["participants"] if p["user_id"] == peer_uid)
            check(peer_entry["display_name"] == "Peer" and peer_entry["role"] == 1, f"participant dictionary: {peer_entry}")
            wait_for(godot, lambda s: any(x["user_id"] == peer_uid for x in s["streams"]), "per-participant stream layout lists the peer", 30)

            peer.evaluate("() => globalThis.__tone(true)")
            wait_for(godot, lambda s: any(e["user_id"] == peer_uid and e["speaking"] for e in events(s, "speaking")), "participant_speaking(true) for the peer", 30)
            peer.evaluate("() => globalThis.__tone(false)")
            wait_for(godot, lambda s: any(e["user_id"] == peer_uid and not e["speaking"] for e in events(s, "speaking")), "participant_speaking(false) after silence", 30)

            res = command(godot, "chat", text="hello from godot")
            check(isinstance(res.get("result"), (int, float)) and res["result"] > 0, "send_chat returned a request id")
            deadline = time.time() + 15
            got = []
            while time.time() < deadline and not got:
                got = [m for m in peer.evaluate("() => globalThis.__chat") if m["text"] == "hello from godot"]
                time.sleep(0.2)
            check(bool(got) and got[0]["from"] == godot_uid, "peer received the chat message from Godot")
            peer.evaluate("(c) => globalThis.__peer.sendMessage(c, 'hello from peer')", channel)
            snap = wait_for(godot, lambda s: any(m["text"] == "hello from peer" for m in events(s, "chat")), "Godot received the peer's chat message")
            msg = next(m for m in events(snap, "chat") if m["text"] == "hello from peer")
            check(msg["sender_id"] == peer_uid and msg["sender_name"] == "Peer" and msg["sent_at_ms"] > 0, f"chat dictionary uses native keys: {sorted(msg)}")

            snap = wait_for(godot, lambda s: bool(events(s, "quality")), "network_quality event arrived", 45)
            q = (events(snap, "quality") or [{}])[-1]
            check(1 <= q.get("bars", 0) <= 5 and "mos" in q and "rtt_ms" in q, f"quality dictionary: bars={q.get('bars')} mos={q.get('mos')}")
            res = command(godot, "stats")
            check(isinstance(res.get("result"), dict) and "rtt_ms" in res["result"], "get_stats returns a snake_case dictionary")
            check(res["result"].get("transport") == expected_transport, f"media runs over the advertised transport ({expected_transport})")

            res = command(godot, "mute", on=True)
            check(res.get("result") is True, "set_muted(true) reflected by is_muted")
            wait_for(godot, lambda s: s["muted"] is True, "snapshot shows muted")
            command(godot, "mute", on=False)
            res = command(godot, "volume", user_id=peer_uid, volume=0.5)
            check(res.get("result") == 0, "set_participant_volume ok")
            res = command(godot, "pin", user_ids=[peer_uid])
            check(res.get("result") == 0, "set_pinned_participants ok")

            res = command(godot, "leave")
            check(res.get("result") == 0, "leave_channel ok")
            wait_for(godot, lambda s: s["channel"] == "", "channel_left clears the lobby")
            peer_roster = peer.evaluate("(c) => globalThis.__peer.participants(c).map((p) => p.userId)", channel)
            deadline = time.time() + 10
            while time.time() < deadline and godot_uid in peer_roster:
                time.sleep(0.3)
                peer_roster = peer.evaluate("(c) => globalThis.__peer.participants(c).map((p) => p.userId)", channel)
            check(godot_uid not in peer_roster, "peer roster no longer lists Godot after leave")
            command(godot, "disconnect")
            wait_for(godot, lambda s: s["state"] == 0, "disconnect → STATE_DISCONNECTED")
            check(not any("PAGEERROR" in c for c in console), "no uncaught page errors during the live run")
            peer.evaluate("() => globalThis.__peer.disconnect()")
            return 0 if failures == 0 else 1
    finally:
        httpd.shutdown()
        if failures:
            print("--- browser console (tail) ---")
            for line in console[-40:]:
                print("  " + line)
        print(f"godot web e2e: {'all checks passed' if failures == 0 else f'{failures} check(s) failed'}")


if __name__ == "__main__":
    sys.exit(main())
